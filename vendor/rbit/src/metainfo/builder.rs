//! Torrent file builder for creating v1, v2, and hybrid torrents.
//!
//! This module provides a builder pattern API for creating torrent files
//! from files on disk or raw byte data.
//!
//! # Overview
//!
//! The [`TorrentBuilder`] supports three torrent versions:
//! - **V1**: Original BitTorrent protocol with SHA1 piece hashes
//! - **V2**: BitTorrent v2 with merkle trees and SHA256 hashes (BEP-52)
//! - **Hybrid**: Compatible with both v1 and v2 clients (BEP-47)
//!
//! # Examples
//!
//! ## Creating a v1 torrent from files on disk
//!
//! ```no_run
//! use rbit::metainfo::{TorrentBuilder, TorrentVersion};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let torrent_bytes = TorrentBuilder::new("My Torrent")
//!     .version(TorrentVersion::V1)
//!     .add_file_from_path("path/to/file.txt")?
//!     .piece_length(262144)  // 256 KiB pieces
//!     .add_tracker("http://tracker.example.com/announce")
//!     .build()?;
//!
//! std::fs::write("my_torrent.torrent", torrent_bytes)?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Creating a v2 torrent from raw data
//!
//! ```
//! use rbit::metainfo::{TorrentBuilder, TorrentVersion};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let file_data = b"Hello, BitTorrent v2!";
//!
//! let torrent_bytes = TorrentBuilder::new("hello")
//!     .version(TorrentVersion::V2)
//!     .add_file("hello.txt", file_data.to_vec())
//!     .piece_length(16384)  // 16 KiB (minimum for v2)
//!     .build()?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Creating a hybrid torrent
//!
//! ```no_run
//! use rbit::metainfo::{TorrentBuilder, TorrentVersion};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let torrent_bytes = TorrentBuilder::new("My Hybrid Torrent")
//!     .version(TorrentVersion::Hybrid)
//!     .add_file_from_path("file1.bin")?
//!     .add_file_from_path("file2.bin")?
//!     .piece_length(262144)
//!     .private(true)
//!     .comment("Created with rbit")
//!     .build()?;
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use sha1::{Digest, Sha1};

use super::error::MetainfoError;
use super::merkle::{hash_block, MerkleTree, MERKLE_BLOCK_SIZE};
use super::torrent::TorrentVersion;
use crate::bencode::{encode, Value};

/// Type alias for piece layers: maps merkle root -> layer hashes.
type PieceLayersMap = BTreeMap<[u8; 32], Vec<[u8; 32]>>;

/// Minimum piece length for v2 torrents (16 KiB per BEP-52).
pub const MIN_V2_PIECE_LENGTH: u64 = 16384;

/// Default piece length (256 KiB).
pub const DEFAULT_PIECE_LENGTH: u64 = 262144;

/// A file to be included in the torrent.
#[derive(Debug, Clone)]
struct BuilderFile {
    /// Path components for the file (relative to torrent root).
    path: Vec<String>,
    /// The file's data.
    data: Vec<u8>,
}

/// Builder for creating torrent files.
///
/// Supports v1, v2, and hybrid torrents with a fluent API.
#[derive(Debug)]
pub struct TorrentBuilder {
    /// Name of the torrent (used as root directory for multi-file torrents).
    name: String,
    /// Torrent version to create.
    version: TorrentVersion,
    /// Files to include.
    files: Vec<BuilderFile>,
    /// Piece length in bytes.
    piece_length: u64,
    /// Primary tracker URL.
    announce: Option<String>,
    /// Additional tracker tiers.
    announce_list: Vec<Vec<String>>,
    /// Whether this is a private torrent.
    private: bool,
    /// Optional comment.
    comment: Option<String>,
    /// Creator string.
    created_by: Option<String>,
    /// Creation timestamp (defaults to now).
    creation_date: Option<i64>,
    /// Web seed URLs (BEP-19).
    url_list: Vec<String>,
}

impl TorrentBuilder {
    /// Creates a new torrent builder with the given name.
    ///
    /// The name is used as:
    /// - The filename for single-file torrents
    /// - The root directory name for multi-file torrents
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: TorrentVersion::V1,
            files: Vec::new(),
            piece_length: DEFAULT_PIECE_LENGTH,
            announce: None,
            announce_list: Vec::new(),
            private: false,
            comment: None,
            created_by: Some(format!("rbit/{}", env!("CARGO_PKG_VERSION"))),
            creation_date: None,
            url_list: Vec::new(),
        }
    }

    /// Sets the torrent version (V1, V2, or Hybrid).
    pub fn version(mut self, version: TorrentVersion) -> Self {
        self.version = version;
        self
    }

    /// Sets the piece length in bytes.
    ///
    /// For v2 torrents, this must be at least 16 KiB and a power of 2.
    /// Common values: 16384, 32768, 65536, 131072, 262144, 524288, 1048576
    pub fn piece_length(mut self, length: u64) -> Self {
        self.piece_length = length;
        self
    }

    /// Adds a file with raw data.
    ///
    /// The path is relative to the torrent root directory.
    pub fn add_file(mut self, path: impl AsRef<Path>, data: Vec<u8>) -> Self {
        let path_components: Vec<String> = path
            .as_ref()
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => s.to_str().map(String::from),
                _ => None,
            })
            .collect();

        self.files.push(BuilderFile {
            path: path_components,
            data,
        });
        self
    }

    /// Adds a file from a reader.
    pub fn add_file_from_reader<R: Read>(
        self,
        path: impl AsRef<Path>,
        mut reader: R,
    ) -> Result<Self, MetainfoError> {
        let mut data = Vec::new();
        reader
            .read_to_end(&mut data)
            .map_err(|e| MetainfoError::InvalidField(Box::leak(e.to_string().into_boxed_str())))?;
        Ok(self.add_file(path, data))
    }

    /// Adds a file from disk.
    ///
    /// The file path on disk is used as the path in the torrent.
    pub fn add_file_from_path(self, path: impl AsRef<Path>) -> Result<Self, MetainfoError> {
        let path = path.as_ref();
        let data = std::fs::read(path)
            .map_err(|e| MetainfoError::InvalidField(Box::leak(e.to_string().into_boxed_str())))?;

        // Use just the filename, not the full path
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or(MetainfoError::InvalidField("invalid filename"))?;

        Ok(self.add_file(filename, data))
    }

    /// Adds a file from disk with a custom path in the torrent.
    pub fn add_file_from_path_as(
        self,
        disk_path: impl AsRef<Path>,
        torrent_path: impl AsRef<Path>,
    ) -> Result<Self, MetainfoError> {
        let data = std::fs::read(disk_path.as_ref())
            .map_err(|e| MetainfoError::InvalidField(Box::leak(e.to_string().into_boxed_str())))?;
        Ok(self.add_file(torrent_path, data))
    }

    /// Adds a directory from disk recursively.
    pub fn add_directory(mut self, dir_path: impl AsRef<Path>) -> Result<Self, MetainfoError> {
        let dir_path = dir_path.as_ref();
        self = self.add_directory_recursive(dir_path, PathBuf::new())?;
        Ok(self)
    }

    fn add_directory_recursive(
        mut self,
        base_path: &Path,
        relative_path: PathBuf,
    ) -> Result<Self, MetainfoError> {
        let current_path = base_path.join(&relative_path);
        let entries = std::fs::read_dir(&current_path)
            .map_err(|e| MetainfoError::InvalidField(Box::leak(e.to_string().into_boxed_str())))?;

        for entry in entries {
            let entry = entry.map_err(|e| {
                MetainfoError::InvalidField(Box::leak(e.to_string().into_boxed_str()))
            })?;
            let path = entry.path();
            let file_name = entry.file_name();
            let new_relative = relative_path.join(&file_name);

            if path.is_dir() {
                self = self.add_directory_recursive(base_path, new_relative)?;
            } else if path.is_file() {
                let data = std::fs::read(&path).map_err(|e| {
                    MetainfoError::InvalidField(Box::leak(e.to_string().into_boxed_str()))
                })?;
                self = self.add_file(&new_relative, data);
            }
        }

        Ok(self)
    }

    /// Sets the primary tracker URL.
    pub fn add_tracker(mut self, url: impl Into<String>) -> Self {
        let url = url.into();
        if self.announce.is_none() {
            self.announce = Some(url);
        } else {
            // Add to announce-list
            self.announce_list.push(vec![url]);
        }
        self
    }

    /// Adds a tracker tier (for multi-tracker torrents).
    pub fn add_tracker_tier(mut self, urls: Vec<String>) -> Self {
        self.announce_list.push(urls);
        self
    }

    /// Sets whether this is a private torrent.
    pub fn private(mut self, private: bool) -> Self {
        self.private = private;
        self
    }

    /// Sets the torrent comment.
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// Sets the creator string.
    pub fn created_by(mut self, created_by: impl Into<String>) -> Self {
        self.created_by = Some(created_by.into());
        self
    }

    /// Sets the creation date (Unix timestamp).
    pub fn creation_date(mut self, timestamp: i64) -> Self {
        self.creation_date = Some(timestamp);
        self
    }

    /// Adds a web seed URL (BEP-19).
    pub fn add_web_seed(mut self, url: impl Into<String>) -> Self {
        self.url_list.push(url.into());
        self
    }

    /// Builds the torrent file and returns the bencoded bytes.
    pub fn build(self) -> Result<Vec<u8>, MetainfoError> {
        self.validate()?;

        match self.version {
            TorrentVersion::V1 => self.build_v1(),
            TorrentVersion::V2 => self.build_v2(),
            TorrentVersion::Hybrid => self.build_hybrid(),
        }
    }

    /// Validates the builder configuration.
    fn validate(&self) -> Result<(), MetainfoError> {
        if self.name.is_empty() {
            return Err(MetainfoError::MissingField("name"));
        }

        if self.files.is_empty() {
            return Err(MetainfoError::MissingField("files"));
        }

        // V2 requirements
        if self.version.supports_v2() {
            if self.piece_length < MIN_V2_PIECE_LENGTH {
                return Err(MetainfoError::InvalidField("piece length too small for v2"));
            }
            if !self.piece_length.is_power_of_two() {
                return Err(MetainfoError::InvalidField(
                    "piece length must be power of 2 for v2",
                ));
            }
        }

        // Validate path components
        for file in &self.files {
            for component in &file.path {
                if component == "." || component == ".." || component.is_empty() {
                    return Err(MetainfoError::InvalidField("invalid path component"));
                }
            }
        }

        Ok(())
    }

    /// Builds a v1 torrent.
    fn build_v1(self) -> Result<Vec<u8>, MetainfoError> {
        let mut root = BTreeMap::new();

        // Build info dictionary
        let info = self.build_info_v1()?;
        root.insert(Bytes::from_static(b"info"), info);

        // Add optional fields
        self.add_common_fields(&mut root);

        encode(&Value::Dict(root)).map_err(|_| MetainfoError::InvalidField("encoding failed"))
    }

    /// Builds a v2 torrent.
    fn build_v2(self) -> Result<Vec<u8>, MetainfoError> {
        let mut root = BTreeMap::new();

        // Build info dictionary and piece layers
        let (info, piece_layers) = self.build_info_v2()?;
        root.insert(Bytes::from_static(b"info"), info);

        // Add piece layers (outside info dict)
        if !piece_layers.is_empty() {
            root.insert(
                Bytes::from_static(b"piece layers"),
                Self::encode_piece_layers(&piece_layers),
            );
        }

        // Add optional fields
        self.add_common_fields(&mut root);

        encode(&Value::Dict(root)).map_err(|_| MetainfoError::InvalidField("encoding failed"))
    }

    /// Builds a hybrid torrent.
    fn build_hybrid(self) -> Result<Vec<u8>, MetainfoError> {
        let mut root = BTreeMap::new();

        // Build info dictionary with both v1 and v2 fields, plus piece layers
        let (info, piece_layers) = self.build_info_hybrid()?;
        root.insert(Bytes::from_static(b"info"), info);

        // Add piece layers (outside info dict)
        if !piece_layers.is_empty() {
            root.insert(
                Bytes::from_static(b"piece layers"),
                Self::encode_piece_layers(&piece_layers),
            );
        }

        // Add optional fields
        self.add_common_fields(&mut root);

        encode(&Value::Dict(root)).map_err(|_| MetainfoError::InvalidField("encoding failed"))
    }

    /// Builds the info dictionary for v1 torrents.
    fn build_info_v1(&self) -> Result<Value, MetainfoError> {
        let mut info = BTreeMap::new();

        // Name
        info.insert(
            Bytes::from_static(b"name"),
            Value::Bytes(Bytes::from(self.name.clone())),
        );

        // Piece length
        info.insert(
            Bytes::from_static(b"piece length"),
            Value::Integer(self.piece_length as i64),
        );

        // Private flag
        if self.private {
            info.insert(Bytes::from_static(b"private"), Value::Integer(1));
        }

        // Concatenate all file data and compute pieces
        let (total_data, pieces) = self.compute_v1_pieces();

        // Pieces (concatenated SHA1 hashes)
        let pieces_bytes: Vec<u8> = pieces.iter().flat_map(|h| h.iter().copied()).collect();
        info.insert(
            Bytes::from_static(b"pieces"),
            Value::Bytes(Bytes::from(pieces_bytes)),
        );

        // Single file vs multi-file
        if self.files.len() == 1 && self.files[0].path.len() == 1 {
            // Single file mode
            info.insert(
                Bytes::from_static(b"length"),
                Value::Integer(total_data.len() as i64),
            );
        } else {
            // Multi-file mode
            let files_list = self.build_files_list_v1();
            info.insert(Bytes::from_static(b"files"), Value::List(files_list));
        }

        Ok(Value::Dict(info))
    }

    /// Builds the info dictionary for v2 torrents.
    fn build_info_v2(&self) -> Result<(Value, PieceLayersMap), MetainfoError> {
        let mut info = BTreeMap::new();
        let mut piece_layers = BTreeMap::new();

        // Name
        info.insert(
            Bytes::from_static(b"name"),
            Value::Bytes(Bytes::from(self.name.clone())),
        );

        // Piece length
        info.insert(
            Bytes::from_static(b"piece length"),
            Value::Integer(self.piece_length as i64),
        );

        // Meta version (required for v2)
        info.insert(Bytes::from_static(b"meta version"), Value::Integer(2));

        // Private flag
        if self.private {
            info.insert(Bytes::from_static(b"private"), Value::Integer(1));
        }

        // Build file tree
        let file_tree = self.build_file_tree_v2(&mut piece_layers)?;
        info.insert(Bytes::from_static(b"file tree"), file_tree);

        Ok((Value::Dict(info), piece_layers))
    }

    /// Builds the info dictionary for hybrid torrents.
    fn build_info_hybrid(&self) -> Result<(Value, PieceLayersMap), MetainfoError> {
        let mut info = BTreeMap::new();
        let mut piece_layers = BTreeMap::new();

        // Name
        info.insert(
            Bytes::from_static(b"name"),
            Value::Bytes(Bytes::from(self.name.clone())),
        );

        // Piece length
        info.insert(
            Bytes::from_static(b"piece length"),
            Value::Integer(self.piece_length as i64),
        );

        // Meta version (required for hybrid too)
        info.insert(Bytes::from_static(b"meta version"), Value::Integer(2));

        // Private flag
        if self.private {
            info.insert(Bytes::from_static(b"private"), Value::Integer(1));
        }

        // V1: pieces (SHA1 hashes)
        let (_total_data, v1_pieces) = self.compute_v1_pieces_with_padding();
        let pieces_bytes: Vec<u8> = v1_pieces.iter().flat_map(|h| h.iter().copied()).collect();
        info.insert(
            Bytes::from_static(b"pieces"),
            Value::Bytes(Bytes::from(pieces_bytes)),
        );

        // V2: file tree
        let file_tree = self.build_file_tree_v2(&mut piece_layers)?;
        info.insert(Bytes::from_static(b"file tree"), file_tree);

        // V1: files list (for multi-file) or length (for single-file)
        if self.files.len() == 1 && self.files[0].path.len() == 1 {
            // Single file mode
            info.insert(
                Bytes::from_static(b"length"),
                Value::Integer(self.files[0].data.len() as i64),
            );
        } else {
            // Multi-file mode with padding files for alignment
            let files_list = self.build_files_list_hybrid();
            info.insert(Bytes::from_static(b"files"), Value::List(files_list));
        }

        Ok((Value::Dict(info), piece_layers))
    }

    /// Computes v1 piece hashes from concatenated file data.
    fn compute_v1_pieces(&self) -> (Vec<u8>, Vec<[u8; 20]>) {
        let total_data: Vec<u8> = self
            .files
            .iter()
            .flat_map(|f| f.data.iter().copied())
            .collect();

        let pieces: Vec<[u8; 20]> = total_data
            .chunks(self.piece_length as usize)
            .map(|chunk| {
                let mut hasher = Sha1::new();
                hasher.update(chunk);
                hasher.finalize().into()
            })
            .collect();

        (total_data, pieces)
    }

    /// Computes v1 piece hashes with padding for hybrid torrents.
    ///
    /// In hybrid torrents, files are piece-aligned, so we need to add padding
    /// between files to match v2's piece boundaries.
    fn compute_v1_pieces_with_padding(&self) -> (Vec<u8>, Vec<[u8; 20]>) {
        let mut total_data = Vec::new();

        for file in &self.files {
            total_data.extend_from_slice(&file.data);

            // Add padding to align to piece boundary
            if !file.data.is_empty() {
                let remainder = file.data.len() % self.piece_length as usize;
                if remainder != 0 {
                    let padding = self.piece_length as usize - remainder;
                    total_data.extend(std::iter::repeat_n(0u8, padding));
                }
            }
        }

        let pieces: Vec<[u8; 20]> = total_data
            .chunks(self.piece_length as usize)
            .map(|chunk| {
                let mut hasher = Sha1::new();
                hasher.update(chunk);
                hasher.finalize().into()
            })
            .collect();

        (total_data, pieces)
    }

    /// Builds the files list for v1 multi-file torrents.
    fn build_files_list_v1(&self) -> Vec<Value> {
        self.files
            .iter()
            .map(|file| {
                let mut file_dict = BTreeMap::new();
                file_dict.insert(
                    Bytes::from_static(b"length"),
                    Value::Integer(file.data.len() as i64),
                );

                let path_list: Vec<Value> = file
                    .path
                    .iter()
                    .map(|p| Value::Bytes(Bytes::from(p.clone())))
                    .collect();
                file_dict.insert(Bytes::from_static(b"path"), Value::List(path_list));

                Value::Dict(file_dict)
            })
            .collect()
    }

    /// Builds the files list for hybrid torrents (with padding files).
    fn build_files_list_hybrid(&self) -> Vec<Value> {
        let mut files_list = Vec::new();

        for (i, file) in self.files.iter().enumerate() {
            // Add the actual file
            let mut file_dict = BTreeMap::new();
            file_dict.insert(
                Bytes::from_static(b"length"),
                Value::Integer(file.data.len() as i64),
            );

            let path_list: Vec<Value> = file
                .path
                .iter()
                .map(|p| Value::Bytes(Bytes::from(p.clone())))
                .collect();
            file_dict.insert(Bytes::from_static(b"path"), Value::List(path_list));

            files_list.push(Value::Dict(file_dict));

            // Add padding file if needed (except for last file)
            if i < self.files.len() - 1 && !file.data.is_empty() {
                let remainder = file.data.len() % self.piece_length as usize;
                if remainder != 0 {
                    let padding_size = self.piece_length as usize - remainder;
                    let mut padding_dict = BTreeMap::new();
                    padding_dict.insert(
                        Bytes::from_static(b"length"),
                        Value::Integer(padding_size as i64),
                    );
                    padding_dict.insert(
                        Bytes::from_static(b"attr"),
                        Value::Bytes(Bytes::from_static(b"p")),
                    );
                    padding_dict.insert(
                        Bytes::from_static(b"path"),
                        Value::List(vec![Value::Bytes(Bytes::from(format!(
                            ".pad/{}",
                            padding_size
                        )))]),
                    );
                    files_list.push(Value::Dict(padding_dict));
                }
            }
        }

        files_list
    }

    /// Builds the file tree for v2 torrents.
    fn build_file_tree_v2(
        &self,
        piece_layers: &mut BTreeMap<[u8; 32], Vec<[u8; 32]>>,
    ) -> Result<Value, MetainfoError> {
        let mut root_tree: BTreeMap<Bytes, Value> = BTreeMap::new();

        for file in &self.files {
            // Compute merkle tree for this file
            let (pieces_root, layer_hashes) = self.compute_file_merkle(&file.data);

            // If file has more than one piece, add to piece layers
            let file_piece_count = file.data.len().div_ceil(self.piece_length as usize);
            if file_piece_count > 1 {
                piece_layers.insert(pieces_root, layer_hashes);
            }

            // Build nested structure for path
            let mut current = &mut root_tree;

            for (i, component) in file.path.iter().enumerate() {
                let key = Bytes::from(component.clone());

                if i == file.path.len() - 1 {
                    // This is the file entry
                    let file_entry = self.build_file_entry(file.data.len() as u64, pieces_root);
                    current.insert(key, file_entry);
                } else {
                    // This is a directory - ensure it exists
                    let entry = current
                        .entry(key)
                        .or_insert_with(|| Value::Dict(BTreeMap::new()));

                    if let Value::Dict(ref mut dict) = entry {
                        current = dict;
                    } else {
                        return Err(MetainfoError::InvalidField("path conflict"));
                    }
                }
            }
        }

        Ok(Value::Dict(root_tree))
    }

    /// Computes the merkle tree for a file and returns (root, layer_hashes).
    fn compute_file_merkle(&self, data: &[u8]) -> ([u8; 32], Vec<[u8; 32]>) {
        if data.is_empty() {
            return ([0u8; 32], Vec::new());
        }

        // Hash each 16 KiB block
        let block_hashes: Vec<[u8; 32]> = data.chunks(MERKLE_BLOCK_SIZE).map(hash_block).collect();

        // Build merkle tree for entire file
        let tree = MerkleTree::from_piece_hashes(block_hashes.clone());
        let file_root = tree.root().unwrap_or([0u8; 32]);

        // Compute piece layer hashes
        // Each piece contains piece_length / MERKLE_BLOCK_SIZE blocks
        let blocks_per_piece = (self.piece_length as usize) / MERKLE_BLOCK_SIZE;
        let mut layer_hashes = Vec::new();

        for piece_blocks in block_hashes.chunks(blocks_per_piece) {
            // Pad to power of 2
            let mut padded = piece_blocks.to_vec();
            while padded.len() < blocks_per_piece {
                padded.push([0u8; 32]);
            }

            let piece_tree = MerkleTree::from_piece_hashes(padded);
            if let Some(root) = piece_tree.root() {
                layer_hashes.push(root);
            }
        }

        (file_root, layer_hashes)
    }

    /// Builds a file entry for the v2 file tree.
    fn build_file_entry(&self, length: u64, pieces_root: [u8; 32]) -> Value {
        let mut entry = BTreeMap::new();

        // Empty string key marks this as a file (not a directory)
        let mut file_props = BTreeMap::new();
        file_props.insert(Bytes::from_static(b"length"), Value::Integer(length as i64));

        if length > 0 {
            file_props.insert(
                Bytes::from_static(b"pieces root"),
                Value::Bytes(Bytes::from(pieces_root.to_vec())),
            );
        }

        entry.insert(Bytes::from_static(b""), Value::Dict(file_props));

        Value::Dict(entry)
    }

    /// Encodes piece layers to bencode value.
    fn encode_piece_layers(layers: &BTreeMap<[u8; 32], Vec<[u8; 32]>>) -> Value {
        let mut dict = BTreeMap::new();

        for (root, hashes) in layers {
            let concat: Vec<u8> = hashes.iter().flat_map(|h| h.iter().copied()).collect();
            dict.insert(
                Bytes::from(root.to_vec()),
                Value::Bytes(Bytes::from(concat)),
            );
        }

        Value::Dict(dict)
    }

    /// Adds common optional fields to the root dictionary.
    fn add_common_fields(&self, root: &mut BTreeMap<Bytes, Value>) {
        // Announce
        if let Some(ref announce) = self.announce {
            root.insert(
                Bytes::from_static(b"announce"),
                Value::Bytes(Bytes::from(announce.clone())),
            );
        }

        // Announce-list
        if !self.announce_list.is_empty() {
            let list: Vec<Value> = self
                .announce_list
                .iter()
                .map(|tier| {
                    Value::List(
                        tier.iter()
                            .map(|url| Value::Bytes(Bytes::from(url.clone())))
                            .collect(),
                    )
                })
                .collect();
            root.insert(Bytes::from_static(b"announce-list"), Value::List(list));
        }

        // Comment
        if let Some(ref comment) = self.comment {
            root.insert(
                Bytes::from_static(b"comment"),
                Value::Bytes(Bytes::from(comment.clone())),
            );
        }

        // Created by
        if let Some(ref created_by) = self.created_by {
            root.insert(
                Bytes::from_static(b"created by"),
                Value::Bytes(Bytes::from(created_by.clone())),
            );
        }

        // Creation date
        let timestamp = self.creation_date.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
        root.insert(
            Bytes::from_static(b"creation date"),
            Value::Integer(timestamp),
        );

        // URL-list (web seeds)
        if !self.url_list.is_empty() {
            if self.url_list.len() == 1 {
                root.insert(
                    Bytes::from_static(b"url-list"),
                    Value::Bytes(Bytes::from(self.url_list[0].clone())),
                );
            } else {
                let list: Vec<Value> = self
                    .url_list
                    .iter()
                    .map(|url| Value::Bytes(Bytes::from(url.clone())))
                    .collect();
                root.insert(Bytes::from_static(b"url-list"), Value::List(list));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::Metainfo;

    #[test]
    fn test_builder_v1_single_file() {
        let data = b"Hello, BitTorrent v1!";
        let torrent_bytes = TorrentBuilder::new("test")
            .version(TorrentVersion::V1)
            .add_file("test.txt", data.to_vec())
            .piece_length(16384)
            .build()
            .unwrap();

        // Parse back and verify
        let metainfo = Metainfo::from_bytes(&torrent_bytes).unwrap();
        assert_eq!(metainfo.info.name, "test");
        assert_eq!(metainfo.info.piece_length, 16384);
        assert_eq!(metainfo.info.total_length, data.len() as u64);
        assert!(metainfo.info.is_v1());
    }

    #[test]
    fn test_builder_v1_multi_file() {
        let torrent_bytes = TorrentBuilder::new("myfiles")
            .version(TorrentVersion::V1)
            .add_file("file1.txt", b"First file content".to_vec())
            .add_file("subdir/file2.txt", b"Second file content".to_vec())
            .piece_length(16384)
            .build()
            .unwrap();

        let metainfo = Metainfo::from_bytes(&torrent_bytes).unwrap();
        assert_eq!(metainfo.info.name, "myfiles");
        assert_eq!(metainfo.info.files.len(), 2);
        assert!(metainfo.info.is_v1());
    }

    #[test]
    fn test_builder_v2_single_file() {
        let data = b"Hello, BitTorrent v2!";
        let torrent_bytes = TorrentBuilder::new("test")
            .version(TorrentVersion::V2)
            .add_file("test.txt", data.to_vec())
            .piece_length(16384)
            .build()
            .unwrap();

        let metainfo = Metainfo::from_bytes(&torrent_bytes).unwrap();
        assert_eq!(metainfo.info.name, "test");
        assert!(metainfo.info.is_v2());
        assert_eq!(metainfo.info.meta_version, Some(2));
    }

    #[test]
    fn test_builder_v2_requires_power_of_two() {
        let result = TorrentBuilder::new("test")
            .version(TorrentVersion::V2)
            .add_file("test.txt", b"data".to_vec())
            .piece_length(30000) // Not power of 2
            .build();

        assert!(result.is_err());
    }

    #[test]
    fn test_builder_v2_requires_min_piece_length() {
        let result = TorrentBuilder::new("test")
            .version(TorrentVersion::V2)
            .add_file("test.txt", b"data".to_vec())
            .piece_length(8192) // Too small
            .build();

        assert!(result.is_err());
    }

    #[test]
    fn test_builder_hybrid() {
        let data = vec![0u8; 32768]; // 2 pieces worth
        let torrent_bytes = TorrentBuilder::new("hybrid_test")
            .version(TorrentVersion::Hybrid)
            .add_file("data.bin", data)
            .piece_length(16384)
            .build()
            .unwrap();

        let metainfo = Metainfo::from_bytes(&torrent_bytes).unwrap();
        assert!(metainfo.info.is_hybrid());
        assert!(metainfo.info.pieces.has_v1());
        assert!(metainfo.info.pieces.has_v2());
    }

    #[test]
    fn test_builder_with_trackers() {
        let torrent_bytes = TorrentBuilder::new("test")
            .add_file("test.txt", b"data".to_vec())
            .add_tracker("http://tracker1.example.com/announce")
            .add_tracker("http://tracker2.example.com/announce")
            .build()
            .unwrap();

        let metainfo = Metainfo::from_bytes(&torrent_bytes).unwrap();
        assert!(metainfo.announce.is_some());
        assert!(!metainfo.announce_list.is_empty());
    }

    #[test]
    fn test_builder_private_torrent() {
        let torrent_bytes = TorrentBuilder::new("test")
            .add_file("test.txt", b"data".to_vec())
            .private(true)
            .build()
            .unwrap();

        let metainfo = Metainfo::from_bytes(&torrent_bytes).unwrap();
        assert!(metainfo.info.private);
    }

    #[test]
    fn test_builder_with_comment() {
        let torrent_bytes = TorrentBuilder::new("test")
            .add_file("test.txt", b"data".to_vec())
            .comment("Test comment")
            .build()
            .unwrap();

        let metainfo = Metainfo::from_bytes(&torrent_bytes).unwrap();
        assert_eq!(metainfo.comment, Some("Test comment".to_string()));
    }

    #[test]
    fn test_builder_empty_name_fails() {
        let result = TorrentBuilder::new("")
            .add_file("test.txt", b"data".to_vec())
            .build();

        assert!(result.is_err());
    }

    #[test]
    fn test_builder_no_files_fails() {
        let result = TorrentBuilder::new("test").build();
        assert!(result.is_err());
    }

    #[test]
    fn test_builder_roundtrip_v1() {
        let original_data = vec![0xAB; 50000]; // Larger than one piece
        let torrent_bytes = TorrentBuilder::new("roundtrip")
            .version(TorrentVersion::V1)
            .add_file("data.bin", original_data.clone())
            .piece_length(16384)
            .add_tracker("http://example.com/announce")
            .comment("Roundtrip test")
            .build()
            .unwrap();

        let metainfo = Metainfo::from_bytes(&torrent_bytes).unwrap();

        assert_eq!(metainfo.info.name, "roundtrip");
        assert_eq!(metainfo.info.total_length, 50000);
        assert_eq!(metainfo.info.piece_length, 16384);
        assert_eq!(metainfo.info.piece_count(), 4); // ceil(50000/16384) = 4
        assert_eq!(metainfo.comment, Some("Roundtrip test".to_string()));
    }

    #[test]
    fn test_builder_roundtrip_v2() {
        let original_data = vec![0xCD; 50000];
        let torrent_bytes = TorrentBuilder::new("roundtrip_v2")
            .version(TorrentVersion::V2)
            .add_file("data.bin", original_data)
            .piece_length(16384)
            .build()
            .unwrap();

        let metainfo = Metainfo::from_bytes(&torrent_bytes).unwrap();

        assert_eq!(metainfo.info.name, "roundtrip_v2");
        assert!(metainfo.info.is_v2());
        assert_eq!(metainfo.info.meta_version, Some(2));
    }
}
