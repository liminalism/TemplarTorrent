//! Linux deletion through `openat` / `unlinkat`.
//!
//! Each directory component is opened with `O_NOFOLLOW | O_DIRECTORY`, and the
//! final name is stat'd and unlinked with `AT_SYMLINK_NOFOLLOW`. A symlink
//! cannot become the removal target. The inode must still match the handle
//! captured while the job file was open, and `nlink` must be 1.
#![allow(unsafe_code)]

use std::{
    ffi::{CString, OsStr},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
};

use super::{JobFiles, StorageError, io_error};

pub(super) struct PlannedFile {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

pub(super) fn plan(job: &JobFiles) -> Result<Vec<PlannedFile>, StorageError> {
    if !job.root.is_absolute() {
        return Err(StorageError::Io(
            "storage root must be an absolute directory".into(),
        ));
    }
    let mut planned = Vec::with_capacity(job.files.len());
    for file in &job.files {
        // Pad files were never created, so there is nothing to unlink.
        let Some(handle) = &file.handle else {
            continue;
        };
        if file.path == job.root || !file.path.starts_with(&job.root) {
            return Err(StorageError::IdentityChanged {
                path: file.path.display().to_string(),
            });
        }
        let live = handle
            .metadata()
            .map_err(|error| io_error(&file.path, error))?;
        let on_path = match std::fs::symlink_metadata(&file.path) {
            Ok(meta) => meta,
            Err(error) => return Err(io_error(&file.path, error)),
        };
        let path_text = file.path.display().to_string();
        if on_path.file_type().is_symlink() {
            return Err(StorageError::Symlink { path: path_text });
        }
        if !on_path.is_file() {
            return Err(StorageError::Io(format!(
                "{path_text}: torrent path is no longer a regular file"
            )));
        }
        if live.nlink() > 1 || on_path.nlink() > 1 {
            return Err(StorageError::Hardlink { path: path_text });
        }
        if on_path.dev() != live.dev() || on_path.ino() != live.ino() {
            return Err(StorageError::IdentityChanged { path: path_text });
        }
        planned.push(PlannedFile {
            path: file.path.clone(),
            dev: live.dev(),
            ino: live.ino(),
        });
    }
    Ok(planned)
}

pub(super) fn unlink_matched(
    root: &Path,
    path: &Path,
    dev: u64,
    ino: u64,
) -> Result<(), StorageError> {
    if path == root || !path.starts_with(root) {
        return Err(StorageError::IdentityChanged {
            path: path.display().to_string(),
        });
    }
    let (dir, name) = open_parent(root, path)?;
    let stat = fstat_nofollow(dir.as_raw_fd(), &name, path)?;
    let mode = stat.st_mode & libc::S_IFMT;
    let path_text = path.display().to_string();
    if mode == libc::S_IFLNK {
        return Err(StorageError::Symlink { path: path_text });
    }
    if mode != libc::S_IFREG {
        return Err(StorageError::Io(format!(
            "{path_text}: refusing to unlink a non-regular file"
        )));
    }
    if stat.st_nlink > 1 {
        return Err(StorageError::Hardlink { path: path_text });
    }
    if stat.st_dev != dev || stat.st_ino != ino {
        return Err(StorageError::IdentityChanged { path: path_text });
    }
    let c_name = cstring(&name)?;
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), c_name.as_ptr(), 0) };
    if rc != 0 {
        return Err(StorageError::Io(format!(
            "{path_text}: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Remove empty directories that used to contain deleted files. Never removes
/// `root`. A symlink or a non-empty directory is left in place.
pub(super) fn prune_empty_parents(root: &Path, deleted: &[PathBuf]) {
    let mut dirs = Vec::new();
    for path in deleted {
        let mut parent = path.parent();
        while let Some(dir) = parent {
            if dir == root || !dir.starts_with(root) {
                break;
            }
            dirs.push(dir.to_path_buf());
            parent = dir.parent();
        }
    }
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    dirs.dedup();
    for dir in dirs {
        let Ok((parent_fd, name)) = open_parent(root, &dir) else {
            continue;
        };
        let Ok(c_name) = cstring(&name) else {
            continue;
        };
        let rc =
            unsafe { libc::unlinkat(parent_fd.as_raw_fd(), c_name.as_ptr(), libc::AT_REMOVEDIR) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOTEMPTY)
                || err.raw_os_error() == Some(libc::ENOENT)
            {
                continue;
            }
        }
    }
}

/// Open `root` without following it if it is itself a symlink. Ancestors are
/// the kernel's normal path walk so an OS temp directory that is a symlink
/// still reaches the real job directory.
fn open_directory(path: &Path) -> Result<OwnedFd, StorageError> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| StorageError::Io(format!("{}: path contains NUL", path.display())))?;
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned(fd, path)
}

fn open_parent(root: &Path, path: &Path) -> Result<(OwnedFd, std::ffi::OsString), StorageError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| StorageError::IdentityChanged {
            path: path.display().to_string(),
        })?;
    let parent = relative.parent().unwrap_or(Path::new(""));
    let name = relative
        .file_name()
        .ok_or_else(|| StorageError::IdentityChanged {
            path: path.display().to_string(),
        })?
        .to_os_string();
    let mut fd = open_directory(root)?;
    for component in parent.components() {
        match component {
            Component::Normal(part) => fd = open_child(&fd, part, path)?,
            Component::CurDir => {}
            _ => {
                return Err(StorageError::Io(format!(
                    "{}: refusing a non-normal directory component",
                    path.display()
                )));
            }
        }
    }
    Ok((fd, name))
}

fn open_child(parent: &OwnedFd, name: &OsStr, full: &Path) -> Result<OwnedFd, StorageError> {
    let c_name = cstring(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned(fd, full)
}

fn fstat_nofollow(dir: i32, name: &OsStr, path: &Path) -> Result<libc::stat, StorageError> {
    let c_name = cstring(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            dir,
            c_name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(StorageError::Io(format!(
            "{}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(unsafe { stat.assume_init() })
}

fn owned(fd: i32, path: &Path) -> Result<OwnedFd, StorageError> {
    if fd < 0 {
        return Err(StorageError::Io(format!(
            "{}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn cstring(name: &OsStr) -> Result<CString, StorageError> {
    CString::new(name.as_bytes()).map_err(|_| StorageError::Io("path contains NUL".into()))
}
