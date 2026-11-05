//! Windows payload open and deletion, with the same refusals as the Linux
//! `openat` module: no symlink or junction is followed under the job root,
//! a hardlinked file is never deleted, and a file is only deleted while its
//! identity (volume serial number + file index) still matches the handle the
//! job held.
//!
//! Windows has no `openat`, so components are resolved by path, but every
//! directory from the root down is first *pinned*: opened with
//! `FILE_FLAG_OPEN_REPARSE_POINT` (so a link is opened as itself and refused,
//! never traversed) and without `FILE_SHARE_DELETE`, which makes the kernel
//! refuse any rename or delete of that directory while the handle is held.
//! With the whole chain pinned, a later path walk through it reaches the same
//! directories that were checked. The final file is opened with
//! `FILE_FLAG_OPEN_REPARSE_POINT` and checked on the handle. Deletion marks
//! the checked handle itself delete-on-close (`FileDispositionInfo`), so the
//! object whose identity was compared is the object removed.
#![allow(unsafe_code)]

use std::{
    fs::{File, OpenOptions},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Component, Path, PathBuf},
};

use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileDispositionInfo, GetFileInformationByHandle, SetFileInformationByHandle,
};

use super::{Identity, JobFiles, StorageError, io_error};

/// Handle budget on Windows. Kernel handles are limited only by memory
/// (about 16 million per process), so this is a policy cap that matches the
/// Unix ceiling `raise_open_file_limit` aims for.
const WINDOWS_HANDLE_LIMIT: u64 = 65_536;

pub(super) fn open_file_limit() -> u64 {
    WINDOWS_HANDLE_LIMIT
}

pub(super) fn raise_open_file_limit() -> u64 {
    WINDOWS_HANDLE_LIMIT
}

/// Open a directory as itself (never through a link) and hold it against
/// rename and delete for as long as the returned handle lives.
fn pin_directory(path: &Path) -> Result<File, StorageError> {
    let handle = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .map_err(|error| io_error(path, error))?;
    let meta = handle.metadata().map_err(|error| io_error(path, error))?;
    if meta.file_type().is_symlink() {
        return Err(StorageError::Symlink {
            path: path.display().to_string(),
        });
    }
    if !meta.is_dir() {
        return Err(StorageError::Io(format!(
            "{}: parent path is not a directory",
            path.display()
        )));
    }
    Ok(handle)
}

/// Pin `root` and each directory named by `parts` under it, creating missing
/// ones. Returns the pins (which must outlive any use of the path) and the
/// directory path they lead to.
fn pin_chain<'a>(
    root: &Path,
    parts: impl IntoIterator<Item = &'a std::ffi::OsStr>,
    create: bool,
) -> Result<(Vec<File>, PathBuf), StorageError> {
    let mut pins = vec![pin_directory(root)?];
    let mut path = root.to_path_buf();
    for part in parts {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(io_error(&path, error)),
                }
            }
            Err(error) => return Err(io_error(&path, error)),
        }
        pins.push(pin_directory(&path)?);
    }
    Ok((pins, path))
}

/// Open (creating when missing) the payload file `parts` names under `root`.
/// Every directory is pinned and checked to be a real directory; the file is
/// opened without following a reparse point and refused if it is a link.
pub(super) fn open_payload(root: &Path, parts: &[String]) -> Result<(File, PathBuf), StorageError> {
    let (name, directories) = parts
        .split_last()
        .ok_or_else(|| StorageError::Io("torrent file path is empty".into()))?;
    let (_pins, mut path) = pin_chain(
        root,
        directories
            .iter()
            .map(|part| std::ffi::OsStr::new(part.as_str())),
        true,
    )?;
    path.push(name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)
        .map_err(|error| io_error(&path, error))?;
    let meta = file.metadata().map_err(|error| io_error(&path, error))?;
    if meta.file_type().is_symlink() {
        return Err(StorageError::Symlink {
            path: path.display().to_string(),
        });
    }
    if !meta.is_file() {
        return Err(StorageError::Io(format!(
            "{}: torrent file path is not a file",
            path.display()
        )));
    }
    Ok((file, path))
}

fn by_handle(file: &File) -> std::io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the handle is open for the life of `file`; the call writes
    // only the struct it is given.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a nonzero return means the struct was filled.
    Ok(unsafe { info.assume_init() })
}

fn identity_of(info: &BY_HANDLE_FILE_INFORMATION) -> Identity {
    Identity {
        dev: u64::from(info.dwVolumeSerialNumber),
        ino: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        nlink: u64::from(info.nNumberOfLinks),
    }
}

pub(super) fn handle_identity(file: &File) -> std::io::Result<Identity> {
    by_handle(file).map(|info| identity_of(&info))
}

/// Identity of the object at `path` itself (a link is not followed).
pub(super) fn probed_identity(path: &Path, _meta: &std::fs::Metadata) -> std::io::Result<Identity> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    handle_identity(&file)
}

/// Last write time as (seconds, nanoseconds) since the Unix epoch.
pub(super) fn mtime(meta: &std::fs::Metadata) -> (i64, i64) {
    // FILETIME: 100 ns ticks since 1601-01-01.
    const TICKS_PER_SEC: i64 = 10_000_000;
    const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
    let ticks = i64::try_from(meta.last_write_time()).unwrap_or(i64::MAX);
    (
        ticks.div_euclid(TICKS_PER_SEC) - EPOCH_DIFF_SECS,
        ticks.rem_euclid(TICKS_PER_SEC) * 100,
    )
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
        let Some(handle) = &file.handle else {
            continue;
        };
        if file.path == job.root || !file.path.starts_with(&job.root) {
            return Err(StorageError::IdentityChanged {
                path: file.path.display().to_string(),
            });
        }
        let live = handle_identity(handle).map_err(|error| io_error(&file.path, error))?;
        let on_path =
            std::fs::symlink_metadata(&file.path).map_err(|error| io_error(&file.path, error))?;
        let path_text = file.path.display().to_string();
        if on_path.file_type().is_symlink() {
            return Err(StorageError::Symlink { path: path_text });
        }
        if !on_path.is_file() {
            return Err(StorageError::Io(format!(
                "{path_text}: torrent path is no longer a regular file"
            )));
        }
        let at_path =
            probed_identity(&file.path, &on_path).map_err(|error| io_error(&file.path, error))?;
        if live.nlink > 1 || at_path.nlink > 1 {
            return Err(StorageError::Hardlink { path: path_text });
        }
        if at_path.dev != live.dev || at_path.ino != live.ino {
            return Err(StorageError::IdentityChanged { path: path_text });
        }
        planned.push(PlannedFile {
            path: file.path.clone(),
            dev: live.dev,
            ino: live.ino,
        });
    }
    Ok(planned)
}

/// Pin every directory between `root` and `path`'s parent; `path` must be a
/// normal descendant of `root`.
fn pin_parents(root: &Path, path: &Path) -> Result<Vec<File>, StorageError> {
    if path == root || !path.starts_with(root) {
        return Err(StorageError::IdentityChanged {
            path: path.display().to_string(),
        });
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| StorageError::IdentityChanged {
            path: path.display().to_string(),
        })?;
    let parent = relative.parent().unwrap_or(Path::new(""));
    let mut parts = Vec::new();
    for component in parent.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            _ => {
                return Err(StorageError::Io(format!(
                    "{}: refusing a non-normal directory component",
                    path.display()
                )));
            }
        }
    }
    Ok(pin_chain(root, parts, false)?.0)
}

enum Target {
    Missing,
    Opened(File, Identity),
}

/// Open the object at `path` for deletion, as itself, and check that it is a
/// regular single-link file.
fn open_for_delete(path: &Path) -> Result<Target, StorageError> {
    let file = match OpenOptions::new()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Target::Missing),
        Err(error) => return Err(io_error(path, error)),
    };
    let path_text = path.display().to_string();
    let meta = file.metadata().map_err(|error| io_error(path, error))?;
    if meta.file_type().is_symlink() {
        return Err(StorageError::Symlink { path: path_text });
    }
    if !meta.is_file() {
        return Err(StorageError::Io(format!(
            "{path_text}: refusing to unlink a non-regular file"
        )));
    }
    let identity = handle_identity(&file).map_err(|error| io_error(path, error))?;
    if identity.nlink > 1 {
        return Err(StorageError::Hardlink { path: path_text });
    }
    Ok(Target::Opened(file, identity))
}

/// Mark the open handle delete-on-close; the name goes when it is dropped.
fn delete_handle(file: File, path: &Path) -> Result<(), StorageError> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the handle is open with DELETE access; the buffer is a valid
    // FILE_DISPOSITION_INFO of the size passed.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&raw const info).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(StorageError::Io(format!(
            "{}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    drop(file);
    Ok(())
}

pub(super) fn unlink_matched(
    root: &Path,
    path: &Path,
    dev: u64,
    ino: u64,
) -> Result<(), StorageError> {
    let _pins = pin_parents(root, path)?;
    match open_for_delete(path)? {
        Target::Missing => Err(StorageError::Io(format!(
            "{}: file is missing",
            path.display()
        ))),
        Target::Opened(file, identity) => {
            if identity.dev != dev || identity.ino != ino {
                return Err(StorageError::IdentityChanged {
                    path: path.display().to_string(),
                });
            }
            delete_handle(file, path)
        }
    }
}

/// Delete one regular file under `root` by name, never through a link,
/// refusing a hardlinked or non-regular target. `Ok(false)` when absent.
pub(super) fn unlink_regular(root: &Path, path: &Path) -> Result<bool, StorageError> {
    let _pins = pin_parents(root, path)?;
    match open_for_delete(path)? {
        Target::Missing => Ok(false),
        Target::Opened(file, _) => delete_handle(file, path).map(|()| true),
    }
}

/// Remove empty directories that used to contain deleted files. Never removes
/// `root`. A link or a non-empty directory is left in place.
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
        let Ok(_pins) = pin_parents(root, &dir) else {
            continue;
        };
        let Ok(meta) = std::fs::symlink_metadata(&dir) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        // Fails (and is left alone) when not empty or in use. A directory
        // swapped for a junction after the check loses only the junction.
        let _ = std::fs::remove_dir(&dir);
    }
}
