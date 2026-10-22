use sha2::{Digest, Sha256};

/// The standard block size for BitTorrent v2 merkle hashing (16 KiB).
pub const MERKLE_BLOCK_SIZE: usize = 16384;

/// A merkle tree for BitTorrent v2 piece verification.
///
/// In BEP-52, each file has a merkle tree built from 16 KiB block hashes.
/// The root of this tree is the file's `pieces_root`.
pub struct MerkleTree {
    leaves: Vec<[u8; 32]>,
    nodes: Vec<[u8; 32]>,
}

impl MerkleTree {
    pub fn new() -> Self {
        Self {
            leaves: Vec::new(),
            nodes: Vec::new(),
        }
    }

    pub fn from_piece_hashes(hashes: Vec<[u8; 32]>) -> Self {
        let mut tree = Self {
            leaves: hashes,
            nodes: Vec::new(),
        };
        tree.build();
        tree
    }

    pub fn add_leaf(&mut self, hash: [u8; 32]) {
        self.leaves.push(hash);
    }

    pub fn build(&mut self) {
        if self.leaves.is_empty() {
            return;
        }

        let leaf_count = self.leaves.len().next_power_of_two();
        let mut level: Vec<[u8; 32]> = self.leaves.clone();

        while level.len() < leaf_count {
            level.push([0u8; 32]);
        }

        self.nodes.clear();
        self.nodes.extend_from_slice(&level);

        while level.len() > 1 {
            let mut next_level = Vec::with_capacity(level.len() / 2);

            for chunk in level.chunks(2) {
                let hash = hash_pair(&chunk[0], &chunk[1]);
                next_level.push(hash);
            }

            self.nodes.extend_from_slice(&next_level);
            level = next_level;
        }
    }

    pub fn root(&self) -> Option<[u8; 32]> {
        self.nodes.last().copied()
    }

    pub fn proof(&self, leaf_index: usize) -> Option<Vec<[u8; 32]>> {
        if leaf_index >= self.leaves.len() {
            return None;
        }

        let leaf_count = self.leaves.len().next_power_of_two();
        let mut proof = Vec::new();
        let mut index = leaf_index;
        let mut level_start = 0;
        let mut level_size = leaf_count;

        while level_size > 1 {
            let sibling_index = if index % 2 == 0 { index + 1 } else { index - 1 };

            if level_start + sibling_index < self.nodes.len() {
                proof.push(self.nodes[level_start + sibling_index]);
            }

            level_start += level_size;
            level_size /= 2;
            index /= 2;
        }

        Some(proof)
    }

    pub fn verify(root: &[u8; 32], leaf: &[u8; 32], index: usize, proof: &[[u8; 32]]) -> bool {
        let mut hash = *leaf;
        let mut idx = index;

        for sibling in proof {
            hash = if idx % 2 == 0 {
                hash_pair(&hash, sibling)
            } else {
                hash_pair(sibling, &hash)
            };
            idx /= 2;
        }

        &hash == root
    }

    pub fn leaf_count(&self) -> usize {
        self.leaves.len()
    }
}

impl Default for MerkleTree {
    fn default() -> Self {
        Self::new()
    }
}

fn hash_pair(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

/// Hashes a single block of data using SHA256.
pub fn hash_block(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Hashes data into 16 KiB block hashes for merkle tree construction.
///
/// This is the first step in v2 piece verification: split data into
/// 16 KiB blocks and hash each one.
pub fn hash_data_blocks(data: &[u8]) -> Vec<[u8; 32]> {
    data.chunks(MERKLE_BLOCK_SIZE).map(hash_block).collect()
}

/// Builds a merkle tree from piece data and returns the root hash.
///
/// This combines `hash_data_blocks` and tree construction into one step.
pub fn compute_root(data: &[u8]) -> [u8; 32] {
    let block_hashes = hash_data_blocks(data);
    if block_hashes.is_empty() {
        return [0u8; 32];
    }
    let tree = MerkleTree::from_piece_hashes(block_hashes);
    tree.root().unwrap_or([0u8; 32])
}

/// Verifies that piece data matches an expected merkle root.
///
/// This is the main verification function for v2 torrents.
pub fn verify_piece(data: &[u8], expected_root: &[u8; 32]) -> bool {
    let computed_root = compute_root(data);
    &computed_root == expected_root
}

/// Verifies piece data using layer hashes from the piece layers dictionary.
///
/// In BEP-52, piece layers contain the merkle root of each piece's subtree.
/// Each piece is divided into 16 KiB blocks, hashed, and formed into a merkle tree.
/// The root of that per-piece tree is stored in piece layers.
///
/// For the last piece of a file (which may be smaller than `piece_length`):
/// - Hash the actual data blocks
/// - Pad the leaf count to match what a full piece would have (power of 2)
/// - The padding uses zero hashes as per BEP-52
///
/// # Arguments
/// * `data` - The piece data to verify
/// * `expected_hash` - The expected merkle root from piece layers
/// * `piece_length` - The torrent's piece length (for calculating expected block count)
pub fn verify_piece_layer(data: &[u8], expected_hash: &[u8; 32], piece_length: u64) -> bool {
    if data.is_empty() {
        // Empty data can't match any valid hash
        return false;
    }

    // Hash the actual data blocks
    let mut block_hashes = hash_data_blocks(data);

    // Calculate how many blocks a full piece would have
    let full_piece_blocks = (piece_length as usize).div_ceil(MERKLE_BLOCK_SIZE);

    // If this is a partial piece (last piece), we need to pad with zero hashes
    // to match the tree structure of a full piece. BEP-52 specifies that
    // "remaining leaf hashes beyond the end of the file... are set to zero."
    while block_hashes.len() < full_piece_blocks {
        block_hashes.push([0u8; 32]);
    }

    // Build merkle tree and compare root
    let tree = MerkleTree::from_piece_hashes(block_hashes);
    let computed = tree.root().unwrap_or([0u8; 32]);

    &computed == expected_hash
}

/// Computes the merkle root for a piece, properly handling partial pieces.
///
/// This pads short pieces with zero hashes to match the expected tree structure.
pub fn compute_piece_root(data: &[u8], piece_length: u64) -> [u8; 32] {
    if data.is_empty() {
        return [0u8; 32];
    }

    let mut block_hashes = hash_data_blocks(data);
    let full_piece_blocks = (piece_length as usize).div_ceil(MERKLE_BLOCK_SIZE);

    // Pad with zero hashes for partial pieces
    while block_hashes.len() < full_piece_blocks {
        block_hashes.push([0u8; 32]);
    }

    let tree = MerkleTree::from_piece_hashes(block_hashes);
    tree.root().unwrap_or([0u8; 32])
}

/// Generates uncle hashes (proof) for a range of hashes in a Hashes response.
///
/// Per BEP-52, when responding to a HashRequest, we include:
/// 1. The requested hashes at `base_layer` starting from `index`
/// 2. Uncle hashes for `proof_layers` ancestor levels
///
/// The uncle hashes allow the requester to verify the hashes connect to
/// the file's pieces_root without having the entire tree.
///
/// # Arguments
/// * `tree` - The merkle tree for the file
/// * `base_layer` - The tree layer being requested (0 = leaves)
/// * `start_index` - Starting index in that layer
/// * `count` - Number of hashes being returned at base_layer
/// * `proof_layers` - Number of uncle hash layers to include
///
/// # Returns
/// Uncle hashes from bottom to top. Each layer has one hash (the sibling
/// subtree root needed to compute the parent).
pub fn generate_proof_hashes(
    tree: &MerkleTree,
    base_layer: usize,
    start_index: usize,
    count: usize,
    proof_layers: usize,
) -> Vec<[u8; 32]> {
    if proof_layers == 0 || tree.nodes.is_empty() {
        return Vec::new();
    }

    let leaf_count = tree.leaves.len().next_power_of_two();
    let mut proof_hashes = Vec::with_capacity(proof_layers);

    // The range of indices we're proving, adjusted for base_layer
    // At layer 0 (leaves), index N covers block N
    // At layer 1, index N covers blocks [2N, 2N+1]
    // etc.
    let scale = 1usize << base_layer;
    let effective_start = start_index * scale;
    let effective_end = effective_start + count * scale;

    // Walk up the tree from base_layer, collecting sibling subtree roots
    let mut current_start = effective_start;
    let mut current_end = effective_end;
    let mut level_size = leaf_count >> base_layer;
    let mut level_start = calculate_level_start(leaf_count, base_layer);

    for _ in 0..proof_layers {
        if level_size <= 1 {
            break;
        }

        // Find which parent node(s) we need
        let parent_start = current_start / 2;
        let parent_end = current_end.div_ceil(2);

        // The uncle is the sibling subtree we DON'T have
        // If our range starts at an even index, uncle is at odd (right)
        // If our range starts at an odd index, uncle is at even (left)
        let uncle_index = if current_start % 2 == 0 {
            // We have left subtree, need right
            current_end
        } else {
            // We have right subtree, need left
            current_start - 1
        };

        // Clamp uncle_index to valid range
        let uncle_index = uncle_index.min(level_size - 1);

        // Get the hash at this position
        if level_start + uncle_index < tree.nodes.len() {
            proof_hashes.push(tree.nodes[level_start + uncle_index]);
        }

        // Move up one level
        current_start = parent_start;
        current_end = parent_end;
        level_start += level_size;
        level_size /= 2;
    }

    proof_hashes
}

/// Calculates the starting index in nodes array for a given tree level.
fn calculate_level_start(leaf_count: usize, level: usize) -> usize {
    let mut start = 0;
    let mut size = leaf_count;
    for _ in 0..level {
        start += size;
        size /= 2;
    }
    start
}

/// Extracts hashes from a specific layer of the merkle tree.
///
/// # Arguments
/// * `tree` - The merkle tree
/// * `layer` - Tree layer (0 = leaves, 1 = first parent level, etc.)
/// * `start_index` - Starting index in that layer
/// * `count` - Number of hashes to extract
///
/// # Returns
/// The requested hashes, or fewer if the range exceeds the layer size.
pub fn extract_layer_hashes(
    tree: &MerkleTree,
    layer: usize,
    start_index: usize,
    count: usize,
) -> Vec<[u8; 32]> {
    if tree.nodes.is_empty() {
        return Vec::new();
    }

    let leaf_count = tree.leaves.len().next_power_of_two();
    let level_size = leaf_count >> layer;

    if level_size == 0 || start_index >= level_size {
        return Vec::new();
    }

    let level_start = calculate_level_start(leaf_count, layer);
    let actual_count = count.min(level_size - start_index);

    tree.nodes[level_start + start_index..level_start + start_index + actual_count].to_vec()
}
