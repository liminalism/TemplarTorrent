use std::fmt;

use super::error::MetainfoError;

/// A BitTorrent v1 info hash (20-byte SHA1).
///
/// This is a dedicated type for v1 info hashes, providing methods specific
/// to the v1 format including URL encoding for tracker announcements.
///
/// [BEP-3]: http://bittorrent.org/beps/bep_0003.html
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct InfoHashV1(pub [u8; 20]);

impl InfoHashV1 {
    /// Creates a v1 info hash from raw bytes.
    pub fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Creates a v1 info hash by hashing info dictionary bytes.
    pub fn from_info_bytes(info_bytes: &[u8]) -> Self {
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(info_bytes);
        let hash: [u8; 20] = hasher.finalize().into();
        Self(hash)
    }

    /// Parses a v1 info hash from a 40-character hex string.
    pub fn from_hex(s: &str) -> Result<Self, MetainfoError> {
        if s.len() != 40 {
            return Err(MetainfoError::InvalidInfoHashLength);
        }
        let bytes = hex_decode(s).ok_or(MetainfoError::InvalidInfoHashLength)?;
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }

    /// Returns the raw bytes of the info hash.
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Converts to a lowercase hexadecimal string.
    pub fn to_hex(&self) -> String {
        hex_encode(&self.0)
    }

    /// URL-encodes the info hash for use in tracker announce requests.
    ///
    /// Each byte is percent-encoded (e.g., `%ab%cd...`).
    pub fn url_encode(&self) -> String {
        self.0.iter().fold(String::with_capacity(60), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "%{:02x}", b);
            s
        })
    }
}

impl fmt::Debug for InfoHashV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InfoHashV1({})", self.to_hex())
    }
}

impl fmt::Display for InfoHashV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// A BitTorrent v2 info hash (32-byte SHA256).
///
/// This is a dedicated type for v2 info hashes, providing methods specific
/// to the v2 format.
///
/// [BEP-52]: http://bittorrent.org/beps/bep_0052.html
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct InfoHashV2(pub [u8; 32]);

impl InfoHashV2 {
    /// Creates a v2 info hash from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Creates a v2 info hash by hashing info dictionary bytes.
    pub fn from_info_bytes(info_bytes: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(info_bytes);
        let hash: [u8; 32] = hasher.finalize().into();
        Self(hash)
    }

    /// Parses a v2 info hash from a 64-character hex string.
    pub fn from_hex(s: &str) -> Result<Self, MetainfoError> {
        if s.len() != 64 {
            return Err(MetainfoError::InvalidInfoHashLength);
        }
        let bytes = hex_decode(s).ok_or(MetainfoError::InvalidInfoHashLength)?;
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }

    /// Returns the raw bytes of the info hash.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Converts to a lowercase hexadecimal string.
    pub fn to_hex(&self) -> String {
        hex_encode(&self.0)
    }
}

impl fmt::Debug for InfoHashV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InfoHashV2({})", self.to_hex())
    }
}

impl fmt::Display for InfoHashV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// A BitTorrent info hash identifying a torrent.
///
/// The info hash is a cryptographic hash of the torrent's `info` dictionary,
/// used to uniquely identify a torrent across the BitTorrent network.
///
/// # Versions
///
/// - **V1**: 20-byte SHA1 hash (original BitTorrent, [BEP-3])
/// - **V2**: 32-byte SHA256 hash (BitTorrent v2, [BEP-52])
/// - **Hybrid**: Both v1 and v2 hashes (BEP-47 hybrid torrents)
///
/// # Examples
///
/// ```
/// use rbit::metainfo::InfoHash;
///
/// // Parse from a hex string (automatically detects version)
/// let v1_hash = InfoHash::from_hex("c12fe1c06bba254a9dc9f519b335aa7c1367a88a").unwrap();
/// assert!(v1_hash.is_v1());
/// assert_eq!(v1_hash.as_bytes().len(), 20);
///
/// // Create from raw bytes
/// let bytes = [0u8; 20];
/// let hash = InfoHash::from_v1_bytes(&bytes).unwrap();
///
/// // Display as hex
/// println!("{}", hash);  // prints 40-character hex string
/// ```
///
/// [BEP-3]: http://bittorrent.org/beps/bep_0003.html
/// [BEP-47]: http://bittorrent.org/beps/bep_0047.html
/// [BEP-52]: http://bittorrent.org/beps/bep_0052.html
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum InfoHash {
    /// BitTorrent v1 info hash (20-byte SHA1).
    V1([u8; 20]),
    /// BitTorrent v2 info hash (32-byte SHA256).
    V2([u8; 32]),
    /// Hybrid torrent with both v1 and v2 info hashes (BEP-47).
    Hybrid {
        /// The v1 (SHA1) info hash.
        v1: InfoHashV1,
        /// The v2 (SHA256) info hash.
        v2: InfoHashV2,
    },
}

impl InfoHash {
    /// Creates a v1 info hash from a 20-byte slice.
    ///
    /// # Errors
    ///
    /// Returns [`MetainfoError::InvalidInfoHashLength`] if the slice is not exactly 20 bytes.
    pub fn from_v1_bytes(bytes: &[u8]) -> Result<Self, MetainfoError> {
        if bytes.len() != 20 {
            return Err(MetainfoError::InvalidInfoHashLength);
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(bytes);
        Ok(InfoHash::V1(arr))
    }

    /// Creates a v2 info hash from a 32-byte slice.
    ///
    /// # Errors
    ///
    /// Returns [`MetainfoError::InvalidInfoHashLength`] if the slice is not exactly 32 bytes.
    pub fn from_v2_bytes(bytes: &[u8]) -> Result<Self, MetainfoError> {
        if bytes.len() != 32 {
            return Err(MetainfoError::InvalidInfoHashLength);
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(bytes);
        Ok(InfoHash::V2(arr))
    }

    /// Creates a hybrid info hash with both v1 and v2 hashes.
    ///
    /// This is used for BEP-47 hybrid torrents that support both
    /// v1 and v2 protocols.
    pub fn hybrid(v1: InfoHashV1, v2: InfoHashV2) -> Self {
        InfoHash::Hybrid { v1, v2 }
    }

    /// Parses an info hash from a hexadecimal string.
    ///
    /// The version is determined by the string length:
    /// - 40 characters → v1 (20 bytes)
    /// - 64 characters → v2 (32 bytes)
    ///
    /// # Errors
    ///
    /// Returns [`MetainfoError::InvalidInfoHashLength`] if the string length
    /// is invalid or contains non-hex characters.
    ///
    /// # Examples
    ///
    /// ```
    /// use rbit::metainfo::InfoHash;
    ///
    /// let hash = InfoHash::from_hex("c12fe1c06bba254a9dc9f519b335aa7c1367a88a").unwrap();
    /// assert!(hash.is_v1());
    /// ```
    pub fn from_hex(s: &str) -> Result<Self, MetainfoError> {
        let bytes = hex_decode(s).ok_or(MetainfoError::InvalidInfoHashLength)?;
        match bytes.len() {
            20 => Self::from_v1_bytes(&bytes),
            32 => Self::from_v2_bytes(&bytes),
            _ => Err(MetainfoError::InvalidInfoHashLength),
        }
    }

    /// Returns the raw bytes of the info hash.
    ///
    /// For v1, returns 20 bytes. For v2, returns 32 bytes.
    /// For hybrid, returns the v1 hash bytes (20 bytes).
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            InfoHash::V1(arr) => arr,
            InfoHash::V2(arr) => arr,
            InfoHash::Hybrid { v1, .. } => v1.as_bytes(),
        }
    }

    /// Returns `true` if this is a v1 (SHA1) info hash.
    pub fn is_v1(&self) -> bool {
        matches!(self, InfoHash::V1(_))
    }

    /// Returns `true` if this is a v2 (SHA256) info hash.
    pub fn is_v2(&self) -> bool {
        matches!(self, InfoHash::V2(_))
    }

    /// Returns `true` if this is a hybrid info hash (BEP-47).
    pub fn is_hybrid(&self) -> bool {
        matches!(self, InfoHash::Hybrid { .. })
    }

    /// Returns the v1 hash if available.
    ///
    /// Returns `Some` for V1 and Hybrid variants, `None` for V2.
    pub fn v1_hash(&self) -> Option<InfoHashV1> {
        match self {
            InfoHash::V1(arr) => Some(InfoHashV1(*arr)),
            InfoHash::Hybrid { v1, .. } => Some(*v1),
            InfoHash::V2(_) => None,
        }
    }

    /// Returns the v2 hash if available.
    ///
    /// Returns `Some` for V2 and Hybrid variants, `None` for V1.
    pub fn v2_hash(&self) -> Option<InfoHashV2> {
        match self {
            InfoHash::V2(arr) => Some(InfoHashV2(*arr)),
            InfoHash::Hybrid { v2, .. } => Some(*v2),
            InfoHash::V1(_) => None,
        }
    }

    /// Converts the info hash to a lowercase hexadecimal string.
    ///
    /// For hybrid hashes, returns the v1 hash hex string.
    ///
    /// # Examples
    ///
    /// ```
    /// use rbit::metainfo::InfoHash;
    ///
    /// let hash = InfoHash::from_v1_bytes(&[0xab; 20]).unwrap();
    /// assert_eq!(hash.to_hex(), "abababababababababababababababababababab");
    /// ```
    pub fn to_hex(&self) -> String {
        hex_encode(self.as_bytes())
    }

    /// URL-encodes the info hash for use in tracker announce requests.
    ///
    /// For hybrid hashes, encodes the v1 hash.
    pub fn url_encode(&self) -> String {
        match self {
            InfoHash::V1(arr) => InfoHashV1(*arr).url_encode(),
            InfoHash::Hybrid { v1, .. } => v1.url_encode(),
            InfoHash::V2(_) => {
                // V2-only torrents can't use traditional trackers
                String::new()
            }
        }
    }
}

impl fmt::Debug for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InfoHash::V1(_) => write!(f, "InfoHash::V1({})", self.to_hex()),
            InfoHash::V2(_) => write!(f, "InfoHash::V2({})", self.to_hex()),
            InfoHash::Hybrid { v1, v2 } => {
                write!(
                    f,
                    "InfoHash::Hybrid {{ v1: {}, v2: {} }}",
                    v1.to_hex(),
                    v2.to_hex()
                )
            }
        }
    }
}

impl fmt::Display for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl From<InfoHashV1> for InfoHash {
    fn from(hash: InfoHashV1) -> Self {
        InfoHash::V1(hash.0)
    }
}

impl From<InfoHashV2> for InfoHash {
    fn from(hash: InfoHashV2) -> Self {
        InfoHash::V2(hash.0)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{:02x}", b);
            s
        })
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
