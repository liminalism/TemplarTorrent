use std::fmt;

use rand::Rng as _;

const PEER_ID_PREFIX: &[u8] = b"-RB0001-";

/// A 20-byte peer identifier.
///
/// Peer IDs identify BitTorrent clients in the swarm. They follow the
/// Azureus-style format: `-XX0000-<random>` where XX is the client ID
/// and 0000 is the version number.
///
/// # Format
///
/// This library generates peer IDs in the format `-RB0001-<12 random bytes>`,
/// where `RB` identifies rbit and `0001` is the version.
///
/// # Examples
///
/// ```
/// use rbit::peer::PeerId;
///
/// // Generate a random peer ID
/// let peer_id = PeerId::generate();
/// println!("Peer ID: {}", peer_id);
///
/// // Access the raw bytes
/// let bytes = peer_id.as_bytes();
/// assert_eq!(bytes.len(), 20);
///
/// // Parse client identifier
/// if let Some(client) = peer_id.client_id() {
///     println!("Client: {}", client);  // e.g., "RB0001"
/// }
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub [u8; 20]);

impl PeerId {
    /// Generates a new random peer ID with the rbit client prefix.
    ///
    /// The generated ID follows the Azureus-style format: `-RB0001-<random>`.
    pub fn generate() -> Self {
        let mut id = [0u8; 20];
        id[..8].copy_from_slice(PEER_ID_PREFIX);
        rand::rng().fill(&mut id[8..]);
        Self(id)
    }

    /// Creates a peer ID from a 20-byte slice.
    ///
    /// Returns `None` if the slice is not exactly 20 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 20 {
            return None;
        }
        let mut id = [0u8; 20];
        id.copy_from_slice(bytes);
        Some(Self(id))
    }

    /// Returns the raw 20-byte peer ID.
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Extracts the client identifier if using Azureus-style format.
    ///
    /// Returns the 6-character client ID (e.g., "UT3500" for uTorrent 3.5.0.0)
    /// if the peer ID follows the `-XXXXXX-` format, otherwise `None`.
    pub fn client_id(&self) -> Option<&str> {
        if self.0[0] == b'-' && self.0[7] == b'-' {
            std::str::from_utf8(&self.0[1..7]).ok()
        } else {
            None
        }
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(client) = self.client_id() {
            write!(f, "PeerId({})", client)
        } else {
            write!(f, "PeerId({:02x?})", &self.0[..8])
        }
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            if byte.is_ascii_alphanumeric() || *byte == b'-' {
                write!(f, "{}", *byte as char)?;
            } else {
                write!(f, "%{:02x}", byte)?;
            }
        }
        Ok(())
    }
}
