use std::collections::BTreeMap;
use std::path::PathBuf;

use bytes::Bytes;
use sha1::{Digest, Sha1};
use sha2::Sha256;

use super::error::MetainfoError;
use super::file_tree::FileTree;
use super::info_hash::{InfoHash, InfoHashV1, InfoHashV2};
use crate::bencode::{decode, encode, Value};

/// The version of a torrent file.
///
/// BitTorrent has evolved through multiple versions:
/// - **V1**: Original BitTorrent protocol (BEP-3)
/// - **V2**: BitTorrent v2 with improved piece hashing (BEP-52)
/// - **Hybrid**: Supports both v1 and v2 clients (BEP-47)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TorrentVersion {
    /// BitTorrent v1 (BEP-3) - SHA1 piece hashes.
    V1,
    /// BitTorrent v2 (BEP-52) - SHA256 piece hashes with merkle trees.
    V2,
    /// Hybrid torrent (BEP-47) - Compatible with both v1 and v2 clients.
    Hybrid,
}

impl TorrentVersion {
    /// Returns true if this version supports v1 clients.
    pub fn supports_v1(&self) -> bool {
        matches!(self, TorrentVersion::V1 | TorrentVersion::Hybrid)
    }

    /// Returns true if this version supports v2 clients.
    pub fn supports_v2(&self) -> bool {
        matches!(self, TorrentVersion::V2 | TorrentVersion::Hybrid)
    }
}

/// Piece layers for BitTorrent v2 torrents.
///
/// Maps each file's merkle root to its layer hashes. The layer hashes are
/// the concatenated SHA256 hashes from the appropriate merkle tree level
/// where each hash covers `piece_length` bytes.
#[derive(Debug, Clone)]
pub struct PieceLayers {
    /// Maps file merkle root (32 bytes) -> layer hashes (32 bytes each).
    pub layers: BTreeMap<[u8; 32], Vec<[u8; 32]>>,
}

impl PieceLayers {
    /// Creates a new empty piece layers structure.
    pub fn new() -> Self {
        Self {
            layers: BTreeMap::new(),
        }
    }

    /// Returns the layer hashes for a file with the given merkle root.
    pub fn get(&self, pieces_root: &[u8; 32]) -> Option<&Vec<[u8; 32]>> {
        self.layers.get(pieces_root)
    }

    /// Returns the total number of files with piece layers.
    pub fn file_count(&self) -> usize {
        self.layers.len()
    }
}

impl Default for PieceLayers {
    fn default() -> Self {
        Self::new()
    }
}

/// Piece hashes for torrent verification.
///
/// Supports both v1 (SHA1) and v2 (SHA256 merkle) hash formats.
#[derive(Debug, Clone)]
pub enum PieceHashes {
    /// BitTorrent v1: Simple concatenated SHA1 hashes (20 bytes each).
    V1(Vec<[u8; 20]>),
    /// BitTorrent v2: Per-file merkle tree layer hashes (32 bytes each).
    V2(PieceLayers),
    /// Hybrid: Both v1 and v2 hashes for compatibility.
    Hybrid {
        /// V1 SHA1 piece hashes.
        v1: Vec<[u8; 20]>,
        /// V2 merkle piece layers.
        v2: PieceLayers,
    },
}

impl PieceHashes {
    /// Returns the number of v1 pieces, if available.
    pub fn v1_piece_count(&self) -> Option<usize> {
        match self {
            PieceHashes::V1(pieces) => Some(pieces.len()),
            PieceHashes::Hybrid { v1, .. } => Some(v1.len()),
            PieceHashes::V2(_) => None,
        }
    }

    /// Returns the v1 piece hashes, if available.
    pub fn v1_pieces(&self) -> Option<&Vec<[u8; 20]>> {
        match self {
            PieceHashes::V1(pieces) => Some(pieces),
            PieceHashes::Hybrid { v1, .. } => Some(v1),
            PieceHashes::V2(_) => None,
        }
    }

    /// Returns the v2 piece layers, if available.
    pub fn v2_layers(&self) -> Option<&PieceLayers> {
        match self {
            PieceHashes::V2(layers) => Some(layers),
            PieceHashes::Hybrid { v2, .. } => Some(v2),
            PieceHashes::V1(_) => None,
        }
    }

    /// Returns true if this contains v1 hashes.
    pub fn has_v1(&self) -> bool {
        matches!(self, PieceHashes::V1(_) | PieceHashes::Hybrid { .. })
    }

    /// Returns true if this contains v2 hashes.
    pub fn has_v2(&self) -> bool {
        matches!(self, PieceHashes::V2(_) | PieceHashes::Hybrid { .. })
    }
}

/// A parsed torrent file.
///
/// Contains all metadata from a `.torrent` file, including file information,
/// piece hashes, and tracker URLs.
///
/// # Examples
///
/// ```no_run
/// use rbit::metainfo::Metainfo;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let data = std::fs::read("example.torrent")?;
/// let metainfo = Metainfo::from_bytes(&data)?;
///
/// println!("Torrent: {}", metainfo.info.name);
/// println!("Size: {} bytes", metainfo.info.total_length);
/// println!("Info hash: {}", metainfo.info_hash);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Metainfo {
    /// The info dictionary containing file and piece information.
    pub info: Info,
    /// The unique identifier for this torrent (hash of the info dictionary).
    pub info_hash: InfoHash,
    /// Primary tracker URL.
    pub announce: Option<String>,
    /// Multi-tier tracker list ([BEP-12](http://bittorrent.org/beps/bep_0012.html)).
    pub announce_list: Vec<Vec<String>>,
    /// Unix timestamp when the torrent was created.
    pub creation_date: Option<i64>,
    /// Optional comment about the torrent.
    pub comment: Option<String>,
    /// Name/version of the program that created the torrent.
    pub created_by: Option<String>,
    /// The torrent version (V1, V2, or Hybrid).
    pub version: TorrentVersion,
    /// BEP-19: Web seed URLs for HTTP/FTP seeding.
    pub url_list: Vec<String>,
    raw_info: Bytes,
}

/// The info dictionary from a torrent file.
///
/// Contains the core metadata that identifies the torrent content.
/// The hash of this dictionary (in bencode format) is the info hash.
#[derive(Debug, Clone)]
pub struct Info {
    /// Suggested name for the file or directory.
    pub name: String,
    /// Number of bytes per piece.
    pub piece_length: u64,
    /// Piece hashes for verification (v1 SHA1, v2 merkle layers, or both).
    pub pieces: PieceHashes,
    /// List of files in the torrent.
    pub files: Vec<File>,
    /// Total size of all files combined.
    pub total_length: u64,
    /// If true, clients should only use trackers in the metainfo (no DHT/PEX).
    pub private: bool,
    /// The meta version field from v2 torrents (2 for BEP-52).
    pub meta_version: Option<u8>,
}

impl Info {
    /// Returns the number of pieces for v1-style piece indexing.
    ///
    /// For v1 and hybrid torrents, this returns the piece count.
    /// For pure v2 torrents, this calculates based on files and piece length.
    pub fn piece_count(&self) -> usize {
        match &self.pieces {
            PieceHashes::V1(pieces) => pieces.len(),
            PieceHashes::Hybrid { v1, .. } => v1.len(),
            PieceHashes::V2(_) => {
                // For v2, calculate total pieces across all files
                // Each file is piece-aligned
                self.files
                    .iter()
                    .map(|f| {
                        if f.length == 0 {
                            0
                        } else {
                            f.length.div_ceil(self.piece_length) as usize
                        }
                    })
                    .sum()
            }
        }
    }

    /// Returns true if this is a v1-only torrent.
    pub fn is_v1(&self) -> bool {
        matches!(self.pieces, PieceHashes::V1(_))
    }

    /// Returns true if this is a v2-only torrent.
    pub fn is_v2(&self) -> bool {
        matches!(self.pieces, PieceHashes::V2(_))
    }

    /// Returns true if this is a hybrid torrent (both v1 and v2).
    pub fn is_hybrid(&self) -> bool {
        matches!(self.pieces, PieceHashes::Hybrid { .. })
    }

    /// Returns true if this torrent supports v1 protocol.
    pub fn supports_v1(&self) -> bool {
        self.pieces.has_v1()
    }

    /// Returns true if this torrent supports v2 protocol.
    pub fn supports_v2(&self) -> bool {
        self.pieces.has_v2()
    }

    /// Gets the v1 piece hash for a given piece index.
    ///
    /// Returns None for pure v2 torrents or invalid indices.
    pub fn get_v1_piece_hash(&self, piece_index: usize) -> Option<[u8; 20]> {
        match &self.pieces {
            PieceHashes::V1(pieces) => pieces.get(piece_index).copied(),
            PieceHashes::Hybrid { v1, .. } => v1.get(piece_index).copied(),
            PieceHashes::V2(_) => None,
        }
    }

    /// Gets the v2 piece hash for a given file and piece index within that file.
    ///
    /// For v2 torrents, pieces are per-file. This returns the layer hash
    /// for the given piece within the specified file.
    pub fn get_v2_piece_hash(&self, file_index: usize, piece_index: usize) -> Option<[u8; 32]> {
        let file = self.files.get(file_index)?;
        let pieces_root = file.pieces_root?;

        let layers = match &self.pieces {
            PieceHashes::V2(layers) => layers,
            PieceHashes::Hybrid { v2, .. } => v2,
            PieceHashes::V1(_) => return None,
        };

        let layer_hashes = layers.get(&pieces_root)?;
        layer_hashes.get(piece_index).copied()
    }

    /// Gets the merkle root (pieces_root) for a file.
    ///
    /// Returns None for v1 torrents or if the file has no pieces_root.
    pub fn get_file_pieces_root(&self, file_index: usize) -> Option<[u8; 32]> {
        self.files.get(file_index).and_then(|f| f.pieces_root)
    }

    /// Returns the number of pieces for a given file (v2 calculation).
    ///
    /// For v2 torrents, each file has its own piece space.
    pub fn file_piece_count(&self, file_index: usize) -> usize {
        self.files
            .get(file_index)
            .map(|f| {
                if f.length == 0 {
                    0
                } else {
                    f.length.div_ceil(self.piece_length) as usize
                }
            })
            .unwrap_or(0)
    }

    /// Returns all non-padding files.
    pub fn content_files(&self) -> impl Iterator<Item = &File> {
        self.files.iter().filter(|f| !f.is_padding())
    }

    /// Returns all padding files.
    pub fn padding_files(&self) -> impl Iterator<Item = &File> {
        self.files.iter().filter(|f| f.is_padding())
    }

    /// Gets the v2 piece hash for a global piece index.
    ///
    /// For v2 torrents, this maps the global piece index to the correct
    /// file and file-local piece index, then returns the layer hash.
    ///
    /// For files that fit within a single piece (length <= piece_length),
    /// the pieces_root IS the layer hash (no piece layers entry).
    pub fn get_v2_piece_hash_global(&self, global_piece_index: usize) -> Option<[u8; 32]> {
        let layers = match &self.pieces {
            PieceHashes::V2(layers) => layers,
            PieceHashes::Hybrid { v2, .. } => v2,
            PieceHashes::V1(_) => return None,
        };

        // Find which file this piece belongs to
        let mut piece_offset = 0usize;
        for file in &self.files {
            if file.length == 0 || file.is_padding() {
                continue;
            }

            let file_pieces = file.length.div_ceil(self.piece_length) as usize;

            if global_piece_index < piece_offset + file_pieces {
                // This piece belongs to this file
                let local_index = global_piece_index - piece_offset;
                let pieces_root = file.pieces_root?;

                // For files with only one piece, pieces_root IS the hash
                // (no entry in piece layers)
                if file_pieces == 1 {
                    return Some(pieces_root);
                }

                // Otherwise, look up in piece layers
                let layer_hashes = layers.get(&pieces_root)?;
                return layer_hashes.get(local_index).copied();
            }

            piece_offset += file_pieces;
        }

        None
    }

    /// Builds a list of all v2 piece hashes in global piece order.
    ///
    /// This flattens the per-file piece layers into a single list
    /// that can be used for storage verification.
    pub fn all_v2_piece_hashes(&self) -> Vec<[u8; 32]> {
        let layers = match &self.pieces {
            PieceHashes::V2(layers) => layers,
            PieceHashes::Hybrid { v2, .. } => v2,
            PieceHashes::V1(_) => return Vec::new(),
        };

        let mut hashes = Vec::with_capacity(self.piece_count());

        for file in &self.files {
            if file.length == 0 || file.is_padding() {
                continue;
            }

            let file_pieces = file.length.div_ceil(self.piece_length) as usize;

            if let Some(pieces_root) = file.pieces_root {
                if file_pieces == 1 {
                    // Single-piece file: pieces_root is the hash
                    hashes.push(pieces_root);
                } else if let Some(layer_hashes) = layers.get(&pieces_root) {
                    // Multi-piece file: use layer hashes
                    hashes.extend(layer_hashes.iter().copied());
                }
            }
        }

        hashes
    }
}

/// A file within a torrent.
///
/// For single-file torrents, there is one file with the torrent name.
/// For multi-file torrents, paths are relative to the torrent's root directory.
#[derive(Debug, Clone)]
pub struct File {
    /// Path to the file (relative to torrent root).
    pub path: PathBuf,
    /// Size of the file in bytes.
    pub length: u64,
    /// Byte offset within the torrent's piece data.
    ///
    /// For v1 torrents, this is the contiguous byte offset.
    /// For v2 torrents, each file is piece-aligned, so this represents
    /// the byte offset accounting for padding between files.
    pub offset: u64,
    /// The merkle root hash for this file (v2 torrents only).
    ///
    /// This is a 32-byte SHA256 hash that is the root of the merkle tree
    /// built from 16 KiB block hashes. Empty files have no pieces_root.
    pub pieces_root: Option<[u8; 32]>,
    /// File attributes (v2 torrents only).
    ///
    /// Common attributes:
    /// - "p": Padding file (used for v1 compatibility in hybrid torrents)
    /// - "x": Executable file
    /// - "h": Hidden file
    pub attr: Option<String>,
}

impl File {
    /// Returns true if this is a padding file.
    ///
    /// Padding files are used in hybrid torrents to align v2 files
    /// to piece boundaries for v1 compatibility.
    pub fn is_padding(&self) -> bool {
        self.attr.as_ref().is_some_and(|a| a.contains('p'))
    }

    /// Returns true if this is marked as executable.
    pub fn is_executable(&self) -> bool {
        self.attr.as_ref().is_some_and(|a| a.contains('x'))
    }

    /// Returns true if this is marked as hidden.
    pub fn is_hidden(&self) -> bool {
        self.attr.as_ref().is_some_and(|a| a.contains('h'))
    }
}

impl Metainfo {
    /// Parses a torrent file from raw bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The data is not valid bencode
    /// - Required fields are missing (info, name, pieces, etc.)
    /// - The pieces field length is not a multiple of 20
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rbit::metainfo::Metainfo;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let data = std::fs::read("example.torrent")?;
    /// let metainfo = Metainfo::from_bytes(&data)?;
    /// println!("Name: {}", metainfo.info.name);
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_bytes(data: &[u8]) -> Result<Self, MetainfoError> {
        let value = decode(data)?;
        let dict = value.as_dict().ok_or(MetainfoError::InvalidField("root"))?;

        let info_value = dict
            .get(b"info".as_slice())
            .ok_or(MetainfoError::MissingField("info"))?;

        let info_dict = info_value
            .as_dict()
            .ok_or(MetainfoError::InvalidField("info"))?;

        let raw_info = Bytes::from(encode(info_value)?);

        // Detect version based on presence of v1/v2 specific fields
        let has_pieces = info_dict.get(b"pieces".as_slice()).is_some();
        let has_file_tree = info_dict.get(b"file tree".as_slice()).is_some();
        let meta_version = info_dict
            .get(b"meta version".as_slice())
            .and_then(|v| v.as_integer());

        let version = match (has_pieces, has_file_tree, meta_version) {
            (true, true, _) => TorrentVersion::Hybrid,
            (false, true, Some(2)) => TorrentVersion::V2,
            (true, false, _) => TorrentVersion::V1,
            _ => TorrentVersion::V1, // Default to V1 for backwards compatibility
        };

        let info_hash = match version {
            TorrentVersion::V1 => compute_info_hash(&raw_info),
            TorrentVersion::V2 => compute_v2_info_hash(&raw_info),
            TorrentVersion::Hybrid => {
                let v1 = InfoHashV1::from_info_bytes(&raw_info);
                let v2 = InfoHashV2::from_info_bytes(&raw_info);
                InfoHash::hybrid(v1, v2)
            }
        };

        // Parse info based on version
        let info = match version {
            TorrentVersion::V1 => parse_info_v1(info_value)?,
            TorrentVersion::V2 => {
                let piece_layers = parse_piece_layers(dict)?;
                parse_info_v2(info_value, piece_layers)?
            }
            TorrentVersion::Hybrid => {
                let piece_layers = parse_piece_layers(dict)?;
                parse_info_hybrid(info_value, piece_layers)?
            }
        };

        let announce = dict
            .get(b"announce".as_slice())
            .and_then(|v| v.as_str())
            .map(String::from);

        let announce_list = dict
            .get(b"announce-list".as_slice())
            .and_then(|v| v.as_list())
            .map(|list| {
                list.iter()
                    .filter_map(|tier| {
                        tier.as_list().map(|urls| {
                            urls.iter()
                                .filter_map(|u| u.as_str().map(String::from))
                                .collect()
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let creation_date = dict
            .get(b"creation date".as_slice())
            .and_then(|v| v.as_integer());

        let comment = dict
            .get(b"comment".as_slice())
            .and_then(|v| v.as_str())
            .map(String::from);

        let created_by = dict
            .get(b"created by".as_slice())
            .and_then(|v| v.as_str())
            .map(String::from);

        // BEP-19: Parse url-list for web seeding
        let url_list = match dict.get(b"url-list".as_slice()) {
            Some(Value::Bytes(url)) => {
                // Single URL as string
                String::from_utf8_lossy(url)
                    .to_string()
                    .split_whitespace()
                    .map(String::from)
                    .collect()
            }
            Some(Value::List(urls)) => {
                // List of URLs
                urls.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            }
            _ => Vec::new(),
        };

        Ok(Self {
            info,
            info_hash,
            announce,
            announce_list,
            creation_date,
            comment,
            created_by,
            version,
            url_list,
            raw_info,
        })
    }

    /// Returns the raw bencoded info dictionary.
    ///
    /// This is useful for computing the info hash or for extension protocols
    /// that need to share the raw info dictionary.
    pub fn raw_info(&self) -> &Bytes {
        &self.raw_info
    }

    /// Returns all tracker URLs from both `announce` and `announce-list`.
    ///
    /// The primary tracker (from `announce`) comes first, followed by
    /// trackers from `announce-list`. Duplicates are removed.
    pub fn trackers(&self) -> Vec<String> {
        let mut trackers = Vec::new();

        if let Some(ref announce) = self.announce {
            trackers.push(announce.clone());
        }

        for tier in &self.announce_list {
            for tracker in tier {
                if !trackers.contains(tracker) {
                    trackers.push(tracker.clone());
                }
            }
        }

        trackers
    }

    /// Returns `true` if this is a BitTorrent v2 torrent.
    pub fn is_v2(&self) -> bool {
        matches!(self.version, TorrentVersion::V2)
    }

    /// Returns `true` if this is a hybrid torrent (BEP-47).
    pub fn is_hybrid(&self) -> bool {
        matches!(self.version, TorrentVersion::Hybrid)
    }
}

/// Parses a v1 torrent info dictionary.
fn parse_info_v1(value: &Value) -> Result<Info, MetainfoError> {
    let dict = value.as_dict().ok_or(MetainfoError::InvalidField("info"))?;

    let name = dict
        .get(b"name".as_slice())
        .and_then(|v| v.as_str())
        .ok_or(MetainfoError::MissingField("name"))?
        .to_string();

    let piece_length = dict
        .get(b"piece length".as_slice())
        .and_then(|v| v.as_integer())
        .ok_or(MetainfoError::MissingField("piece length"))? as u64;

    let pieces_bytes = dict
        .get(b"pieces".as_slice())
        .and_then(|v| v.as_bytes())
        .ok_or(MetainfoError::MissingField("pieces"))?;

    if pieces_bytes.len() % 20 != 0 {
        return Err(MetainfoError::InvalidField("pieces"));
    }

    let pieces: Vec<[u8; 20]> = pieces_bytes
        .chunks_exact(20)
        .map(|chunk| {
            let mut arr = [0u8; 20];
            arr.copy_from_slice(chunk);
            arr
        })
        .collect();

    let private = dict
        .get(b"private".as_slice())
        .and_then(|v| v.as_integer())
        .map(|v| v == 1)
        .unwrap_or(false);

    let (files, total_length) = if let Some(length) =
        dict.get(b"length".as_slice()).and_then(|v| v.as_integer())
    {
        let length = length as u64;
        let file = File {
            path: PathBuf::from(&name),
            length,
            offset: 0,
            pieces_root: None,
            attr: None,
        };
        (vec![file], length)
    } else if let Some(files_list) = dict.get(b"files".as_slice()).and_then(|v| v.as_list()) {
        let mut files = Vec::new();
        let mut offset = 0u64;

        for file_value in files_list {
            let file_dict = file_value
                .as_dict()
                .ok_or(MetainfoError::InvalidField("files"))?;

            let length = file_dict
                .get(b"length".as_slice())
                .and_then(|v| v.as_integer())
                .ok_or(MetainfoError::MissingField("file length"))? as u64;

            let path_list = file_dict
                .get(b"path".as_slice())
                .and_then(|v| v.as_list())
                .ok_or(MetainfoError::MissingField("file path"))?;

            let path: PathBuf = std::iter::once(name.clone())
                .chain(
                    path_list
                        .iter()
                        .filter_map(|p| p.as_str().map(String::from)),
                )
                .collect();

            // v1 torrents may also have attr field (BEP-47)
            let attr = file_dict
                .get(b"attr".as_slice())
                .and_then(|v| v.as_str())
                .map(String::from);

            files.push(File {
                path,
                length,
                offset,
                pieces_root: None,
                attr,
            });

            offset += length;
        }

        let total = offset;
        (files, total)
    } else {
        return Err(MetainfoError::MissingField("length or files"));
    };

    Ok(Info {
        name,
        piece_length,
        pieces: PieceHashes::V1(pieces),
        files,
        total_length,
        private,
        meta_version: None,
    })
}

/// Parses the "piece layers" dictionary from the top-level metainfo.
///
/// The piece layers dictionary maps each file's merkle root (32 bytes) to
/// the concatenated layer hashes (32 bytes each) from the appropriate tree level.
fn parse_piece_layers(dict: &BTreeMap<Bytes, Value>) -> Result<PieceLayers, MetainfoError> {
    let layers_value = match dict.get(b"piece layers".as_slice()) {
        Some(v) => v,
        None => return Ok(PieceLayers::new()), // Empty piece layers for single empty file
    };

    let layers_dict = layers_value
        .as_dict()
        .ok_or(MetainfoError::InvalidField("piece layers"))?;

    let mut layers = BTreeMap::new();

    for (key, value) in layers_dict {
        // Key is the 32-byte merkle root
        if key.len() != 32 {
            return Err(MetainfoError::InvalidField("piece layers key"));
        }
        let mut root = [0u8; 32];
        root.copy_from_slice(key);

        // Value is concatenated 32-byte hashes
        let hashes_bytes = value
            .as_bytes()
            .ok_or(MetainfoError::InvalidField("piece layers value"))?;

        if hashes_bytes.len() % 32 != 0 {
            return Err(MetainfoError::InvalidField("piece layers hash length"));
        }

        let hashes: Vec<[u8; 32]> = hashes_bytes
            .chunks_exact(32)
            .map(|chunk| {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(chunk);
                arr
            })
            .collect();

        layers.insert(root, hashes);
    }

    Ok(PieceLayers { layers })
}

/// Validates piece length according to BEP-52 requirements.
///
/// - Must be a power of 2
/// - Must be at least 16 KiB (16384 bytes)
fn validate_piece_length(piece_length: u64) -> Result<(), MetainfoError> {
    const MIN_PIECE_LENGTH: u64 = 16384; // 16 KiB

    if piece_length < MIN_PIECE_LENGTH {
        return Err(MetainfoError::InvalidField("piece length too small"));
    }

    // Check if power of 2: n & (n - 1) == 0 for powers of 2
    if piece_length & (piece_length - 1) != 0 {
        return Err(MetainfoError::InvalidField("piece length not power of 2"));
    }

    Ok(())
}

/// Validates that a path component is safe (no directory traversal).
fn validate_path_component(component: &str) -> Result<(), MetainfoError> {
    if component == "." || component == ".." {
        return Err(MetainfoError::InvalidField("path traversal detected"));
    }
    if component.is_empty() {
        return Err(MetainfoError::InvalidField("empty path component"));
    }
    Ok(())
}

/// Parses a v2 torrent info dictionary.
fn parse_info_v2(value: &Value, piece_layers: PieceLayers) -> Result<Info, MetainfoError> {
    let dict = value.as_dict().ok_or(MetainfoError::InvalidField("info"))?;

    let name = dict
        .get(b"name".as_slice())
        .and_then(|v| v.as_str())
        .ok_or(MetainfoError::MissingField("name"))?
        .to_string();

    // Validate the name component
    validate_path_component(&name)?;

    let piece_length = dict
        .get(b"piece length".as_slice())
        .and_then(|v| v.as_integer())
        .ok_or(MetainfoError::MissingField("piece length"))? as u64;

    // Validate piece length per BEP-52
    validate_piece_length(piece_length)?;

    let meta_version = dict
        .get(b"meta version".as_slice())
        .and_then(|v| v.as_integer())
        .map(|v| v as u8);

    if meta_version != Some(2) {
        return Err(MetainfoError::InvalidField("meta version must be 2"));
    }

    let private = dict
        .get(b"private".as_slice())
        .and_then(|v| v.as_integer())
        .map(|v| v == 1)
        .unwrap_or(false);

    // Parse file tree
    let file_tree_value = dict
        .get(b"file tree".as_slice())
        .ok_or(MetainfoError::MissingField("file tree"))?;

    let file_tree = FileTree::from_bencode(file_tree_value)?;

    // Flatten file tree and compute offsets
    // In v2, each file is aligned to piece boundary
    let flattened = file_tree.flatten();

    let mut files = Vec::new();
    let mut offset = 0u64;
    let mut total_length = 0u64;

    for flat_file in flattened {
        // Validate path components
        for component in flat_file.path.components() {
            if let std::path::Component::Normal(s) = component {
                if let Some(s) = s.to_str() {
                    validate_path_component(s)?;
                }
            }
        }

        // Prepend the torrent name to the path
        let path = PathBuf::from(&name).join(&flat_file.path);

        // Validate pieces_root requirements per BEP-52:
        // - Empty files: no pieces_root
        // - Files with length > 0: must have pieces_root
        // - Files with length > piece_length: pieces_root must exist in piece layers
        // - Files with length <= piece_length: pieces_root IS the layer hash (no entry needed)
        if flat_file.length > 0 {
            if let Some(root) = &flat_file.pieces_root {
                // Only files larger than piece_length need an entry in piece layers
                // For smaller files, the pieces_root directly is the merkle root
                if flat_file.length > piece_length && !piece_layers.layers.contains_key(root) {
                    return Err(MetainfoError::InvalidField(
                        "pieces root not in piece layers for file larger than piece length",
                    ));
                }
                // Validate layer hash count for files in piece layers
                if let Some(layer_hashes) = piece_layers.layers.get(root) {
                    let expected_pieces = flat_file.length.div_ceil(piece_length) as usize;
                    if layer_hashes.len() != expected_pieces {
                        return Err(MetainfoError::InvalidField(
                            "piece layers hash count mismatch",
                        ));
                    }
                }
            } else {
                return Err(MetainfoError::MissingField(
                    "pieces root for non-empty file",
                ));
            }
        }

        files.push(File {
            path,
            length: flat_file.length,
            offset,
            pieces_root: flat_file.pieces_root,
            attr: flat_file.attr,
        });

        total_length += flat_file.length;

        // In v2, files are piece-aligned
        // Next file starts at the next piece boundary
        if flat_file.length > 0 {
            let pieces_for_file = flat_file.length.div_ceil(piece_length);
            offset += pieces_for_file * piece_length;
        }
    }

    Ok(Info {
        name,
        piece_length,
        pieces: PieceHashes::V2(piece_layers),
        files,
        total_length,
        private,
        meta_version,
    })
}

/// Parses a hybrid torrent info dictionary (both v1 and v2).
fn parse_info_hybrid(value: &Value, piece_layers: PieceLayers) -> Result<Info, MetainfoError> {
    let dict = value.as_dict().ok_or(MetainfoError::InvalidField("info"))?;

    let name = dict
        .get(b"name".as_slice())
        .and_then(|v| v.as_str())
        .ok_or(MetainfoError::MissingField("name"))?
        .to_string();

    // Validate the name component
    validate_path_component(&name)?;

    let piece_length = dict
        .get(b"piece length".as_slice())
        .and_then(|v| v.as_integer())
        .ok_or(MetainfoError::MissingField("piece length"))? as u64;

    // Validate piece length per BEP-52 (applies to hybrid too)
    validate_piece_length(piece_length)?;

    // Parse v1 pieces
    let pieces_bytes = dict
        .get(b"pieces".as_slice())
        .and_then(|v| v.as_bytes())
        .ok_or(MetainfoError::MissingField("pieces"))?;

    if pieces_bytes.len() % 20 != 0 {
        return Err(MetainfoError::InvalidField("pieces"));
    }

    let v1_pieces: Vec<[u8; 20]> = pieces_bytes
        .chunks_exact(20)
        .map(|chunk| {
            let mut arr = [0u8; 20];
            arr.copy_from_slice(chunk);
            arr
        })
        .collect();

    let private = dict
        .get(b"private".as_slice())
        .and_then(|v| v.as_integer())
        .map(|v| v == 1)
        .unwrap_or(false);

    // Parse file tree for v2 structure
    let file_tree_value = dict
        .get(b"file tree".as_slice())
        .ok_or(MetainfoError::MissingField("file tree"))?;

    let file_tree = FileTree::from_bencode(file_tree_value)?;
    let flattened = file_tree.flatten();

    // For hybrid torrents, we use v1-style contiguous offsets
    // but include pieces_root from v2 file tree
    let mut files = Vec::new();
    let mut offset = 0u64;
    let mut total_length = 0u64;

    for flat_file in flattened {
        // Validate path components
        for component in flat_file.path.components() {
            if let std::path::Component::Normal(s) = component {
                if let Some(s) = s.to_str() {
                    validate_path_component(s)?;
                }
            }
        }

        let path = PathBuf::from(&name).join(&flat_file.path);

        files.push(File {
            path,
            length: flat_file.length,
            offset,
            pieces_root: flat_file.pieces_root,
            attr: flat_file.attr,
        });

        total_length += flat_file.length;
        offset += flat_file.length; // v1-style contiguous offset
    }

    // Validate hybrid torrent consistency per BEP-52:
    // The v1 and v2 parts must describe identical content

    // 1. Validate v1 piece count matches total length
    let expected_v1_pieces = if total_length == 0 {
        0
    } else {
        total_length.div_ceil(piece_length) as usize
    };
    if v1_pieces.len() != expected_v1_pieces {
        return Err(MetainfoError::InvalidField(
            "hybrid: v1 piece count doesn't match total length",
        ));
    }

    // 2. Validate that v2 piece layers cover the same content
    // Count total pieces from v2 files (excluding padding)
    let _v2_piece_count: u64 = files
        .iter()
        .filter(|f| !f.is_padding() && f.length > 0)
        .map(|f| f.length.div_ceil(piece_length))
        .sum();

    // For hybrid torrents, padding files align v1 and v2 piece boundaries.
    // The v2 piece count may differ due to per-file alignment, but the
    // actual content bytes covered must be identical.

    Ok(Info {
        name,
        piece_length,
        pieces: PieceHashes::Hybrid {
            v1: v1_pieces,
            v2: piece_layers,
        },
        files,
        total_length,
        private,
        meta_version: Some(2),
    })
}

fn compute_info_hash(raw_info: &[u8]) -> InfoHash {
    let mut hasher = Sha1::new();
    hasher.update(raw_info);
    let hash: [u8; 20] = hasher.finalize().into();
    InfoHash::V1(hash)
}

fn compute_v2_info_hash(raw_info: &[u8]) -> InfoHash {
    let mut hasher = Sha256::new();
    hasher.update(raw_info);
    let hash: [u8; 32] = hasher.finalize().into();
    InfoHash::V2(hash)
}
