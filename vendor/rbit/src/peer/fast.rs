use std::collections::HashSet;
use std::net::IpAddr;

use sha1::{Digest, Sha1};

const ALLOWED_FAST_SET_SIZE: usize = 10;

/// State for the Fast Extension (BEP-6) per peer.
///
/// Tracks allowed fast pieces, suggested pieces, and reject handling.
#[derive(Debug, Clone, Default)]
pub struct FastExtensionState {
    /// Pieces the remote peer has allowed us to download while choked.
    pub allowed_fast_incoming: HashSet<u32>,
    /// Pieces we have allowed the remote peer to download while choked.
    pub allowed_fast_outgoing: HashSet<u32>,
    /// Pieces the remote peer has suggested we download.
    pub suggested_pieces: Vec<u32>,
    /// Whether the remote peer sent HaveAll.
    pub peer_has_all: bool,
    /// Whether the remote peer sent HaveNone.
    pub peer_has_none: bool,
}

impl FastExtensionState {
    /// Creates a new FastExtensionState.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an AllowedFast piece from the remote peer.
    pub fn add_allowed_fast_incoming(&mut self, piece: u32) {
        self.allowed_fast_incoming.insert(piece);
    }

    /// Records an AllowedFast piece we're sending to the remote peer.
    pub fn add_allowed_fast_outgoing(&mut self, piece: u32) {
        self.allowed_fast_outgoing.insert(piece);
    }

    /// Checks if we can request a piece while choked.
    pub fn can_request_while_choked(&self, piece: u32) -> bool {
        self.allowed_fast_incoming.contains(&piece)
    }

    /// Checks if we should serve a request from a choked peer.
    pub fn should_serve_choked_request(&self, piece: u32) -> bool {
        self.allowed_fast_outgoing.contains(&piece)
    }

    /// Records a SuggestPiece from the remote peer.
    pub fn add_suggested(&mut self, piece: u32) {
        if !self.suggested_pieces.contains(&piece) {
            self.suggested_pieces.push(piece);
        }
    }

    /// Records that the peer sent HaveAll.
    pub fn set_have_all(&mut self) {
        self.peer_has_all = true;
        self.peer_has_none = false;
    }

    /// Records that the peer sent HaveNone.
    pub fn set_have_none(&mut self) {
        self.peer_has_none = true;
        self.peer_has_all = false;
    }

    /// Clears all state.
    pub fn clear(&mut self) {
        self.allowed_fast_incoming.clear();
        self.allowed_fast_outgoing.clear();
        self.suggested_pieces.clear();
        self.peer_has_all = false;
        self.peer_has_none = false;
    }
}

/// Generates the allowed fast set for a peer (BEP-6).
///
/// This deterministically generates a set of piece indices that a peer
/// is allowed to request even while choked. The algorithm uses the peer's
/// IP address and the torrent's info hash to generate the set.
///
/// # Arguments
///
/// * `info_hash` - The 20-byte info hash of the torrent
/// * `peer_ip` - The IP address of the peer
/// * `num_pieces` - The total number of pieces in the torrent
/// * `set_size` - The size of the allowed fast set (typically 10)
///
/// # Returns
///
/// A vector of piece indices that should be allowed fast.
pub fn generate_allowed_fast_set(
    info_hash: &[u8; 20],
    peer_ip: IpAddr,
    num_pieces: u32,
    set_size: usize,
) -> Vec<u32> {
    if num_pieces == 0 {
        return Vec::new();
    }

    let mut allowed_set = Vec::with_capacity(set_size);

    // Mask the IP to /24 for IPv4 or use first 4 bytes for IPv6
    let ip_bytes = match peer_ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            [octets[0], octets[1], octets[2], 0]
        }
        IpAddr::V6(ip) => {
            let octets = ip.octets();
            [octets[0], octets[1], octets[2], octets[3]]
        }
    };

    let mut x = Vec::with_capacity(24);
    x.extend_from_slice(&ip_bytes);
    x.extend_from_slice(info_hash);

    while allowed_set.len() < set_size {
        let mut hasher = Sha1::new();
        hasher.update(&x);
        let hash = hasher.finalize();

        for chunk in hash.chunks(4) {
            if allowed_set.len() >= set_size {
                break;
            }

            let index = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) % num_pieces;

            if !allowed_set.contains(&index) {
                allowed_set.push(index);
            }
        }

        x = hash.to_vec();
    }

    allowed_set
}

pub struct FastExtension {
    allowed_fast_set: Vec<u32>,
}

impl FastExtension {
    pub fn new() -> Self {
        Self {
            allowed_fast_set: Vec::new(),
        }
    }

    pub fn compute_allowed_fast_set(
        &mut self,
        info_hash: &[u8; 20],
        peer_ip: IpAddr,
        num_pieces: u32,
    ) {
        self.allowed_fast_set =
            generate_allowed_fast_set(info_hash, peer_ip, num_pieces, ALLOWED_FAST_SET_SIZE);
    }

    pub fn allowed_fast_set(&self) -> &[u32] {
        &self.allowed_fast_set
    }

    pub fn is_allowed_fast(&self, piece: u32) -> bool {
        self.allowed_fast_set.contains(&piece)
    }
}

impl Default for FastExtension {
    fn default() -> Self {
        Self::new()
    }
}
