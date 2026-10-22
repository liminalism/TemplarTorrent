//! Memory caching for pieces and blocks.
//!
//! This module provides in-memory caching to reduce disk I/O and improve
//! download performance.
//!
//! # Overview
//!
//! The cache system has several components:
//!
//! - [`PieceCache`] - Caches complete pieces using the ARC algorithm
//! - [`BlockCache`] - Caches individual blocks being downloaded
//! - [`BufferPool`] - Reusable buffer allocation
//! - [`MemoryBudget`] - Memory limit enforcement
//!
//! # ARC Caching Algorithm
//!
//! The [`PieceCache`] uses the Adaptive Replacement Cache (ARC) algorithm,
//! which adapts to access patterns by tracking both recent and frequent
//! access. This provides better hit rates than simple LRU for mixed workloads.
//!
//! # Examples
//!
//! ## Using the piece cache
//!
//! ```
//! use rbit::cache::PieceCache;
//! use bytes::Bytes;
//!
//! let cache = PieceCache::new(100); // capacity of 100 pieces
//!
//! // Cache a piece
//! let data = Bytes::from(vec![0u8; 16384]);
//! cache.insert("info_hash", 0, data.clone(), true);
//!
//! // Retrieve from cache
//! if let Some(cached) = cache.get("info_hash", 0) {
//!     assert_eq!(cached.len(), 16384);
//! }
//! ```
//!
//! ## Memory budgeting
//!
//! ```
//! use rbit::cache::MemoryBudget;
//!
//! // Create a 256MB budget
//! let budget = MemoryBudget::new(256 * 1024 * 1024);
//!
//! // Try to allocate memory
//! if let Some(permit) = budget.try_allocate(16384) {
//!     // Use the allocated memory
//!     println!("Allocated {} bytes", permit.bytes());
//!     // Memory is released when permit is dropped
//! }
//! ```

mod block_cache;
mod buffer_pool;
mod memory_budget;
mod piece_cache;

pub use block_cache::{BlockCache, HashState, BLOCK_SIZE, MERKLE_BLOCK_SIZE};
pub use buffer_pool::BufferPool;
pub use memory_budget::{MemoryBudget, MemoryPermit};
pub use piece_cache::PieceCache;
