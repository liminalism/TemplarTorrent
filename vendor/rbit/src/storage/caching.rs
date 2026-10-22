//! High-level disk management with integrated caching.
//!
//! This module provides a `CachingDiskManager` that combines disk I/O
//! with memory caching for efficient piece and block operations.

use std::sync::Arc;

use bytes::Bytes;

use super::error::StorageError;
use super::io::WriteCoalescer;
use super::manager::{DiskManager, TorrentStorage};
use crate::cache::{BlockCache, MemoryBudget, PieceCache};

/// Result of a write operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteResult {
    /// The block was buffered in memory.
    Buffered,
    /// The piece is now complete and was verified.
    PieceComplete {
        /// Whether the piece passed hash verification.
        valid: bool,
    },
}

/// A disk manager with integrated caching.
///
/// Combines the `DiskManager` with `BlockCache` and `PieceCache` to provide
/// efficient disk I/O with memory caching. Blocks are accumulated in memory
/// and coalesced into piece writes.
pub struct CachingDiskManager {
    /// The underlying disk manager.
    disk: DiskManager,
    /// Cache for blocks being downloaded.
    block_cache: Arc<BlockCache>,
    /// Cache for complete pieces.
    piece_cache: Arc<PieceCache>,
    /// Memory budget for caching.
    memory_budget: Arc<MemoryBudget>,
    /// Write coalescer for batching disk writes.
    coalescer: parking_lot::Mutex<WriteCoalescer>,
}

impl CachingDiskManager {
    /// Creates a new caching disk manager.
    ///
    /// # Arguments
    ///
    /// * `memory_limit` - Maximum memory to use for caching.
    /// * `piece_cache_size` - Number of pieces to cache.
    pub fn new(memory_limit: usize, piece_cache_size: usize) -> Self {
        // Allocate memory budget: 70% for blocks, 30% for pieces
        let block_cache_limit = (memory_limit as f64 * 0.7) as usize;
        let coalescer_limit = (memory_limit as f64 * 0.1) as usize;

        Self {
            disk: DiskManager::new(),
            block_cache: BlockCache::new(block_cache_limit),
            piece_cache: PieceCache::new(piece_cache_size),
            memory_budget: MemoryBudget::new(memory_limit),
            coalescer: parking_lot::Mutex::new(WriteCoalescer::new(coalescer_limit)),
        }
    }

    /// Returns a reference to the underlying disk manager.
    pub fn disk(&self) -> &DiskManager {
        &self.disk
    }

    /// Returns a reference to the block cache.
    pub fn block_cache(&self) -> &Arc<BlockCache> {
        &self.block_cache
    }

    /// Returns a reference to the piece cache.
    pub fn piece_cache(&self) -> &Arc<PieceCache> {
        &self.piece_cache
    }

    /// Returns a reference to the memory budget.
    pub fn memory_budget(&self) -> &Arc<MemoryBudget> {
        &self.memory_budget
    }

    /// Registers a torrent's storage.
    pub fn register(&self, info_hash: String, storage: TorrentStorage) {
        self.disk.register(info_hash, storage);
    }

    /// Unregisters a torrent's storage and flushes pending writes.
    pub async fn unregister(&self, info_hash: &str) {
        // Flush any pending writes for this torrent
        let regions = self.coalescer.lock().flush_torrent(info_hash);
        if !regions.is_empty() {
            // In a full implementation, we'd write these regions to disk
            tracing::debug!(
                "Flushing {} regions for unregistered torrent",
                regions.len()
            );
        }

        self.disk.unregister(info_hash);
    }

    /// Writes a block to the cache.
    ///
    /// The block is accumulated in the block cache. When all blocks for a piece
    /// are received, the piece is verified and written to disk.
    ///
    /// # Arguments
    ///
    /// * `info_hash` - The torrent's info hash (hex string).
    /// * `piece_index` - The piece index.
    /// * `offset` - The block offset within the piece.
    /// * `data` - The block data.
    /// * `piece_length` - The total length of the piece.
    /// * `expected_hash` - The expected hash of the complete piece.
    ///
    /// # Returns
    ///
    /// - `WriteResult::Buffered` if the block was cached.
    /// - `WriteResult::PieceComplete { valid }` if the piece is now complete.
    pub async fn write_block(
        &self,
        info_hash: &str,
        piece_index: u32,
        offset: u32,
        data: Bytes,
        piece_length: u32,
        expected_hash: &[u8],
    ) -> Result<WriteResult, StorageError> {
        let hash_version = if expected_hash.len() == 32 { 2 } else { 1 };

        // Add block to cache
        let is_complete = self.block_cache.add_block(
            info_hash,
            piece_index,
            offset,
            data,
            piece_length,
            hash_version,
        );

        if !is_complete {
            return Ok(WriteResult::Buffered);
        }

        // Piece is complete - verify and write to disk
        // Use finalize_and_verify_auto which handles both v1 (SHA1) and v2 (merkle) verification
        let valid = self.block_cache.finalize_and_verify_auto(
            info_hash,
            piece_index,
            expected_hash,
            piece_length,
        );

        if valid {
            // Get assembled piece data
            if let Some(piece_data) = self.block_cache.remove_piece(info_hash, piece_index) {
                // Write to disk
                self.disk
                    .write_piece(info_hash, piece_index, &piece_data)
                    .await?;

                // Cache the piece for potential uploads
                self.piece_cache
                    .insert(info_hash, piece_index, piece_data, true);
            }
        } else {
            // Invalid piece - remove from cache
            self.block_cache.remove_piece(info_hash, piece_index);
        }

        Ok(WriteResult::PieceComplete { valid })
    }

    /// Reads a piece, checking cache first.
    ///
    /// # Arguments
    ///
    /// * `info_hash` - The torrent's info hash (hex string).
    /// * `piece_index` - The piece index.
    pub async fn read_piece(
        &self,
        info_hash: &str,
        piece_index: u32,
    ) -> Result<Bytes, StorageError> {
        // Check piece cache first
        if let Some(data) = self.piece_cache.get(info_hash, piece_index) {
            return Ok(data);
        }

        // Read from disk
        let data = self.disk.read_piece(info_hash, piece_index).await?;

        // Cache for future reads
        self.piece_cache
            .insert(info_hash, piece_index, data.clone(), false);

        Ok(data)
    }

    /// Reads a block from a piece.
    ///
    /// # Arguments
    ///
    /// * `info_hash` - The torrent's info hash (hex string).
    /// * `piece_index` - The piece index.
    /// * `offset` - The block offset within the piece.
    /// * `length` - The block length.
    pub async fn read_block(
        &self,
        info_hash: &str,
        piece_index: u32,
        offset: u32,
        length: u32,
    ) -> Result<Bytes, StorageError> {
        // Check piece cache first
        if let Some(piece_data) = self.piece_cache.get(info_hash, piece_index) {
            let start = offset as usize;
            let end = start + length as usize;
            if end <= piece_data.len() {
                return Ok(piece_data.slice(start..end));
            }
        }

        // Read block directly from disk
        self.disk
            .read_block(info_hash, piece_index, offset, length)
            .await
    }

    /// Verifies a piece against its expected hash.
    pub async fn verify_piece(
        &self,
        info_hash: &str,
        piece_index: u32,
    ) -> Result<bool, StorageError> {
        self.disk.verify_piece(info_hash, piece_index).await
    }

    /// Gets memory usage statistics.
    pub fn memory_stats(&self) -> MemoryStats {
        MemoryStats {
            block_cache_used: self.block_cache.memory_used(),
            block_cache_limit: self.block_cache.memory_limit(),
            total_budget: self.memory_budget.total_limit(),
            budget_used: self.memory_budget.current_usage(),
        }
    }

    /// Flushes all pending writes to disk.
    pub async fn flush(&self, info_hash: &str) -> Result<(), StorageError> {
        self.disk.flush(info_hash).await
    }
}

/// Memory usage statistics.
#[derive(Debug, Clone, Copy)]
pub struct MemoryStats {
    /// Bytes used by the block cache.
    pub block_cache_used: usize,
    /// Maximum bytes for the block cache.
    pub block_cache_limit: usize,
    /// Total memory budget.
    pub total_budget: usize,
    /// Total budget currently used.
    pub budget_used: usize,
}

impl Default for CachingDiskManager {
    fn default() -> Self {
        // Default: 256MB memory limit, 1000 piece cache
        Self::new(256 * 1024 * 1024, 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_stats() {
        let manager = CachingDiskManager::new(64 * 1024 * 1024, 100);
        let stats = manager.memory_stats();

        assert!(stats.total_budget > 0);
        assert_eq!(stats.budget_used, 0);
    }
}
