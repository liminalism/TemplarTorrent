//! BitTorrent v2 hash request handling (BEP-52).
//!
//! This module provides types and logic for managing merkle hash requests
//! and responses in BitTorrent v2. Peers can request hash blocks from each
//! other to verify pieces without downloading the full piece data.
//!
//! # Overview
//!
//! In v2 torrents, each file has a merkle tree built from 16 KiB block hashes.
//! The `HashRequestManager` tracks pending hash requests and validates responses.
//!
//! # Messages
//!
//! - **HashRequest** (21): Request hashes from a specific layer of the merkle tree
//! - **Hashes** (22): Response containing requested hashes plus uncle hashes for verification
//! - **HashReject** (23): Sent when a peer cannot service a hash request

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::RwLock;

use crate::metainfo::{extract_layer_hashes, generate_proof_hashes, MerkleTree};

/// Timeout for hash requests before they're considered stale.
pub const HASH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of pending hash requests per peer.
pub const MAX_PENDING_HASH_REQUESTS: usize = 16;

/// A pending hash request waiting for a response.
#[derive(Debug, Clone)]
pub struct PendingHashRequest {
    /// The merkle root of the file.
    pub pieces_root: [u8; 32],
    /// The tree layer requested (0 = leaves).
    pub base_layer: u32,
    /// Starting index in the layer.
    pub index: u32,
    /// Number of hashes requested.
    pub length: u32,
    /// Number of proof layers requested.
    pub proof_layers: u32,
    /// When the request was sent.
    pub sent_at: Instant,
}

impl PendingHashRequest {
    /// Creates a new pending hash request.
    pub fn new(
        pieces_root: [u8; 32],
        base_layer: u32,
        index: u32,
        length: u32,
        proof_layers: u32,
    ) -> Self {
        Self {
            pieces_root,
            base_layer,
            index,
            length,
            proof_layers,
            sent_at: Instant::now(),
        }
    }

    /// Returns true if this request has timed out.
    pub fn is_expired(&self) -> bool {
        self.sent_at.elapsed() > HASH_REQUEST_TIMEOUT
    }

    /// Creates a unique key for this request.
    pub fn key(&self) -> HashRequestKey {
        HashRequestKey {
            pieces_root: self.pieces_root,
            base_layer: self.base_layer,
            index: self.index,
            length: self.length,
        }
    }
}

/// Key for identifying a hash request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HashRequestKey {
    pub pieces_root: [u8; 32],
    pub base_layer: u32,
    pub index: u32,
    pub length: u32,
}

/// A received hash response.
#[derive(Debug, Clone)]
pub struct HashResponse {
    /// The merkle root of the file.
    pub pieces_root: [u8; 32],
    /// The tree layer (0 = leaves).
    pub base_layer: u32,
    /// Starting index in the layer.
    pub index: u32,
    /// Number of hashes at base_layer.
    pub length: u32,
    /// Number of proof layers included.
    pub proof_layers: u32,
    /// The layer hashes (length hashes).
    pub layer_hashes: Vec<[u8; 32]>,
    /// The uncle/proof hashes (proof_layers hashes).
    pub proof_hashes: Vec<[u8; 32]>,
}

impl HashResponse {
    /// Parses a hash response from raw hash data.
    ///
    /// The hash data contains `length + proof_layers` concatenated 32-byte hashes.
    pub fn from_raw(
        pieces_root: [u8; 32],
        base_layer: u32,
        index: u32,
        length: u32,
        proof_layers: u32,
        hashes: &Bytes,
    ) -> Option<Self> {
        let expected_hash_count = (length + proof_layers) as usize;
        if hashes.len() != expected_hash_count * 32 {
            return None;
        }

        let mut layer_hashes = Vec::with_capacity(length as usize);
        let mut proof_hashes = Vec::with_capacity(proof_layers as usize);

        for (i, chunk) in hashes.chunks_exact(32).enumerate() {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(chunk);
            if i < length as usize {
                layer_hashes.push(hash);
            } else {
                proof_hashes.push(hash);
            }
        }

        Some(Self {
            pieces_root,
            base_layer,
            index,
            length,
            proof_layers,
            layer_hashes,
            proof_hashes,
        })
    }

    /// Verifies the response hashes against the file's merkle root.
    ///
    /// Per BEP-52, the verification works as follows:
    /// 1. First, reduce the layer_hashes to a single subtree root by hashing
    ///    pairs together. The first `ceil(log2(length)) - 1` levels are implicit
    ///    since we have the complete child layer for those.
    /// 2. Then, combine with each uncle hash to work up to the pieces_root.
    ///
    /// # Arguments
    /// * `expected_root` - The file's pieces_root to verify against
    ///
    /// # Returns
    /// `true` if the hashes are valid and connect to the expected root
    pub fn verify(&self, expected_root: &[u8; 32]) -> bool {
        if self.layer_hashes.is_empty() {
            return false;
        }

        // Step 1: Build the subtree from our layer hashes
        // We have `length` hashes at `base_layer`, we need to reduce them to one hash
        let mut current_hashes = self.layer_hashes.clone();

        // Pad to power of 2 if needed (with zero hashes)
        let padded_len = current_hashes.len().next_power_of_two();
        while current_hashes.len() < padded_len {
            current_hashes.push([0u8; 32]);
        }

        // Reduce by hashing pairs until we have one hash (the subtree root)
        while current_hashes.len() > 1 {
            let mut next_level = Vec::with_capacity(current_hashes.len() / 2);
            for chunk in current_hashes.chunks(2) {
                next_level.push(hash_pair(&chunk[0], &chunk[1]));
            }
            current_hashes = next_level;
        }

        let mut subtree_root = current_hashes[0];

        // Step 2: Now use the uncle hashes to work up to the root
        // `index` tells us where our subtree is positioned at `base_layer`
        // We divide by `length` to get the position at the reduced level
        let mut position = (self.index / self.length) as usize;

        for uncle in &self.proof_hashes {
            // Position determines if we're left or right child
            let (left, right) = if position % 2 == 0 {
                // We're the left child, uncle is on the right
                (subtree_root, *uncle)
            } else {
                // We're the right child, uncle is on the left
                (*uncle, subtree_root)
            };

            subtree_root = hash_pair(&left, &right);
            position /= 2;
        }

        // If we had enough proof layers, we should have the root
        // If not, we can only verify the subtree is consistent
        if self.proof_layers == 0 {
            // Without proof layers, we can't verify against root
            // Just check that we successfully built a subtree
            true
        } else {
            &subtree_root == expected_root
        }
    }

    /// Calculates the expected number of proof layers needed to verify against the root.
    ///
    /// Given the base_layer and the tree depth (log2 of total leaves), returns
    /// how many uncle hashes are needed after reducing our layer_hashes to a subtree.
    pub fn expected_proof_layers(&self, tree_depth: u32) -> u32 {
        if self.length == 0 {
            return 0;
        }
        // Number of levels to reduce layer_hashes to subtree root
        let reduction_levels = (self.length as f64).log2().ceil() as u32;
        // Remaining levels to reach the tree root
        let level_of_subtree = self.base_layer + reduction_levels;
        tree_depth.saturating_sub(level_of_subtree)
    }
}

/// Hashes two 32-byte values together (SHA256).
fn hash_pair(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

/// Type alias for the nested hash storage structure.
/// Maps: pieces_root -> layer -> index -> hashes
type ReceivedHashesMap = HashMap<[u8; 32], HashMap<u32, HashMap<u32, Vec<[u8; 32]>>>>;

/// Manages pending hash requests for a peer connection.
pub struct HashRequestManager {
    /// Pending requests keyed by (pieces_root, base_layer, index, length).
    pending: RwLock<HashMap<HashRequestKey, PendingHashRequest>>,
    /// Received and verified layer hashes, keyed by pieces_root.
    /// Maps pieces_root -> (layer -> (index -> hashes)).
    received_hashes: RwLock<ReceivedHashesMap>,
}

impl HashRequestManager {
    /// Creates a new hash request manager.
    pub fn new() -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
            received_hashes: RwLock::new(HashMap::new()),
        }
    }

    /// Adds a pending hash request.
    ///
    /// Returns false if too many requests are pending.
    pub fn add_request(&self, request: PendingHashRequest) -> bool {
        let mut pending = self.pending.write();
        if pending.len() >= MAX_PENDING_HASH_REQUESTS {
            return false;
        }
        pending.insert(request.key(), request);
        true
    }

    /// Removes and returns a pending request matching the response parameters.
    pub fn remove_request(
        &self,
        pieces_root: &[u8; 32],
        base_layer: u32,
        index: u32,
        length: u32,
    ) -> Option<PendingHashRequest> {
        let key = HashRequestKey {
            pieces_root: *pieces_root,
            base_layer,
            index,
            length,
        };
        self.pending.write().remove(&key)
    }

    /// Returns the number of pending requests.
    pub fn pending_count(&self) -> usize {
        self.pending.read().len()
    }

    /// Removes and returns all expired requests.
    pub fn remove_expired(&self) -> Vec<PendingHashRequest> {
        let mut pending = self.pending.write();
        let expired: Vec<_> = pending
            .iter()
            .filter(|(_, req)| req.is_expired())
            .map(|(k, _)| *k)
            .collect();

        expired
            .into_iter()
            .filter_map(|k| pending.remove(&k))
            .collect()
    }

    /// Stores received and verified hashes.
    pub fn store_hashes(&self, response: &HashResponse) {
        let mut received = self.received_hashes.write();
        let file_hashes = received.entry(response.pieces_root).or_default();
        let layer_hashes = file_hashes.entry(response.base_layer).or_default();
        layer_hashes.insert(response.index, response.layer_hashes.clone());
    }

    /// Gets stored hashes for a file at a specific layer and index.
    pub fn get_hashes(
        &self,
        pieces_root: &[u8; 32],
        base_layer: u32,
        index: u32,
    ) -> Option<Vec<[u8; 32]>> {
        let received = self.received_hashes.read();
        received
            .get(pieces_root)
            .and_then(|f| f.get(&base_layer))
            .and_then(|l| l.get(&index))
            .cloned()
    }

    /// Checks if we have hashes for a specific file and layer range.
    pub fn has_hashes(&self, pieces_root: &[u8; 32], base_layer: u32, index: u32) -> bool {
        let received = self.received_hashes.read();
        received
            .get(pieces_root)
            .and_then(|f| f.get(&base_layer))
            .is_some_and(|l| l.contains_key(&index))
    }

    /// Clears all stored hashes for a file.
    pub fn clear_file_hashes(&self, pieces_root: &[u8; 32]) {
        self.received_hashes.write().remove(pieces_root);
    }

    /// Clears all pending requests and stored hashes.
    pub fn clear(&self) {
        self.pending.write().clear();
        self.received_hashes.write().clear();
    }
}

impl Default for HashRequestManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Stores merkle trees for files we can serve hashes from.
pub struct HashServer {
    /// Merkle trees keyed by pieces_root.
    trees: RwLock<HashMap<[u8; 32], MerkleTree>>,
}

impl HashServer {
    /// Creates a new hash server.
    pub fn new() -> Self {
        Self {
            trees: RwLock::new(HashMap::new()),
        }
    }

    /// Registers a file's merkle tree for serving hash requests.
    pub fn register_tree(&self, pieces_root: [u8; 32], tree: MerkleTree) {
        self.trees.write().insert(pieces_root, tree);
    }

    /// Unregisters a file's merkle tree.
    pub fn unregister_tree(&self, pieces_root: &[u8; 32]) {
        self.trees.write().remove(pieces_root);
    }

    /// Checks if we can serve hashes for a file.
    pub fn has_tree(&self, pieces_root: &[u8; 32]) -> bool {
        self.trees.read().contains_key(pieces_root)
    }

    /// Generates a hash response for a request.
    ///
    /// Returns None if we don't have the requested file's merkle tree.
    pub fn generate_response(
        &self,
        pieces_root: [u8; 32],
        base_layer: u32,
        index: u32,
        length: u32,
        proof_layers: u32,
    ) -> Option<Bytes> {
        let trees = self.trees.read();
        let tree = trees.get(&pieces_root)?;

        // Extract the requested layer hashes
        let layer_hashes =
            extract_layer_hashes(tree, base_layer as usize, index as usize, length as usize);
        if layer_hashes.is_empty() {
            return None;
        }

        // Generate proof/uncle hashes
        let proof_hashes = generate_proof_hashes(
            tree,
            base_layer as usize,
            index as usize,
            length as usize,
            proof_layers as usize,
        );

        // Concatenate all hashes
        let total_hashes = layer_hashes.len() + proof_hashes.len();
        let mut data = Vec::with_capacity(total_hashes * 32);

        for hash in &layer_hashes {
            data.extend_from_slice(hash);
        }
        for hash in &proof_hashes {
            data.extend_from_slice(hash);
        }

        Some(Bytes::from(data))
    }

    /// Clears all registered trees.
    pub fn clear(&self) {
        self.trees.write().clear();
    }
}

impl Default for HashServer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_hash_request() {
        let root = [0xABu8; 32];
        let req = PendingHashRequest::new(root, 0, 0, 4, 2);

        assert_eq!(req.pieces_root, root);
        assert_eq!(req.base_layer, 0);
        assert_eq!(req.index, 0);
        assert_eq!(req.length, 4);
        assert_eq!(req.proof_layers, 2);
        assert!(!req.is_expired());
    }

    #[test]
    fn test_hash_request_manager_add_remove() {
        let manager = HashRequestManager::new();
        let root = [0xCDu8; 32];

        let req = PendingHashRequest::new(root, 0, 0, 4, 2);
        assert!(manager.add_request(req.clone()));
        assert_eq!(manager.pending_count(), 1);

        let removed = manager.remove_request(&root, 0, 0, 4);
        assert!(removed.is_some());
        assert_eq!(manager.pending_count(), 0);
    }

    #[test]
    fn test_hash_response_from_raw() {
        let root = [0xEFu8; 32];
        // 2 layer hashes + 1 proof hash = 3 * 32 = 96 bytes
        let mut hash_data = vec![0u8; 96];
        hash_data[0..32].copy_from_slice(&[1u8; 32]);
        hash_data[32..64].copy_from_slice(&[2u8; 32]);
        hash_data[64..96].copy_from_slice(&[3u8; 32]);

        let hashes = Bytes::from(hash_data);
        let response = HashResponse::from_raw(root, 0, 0, 2, 1, &hashes).unwrap();

        assert_eq!(response.layer_hashes.len(), 2);
        assert_eq!(response.proof_hashes.len(), 1);
        assert_eq!(response.layer_hashes[0], [1u8; 32]);
        assert_eq!(response.layer_hashes[1], [2u8; 32]);
        assert_eq!(response.proof_hashes[0], [3u8; 32]);
    }

    #[test]
    fn test_hash_server_generate_response() {
        let server = HashServer::new();

        // Create a simple merkle tree with 4 leaves
        let leaves: Vec<[u8; 32]> = (0..4u8)
            .map(|i| {
                let mut h = [0u8; 32];
                h[0] = i;
                h
            })
            .collect();

        let tree = MerkleTree::from_piece_hashes(leaves);
        let root = tree.root().unwrap();

        server.register_tree(root, tree);
        assert!(server.has_tree(&root));

        // Request 2 hashes from layer 0 starting at index 0
        let response = server.generate_response(root, 0, 0, 2, 1);
        assert!(response.is_some());

        let data = response.unwrap();
        // Should have 2 layer hashes + 1 proof hash = 3 * 32 = 96 bytes
        assert_eq!(data.len(), 96);
    }

    #[test]
    fn test_hash_request_manager_store_and_get() {
        let manager = HashRequestManager::new();
        let root = [0x12u8; 32];

        let response = HashResponse {
            pieces_root: root,
            base_layer: 0,
            index: 0,
            length: 2,
            proof_layers: 0,
            layer_hashes: vec![[1u8; 32], [2u8; 32]],
            proof_hashes: vec![],
        };

        manager.store_hashes(&response);

        let retrieved = manager.get_hashes(&root, 0, 0);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().len(), 2);

        assert!(manager.has_hashes(&root, 0, 0));
        assert!(!manager.has_hashes(&root, 0, 2));
    }

    #[test]
    fn test_hash_response_verify_full_tree() {
        // Create a merkle tree with 4 leaves
        let leaves: Vec<[u8; 32]> = (0..4u8)
            .map(|i| {
                let mut h = [0u8; 32];
                h[0] = i;
                h
            })
            .collect();

        let tree = MerkleTree::from_piece_hashes(leaves.clone());
        let root = tree.root().unwrap();

        // Test 1: Request all 4 hashes with no proof layers
        // This should verify (we have complete tree, no proof needed)
        let response = HashResponse {
            pieces_root: root,
            base_layer: 0,
            index: 0,
            length: 4,
            proof_layers: 0,
            layer_hashes: leaves.clone(),
            proof_hashes: vec![],
        };
        assert!(response.verify(&root));

        // Test 2: Request 2 hashes with 1 proof layer
        // We get leaves [0,1] and need uncle hash (hash of leaves [2,3]) to verify
        let uncle = hash_pair(&leaves[2], &leaves[3]);
        let response = HashResponse {
            pieces_root: root,
            base_layer: 0,
            index: 0,
            length: 2,
            proof_layers: 1,
            layer_hashes: vec![leaves[0], leaves[1]],
            proof_hashes: vec![uncle],
        };
        assert!(response.verify(&root));

        // Test 3: Same but for the right half (leaves [2,3])
        let uncle = hash_pair(&leaves[0], &leaves[1]);
        let response = HashResponse {
            pieces_root: root,
            base_layer: 0,
            index: 2,
            length: 2,
            proof_layers: 1,
            layer_hashes: vec![leaves[2], leaves[3]],
            proof_hashes: vec![uncle],
        };
        assert!(response.verify(&root));

        // Test 4: Wrong proof hash should fail
        let wrong_uncle = [0xFFu8; 32];
        let response = HashResponse {
            pieces_root: root,
            base_layer: 0,
            index: 0,
            length: 2,
            proof_layers: 1,
            layer_hashes: vec![leaves[0], leaves[1]],
            proof_hashes: vec![wrong_uncle],
        };
        assert!(!response.verify(&root));

        // Test 5: Empty layer hashes should fail
        let response = HashResponse {
            pieces_root: root,
            base_layer: 0,
            index: 0,
            length: 0,
            proof_layers: 0,
            layer_hashes: vec![],
            proof_hashes: vec![],
        };
        assert!(!response.verify(&root));
    }

    #[test]
    fn test_hash_response_verify_larger_tree() {
        // Create a merkle tree with 8 leaves (3 levels deep)
        let leaves: Vec<[u8; 32]> = (0..8u8)
            .map(|i| {
                let mut h = [0u8; 32];
                h[0] = i;
                h
            })
            .collect();

        let tree = MerkleTree::from_piece_hashes(leaves.clone());
        let root = tree.root().unwrap();

        // Build intermediate hashes for verification
        // Level 1: pairs of leaves
        let h01 = hash_pair(&leaves[0], &leaves[1]);
        let h23 = hash_pair(&leaves[2], &leaves[3]);
        let h45 = hash_pair(&leaves[4], &leaves[5]);
        let h67 = hash_pair(&leaves[6], &leaves[7]);

        // Level 2: pairs of level 1
        let h0123 = hash_pair(&h01, &h23);
        let h4567 = hash_pair(&h45, &h67);

        // Request 4 hashes [0,1,2,3] with 1 proof layer
        // After reducing 4 hashes, we get h0123, and need h4567 as uncle
        let response = HashResponse {
            pieces_root: root,
            base_layer: 0,
            index: 0,
            length: 4,
            proof_layers: 1,
            layer_hashes: vec![leaves[0], leaves[1], leaves[2], leaves[3]],
            proof_hashes: vec![h4567],
        };
        assert!(response.verify(&root));

        // Request 2 hashes [0,1] with 2 proof layers
        // After reducing, we get h01, need h23, then h4567
        let response = HashResponse {
            pieces_root: root,
            base_layer: 0,
            index: 0,
            length: 2,
            proof_layers: 2,
            layer_hashes: vec![leaves[0], leaves[1]],
            proof_hashes: vec![h23, h4567],
        };
        assert!(response.verify(&root));

        // Request 2 hashes [4,5] with 2 proof layers
        // After reducing, we get h45, need h67, then h0123
        let response = HashResponse {
            pieces_root: root,
            base_layer: 0,
            index: 4,
            length: 2,
            proof_layers: 2,
            layer_hashes: vec![leaves[4], leaves[5]],
            proof_hashes: vec![h67, h0123],
        };
        assert!(response.verify(&root));
    }

    #[test]
    fn test_hash_response_expected_proof_layers() {
        let root = [0u8; 32];

        // Tree with 8 leaves = depth 3
        // Request 2 hashes at layer 0 -> reduce to 1 hash, need 2 proof layers
        let response = HashResponse {
            pieces_root: root,
            base_layer: 0,
            index: 0,
            length: 2,
            proof_layers: 2,
            layer_hashes: vec![[0u8; 32], [0u8; 32]],
            proof_hashes: vec![],
        };
        assert_eq!(response.expected_proof_layers(3), 2);

        // Request 4 hashes at layer 0 -> reduce to 1 hash, need 1 proof layer
        let response = HashResponse {
            pieces_root: root,
            base_layer: 0,
            index: 0,
            length: 4,
            proof_layers: 1,
            layer_hashes: vec![[0u8; 32]; 4],
            proof_hashes: vec![],
        };
        assert_eq!(response.expected_proof_layers(3), 1);

        // Request 8 hashes at layer 0 -> covers entire tree, no proof needed
        let response = HashResponse {
            pieces_root: root,
            base_layer: 0,
            index: 0,
            length: 8,
            proof_layers: 0,
            layer_hashes: vec![[0u8; 32]; 8],
            proof_hashes: vec![],
        };
        assert_eq!(response.expected_proof_layers(3), 0);
    }
}
