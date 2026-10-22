use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::RwLock;
use sha1::{Digest, Sha1};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex as TokioMutex, Semaphore};

use super::error::StorageError;
use super::file::{AllocationMode, FileEntry, PieceFileSpan, PieceInfo, V2PieceMap};
use crate::metainfo::{compute_root, verify_piece_layer};

const MAX_CONCURRENT_OPS: usize = 512;
const FILE_HANDLE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

fn validate_file_path(file_path: &Path) -> Result<(), StorageError> {
    for component in file_path.components() {
        match component {
            Component::ParentDir => {
                return Err(StorageError::PathTraversal(file_path.display().to_string()));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(StorageError::PathTraversal(file_path.display().to_string()));
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_all_file_paths(files: &[FileEntry]) -> Result<(), StorageError> {
    for file in files {
        validate_file_path(&file.path)?;
    }
    Ok(())
}

struct PerFileHandle {
    file: TokioMutex<File>,
    last_used: parking_lot::Mutex<Instant>,
    is_write: bool,
}

struct FileHandleCache {
    handles: DashMap<usize, Arc<PerFileHandle>>,
    base_path: PathBuf,
    files: Vec<FileEntry>,
}

impl FileHandleCache {
    fn new(base_path: PathBuf, files: Vec<FileEntry>) -> Self {
        Self {
            handles: DashMap::new(),
            base_path,
            files,
        }
    }

    fn file_path(&self, file_index: usize) -> PathBuf {
        self.base_path.join(&self.files[file_index].path)
    }

    async fn ensure_parent_dirs(path: &std::path::Path) -> Result<(), StorageError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Ok(())
    }

    async fn get_or_open_read(
        &self,
        file_index: usize,
    ) -> Result<Arc<PerFileHandle>, StorageError> {
        if let Some(handle) = self.handles.get(&file_index) {
            *handle.last_used.lock() = Instant::now();
            return Ok(handle.clone());
        }

        let path = self.file_path(file_index);
        let file = File::open(&path)
            .await
            .map_err(|_| StorageError::FileNotFound(path.display().to_string()))?;

        let handle = Arc::new(PerFileHandle {
            file: TokioMutex::new(file),
            last_used: parking_lot::Mutex::new(Instant::now()),
            is_write: false,
        });

        self.handles.insert(file_index, handle.clone());
        Ok(handle)
    }

    async fn get_or_open_write(
        &self,
        file_index: usize,
    ) -> Result<Arc<PerFileHandle>, StorageError> {
        if let Some(handle) = self.handles.get(&file_index) {
            if handle.is_write {
                *handle.last_used.lock() = Instant::now();
                return Ok(handle.clone());
            }
            drop(handle);
            self.handles.remove(&file_index);
        }

        let path = self.file_path(file_index);
        Self::ensure_parent_dirs(&path).await?;

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .await
            .map_err(StorageError::from)?;

        let handle = Arc::new(PerFileHandle {
            file: TokioMutex::new(file),
            last_used: parking_lot::Mutex::new(Instant::now()),
            is_write: true,
        });

        self.handles.insert(file_index, handle.clone());
        Ok(handle)
    }

    async fn flush_all(&self) {
        let keys: Vec<usize> = self.handles.iter().map(|r| *r.key()).collect();
        for key in keys {
            if let Some((_, handle)) = self.handles.remove(&key) {
                if handle.is_write {
                    let file = handle.file.lock().await;
                    let _ = file.sync_data().await;
                }
            }
        }
    }

    async fn evict_idle(&self) {
        let now = Instant::now();
        let to_evict: Vec<usize> = self
            .handles
            .iter()
            .filter(|r| now.duration_since(*r.last_used.lock()) > FILE_HANDLE_IDLE_TIMEOUT)
            .map(|r| *r.key())
            .collect();

        for idx in to_evict {
            if let Some((_, handle)) = self.handles.remove(&idx) {
                if handle.is_write {
                    let file = handle.file.lock().await;
                    let _ = file.sync_data().await;
                }
            }
        }
    }
}

pub struct TorrentStorage {
    base_path: PathBuf,
    files: Vec<FileEntry>,
    pieces: Vec<PieceInfo>,
    total_length: u64,
    allocation_mode: AllocationMode,
    is_v2: bool,
    handle_cache: FileHandleCache,
    /// V2 piece-to-file mapping (only set for v2/hybrid torrents)
    v2_piece_map: Option<V2PieceMap>,
    /// Piece length in bytes (needed for v2 piece calculations)
    piece_length: u64,
}

impl TorrentStorage {
    /// Creates a new torrent storage (v1 style).
    pub fn new(
        base_path: PathBuf,
        files: Vec<FileEntry>,
        pieces: Vec<PieceInfo>,
        total_length: u64,
        is_v2: bool,
    ) -> Result<Self, StorageError> {
        // Infer piece length from first piece if available
        let piece_length = pieces.first().map(|p| p.length).unwrap_or(0);
        Self::with_piece_length(base_path, files, pieces, total_length, is_v2, piece_length)
    }

    /// Creates a new torrent storage with explicit piece length (needed for v2).
    pub fn with_piece_length(
        base_path: PathBuf,
        files: Vec<FileEntry>,
        pieces: Vec<PieceInfo>,
        total_length: u64,
        is_v2: bool,
        piece_length: u64,
    ) -> Result<Self, StorageError> {
        validate_all_file_paths(&files)?;

        let handle_cache = FileHandleCache::new(base_path.clone(), files.clone());

        // Build v2 piece map if this is a v2 torrent
        let v2_piece_map = if is_v2 && piece_length > 0 {
            Some(V2PieceMap::new(&files, piece_length))
        } else {
            None
        };

        Ok(Self {
            base_path,
            files,
            pieces,
            total_length,
            allocation_mode: AllocationMode::Sparse,
            is_v2,
            handle_cache,
            v2_piece_map,
            piece_length,
        })
    }

    /// Returns the v2 piece map if available.
    pub fn v2_piece_map(&self) -> Option<&V2PieceMap> {
        self.v2_piece_map.as_ref()
    }

    /// Returns the piece length.
    pub fn get_piece_length(&self) -> u64 {
        self.piece_length
    }

    /// Returns the file entry for a given file index.
    pub fn get_file(&self, file_index: usize) -> Option<&FileEntry> {
        self.files.get(file_index)
    }

    /// Returns all file entries.
    pub fn files(&self) -> &[FileEntry] {
        &self.files
    }

    pub fn with_allocation_mode(mut self, mode: AllocationMode) -> Self {
        self.allocation_mode = mode;
        self
    }

    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    pub fn piece_count(&self) -> usize {
        self.pieces.len()
    }

    pub fn piece_length(&self, index: u32) -> u64 {
        self.pieces
            .get(index as usize)
            .map(|p| p.length)
            .unwrap_or(0)
    }

    fn piece_file_spans(&self, piece_index: u32) -> Result<Vec<PieceFileSpan>, StorageError> {
        // For v2 torrents, use the optimized piece map (O(1) lookup)
        // In v2, pieces never span files - each piece belongs to exactly one file
        if let Some(ref v2_map) = self.v2_piece_map {
            return self.piece_file_spans_v2(piece_index, v2_map);
        }

        // V1 path: pieces can span multiple files
        let piece = self
            .pieces
            .get(piece_index as usize)
            .ok_or(StorageError::InvalidPieceIndex(piece_index))?;

        let mut spans = Vec::new();
        let mut remaining = piece.length;
        let mut current_offset = piece.offset;

        for (file_idx, file) in self.files.iter().enumerate() {
            if remaining == 0 {
                break;
            }

            let file_end = file.offset + file.length;

            if current_offset >= file.offset && current_offset < file_end {
                let file_offset = current_offset - file.offset;
                let available = file_end - current_offset;
                let take = remaining.min(available);

                spans.push(PieceFileSpan {
                    file_index: file_idx,
                    file_offset,
                    length: take,
                });

                current_offset += take;
                remaining -= take;
            }
        }

        Ok(spans)
    }

    /// Optimized piece-to-file mapping for v2 torrents.
    ///
    /// In v2 torrents, each piece belongs to exactly one file (pieces never span files).
    /// This uses the V2PieceMap for O(1) file lookup instead of iterating all files.
    fn piece_file_spans_v2(
        &self,
        piece_index: u32,
        v2_map: &V2PieceMap,
    ) -> Result<Vec<PieceFileSpan>, StorageError> {
        // Look up which file this piece belongs to
        let (file_idx, local_piece_idx) = v2_map
            .global_to_file(piece_index)
            .ok_or(StorageError::InvalidPieceIndex(piece_index))?;

        let file = self
            .files
            .get(file_idx)
            .ok_or(StorageError::InvalidPieceIndex(piece_index))?;

        // Calculate piece offset within the file
        let piece_offset_in_file = local_piece_idx as u64 * self.piece_length;

        // Calculate piece length (last piece of file may be smaller)
        let remaining_in_file = file.length.saturating_sub(piece_offset_in_file);
        let piece_len = remaining_in_file.min(self.piece_length);

        if piece_len == 0 {
            return Err(StorageError::InvalidPieceIndex(piece_index));
        }

        Ok(vec![PieceFileSpan {
            file_index: file_idx,
            file_offset: piece_offset_in_file,
            length: piece_len,
        }])
    }

    fn block_file_spans(
        &self,
        piece_index: u32,
        offset: u32,
        length: u32,
    ) -> Result<Vec<PieceFileSpan>, StorageError> {
        // For v2 torrents, use optimized path (blocks never span files)
        if let Some(ref v2_map) = self.v2_piece_map {
            return self.block_file_spans_v2(piece_index, offset, length, v2_map);
        }

        // V1 path: blocks can span files
        let piece = self
            .pieces
            .get(piece_index as usize)
            .ok_or(StorageError::InvalidPieceIndex(piece_index))?;

        if offset as u64 + length as u64 > piece.length {
            return Err(StorageError::InvalidBlockOffset {
                piece: piece_index,
                offset,
            });
        }

        let block_start = piece.offset + offset as u64;
        let mut spans = Vec::new();
        let mut remaining = length as u64;
        let mut current_offset = block_start;

        for (file_idx, file) in self.files.iter().enumerate() {
            if remaining == 0 {
                break;
            }

            let file_end = file.offset + file.length;

            if current_offset >= file.offset && current_offset < file_end {
                let file_offset = current_offset - file.offset;
                let available = file_end - current_offset;
                let take = remaining.min(available);

                spans.push(PieceFileSpan {
                    file_index: file_idx,
                    file_offset,
                    length: take,
                });

                current_offset += take;
                remaining -= take;
            }
        }

        Ok(spans)
    }

    /// Optimized block-to-file mapping for v2 torrents.
    ///
    /// In v2 torrents, blocks never span files since pieces don't span files.
    fn block_file_spans_v2(
        &self,
        piece_index: u32,
        offset: u32,
        length: u32,
        v2_map: &V2PieceMap,
    ) -> Result<Vec<PieceFileSpan>, StorageError> {
        // Look up which file this piece belongs to
        let (file_idx, local_piece_idx) = v2_map
            .global_to_file(piece_index)
            .ok_or(StorageError::InvalidPieceIndex(piece_index))?;

        let file = self
            .files
            .get(file_idx)
            .ok_or(StorageError::InvalidPieceIndex(piece_index))?;

        // Calculate piece offset within the file
        let piece_offset_in_file = local_piece_idx as u64 * self.piece_length;

        // Calculate the actual piece length for this piece
        let remaining_in_file = file.length.saturating_sub(piece_offset_in_file);
        let actual_piece_len = remaining_in_file.min(self.piece_length);

        // Validate block bounds
        if offset as u64 + length as u64 > actual_piece_len {
            return Err(StorageError::InvalidBlockOffset {
                piece: piece_index,
                offset,
            });
        }

        // Block offset in file = piece offset + block offset within piece
        let block_offset_in_file = piece_offset_in_file + offset as u64;

        Ok(vec![PieceFileSpan {
            file_index: file_idx,
            file_offset: block_offset_in_file,
            length: length as u64,
        }])
    }

    fn file_path(&self, file: &FileEntry) -> PathBuf {
        self.base_path.join(&file.path)
    }

    async fn ensure_parent_dirs(&self, path: &std::path::Path) -> Result<(), StorageError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Ok(())
    }

    pub async fn preallocate(&self) -> Result<(), StorageError> {
        for file in &self.files {
            let path = self.file_path(file);
            self.ensure_parent_dirs(&path).await?;

            let f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .await?;

            f.set_len(file.length).await?;
        }

        Ok(())
    }

    pub async fn read_piece(&self, piece_index: u32) -> Result<Bytes, StorageError> {
        let piece = self
            .pieces
            .get(piece_index as usize)
            .ok_or(StorageError::InvalidPieceIndex(piece_index))?;

        let spans = self.piece_file_spans(piece_index)?;
        let mut data = Vec::with_capacity(piece.length as usize);

        for span in spans {
            let handle = self.handle_cache.get_or_open_read(span.file_index).await?;
            let mut file = handle.file.lock().await;
            file.seek(SeekFrom::Start(span.file_offset)).await?;

            let mut buf = vec![0u8; span.length as usize];
            file.read_exact(&mut buf).await?;
            data.extend_from_slice(&buf);
        }

        Ok(Bytes::from(data))
    }

    pub async fn read_block(
        &self,
        piece_index: u32,
        offset: u32,
        length: u32,
    ) -> Result<Bytes, StorageError> {
        let spans = self.block_file_spans(piece_index, offset, length)?;
        let mut data = Vec::with_capacity(length as usize);

        for span in spans {
            let handle = self.handle_cache.get_or_open_read(span.file_index).await?;
            let mut file = handle.file.lock().await;
            file.seek(SeekFrom::Start(span.file_offset)).await?;

            let mut buf = vec![0u8; span.length as usize];
            file.read_exact(&mut buf).await?;
            data.extend_from_slice(&buf);
        }

        Ok(Bytes::from(data))
    }

    pub async fn write_piece(&self, piece_index: u32, data: &[u8]) -> Result<(), StorageError> {
        let piece = self
            .pieces
            .get(piece_index as usize)
            .ok_or(StorageError::InvalidPieceIndex(piece_index))?;

        if data.len() != piece.length as usize {
            return Err(StorageError::InvalidPieceIndex(piece_index));
        }

        let spans = self.piece_file_spans(piece_index)?;
        let mut data_offset = 0usize;

        for span in spans {
            let handle = self.handle_cache.get_or_open_write(span.file_index).await?;
            let mut file = handle.file.lock().await;
            file.seek(SeekFrom::Start(span.file_offset)).await?;

            let chunk = &data[data_offset..data_offset + span.length as usize];
            file.write_all(chunk).await?;

            data_offset += span.length as usize;
        }

        Ok(())
    }

    pub async fn write_block(
        &self,
        piece_index: u32,
        offset: u32,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let spans = self.block_file_spans(piece_index, offset, data.len() as u32)?;
        let mut data_offset = 0usize;

        for span in spans {
            let handle = self.handle_cache.get_or_open_write(span.file_index).await?;
            let mut file = handle.file.lock().await;
            file.seek(SeekFrom::Start(span.file_offset)).await?;

            let chunk = &data[data_offset..data_offset + span.length as usize];
            file.write_all(chunk).await?;

            data_offset += span.length as usize;
        }

        Ok(())
    }

    pub async fn verify_piece(&self, piece_index: u32) -> Result<bool, StorageError> {
        let piece = self
            .pieces
            .get(piece_index as usize)
            .ok_or(StorageError::InvalidPieceIndex(piece_index))?;

        let data = self.read_piece(piece_index).await?;
        let expected_hash = piece.hash.clone();
        let is_v2 = self.is_v2;
        let piece_length = self.piece_length;

        let valid = tokio::task::spawn_blocking(move || {
            if is_v2 {
                // For v2, use merkle tree verification with proper padding
                // The expected_hash is the merkle root of this piece's subtree
                if expected_hash.len() == 32 {
                    let mut expected = [0u8; 32];
                    expected.copy_from_slice(&expected_hash);
                    verify_piece_layer(&data, &expected, piece_length)
                } else {
                    // Fallback for misconfigured piece info
                    compute_root(&data).to_vec() == expected_hash
                }
            } else {
                let mut hasher = Sha1::new();
                hasher.update(&data);
                hasher.finalize().to_vec() == expected_hash
            }
        })
        .await
        .map_err(|e| StorageError::Io(std::io::Error::other(e)))?;

        Ok(valid)
    }

    /// Verifies a v2 piece using merkle tree verification.
    ///
    /// This is more explicit than `verify_piece` and always uses
    /// merkle tree verification regardless of the `is_v2` flag.
    /// It properly handles partial pieces (last piece of a file) by
    /// padding with zero hashes.
    pub async fn verify_piece_merkle(
        &self,
        piece_index: u32,
        expected_root: &[u8; 32],
    ) -> Result<bool, StorageError> {
        let data = self.read_piece(piece_index).await?;
        let expected = *expected_root;
        let piece_length = self.piece_length;

        let valid =
            tokio::task::spawn_blocking(move || verify_piece_layer(&data, &expected, piece_length))
                .await
                .map_err(|e| StorageError::Io(std::io::Error::other(e)))?;

        Ok(valid)
    }

    /// Gets the expected piece hash for v2 verification.
    ///
    /// For v2 torrents, looks up the piece layer hash for the given piece.
    /// Returns None for v1 torrents or if the piece is not found.
    pub fn get_v2_piece_hash(&self, piece_index: u32) -> Option<[u8; 32]> {
        if !self.is_v2 {
            return None;
        }

        self.pieces
            .get(piece_index as usize)
            .and_then(|p| p.hash_v2())
    }

    pub async fn verify_all(&self) -> Result<Vec<bool>, StorageError> {
        let piece_count = self.pieces.len();
        if piece_count == 0 {
            return Ok(vec![]);
        }

        tracing::debug!("Starting verification of {} pieces", piece_count);

        const BATCH_SIZE: usize = 32;
        const BATCH_TIMEOUT: Duration = Duration::from_secs(120);

        let mut results = vec![false; piece_count];
        let mut verified_count = 0usize;

        for batch_start in (0..piece_count).step_by(BATCH_SIZE) {
            let batch_end = (batch_start + BATCH_SIZE).min(piece_count);
            let mut futures = Vec::with_capacity(batch_end - batch_start);

            for i in batch_start..batch_end {
                futures.push(self.verify_piece(i as u32));
            }

            let batch_results =
                match tokio::time::timeout(BATCH_TIMEOUT, futures::future::join_all(futures)).await
                {
                    Ok(results) => results,
                    Err(_) => {
                        tracing::warn!(
                            "Verification batch {}-{} timed out, marking as invalid",
                            batch_start,
                            batch_end
                        );
                        continue;
                    }
                };

            for (i, result) in batch_results.into_iter().enumerate() {
                let piece_idx = batch_start + i;
                results[piece_idx] = match result {
                    Ok(valid) => {
                        if valid {
                            verified_count += 1;
                        }
                        valid
                    }
                    Err(StorageError::FileNotFound(_)) => false,
                    Err(e) => {
                        tracing::trace!("Piece {} verification error: {}", piece_idx, e);
                        false
                    }
                };
            }

            if piece_count > 100 && batch_end % 100 == 0 {
                tracing::debug!(
                    "Verified {}/{} pieces ({} valid so far)",
                    batch_end,
                    piece_count,
                    verified_count
                );
            }
        }

        tracing::debug!(
            "Verification complete: {}/{} pieces valid",
            verified_count,
            piece_count
        );

        Ok(results)
    }

    pub async fn flush(&self) {
        self.handle_cache.flush_all().await;
    }

    pub async fn evict_idle_handles(&self) {
        self.handle_cache.evict_idle().await;
    }
}

pub struct DiskManager {
    torrents: RwLock<HashMap<String, Arc<TorrentStorage>>>,
    semaphore: Arc<Semaphore>,
}

impl DiskManager {
    pub fn new() -> Self {
        Self {
            torrents: RwLock::new(HashMap::new()),
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_OPS)),
        }
    }

    pub fn register(&self, info_hash: String, storage: TorrentStorage) {
        self.torrents.write().insert(info_hash, Arc::new(storage));
    }

    pub fn unregister(&self, info_hash: &str) {
        if let Some(storage) = self.torrents.write().remove(info_hash) {
            tokio::spawn(async move {
                storage.flush().await;
            });
        }
    }

    fn get_storage(&self, info_hash: &str) -> Result<Arc<TorrentStorage>, StorageError> {
        self.torrents
            .read()
            .get(info_hash)
            .cloned()
            .ok_or_else(|| StorageError::TorrentNotFound(info_hash.to_string()))
    }

    pub async fn read_piece(
        &self,
        info_hash: &str,
        piece_index: u32,
    ) -> Result<Bytes, StorageError> {
        let storage = self.get_storage(info_hash)?;
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("semaphore closed")))?;
        storage.read_piece(piece_index).await
    }

    pub async fn read_block(
        &self,
        info_hash: &str,
        piece_index: u32,
        offset: u32,
        length: u32,
    ) -> Result<Bytes, StorageError> {
        let storage = self.get_storage(info_hash)?;
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("semaphore closed")))?;
        storage.read_block(piece_index, offset, length).await
    }

    pub async fn write_piece(
        &self,
        info_hash: &str,
        piece_index: u32,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let storage = self.get_storage(info_hash)?;
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("semaphore closed")))?;
        storage.write_piece(piece_index, data).await
    }

    pub async fn write_block(
        &self,
        info_hash: &str,
        piece_index: u32,
        offset: u32,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let storage = self.get_storage(info_hash)?;
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("semaphore closed")))?;
        storage.write_block(piece_index, offset, data).await
    }

    pub async fn verify_piece(
        &self,
        info_hash: &str,
        piece_index: u32,
    ) -> Result<bool, StorageError> {
        let storage = self.get_storage(info_hash)?;
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("semaphore closed")))?;
        storage.verify_piece(piece_index).await
    }

    pub async fn verify_all(&self, info_hash: &str) -> Result<Vec<bool>, StorageError> {
        let storage = self.get_storage(info_hash)?;
        storage.verify_all().await
    }

    pub fn piece_count(&self, info_hash: &str) -> Result<usize, StorageError> {
        let storage = self.get_storage(info_hash)?;
        Ok(storage.pieces.len())
    }

    pub async fn flush(&self, info_hash: &str) -> Result<(), StorageError> {
        let storage = self.get_storage(info_hash)?;
        storage.flush().await;
        Ok(())
    }

    pub async fn evict_idle_handles(&self) {
        let storages: Vec<Arc<TorrentStorage>> = self.torrents.read().values().cloned().collect();
        for storage in storages {
            storage.evict_idle_handles().await;
        }
    }
}

impl Default for DiskManager {
    fn default() -> Self {
        Self::new()
    }
}
