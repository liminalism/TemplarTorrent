//! Disk storage and I/O management.
//!
//! This module handles reading and writing torrent piece data to disk,
//! including file mapping across multiple files and piece verification.
//!
//! # Overview
//!
//! Torrents split data into fixed-size pieces, and pieces may span multiple
//! files. This module handles the mapping between pieces/blocks and files.
//!
//! # Components
//!
//! - [`TorrentStorage`] - Per-torrent storage handler
//! - [`DiskManager`] - Manages storage for multiple torrents
//! - [`CachingDiskManager`] - High-level manager with integrated caching
//! - [`FileEntry`] - Metadata about a file in the torrent
//! - [`PieceInfo`] - Metadata about a piece (hash, offset, length)
//! - [`PieceFileSpan`] - Mapping of a piece to file regions
//!
//! # Examples
//!
//! ## Creating storage for a torrent
//!
//! ```no_run
//! use rbit::storage::{TorrentStorage, FileEntry, PieceInfo};
//! use std::path::PathBuf;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let files = vec![
//!     FileEntry::new(PathBuf::from("file1.txt"), 1000, 0),
//!     FileEntry::new(PathBuf::from("file2.txt"), 500, 1000),
//! ];
//!
//! let pieces = vec![
//!     PieceInfo::v1(0, [0u8; 20], 0, 512),
//!     PieceInfo::v1(1, [0u8; 20], 512, 512),
//!     PieceInfo::v1(2, [0u8; 20], 1024, 476),
//! ];
//!
//! let storage = TorrentStorage::new(
//!     PathBuf::from("./downloads"),
//!     files,
//!     pieces,
//!     1500,  // total length
//!     false, // is_v2
//! )?;
//!
//! // Write a piece
//! let data = vec![0u8; 512];
//! storage.write_piece(0, &data).await?;
//!
//! // Verify the piece hash
//! let valid = storage.verify_piece(0).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Using the DiskManager
//!
//! ```no_run
//! use rbit::storage::{DiskManager, TorrentStorage, FileEntry, PieceInfo};
//! use std::path::PathBuf;
//!
//! # fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let manager = DiskManager::new();
//!
//! // Register a torrent
//! let storage = TorrentStorage::new(
//!     PathBuf::from("./downloads"),
//!     vec![FileEntry::new(PathBuf::from("file.txt"), 1000, 0)],
//!     vec![PieceInfo::v1(0, [0u8; 20], 0, 1000)],
//!     1000,
//!     false,
//! )?;
//!
//! manager.register("info_hash_hex".to_string(), storage);
//! # Ok(())
//! # }
//! ```
//!
//! # Security
//!
//! The storage layer validates file paths to prevent directory traversal
//! attacks. Paths containing `..` or absolute paths are rejected.

mod caching;
mod error;
mod file;
mod io;
mod manager;

pub use caching::{CachingDiskManager, MemoryStats, WriteResult};
pub use error::StorageError;
pub use file::{AllocationMode, FileEntry, PieceFileSpan, PieceInfo, V2PieceMap};
pub use io::{
    coalesce_blocks, FlushRequest, FlushResult, IoQueue, IoWorker, WriteCoalescer, WriteOp,
    WritePriority, WriteRegion,
};
pub use manager::{DiskManager, TorrentStorage};

#[cfg(test)]
mod tests;
