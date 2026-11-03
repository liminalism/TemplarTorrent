//! File-backed piece storage: one validated directory tree per job, v1
//! concatenated-file mapping, explicit errors instead of silent overwrite.
//!
//! Paths are validated lexically ([`RelativeFile`]) and opened handle-relative
//! with `O_NOFOLLOW` at every level, so a symlink anywhere under the root is
//! refused rather than followed, even one planted while the job is being set up. Pre-existing files at or under the torrent length resume in
//! place; longer files are an explicit [`StorageError::LengthMismatch`], never
//! silently truncated. Every piece write ends with a data sync so verified
//! bytes are durable before the engine credits them. A resume checkpoint
//! calls [`FileStore::durable_identity`], which syncs inode fields as well
//! and refuses a path whose inode is no longer the one the handle wrote.
//!
//! Two handles: [`FileStore`] for direct single-owner use, [`SharedStore`]
//! (cheaply cloneable) for engine sessions, where the sink moves into the
//! session before the job id exists to register paths against.
//!
//! BEP 47 pad files are mapped but never created: they have no handle,
//! writes into them are dropped (the piece hash already covered their zero
//! bytes), reads return zeros, and their identity stamp is a fixed
//! placeholder so resume records keep one entry per torrent file.
//!
//! Deletion is Linux handle-relative too: `openat`/`unlinkat` with
//! `O_NOFOLLOW`, inode identity taken from the still-open handle, hardlink
//! refusal, and no removal of the destination directory itself. Reads and
//! writes go through the handles opened at set-up, never through paths.

#![deny(unsafe_code)]

use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use templartorrent_core::{
    model::TorrentId,
    paths::{PathError, RelativeFile},
};
use templartorrent_engine::worker::{PieceReader, PieceSink, SinkError};

mod deletion;

/// Descriptors kept free for sockets, the display connection and reads.
const FD_RESERVE: u64 = 512;

/// Payload handles this process may hold across all jobs: the soft
/// `RLIMIT_NOFILE` less [`FD_RESERVE`]. Each real torrent file keeps one
/// handle open for the job's life (deletion and resume identity rely on it).
pub fn handle_budget() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit only writes the struct it is given.
    #[allow(unsafe_code)]
    let ok = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0;
    let soft = if ok { limit.rlim_cur } else { 1024 };
    soft.saturating_sub(FD_RESERVE)
}

/// Raise the soft open-file limit to the hard limit, capped at 65,536, as
/// Transmission and qBittorrent do. Returns the soft limit in effect after.
pub fn raise_open_file_limit() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit read and write only the given struct.
    #[allow(unsafe_code)]
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return 1024;
        }
        let wanted = limit.rlim_max.min(65_536);
        if wanted > limit.rlim_cur {
            let raised = libc::rlimit {
                rlim_cur: wanted,
                rlim_max: limit.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raised) == 0 {
                return wanted;
            }
        }
        limit.rlim_cur
    }
}

/// Same bound as the engine; actual admission also requires enough session
/// budget for piece assembly and incoming copies.
pub const MAX_STORED_PIECE_LENGTH: u64 = templartorrent_engine::session::MAX_PIECE_LENGTH;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    Path(PathError),
    Io(String),
    LengthMismatch {
        path: String,
        expected: u64,
        found: u64,
    },
    Symlink {
        path: String,
    },
    PieceTooLarge(u64),
    UnknownPiece(u32),
    DuplicateJob(TorrentId),
    UnknownJob(TorrentId),
    Hardlink {
        path: String,
    },
    IdentityChanged {
        path: String,
    },
    PartialDelete {
        deleted: Vec<String>,
        failed: String,
        detail: String,
    },
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(inner) => write!(f, "rejected path: {inner}"),
            Self::Io(detail) => write!(f, "storage I/O: {detail}"),
            Self::LengthMismatch {
                path,
                expected,
                found,
            } => write!(
                f,
                "{path}: length {found} exceeds torrent length {expected}; refusing to truncate"
            ),
            Self::Symlink { path } => write!(f, "{path}: refusing symlink in torrent storage"),
            Self::PieceTooLarge(length) => write!(
                f,
                "piece length {length} is outside 1..={MAX_STORED_PIECE_LENGTH} bytes"
            ),
            Self::UnknownPiece(piece) => write!(f, "piece {piece} out of range"),
            Self::DuplicateJob(id) => write!(f, "job {} already has storage", id.0),
            Self::UnknownJob(id) => write!(f, "job {} has no storage", id.0),
            Self::Hardlink { path } => write!(f, "{path}: refusing to delete a hardlinked file"),
            Self::IdentityChanged { path } => {
                write!(f, "{path}: file identity changed before deletion")
            }
            Self::PartialDelete {
                deleted,
                failed,
                detail,
            } => write!(
                f,
                "deleted {} file(s) then stopped at {failed}: {detail}",
                deleted.len()
            ),
        }
    }
}

impl std::error::Error for StorageError {}

fn io_error(path: &Path, error: std::io::Error) -> StorageError {
    StorageError::Io(format!("{}: {error}", path.display()))
}

/// `Ok(None)` when `path` is missing; refuses symlinks either way.
fn probe(path: &Path) -> Result<Option<std::fs::Metadata>, StorageError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(StorageError::Symlink {
                    path: path.display().to_string(),
                });
            }
            Ok(Some(meta))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(path, error)),
    }
}

/// Identity of one open payload file, taken from the handle after a sync.
/// A resume bitmap may trust bytes only while a later open still reports
/// this device, inode, size and mtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
}

struct StoredFile {
    path: PathBuf,
    /// `None` for a BEP 47 pad file, which has no bytes on disk.
    handle: Option<File>,
    length: u64,
    offset: u64,
}

struct JobFiles {
    root: PathBuf,
    files: Vec<StoredFile>,
    piece_length: u64,
    total: u64,
    piece_count: usize,
}

impl JobFiles {
    /// Byte range of one piece, or `None` when out of range.
    fn piece_range(&self, piece: u32) -> Option<(u64, usize)> {
        let index = usize::try_from(piece).ok()?;
        if index >= self.piece_count {
            return None;
        }
        let start = u64::from(piece).checked_mul(self.piece_length)?;
        let end = start.saturating_add(self.piece_length).min(self.total);
        Some((start, usize::try_from(end - start).ok()?))
    }

    /// Index of the file containing `offset`. Files tile `[0, total)`
    /// contiguously, so the last file starting at or before `offset` always
    /// contains it; zero-length files at the boundary are skipped by the
    /// partition point, never selected.
    fn locate(&self, offset: u64) -> Option<usize> {
        if offset >= self.total || self.files.is_empty() {
            return None;
        }
        let index = self
            .files
            .partition_point(|file| file.offset <= offset)
            .checked_sub(1)?;
        let file = &self.files[index];
        (offset < file.offset + file.length).then_some(index)
    }
}

/// Delete one file a job owns outright, by name under `root`: every directory
/// component is opened `O_NOFOLLOW`, the target is stat'd without following,
/// and a symlink, a non-regular file or a hardlinked file is refused. The
/// root itself is never removed. `Ok(false)` when the name is absent.
pub fn delete_owned_file(root: &Path, path: &Path) -> Result<bool, StorageError> {
    if !root.is_absolute() {
        return Err(StorageError::Io(
            "storage root must be an absolute directory".into(),
        ));
    }
    deletion::unlink_regular(root, path)
}

/// Sees every payload write and every durability barrier, in order. A test
/// harness models power loss with it: bytes written since the last `synced`
/// may vanish, bytes before it may not. Production installs none.
pub trait DiskObserver: Send {
    /// `data` was written to `path` at `offset` within that file.
    fn wrote(&mut self, path: &Path, offset: u64, data: &[u8]);
    /// Every handle of `job` completed `sync_all`.
    fn synced(&mut self, job: TorrentId);
}

/// Direct single-owner file storage. Register each job with [`FileStore::add_job`].
#[derive(Default)]
pub struct FileStore {
    jobs: HashMap<u64, JobFiles>,
    observer: Mutex<Option<Box<dyn DiskObserver>>>,
}

impl FileStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install or remove the write and sync observer (fault injection).
    pub fn set_observer(&self, observer: Option<Box<dyn DiskObserver>>) {
        *self
            .observer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = observer;
    }

    fn observe(&self, notify: impl FnOnce(&mut dyn DiskObserver)) {
        if let Some(observer) = self
            .observer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_mut()
        {
            notify(observer.as_mut());
        }
    }

    /// Close every handle for a quiesced job; leaves all data intact.
    pub fn close_job(&mut self, id: TorrentId) -> bool {
        self.jobs.remove(&id.0).is_some()
    }

    /// Map one torrent onto `root`. Creates missing directories and files but
    /// never truncates: pre-existing files at or under the torrent length stay
    /// for resume, longer ones fail with [`StorageError::LengthMismatch`].
    pub fn add_job(
        &mut self,
        id: TorrentId,
        root: &Path,
        files: &[(Vec<String>, u64)],
        piece_length: u64,
    ) -> Result<(), StorageError> {
        self.add_job_with_pads(id, root, files, &[], piece_length)
    }

    /// [`FileStore::add_job`] where `pads[i]` marks file `i` as a BEP 47
    /// pad file: it occupies its byte range but is never created or opened.
    /// A shorter `pads` slice leaves the remaining files real.
    pub fn add_job_with_pads(
        &mut self,
        id: TorrentId,
        root: &Path,
        files: &[(Vec<String>, u64)],
        pads: &[bool],
        piece_length: u64,
    ) -> Result<(), StorageError> {
        if piece_length == 0 || piece_length > MAX_STORED_PIECE_LENGTH {
            return Err(StorageError::PieceTooLarge(piece_length));
        }
        if self.jobs.contains_key(&id.0) {
            return Err(StorageError::DuplicateJob(id));
        }
        let open: usize = self
            .jobs
            .values()
            .map(|job| {
                job.files
                    .iter()
                    .filter(|file| file.handle.is_some())
                    .count()
            })
            .sum();
        let wanted = (0..files.len())
            .filter(|index| !pads.get(*index).copied().unwrap_or(false))
            .count();
        if (open + wanted) as u64 > handle_budget() {
            return Err(StorageError::Io(format!(
                "{wanted} files would exceed the open-file budget of {} (raise ulimit -n)",
                handle_budget()
            )));
        }
        match probe(root)? {
            Some(meta) if !meta.is_dir() => {
                return Err(StorageError::Io(format!(
                    "{}: storage root is not a directory",
                    root.display()
                )));
            }
            Some(_) => {}
            None => std::fs::create_dir_all(root).map_err(|error| io_error(root, error))?,
        }
        let mut stored = Vec::with_capacity(files.len());
        let mut total = 0u64;
        for (index, (components, length)) in files.iter().enumerate() {
            if pads.get(index).copied().unwrap_or(false) {
                // Never created or opened, so its name is never used as a
                // path; it is not validated as one either.
                let offset = total;
                total = total
                    .checked_add(*length)
                    .ok_or_else(|| StorageError::Io("total size overflows u64".into()))?;
                stored.push(StoredFile {
                    path: root.join(format!("pad-{index}")),
                    handle: None,
                    length: *length,
                    offset,
                });
                continue;
            }
            let relative = RelativeFile::new(components.clone()).map_err(StorageError::Path)?;
            // Handle-relative, no-follow open: see `deletion::open_payload`.
            let (handle, path) = deletion::open_payload(root, relative.components())?;
            let found = handle
                .metadata()
                .map_err(|error| io_error(&path, error))?
                .len();
            if found > *length {
                return Err(StorageError::LengthMismatch {
                    path: path.display().to_string(),
                    expected: *length,
                    found,
                });
            }
            let offset = total;
            total = total
                .checked_add(*length)
                .ok_or_else(|| StorageError::Io("total size overflows u64".into()))?;
            stored.push(StoredFile {
                path,
                handle: Some(handle),
                length: *length,
                offset,
            });
        }
        let piece_count = usize::try_from(total.div_ceil(piece_length))
            .map_err(|_| StorageError::Io("piece count overflows usize".into()))?;
        self.jobs.insert(
            id.0,
            JobFiles {
                root: root.to_path_buf(),
                files: stored,
                piece_length,
                total,
                piece_count,
            },
        );
        Ok(())
    }

    /// Check that every registered file is still the inode we opened.
    /// Does not close handles or unlink anything.
    pub fn preflight_delete(&self, id: TorrentId) -> Result<(), StorageError> {
        let job = self.jobs.get(&id.0).ok_or(StorageError::UnknownJob(id))?;
        deletion::plan(job)?;
        Ok(())
    }

    /// Close this job's handles, then unlink only the files whose inode still
    /// matches. The destination root is kept. Empty directories that contained
    /// only those files are removed. A hardlink, symlink, or inode swap fails
    /// before any unlink when `preflight_delete` caught it; a race after the
    /// handles close returns [`StorageError::PartialDelete`] or
    /// [`StorageError::IdentityChanged`] and leaves the job unregistered.
    pub fn delete_job_data(&mut self, id: TorrentId) -> Result<(), StorageError> {
        let job = self.jobs.get(&id.0).ok_or(StorageError::UnknownJob(id))?;
        let plan = deletion::plan(job)?;
        let job = self
            .jobs
            .remove(&id.0)
            .ok_or(StorageError::UnknownJob(id))?;
        let root = job.root.clone();
        drop(job);
        let mut deleted_paths = Vec::new();
        for file in plan {
            match deletion::unlink_matched(&root, &file.path, file.dev, file.ino) {
                Ok(()) => deleted_paths.push(file.path),
                Err(error) => {
                    return Err(StorageError::PartialDelete {
                        deleted: deleted_paths
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect(),
                        failed: file.path.display().to_string(),
                        detail: error.to_string(),
                    });
                }
            }
        }
        deletion::prune_empty_parents(&root, &deleted_paths);
        Ok(())
    }

    /// Device, inode, size and mtime of each open handle, in file order.
    /// Fails when the path no longer names that inode.
    pub fn file_identity(&self, id: TorrentId) -> Result<Vec<FileStamp>, StorageError> {
        let job = self.jobs.get(&id.0).ok_or(StorageError::UnknownJob(id))?;
        let mut stamps = Vec::with_capacity(job.files.len());
        for file in &job.files {
            stamps.push(stamp_handle(file)?);
        }
        Ok(stamps)
    }

    /// [`FileStore::file_identity`] after `sync_all` on every handle. This is
    /// the only data barrier: piece writes leave bytes in the page cache, and
    /// `sync_all` makes every piece written so far durable together with the
    /// mtime the resume stamp records, so a crash cannot keep the bitmap and
    /// lose either the bytes or the identity it names.
    pub fn durable_identity(&self, id: TorrentId) -> Result<Vec<FileStamp>, StorageError> {
        let job = self.jobs.get(&id.0).ok_or(StorageError::UnknownJob(id))?;
        for file in &job.files {
            if let Some(handle) = &file.handle {
                handle
                    .sync_all()
                    .map_err(|error| io_error(&file.path, error))?;
            }
        }
        self.observe(|observer| observer.synced(id));
        self.file_identity(id)
    }

    fn job(&self, torrent: TorrentId) -> Result<&JobFiles, SinkError> {
        self.jobs.get(&torrent.0).ok_or(SinkError::UnknownJob)
    }

    fn job_mut(&mut self, torrent: TorrentId) -> Result<&mut JobFiles, SinkError> {
        self.jobs.get_mut(&torrent.0).ok_or(SinkError::UnknownJob)
    }
}

fn stamp_handle(file: &StoredFile) -> Result<FileStamp, StorageError> {
    let Some(handle) = &file.handle else {
        // A pad file has no inode; this placeholder matches only itself.
        return Ok(FileStamp {
            dev: 0,
            ino: 0,
            size: file.length,
            mtime_sec: 0,
            mtime_nsec: 0,
        });
    };
    let live = handle
        .metadata()
        .map_err(|error| io_error(&file.path, error))?;
    let on_path = match probe(&file.path)? {
        Some(meta) if meta.is_file() => meta,
        Some(_) => {
            return Err(StorageError::Io(format!(
                "{}: torrent path is no longer a regular file",
                file.path.display()
            )));
        }
        None => {
            return Err(StorageError::IdentityChanged {
                path: file.path.display().to_string(),
            });
        }
    };
    if on_path.dev() != live.dev() || on_path.ino() != live.ino() {
        return Err(StorageError::IdentityChanged {
            path: file.path.display().to_string(),
        });
    }
    Ok(FileStamp {
        dev: live.dev(),
        ino: live.ino(),
        size: live.len(),
        mtime_sec: live.mtime(),
        mtime_nsec: live.mtime_nsec(),
    })
}

fn storage_to_sink(error: StorageError) -> SinkError {
    SinkError::Io(error.to_string())
}

impl PieceSink for FileStore {
    fn write_piece(
        &mut self,
        torrent: TorrentId,
        piece: u32,
        data: &[u8],
    ) -> Result<(), SinkError> {
        let (mut offset, expected) = self
            .job(torrent)?
            .piece_range(piece)
            .ok_or_else(|| storage_to_sink(StorageError::UnknownPiece(piece)))?;
        if data.len() != expected {
            return Err(SinkError::Io(format!(
                "piece {piece}: {} bytes disagree with torrent length {expected}",
                data.len()
            )));
        }
        let observing = self
            .observer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_some();
        let mut written: Vec<(PathBuf, u64, Vec<u8>)> = Vec::new();
        let job = self.job_mut(torrent)?;
        let mut remaining = data;
        while !remaining.is_empty() {
            let index = job
                .locate(offset)
                .ok_or_else(|| storage_to_sink(StorageError::UnknownPiece(piece)))?;
            let file = &mut job.files[index];
            let writable = (file.length - (offset - file.offset)).min(remaining.len() as u64);
            if writable == 0 {
                return Err(SinkError::Io(format!("piece {piece}: unmappable offset")));
            }
            let writable = writable as usize;
            let Some(handle) = file.handle.as_mut() else {
                // Pad bytes: the verified piece already proved they are the
                // creator's zeros; nothing is stored.
                remaining = &remaining[writable..];
                offset += writable as u64;
                continue;
            };
            handle
                .seek(SeekFrom::Start(offset - file.offset))
                .map_err(|error| storage_to_sink(io_error(&file.path, error)))?;
            // No per-piece sync: durability is batched at the resume
            // checkpoint (`durable_identity`), and a piece written after the
            // last checkpoint is rechecked by hash on restart.
            handle
                .write_all(&remaining[..writable])
                .map_err(|error| storage_to_sink(io_error(&file.path, error)))?;
            if observing {
                written.push((
                    file.path.clone(),
                    offset - file.offset,
                    remaining[..writable].to_vec(),
                ));
            }
            remaining = &remaining[writable..];
            offset += writable as u64;
        }
        for (path, at, bytes) in written {
            self.observe(|observer| observer.wrote(&path, at, &bytes));
        }
        Ok(())
    }
}

impl PieceReader for FileStore {
    fn read_piece(
        &self,
        torrent: TorrentId,
        piece: u32,
        expected_len: usize,
    ) -> Result<Option<Vec<u8>>, SinkError> {
        let job = self.job(torrent)?;
        let (mut offset, expected) = job
            .piece_range(piece)
            .ok_or_else(|| storage_to_sink(StorageError::UnknownPiece(piece)))?;
        // A caller/engine length disagreement fails open toward re-download:
        // the piece is treated as missing, never as verified.
        if expected_len != expected {
            return Ok(None);
        }
        let mut out = vec![0; expected];
        let mut remaining = expected;
        while remaining > 0 {
            let index = job
                .locate(offset)
                .ok_or_else(|| storage_to_sink(StorageError::UnknownPiece(piece)))?;
            let file = &job.files[index];
            let readable = (file.length - (offset - file.offset)).min(remaining as u64) as usize;
            if readable == 0 {
                return Err(SinkError::Io(format!("piece {piece}: unmappable offset")));
            }
            if file.handle.is_none() {
                // Pad file: `out` already holds zeros for this range.
                remaining -= readable;
                offset += readable as u64;
                continue;
            }
            // A fresh handle per read keeps offsets independent of the write
            // path; a short file (partial download) reads as missing.
            let mut handle = File::open(&file.path)
                .map_err(|error| storage_to_sink(io_error(&file.path, error)))?;
            handle
                .seek(SeekFrom::Start(offset - file.offset))
                .map_err(|error| storage_to_sink(io_error(&file.path, error)))?;
            let written = expected - remaining;
            match handle.read_exact(&mut out[written..written + readable]) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(error) => return Err(storage_to_sink(io_error(&file.path, error))),
            }
            remaining -= readable;
            offset += readable as u64;
        }
        Ok(Some(out))
    }
}

/// Cloneable file storage for engine sessions. The session takes one clone up
/// front; paths register later through another once the job id exists.
#[derive(Clone, Default)]
pub struct SharedStore {
    inner: Arc<Mutex<FileStore>>,
}

impl SharedStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// See [`FileStore::set_observer`].
    pub fn set_observer(&self, observer: Option<Box<dyn DiskObserver>>) {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .set_observer(observer);
    }

    pub fn close_job(&self, id: TorrentId) -> Result<bool, StorageError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| StorageError::Io("store lock poisoned".into()))?
            .close_job(id))
    }

    pub fn preflight_delete(&self, id: TorrentId) -> Result<(), StorageError> {
        self.inner
            .lock()
            .map_err(|_| StorageError::Io("store lock poisoned".into()))?
            .preflight_delete(id)
    }

    pub fn delete_job_data(&self, id: TorrentId) -> Result<(), StorageError> {
        self.inner
            .lock()
            .map_err(|_| StorageError::Io("store lock poisoned".into()))?
            .delete_job_data(id)
    }

    pub fn add_job(
        &self,
        id: TorrentId,
        root: &Path,
        files: &[(Vec<String>, u64)],
        piece_length: u64,
    ) -> Result<(), StorageError> {
        self.add_job_with_pads(id, root, files, &[], piece_length)
    }

    pub fn add_job_with_pads(
        &self,
        id: TorrentId,
        root: &Path,
        files: &[(Vec<String>, u64)],
        pads: &[bool],
        piece_length: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .lock()
            .map_err(|_| StorageError::Io("store lock poisoned".into()))?
            .add_job_with_pads(id, root, files, pads, piece_length)
    }

    pub fn file_identity(&self, id: TorrentId) -> Result<Vec<FileStamp>, StorageError> {
        self.inner
            .lock()
            .map_err(|_| StorageError::Io("store lock poisoned".into()))?
            .file_identity(id)
    }

    pub fn durable_identity(&self, id: TorrentId) -> Result<Vec<FileStamp>, StorageError> {
        self.inner
            .lock()
            .map_err(|_| StorageError::Io("store lock poisoned".into()))?
            .durable_identity(id)
    }
}

impl PieceSink for SharedStore {
    fn write_piece(
        &mut self,
        torrent: TorrentId,
        piece: u32,
        data: &[u8],
    ) -> Result<(), SinkError> {
        self.inner
            .lock()
            .map_err(|_| SinkError::Io("store lock poisoned".into()))?
            .write_piece(torrent, piece, data)
    }
}

impl PieceReader for SharedStore {
    fn read_piece(
        &self,
        torrent: TorrentId,
        piece: u32,
        expected_len: usize,
    ) -> Result<Option<Vec<u8>>, SinkError> {
        self.inner
            .lock()
            .map_err(|_| SinkError::Io("store lock poisoned".into()))?
            .read_piece(torrent, piece, expected_len)
    }
}
