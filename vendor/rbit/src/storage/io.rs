//! Disk I/O operations with write coalescing and batching.
//!
//! This module provides optimized disk I/O through write coalescing,
//! which combines multiple small writes into fewer larger writes.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};

use super::error::StorageError;

/// Priority levels for write operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum WritePriority {
    /// Low priority writes (can be delayed).
    Low = 0,
    /// Normal priority writes.
    #[default]
    Normal = 1,
    /// High priority writes (process immediately).
    High = 2,
}

/// A write operation to be performed.
#[derive(Debug, Clone)]
pub struct WriteOp {
    /// The torrent this write belongs to (info hash hex).
    pub torrent_hash: String,
    /// The file index within the torrent.
    pub file_index: usize,
    /// The offset within the file.
    pub file_offset: u64,
    /// The data to write.
    pub data: Bytes,
    /// The priority of this write.
    pub priority: WritePriority,
}

/// A contiguous region of data to write.
#[derive(Debug, Clone)]
pub struct WriteRegion {
    /// The file index within the torrent.
    pub file_index: usize,
    /// The starting offset within the file.
    pub file_offset: u64,
    /// The coalesced data to write.
    pub data: Bytes,
}

/// A request to flush a complete piece to disk.
#[derive(Debug)]
pub struct FlushRequest {
    /// The torrent this piece belongs to.
    pub torrent_hash: String,
    /// The piece index.
    pub piece_index: u32,
    /// The coalesced write regions.
    pub regions: Vec<WriteRegion>,
    /// The complete piece data for verification.
    pub piece_data: Bytes,
    /// The expected hash of the piece.
    pub expected_hash: Vec<u8>,
}

/// Result of a flush operation.
#[derive(Debug)]
pub enum FlushResult {
    /// The piece was written and verified successfully.
    Success,
    /// The piece hash did not match.
    HashMismatch,
    /// An I/O error occurred.
    Error(StorageError),
}

/// Coalesces multiple block writes into contiguous regions.
///
/// This reduces the number of disk I/O operations by combining
/// adjacent writes into single larger writes.
pub struct WriteCoalescer {
    /// Pending blocks organized by file, indexed by (file_index, offset).
    blocks: HashMap<String, BTreeMap<(usize, u64), Bytes>>,
    /// Maximum bytes to buffer before forcing a flush.
    max_buffer_size: usize,
    /// Current buffered size.
    current_size: AtomicU64,
}

impl WriteCoalescer {
    /// Creates a new write coalescer.
    ///
    /// # Arguments
    ///
    /// * `max_buffer_size` - Maximum bytes to buffer before forcing a flush.
    pub fn new(max_buffer_size: usize) -> Self {
        Self {
            blocks: HashMap::new(),
            max_buffer_size,
            current_size: AtomicU64::new(0),
        }
    }

    /// Adds a block to the coalescer.
    pub fn add_block(
        &mut self,
        torrent_hash: &str,
        file_index: usize,
        file_offset: u64,
        data: Bytes,
    ) {
        let len = data.len() as u64;
        self.blocks
            .entry(torrent_hash.to_string())
            .or_default()
            .insert((file_index, file_offset), data);
        self.current_size.fetch_add(len, Ordering::Relaxed);
    }

    /// Checks if the buffer is full and should be flushed.
    pub fn should_flush(&self) -> bool {
        self.current_size.load(Ordering::Relaxed) as usize >= self.max_buffer_size
    }

    /// Gets the current buffered size.
    pub fn buffered_size(&self) -> usize {
        self.current_size.load(Ordering::Relaxed) as usize
    }

    /// Coalesces and removes all blocks for a specific torrent.
    ///
    /// Returns a list of contiguous write regions.
    pub fn flush_torrent(&mut self, torrent_hash: &str) -> Vec<WriteRegion> {
        let Some(blocks) = self.blocks.remove(torrent_hash) else {
            return Vec::new();
        };

        let regions = coalesce_blocks_from_map(blocks);
        let freed: u64 = regions.iter().map(|r| r.data.len() as u64).sum();
        self.current_size.fetch_sub(freed, Ordering::Relaxed);
        regions
    }

    /// Coalesces and removes all buffered blocks.
    pub fn flush_all(&mut self) -> HashMap<String, Vec<WriteRegion>> {
        let mut result = HashMap::new();
        let keys: Vec<String> = self.blocks.keys().cloned().collect();
        for key in keys {
            let regions = self.flush_torrent(&key);
            if !regions.is_empty() {
                result.insert(key, regions);
            }
        }
        self.current_size.store(0, Ordering::Relaxed);
        result
    }

    /// Clears all buffered data without flushing.
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.current_size.store(0, Ordering::Relaxed);
    }
}

/// Coalesces a sorted map of blocks into contiguous write regions.
fn coalesce_blocks_from_map(blocks: BTreeMap<(usize, u64), Bytes>) -> Vec<WriteRegion> {
    let mut regions = Vec::new();

    let mut current_file: Option<usize> = None;
    let mut current_offset: u64 = 0;
    let mut current_data: Vec<u8> = Vec::new();

    for ((file_index, offset), data) in blocks {
        let can_coalesce = current_file == Some(file_index)
            && offset == current_offset + current_data.len() as u64;

        if can_coalesce {
            // Extend current region
            current_data.extend_from_slice(&data);
        } else {
            // Flush current region and start new one
            if !current_data.is_empty() {
                regions.push(WriteRegion {
                    file_index: current_file.unwrap(),
                    file_offset: current_offset,
                    data: Bytes::from(std::mem::take(&mut current_data)),
                });
            }
            current_file = Some(file_index);
            current_offset = offset;
            current_data = data.to_vec();
        }
    }

    // Flush final region
    if !current_data.is_empty() {
        regions.push(WriteRegion {
            file_index: current_file.unwrap(),
            file_offset: current_offset,
            data: Bytes::from(current_data),
        });
    }

    regions
}

/// Coalesces a list of blocks into contiguous write regions.
///
/// # Arguments
///
/// * `blocks` - List of (file_index, file_offset, data) tuples.
///
/// # Returns
///
/// A list of coalesced write regions.
pub fn coalesce_blocks(blocks: Vec<(usize, u64, Bytes)>) -> Vec<WriteRegion> {
    let map: BTreeMap<(usize, u64), Bytes> = blocks
        .into_iter()
        .map(|(file_index, offset, data)| ((file_index, offset), data))
        .collect();
    coalesce_blocks_from_map(map)
}

type WriteQueueItem = (WriteOp, oneshot::Sender<Result<(), StorageError>>);

/// An I/O operation queue for batched disk operations.
#[allow(dead_code)]
pub struct IoQueue {
    /// Pending write operations.
    writes: Mutex<VecDeque<WriteQueueItem>>,
    /// Semaphore to limit concurrent operations.
    semaphore: Arc<Semaphore>,
    /// Maximum queue size.
    max_queue_size: usize,
}

impl IoQueue {
    /// Creates a new I/O queue.
    ///
    /// # Arguments
    ///
    /// * `max_concurrent` - Maximum concurrent I/O operations.
    /// * `max_queue_size` - Maximum pending operations before blocking.
    pub fn new(max_concurrent: usize, max_queue_size: usize) -> Self {
        Self {
            writes: Mutex::new(VecDeque::with_capacity(max_queue_size)),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_queue_size,
        }
    }

    /// Submits a write operation to the queue.
    ///
    /// Returns a receiver that will be notified when the write completes.
    pub async fn submit(
        &self,
        op: WriteOp,
    ) -> Result<oneshot::Receiver<Result<(), StorageError>>, StorageError> {
        let (tx, rx) = oneshot::channel();

        let mut writes = self.writes.lock().await;
        if writes.len() >= self.max_queue_size {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "I/O queue full",
            )));
        }

        writes.push_back((op, tx));
        Ok(rx)
    }

    /// Submits a write operation and waits for completion.
    pub async fn submit_and_wait(&self, op: WriteOp) -> Result<(), StorageError> {
        let rx = self.submit(op).await?;
        rx.await
            .map_err(|_| StorageError::Io(std::io::Error::other("channel closed")))?
    }

    /// Gets the number of pending operations.
    pub async fn pending_count(&self) -> usize {
        self.writes.lock().await.len()
    }
}

/// A background I/O worker that processes queued operations.
pub struct IoWorker {
    /// Channel to receive shutdown signals.
    shutdown_rx: mpsc::Receiver<()>,
    /// The I/O queue to process.
    queue: Arc<IoQueue>,
}

impl IoWorker {
    /// Creates a new I/O worker.
    pub fn new(queue: Arc<IoQueue>, shutdown_rx: mpsc::Receiver<()>) -> Self {
        Self { shutdown_rx, queue }
    }

    /// Runs the worker until shutdown.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown_rx.recv() => {
                    break;
                }
                _ = Self::process_batch(&self.queue) => {}
            }
        }
    }

    /// Processes a batch of pending operations.
    async fn process_batch(_queue: &IoQueue) {
        // For now, just sleep if there's nothing to do
        // In a real implementation, this would process the queue
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coalesce_adjacent_blocks() {
        let blocks = vec![
            (0, 0, Bytes::from(vec![1, 2, 3])),
            (0, 3, Bytes::from(vec![4, 5, 6])),
            (0, 6, Bytes::from(vec![7, 8, 9])),
        ];

        let regions = coalesce_blocks(blocks);

        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].file_index, 0);
        assert_eq!(regions[0].file_offset, 0);
        assert_eq!(regions[0].data.as_ref(), &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn test_coalesce_non_adjacent_blocks() {
        let blocks = vec![
            (0, 0, Bytes::from(vec![1, 2, 3])),
            (0, 10, Bytes::from(vec![4, 5, 6])), // Gap at offset 3-10
        ];

        let regions = coalesce_blocks(blocks);

        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].file_offset, 0);
        assert_eq!(regions[1].file_offset, 10);
    }

    #[test]
    fn test_coalesce_different_files() {
        let blocks = vec![
            (0, 0, Bytes::from(vec![1, 2, 3])),
            (1, 0, Bytes::from(vec![4, 5, 6])), // Different file
        ];

        let regions = coalesce_blocks(blocks);

        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].file_index, 0);
        assert_eq!(regions[1].file_index, 1);
    }

    #[test]
    fn test_write_coalescer() {
        let mut coalescer = WriteCoalescer::new(1024 * 1024);

        coalescer.add_block("hash1", 0, 0, Bytes::from(vec![1, 2, 3]));
        coalescer.add_block("hash1", 0, 3, Bytes::from(vec![4, 5, 6]));
        coalescer.add_block("hash2", 0, 0, Bytes::from(vec![7, 8, 9]));

        assert_eq!(coalescer.buffered_size(), 9);

        let regions = coalescer.flush_torrent("hash1");
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].data.as_ref(), &[1, 2, 3, 4, 5, 6]);

        assert_eq!(coalescer.buffered_size(), 3);
    }
}
