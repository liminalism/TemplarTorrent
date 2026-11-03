//! Linux handle-relative file access: opening payload files and deleting
//! them through `openat` / `mkdirat` / `unlinkat`.
//!
//! Each directory component is opened with `O_NOFOLLOW | O_DIRECTORY` from
//! the previous directory's descriptor, so no component is ever re-walked by
//! the kernel from a path string between a check and a use. A payload file is
//! created and opened with `O_NOFOLLOW` on its parent descriptor and then
//! checked by `fstat` to be a regular file. For deletion the final name is
//! stat'd and unlinked with `AT_SYMLINK_NOFOLLOW`; a symlink cannot become
//! the removal target, the inode must still match the handle captured while
//! the job file was open, and `nlink` must be 1.
#![allow(unsafe_code)]

use std::{
    ffi::{CString, OsStr},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
};

use super::{JobFiles, StorageError, io_error};

/// Open (creating when missing) the payload file `parts` names under `root`,
/// handle-relative: every directory is opened `O_NOFOLLOW | O_DIRECTORY` from
/// its parent's descriptor (and created with `mkdirat` when absent), and the
/// file is opened `O_RDWR | O_CREAT | O_NOFOLLOW` on the last directory's
/// descriptor. A symlink at any level, including one planted between two
/// steps, fails the open instead of being followed. The returned path is for
/// labels and later identity checks only.
pub(super) fn open_payload(
    root: &Path,
    parts: &[String],
) -> Result<(std::fs::File, PathBuf), StorageError> {
    let (name, directories) = parts
        .split_last()
        .ok_or_else(|| StorageError::Io("torrent file path is empty".into()))?;
    let mut path = root.to_path_buf();
    let mut dir = open_directory(root)?;
    for part in directories {
        path.push(part);
        let c_part = cstring(OsStr::new(part))?;
        dir = match try_open_child(&dir, &c_part) {
            Ok(next) => next,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                let rc = unsafe { libc::mkdirat(dir.as_raw_fd(), c_part.as_ptr(), 0o755) };
                if rc != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(StorageError::Io(format!("{}: {error}", path.display())));
                    }
                }
                try_open_child(&dir, &c_part)
                    .map_err(|error| classify_open_error(error, &dir, &c_part, &path))?
            }
            Err(error) => return Err(classify_open_error(error, &dir, &c_part, &path)),
        };
    }
    path.push(name);
    let c_name = cstring(OsStr::new(name))?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o644,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(libc::ELOOP) {
            StorageError::Symlink {
                path: path.display().to_string(),
            }
        } else {
            StorageError::Io(format!("{}: {error}", path.display()))
        });
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let meta = file.metadata().map_err(|error| io_error(&path, error))?;
    if !meta.is_file() {
        return Err(StorageError::Io(format!(
            "{}: torrent file path is not a file",
            path.display()
        )));
    }
    Ok((file, path))
}

/// `openat(O_DIRECTORY | O_NOFOLLOW)` of one child, with the raw error so
/// the caller can tell a missing directory from a symlink or a file.
fn try_open_child(parent: &OwnedFd, c_name: &CString) -> Result<OwnedFd, std::io::Error> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A failed directory open, named. Linux answers `ENOTDIR` for a symlink
/// opened with `O_DIRECTORY | O_NOFOLLOW` (and `ELOOP` on some paths), so
/// the name is stat'd without following to tell a link from a plain file.
fn classify_open_error(
    error: std::io::Error,
    parent: &OwnedFd,
    c_name: &CString,
    path: &Path,
) -> StorageError {
    if matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let rc = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                c_name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        let is_link =
            rc == 0 && unsafe { stat.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFLNK;
        return if is_link || error.raw_os_error() == Some(libc::ELOOP) {
            StorageError::Symlink {
                path: path.display().to_string(),
            }
        } else {
            StorageError::Io(format!(
                "{}: parent path is not a directory",
                path.display()
            ))
        };
    }
    StorageError::Io(format!("{}: {error}", path.display()))
}

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

/// Unlink one regular file under `root` by name, never following a symlink
/// at any component, refusing a hardlinked or non-regular target. `Ok(false)`
/// when nothing is at that name. Used for files a job owns outright (an HTTP
/// download and its temporary sibling), where no handle-captured inode exists.
pub(super) fn unlink_regular(root: &Path, path: &Path) -> Result<bool, StorageError> {
    if path == root || !path.starts_with(root) {
        return Err(StorageError::IdentityChanged {
            path: path.display().to_string(),
        });
    }
    let (dir, name) = open_parent(root, path)?;
    let c_name = cstring(&name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c_name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(false);
        }
        return Err(StorageError::Io(format!("{}: {error}", path.display())));
    }
    let stat = unsafe { stat.assume_init() };
    let path_text = path.display().to_string();
    let mode = stat.st_mode & libc::S_IFMT;
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
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), c_name.as_ptr(), 0) };
    if rc != 0 {
        return Err(StorageError::Io(format!(
            "{path_text}: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(true)
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
