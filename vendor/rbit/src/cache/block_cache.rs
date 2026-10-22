use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use dashmap::DashMap;
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::metainfo::MerkleTree;

/// Standard block size for BitTorrent (16 KiB).
pub const BLOCK_SIZE: u32 = 16384;

/// Merkle block size for v2 verification (also 16 KiB).
pub const MERKLE_BLOCK_SIZE: u32 = 16384;

/// Incremental hash state for piece verification.
///
/// For v1 torrents, this is used for the final piece hash verification.
/// For v2 torrents, this is maintained for compatibility but actual verification
/// uses merkle trees built from per-block hashes (see `finalize_and_verify_v2`).
#[derive(Clone)]
pub enum HashState {
    /// SHA1 hash state for v1 torrents.
    V1(Sha1),
    /// SHA256 hash state for v2 torrents.
    /// Note: For v2, the actual verification uses merkle trees from block hashes,
    /// not this flat hash. This is kept for potential future use and consistency.
    V2(Sha256),
}

impl HashState {
    pub fn new_v1() -> Self {
        HashState::V1(Sha1::new())
    }

    pub fn new_v2() -> Self {
        HashState::V2(Sha256::new())
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            HashState::V1(h) => h.update(data),
            HashState::V2(h) => h.update(data),
        }
    }

    pub fn finalize(self) -> Vec<u8> {
        match self {
            HashState::V1(h) => h.finalize().to_vec(),
            HashState::V2(h) => h.finalize().to_vec(),
        }
    }

    /// Returns true if this is a v1 hash state.
    pub fn is_v1(&self) -> bool {
        matches!(self, HashState::V1(_))
    }

    /// Returns true if this is a v2 hash state.
    pub fn is_v2(&self) -> bool {
        matches!(self, HashState::V2(_))
    }
}

/// Internal state for tracking blocks of an in-progress piece download.
///
/// For v1 torrents, blocks are assembled and hashed incrementally using SHA1.
/// For v2 torrents, each 16 KiB block is hashed individually with SHA256,
/// then a merkle tree is built from those hashes for verification.
struct PieceBlocks {
    /// Downloaded blocks, keyed by offset within the piece.
    blocks: BTreeMap<u32, Bytes>,
    /// Expected piece length in bytes.
    piece_length: u32,
    /// Number of bytes that have been fed to the incremental hasher.
    bytes_hashed: u32,
    /// When download of this piece started.
    #[allow(dead_code)]
    started_at: Instant,
    /// For v2: individual SHA256 hashes of each 16 KiB block.
    /// These are combined into a merkle tree for verification.
    /// None for v1 torrents.
    block_hashes: Option<Vec<[u8; 32]>>,
}

impl PieceBlocks {
    fn new(piece_length: u32) -> Self {
        Self {
            blocks: BTreeMap::new(),
            piece_length,
            bytes_hashed: 0,
            started_at: Instant::now(),
            block_hashes: None,
        }
    }

    fn new_v2(piece_length: u32) -> Self {
        let block_count = piece_length.div_ceil(MERKLE_BLOCK_SIZE) as usize;
        Self {
            blocks: BTreeMap::new(),
            piece_length,
            bytes_hashed: 0,
            started_at: Instant::now(),
            block_hashes: Some(vec![[0u8; 32]; block_count]),
        }
    }

    fn is_complete(&self) -> bool {
        let block_count = self.piece_length.div_ceil(BLOCK_SIZE);
        self.blocks.len() as u32 == block_count
    }

    fn total_bytes(&self) -> usize {
        self.blocks.values().map(|b| b.len()).sum()
    }

    fn assemble(&self) -> Bytes {
        let mut data = Vec::with_capacity(self.piece_length as usize);
        for block in self.blocks.values() {
            data.extend_from_slice(block);
        }
        Bytes::from(data)
    }
}

type CacheKey = (String, u32);

/// In-memory cache for piece blocks during download.
///
/// The `BlockCache` stores downloaded blocks before they're assembled into
/// complete pieces and written to disk. It supports both v1 (SHA1) and v2 (merkle)
/// verification modes.
///
/// # V1 vs V2 Verification
///
/// - **V1**: Uses incremental SHA1 hashing. Blocks are fed to the hasher in order,
///   and the final hash is compared against the expected piece hash.
///
/// - **V2**: Uses merkle tree verification. Each 16 KiB block is hashed individually
///   with SHA256. When complete, a merkle tree is built from these block hashes
///   and the root is compared against the expected hash from piece layers.
///
/// # Example
///
/// ```
/// use rbit::cache::BlockCache;
/// use bytes::Bytes;
///
/// let cache = BlockCache::new(64 * 1024 * 1024); // 64 MB limit
///
/// // Add a block (returns true if piece is complete)
/// let is_complete = cache.add_block(
///     "info_hash_hex",
///     0,      // piece index
///     0,      // offset
///     Bytes::from(vec![0u8; 16384]),
///     16384,  // piece length
///     1,      // hash version (1 = SHA1, 2 = merkle)
/// );
/// ```
pub struct BlockCache {
    /// In-progress pieces, keyed by (info_hash, piece_index).
    pieces: DashMap<CacheKey, PieceBlocks>,
    /// Incremental hash states for v1 verification.
    hash_states: DashMap<CacheKey, HashState>,
    /// Current memory usage in bytes.
    memory_used: AtomicUsize,
    /// Maximum memory allowed for caching.
    memory_limit: usize,
}

impl BlockCache {
    pub fn new(memory_limit: usize) -> Arc<Self> {
        Arc::new(Self {
            pieces: DashMap::new(),
            hash_states: DashMap::new(),
            memory_used: AtomicUsize::new(0),
            memory_limit,
        })
    }

    pub fn add_block(
        &self,
        info_hash: &str,
        piece_index: u32,
        offset: u32,
        data: Bytes,
        piece_length: u32,
        hash_version: u8,
    ) -> bool {
        let key = (info_hash.to_string(), piece_index);
        let data_len = data.len();
        let is_v2 = hash_version == 2;

        {
            let mut piece = self.pieces.entry(key.clone()).or_insert_with(|| {
                let state = if is_v2 {
                    HashState::new_v2()
                } else {
                    HashState::new_v1()
                };
                self.hash_states.insert(key.clone(), state);
                if is_v2 {
                    PieceBlocks::new_v2(piece_length)
                } else {
                    PieceBlocks::new(piece_length)
                }
            });

            // For v2, compute and store individual block hash
            if is_v2 {
                if let Some(ref mut block_hashes) = piece.block_hashes {
                    let block_index = (offset / MERKLE_BLOCK_SIZE) as usize;
                    if block_index < block_hashes.len() {
                        let mut hasher = Sha256::new();
                        hasher.update(&data);
                        block_hashes[block_index] = hasher.finalize().into();
                    }
                }
            }

            if piece.blocks.insert(offset, data).is_none() {
                self.memory_used.fetch_add(data_len, Ordering::Relaxed);
            }

            self.try_advance_hash(&key, &mut piece);
            piece.is_complete()
        }
    }

    fn try_advance_hash(&self, key: &CacheKey, piece: &mut PieceBlocks) {
        if let Some(mut state) = self.hash_states.get_mut(key) {
            let mut next_offset = piece.bytes_hashed;
            while let Some(block) = piece.blocks.get(&next_offset) {
                state.update(block);
                next_offset += block.len() as u32;
            }
            piece.bytes_hashed = next_offset;
        }
    }

    pub fn finalize_and_verify(&self, info_hash: &str, piece_index: u32, expected: &[u8]) -> bool {
        let key = (info_hash.to_string(), piece_index);

        if let Some(mut piece) = self.pieces.get_mut(&key) {
            self.try_advance_hash(&key, &mut piece);
            if piece.bytes_hashed != piece.piece_length {
                return false;
            }
        }

        if let Some((_, state)) = self.hash_states.remove(&key) {
            let computed = state.finalize();
            computed == expected
        } else {
            false
        }
    }

    /// Verifies a v2 piece using merkle tree verification.
    ///
    /// This builds a merkle tree from the stored block hashes and compares
    /// the root against the expected hash (from piece layers).
    ///
    /// # Arguments
    /// * `info_hash` - The torrent's info hash (as hex string)
    /// * `piece_index` - The piece index
    /// * `expected` - The expected merkle root from piece layers
    /// * `full_piece_length` - The torrent's piece length (for padding calculation)
    ///
    /// For partial pieces (last piece of a file), the block hashes are padded
    /// with zeros to match the expected tree structure.
    pub fn finalize_and_verify_v2(
        &self,
        info_hash: &str,
        piece_index: u32,
        expected: &[u8; 32],
        full_piece_length: u32,
    ) -> bool {
        let key = (info_hash.to_string(), piece_index);

        if let Some(piece) = self.pieces.get(&key) {
            if !piece.is_complete() {
                return false;
            }

            // Use stored block hashes to build merkle tree
            if let Some(ref block_hashes) = piece.block_hashes {
                // Calculate expected block count for a full piece
                let expected_blocks =
                    (full_piece_length as usize).div_ceil(MERKLE_BLOCK_SIZE as usize);

                // Pad with zero hashes if this is a partial piece
                let mut padded_hashes = block_hashes.clone();
                while padded_hashes.len() < expected_blocks {
                    padded_hashes.push([0u8; 32]);
                }

                let computed_root = Self::compute_merkle_root(&padded_hashes);
                return &computed_root == expected;
            }
        }

        false
    }

    /// Verifies a piece using the appropriate method based on version.
    ///
    /// This is a convenience method that automatically selects v1 or v2
    /// verification based on the expected hash length.
    pub fn finalize_and_verify_auto(
        &self,
        info_hash: &str,
        piece_index: u32,
        expected: &[u8],
        piece_length: u32,
    ) -> bool {
        if expected.len() == 32 {
            // V2 verification with merkle tree
            let mut expected_arr = [0u8; 32];
            expected_arr.copy_from_slice(expected);
            self.finalize_and_verify_v2(info_hash, piece_index, &expected_arr, piece_length)
        } else {
            // V1 verification with simple hash
            self.finalize_and_verify(info_hash, piece_index, expected)
        }
    }

    /// Computes the merkle root from a list of block hashes.
    ///
    /// Uses the MerkleTree from the metainfo module for consistency with
    /// the rest of the v2 implementation.
    fn compute_merkle_root(block_hashes: &[[u8; 32]]) -> [u8; 32] {
        if block_hashes.is_empty() {
            return [0u8; 32];
        }

        let tree = MerkleTree::from_piece_hashes(block_hashes.to_vec());
        tree.root().unwrap_or([0u8; 32])
    }

    /// Gets the block hashes for a piece (for v2 verification).
    pub fn get_block_hashes(&self, info_hash: &str, piece_index: u32) -> Option<Vec<[u8; 32]>> {
        let key = (info_hash.to_string(), piece_index);
        self.pieces.get(&key).and_then(|p| p.block_hashes.clone())
    }

    pub fn get_assembled_piece(&self, info_hash: &str, piece_index: u32) -> Option<Bytes> {
        let key = (info_hash.to_string(), piece_index);
        self.pieces.get(&key).map(|p| p.assemble())
    }

    pub fn remove_piece(&self, info_hash: &str, piece_index: u32) -> Option<Bytes> {
        let key = (info_hash.to_string(), piece_index);
        self.hash_states.remove(&key);
        if let Some((_, piece)) = self.pieces.remove(&key) {
            let bytes_freed = piece.total_bytes();
            self.memory_used.fetch_sub(bytes_freed, Ordering::Relaxed);
            Some(piece.assemble())
        } else {
            None
        }
    }

    pub fn has_piece(&self, info_hash: &str, piece_index: u32) -> bool {
        let key = (info_hash.to_string(), piece_index);
        self.pieces.contains_key(&key)
    }

    pub fn is_piece_complete(&self, info_hash: &str, piece_index: u32) -> bool {
        let key = (info_hash.to_string(), piece_index);
        self.pieces
            .get(&key)
            .map(|p| p.is_complete())
            .unwrap_or(false)
    }

    pub fn memory_used(&self) -> usize {
        self.memory_used.load(Ordering::Relaxed)
    }

    pub fn memory_limit(&self) -> usize {
        self.memory_limit
    }

    pub fn is_under_pressure(&self) -> bool {
        self.memory_used() > (self.memory_limit as f32 * 0.9) as usize
    }

    pub fn pieces_count(&self) -> usize {
        self.pieces.len()
    }

    pub fn clear(&self) {
        self.pieces.clear();
        self.hash_states.clear();
        self.memory_used.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::hash_block;

    #[test]
    fn test_block_cache_basic_operations() {
        let cache = BlockCache::new(64 * 1024 * 1024);

        // Add a single block
        let data = Bytes::from(vec![0u8; BLOCK_SIZE as usize]);
        let is_complete = cache.add_block("test_hash", 0, 0, data, BLOCK_SIZE, 1);
        assert!(is_complete);
        assert!(cache.is_piece_complete("test_hash", 0));
        assert!(cache.has_piece("test_hash", 0));

        // Get assembled piece
        let piece = cache.get_assembled_piece("test_hash", 0);
        assert!(piece.is_some());
        assert_eq!(piece.unwrap().len(), BLOCK_SIZE as usize);

        // Remove piece
        let removed = cache.remove_piece("test_hash", 0);
        assert!(removed.is_some());
        assert!(!cache.has_piece("test_hash", 0));
    }

    #[test]
    fn test_block_cache_multi_block_piece() {
        let cache = BlockCache::new(64 * 1024 * 1024);

        // A piece with 4 blocks (64 KiB total)
        let piece_length = BLOCK_SIZE * 4;

        // Add blocks out of order
        let block2 = Bytes::from(vec![2u8; BLOCK_SIZE as usize]);
        let block0 = Bytes::from(vec![0u8; BLOCK_SIZE as usize]);
        let block3 = Bytes::from(vec![3u8; BLOCK_SIZE as usize]);
        let block1 = Bytes::from(vec![1u8; BLOCK_SIZE as usize]);

        assert!(!cache.add_block("test", 0, BLOCK_SIZE * 2, block2, piece_length, 1));
        assert!(!cache.add_block("test", 0, 0, block0, piece_length, 1));
        assert!(!cache.add_block("test", 0, BLOCK_SIZE * 3, block3, piece_length, 1));
        assert!(cache.add_block("test", 0, BLOCK_SIZE, block1, piece_length, 1));

        assert!(cache.is_piece_complete("test", 0));

        // Verify assembled piece is correct
        let piece = cache.get_assembled_piece("test", 0).unwrap();
        assert_eq!(piece.len(), piece_length as usize);
        // First block should be all 0s
        assert!(piece[0..BLOCK_SIZE as usize].iter().all(|&b| b == 0));
        // Second block should be all 1s
        assert!(piece[BLOCK_SIZE as usize..(BLOCK_SIZE * 2) as usize]
            .iter()
            .all(|&b| b == 1));
    }

    #[test]
    fn test_block_cache_v1_verification() {
        let cache = BlockCache::new(64 * 1024 * 1024);

        // Create deterministic data
        let data: Vec<u8> = (0..BLOCK_SIZE as usize).map(|i| (i % 256) as u8).collect();
        let data = Bytes::from(data);

        // Compute expected SHA1 hash
        let expected_hash = {
            use sha1::{Digest, Sha1};
            let mut hasher = Sha1::new();
            hasher.update(&data);
            hasher.finalize().to_vec()
        };

        // Add block
        cache.add_block("v1test", 0, 0, data, BLOCK_SIZE, 1);

        // Verify with correct hash
        assert!(cache.finalize_and_verify("v1test", 0, &expected_hash));
    }

    #[test]
    fn test_block_cache_v1_verification_fails_wrong_hash() {
        let cache = BlockCache::new(64 * 1024 * 1024);

        let data = Bytes::from(vec![42u8; BLOCK_SIZE as usize]);
        cache.add_block("v1test", 0, 0, data, BLOCK_SIZE, 1);

        // Verify with wrong hash should fail
        let wrong_hash = vec![0u8; 20];
        assert!(!cache.finalize_and_verify("v1test", 0, &wrong_hash));
    }

    #[test]
    fn test_block_cache_v2_single_block() {
        let cache = BlockCache::new(64 * 1024 * 1024);

        // Create a piece with exactly one block
        let data: Vec<u8> = (0..BLOCK_SIZE as usize).map(|i| (i % 256) as u8).collect();
        let data = Bytes::from(data);

        // Compute expected merkle root (for single block, it's just the block hash)
        let expected_root = hash_block(&data);

        // Add block with v2 mode
        cache.add_block("v2test", 0, 0, data, BLOCK_SIZE, 2);

        // Verify
        assert!(cache.finalize_and_verify_v2("v2test", 0, &expected_root, BLOCK_SIZE));

        // Should also work with finalize_and_verify_auto
        assert!(cache.finalize_and_verify_auto("v2test", 0, &expected_root, BLOCK_SIZE));
    }

    #[test]
    fn test_block_cache_v2_multi_block() {
        let cache = BlockCache::new(64 * 1024 * 1024);

        // Create a piece with 4 blocks
        let piece_length = BLOCK_SIZE * 4;
        let block0: Vec<u8> = (0..BLOCK_SIZE as usize).map(|_| 0u8).collect();
        let block1: Vec<u8> = (0..BLOCK_SIZE as usize).map(|_| 1u8).collect();
        let block2: Vec<u8> = (0..BLOCK_SIZE as usize).map(|_| 2u8).collect();
        let block3: Vec<u8> = (0..BLOCK_SIZE as usize).map(|_| 3u8).collect();

        // Compute expected merkle root
        let h0 = hash_block(&block0);
        let h1 = hash_block(&block1);
        let h2 = hash_block(&block2);
        let h3 = hash_block(&block3);
        let tree = MerkleTree::from_piece_hashes(vec![h0, h1, h2, h3]);
        let expected_root = tree.root().unwrap();

        // Add blocks
        cache.add_block("v2multi", 0, 0, Bytes::from(block0), piece_length, 2);
        cache.add_block(
            "v2multi",
            0,
            BLOCK_SIZE,
            Bytes::from(block1),
            piece_length,
            2,
        );
        cache.add_block(
            "v2multi",
            0,
            BLOCK_SIZE * 2,
            Bytes::from(block2),
            piece_length,
            2,
        );
        cache.add_block(
            "v2multi",
            0,
            BLOCK_SIZE * 3,
            Bytes::from(block3),
            piece_length,
            2,
        );

        assert!(cache.is_piece_complete("v2multi", 0));
        assert!(cache.finalize_and_verify_v2("v2multi", 0, &expected_root, piece_length));
    }

    #[test]
    fn test_block_cache_v2_partial_piece() {
        let cache = BlockCache::new(64 * 1024 * 1024);

        // Create a partial piece (smaller than full piece length)
        // This simulates the last piece of a file
        let full_piece_length = BLOCK_SIZE * 4;
        let actual_data_len = BLOCK_SIZE + 1000; // 1 full block + 1000 bytes

        let block0: Vec<u8> = (0..BLOCK_SIZE as usize).map(|_| 0xAA).collect();
        let block1: Vec<u8> = (0..1000usize).map(|_| 0xBB).collect();

        // Compute expected merkle root
        // Block hashes for actual data
        let h0 = hash_block(&block0);
        let h1 = hash_block(&block1);
        // Pad with zero hashes to match full piece block count
        let tree = MerkleTree::from_piece_hashes(vec![h0, h1, [0u8; 32], [0u8; 32]]);
        let expected_root = tree.root().unwrap();

        // Add blocks - note: we need to set piece_length to actual size for block counting
        // but pass full_piece_length to verification for proper padding
        cache.add_block("partial", 0, 0, Bytes::from(block0), actual_data_len, 2);
        cache.add_block(
            "partial",
            0,
            BLOCK_SIZE,
            Bytes::from(block1),
            actual_data_len,
            2,
        );

        assert!(cache.is_piece_complete("partial", 0));
        assert!(cache.finalize_and_verify_v2("partial", 0, &expected_root, full_piece_length));
    }

    #[test]
    fn test_block_cache_v2_verification_fails_wrong_hash() {
        let cache = BlockCache::new(64 * 1024 * 1024);

        let data = Bytes::from(vec![42u8; BLOCK_SIZE as usize]);
        cache.add_block("v2wrong", 0, 0, data, BLOCK_SIZE, 2);

        // Verify with wrong hash should fail
        let wrong_root = [0xFFu8; 32];
        assert!(!cache.finalize_and_verify_v2("v2wrong", 0, &wrong_root, BLOCK_SIZE));
    }

    #[test]
    fn test_block_cache_get_block_hashes() {
        let cache = BlockCache::new(64 * 1024 * 1024);

        let block0 = Bytes::from(vec![0u8; BLOCK_SIZE as usize]);
        let block1 = Bytes::from(vec![1u8; BLOCK_SIZE as usize]);
        let piece_length = BLOCK_SIZE * 2;

        cache.add_block("hashes", 0, 0, block0.clone(), piece_length, 2);
        cache.add_block("hashes", 0, BLOCK_SIZE, block1.clone(), piece_length, 2);

        let block_hashes = cache.get_block_hashes("hashes", 0);
        assert!(block_hashes.is_some());
        let block_hashes = block_hashes.unwrap();
        assert_eq!(block_hashes.len(), 2);

        // Verify hashes are correct
        assert_eq!(block_hashes[0], hash_block(&block0));
        assert_eq!(block_hashes[1], hash_block(&block1));
    }

    #[test]
    fn test_block_cache_memory_tracking() {
        let cache = BlockCache::new(64 * 1024 * 1024);

        assert_eq!(cache.memory_used(), 0);

        let data = Bytes::from(vec![0u8; BLOCK_SIZE as usize]);
        cache.add_block("mem", 0, 0, data, BLOCK_SIZE, 1);

        assert_eq!(cache.memory_used(), BLOCK_SIZE as usize);

        cache.remove_piece("mem", 0);
        assert_eq!(cache.memory_used(), 0);
    }

    #[test]
    fn test_block_cache_clear() {
        let cache = BlockCache::new(64 * 1024 * 1024);

        // Add some pieces
        let data = Bytes::from(vec![0u8; BLOCK_SIZE as usize]);
        cache.add_block("p1", 0, 0, data.clone(), BLOCK_SIZE, 1);
        cache.add_block("p2", 0, 0, data.clone(), BLOCK_SIZE, 1);
        cache.add_block("p3", 0, 0, data, BLOCK_SIZE, 1);

        assert_eq!(cache.pieces_count(), 3);

        cache.clear();

        assert_eq!(cache.pieces_count(), 0);
        assert_eq!(cache.memory_used(), 0);
    }

    #[test]
    fn test_block_cache_finalize_and_verify_auto() {
        let cache_v1 = BlockCache::new(64 * 1024 * 1024);
        let cache_v2 = BlockCache::new(64 * 1024 * 1024);

        let data = Bytes::from(vec![42u8; BLOCK_SIZE as usize]);

        // V1 hash (20 bytes)
        let v1_hash = {
            use sha1::{Digest, Sha1};
            let mut hasher = Sha1::new();
            hasher.update(&data);
            hasher.finalize().to_vec()
        };

        // V2 hash (32 bytes)
        let v2_hash = hash_block(&data);

        // Add to v1 cache
        cache_v1.add_block("auto_v1", 0, 0, data.clone(), BLOCK_SIZE, 1);
        // Add to v2 cache
        cache_v2.add_block("auto_v2", 0, 0, data, BLOCK_SIZE, 2);

        // finalize_and_verify_auto should detect correct version
        assert!(cache_v1.finalize_and_verify_auto("auto_v1", 0, &v1_hash, BLOCK_SIZE));
        assert!(cache_v2.finalize_and_verify_auto("auto_v2", 0, &v2_hash, BLOCK_SIZE));
    }

    #[test]
    fn test_compute_merkle_root_consistency() {
        // Verify that BlockCache::compute_merkle_root matches MerkleTree
        let hashes: Vec<[u8; 32]> = (0..4u8)
            .map(|i| {
                let mut h = [0u8; 32];
                h[0] = i;
                h
            })
            .collect();

        let tree = MerkleTree::from_piece_hashes(hashes.clone());
        let tree_root = tree.root().unwrap();

        let cache_root = BlockCache::compute_merkle_root(&hashes);

        assert_eq!(tree_root, cache_root);
    }
}
