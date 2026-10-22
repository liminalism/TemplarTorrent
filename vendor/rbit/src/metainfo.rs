//! Torrent metainfo handling ([BEP-3], [BEP-9], [BEP-52]).
//!
//! This module provides types for working with torrent files and magnet links,
//! supporting both BitTorrent v1 and v2 torrents.
//!
//! # Overview
//!
//! A torrent file (`.torrent`) contains metadata about files to be shared:
//! - File names, sizes, and directory structure
//! - Piece hashes for data integrity verification
//! - Tracker URLs for peer discovery
//!
//! The [`Metainfo`] struct represents a parsed torrent file, while [`MagnetLink`]
//! handles magnet URIs which contain only an info hash and optional metadata.
//!
//! # Examples
//!
//! ## Parsing a torrent file
//!
//! ```no_run
//! use rbit::metainfo::Metainfo;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let data = std::fs::read("example.torrent")?;
//! let torrent = Metainfo::from_bytes(&data)?;
//!
//! println!("Name: {}", torrent.info.name);
//! println!("Info hash: {}", torrent.info_hash);
//! println!("Total size: {} bytes", torrent.info.total_length);
//! println!("Piece length: {} bytes", torrent.info.piece_length);
//! println!("Number of pieces: {}", torrent.info.piece_count());
//!
//! // List files in a multi-file torrent
//! for file in &torrent.info.files {
//!     println!("  {} ({} bytes)", file.path.display(), file.length);
//! }
//!
//! // Get tracker URLs
//! for tracker in torrent.trackers() {
//!     println!("Tracker: {}", tracker);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Parsing a magnet link
//!
//! ```
//! use rbit::metainfo::MagnetLink;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let magnet = MagnetLink::parse(
//!     "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a\
//!      &dn=Example%20File&tr=http%3A%2F%2Ftracker.example.com%2Fannounce"
//! )?;
//!
//! println!("Info hash: {}", magnet.info_hash);
//! println!("Display name: {:?}", magnet.display_name);
//! println!("Trackers: {:?}", magnet.trackers);
//!
//! // Convert back to a magnet URI
//! let uri = magnet.to_uri();
//! # Ok(())
//! # }
//! ```
//!
//! ## Working with info hashes
//!
//! ```
//! use rbit::metainfo::InfoHash;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Parse from hex string
//! let hash = InfoHash::from_hex("c12fe1c06bba254a9dc9f519b335aa7c1367a88a")?;
//!
//! // Check version
//! assert!(hash.is_v1());  // 20-byte SHA1 hash
//!
//! // Convert to bytes
//! let bytes = hash.as_bytes();
//! assert_eq!(bytes.len(), 20);
//!
//! // Convert back to hex
//! let hex = hash.to_hex();
//! # Ok(())
//! # }
//! ```
//!
//! # Torrent Structure
//!
//! A torrent file contains:
//!
//! - **info** - Core torrent metadata (hashed to create the info hash)
//!   - `name` - Suggested file/directory name
//!   - `piece length` - Size of each piece in bytes
//!   - `pieces` - Concatenated SHA1 hashes of each piece
//!   - `length` - Total size (single-file) OR `files` list (multi-file)
//! - **announce** - Primary tracker URL
//! - **announce-list** - Additional tracker tiers (BEP-12)
//! - **creation date** - Unix timestamp when created
//! - **comment** - Optional comment
//! - **created by** - Client that created the torrent
//!
//! [BEP-3]: http://bittorrent.org/beps/bep_0003.html
//! [BEP-9]: http://bittorrent.org/beps/bep_0009.html
//! [BEP-52]: http://bittorrent.org/beps/bep_0052.html

mod builder;
mod error;
mod file_tree;
mod info_hash;
mod magnet;
mod merkle;
mod torrent;

pub use builder::{TorrentBuilder, DEFAULT_PIECE_LENGTH, MIN_V2_PIECE_LENGTH};
pub use error::MetainfoError;
pub use file_tree::{FileTree, FlattenedFile};
pub use info_hash::{InfoHash, InfoHashV1, InfoHashV2};
pub use magnet::MagnetLink;
pub use merkle::{
    compute_piece_root, compute_root, extract_layer_hashes, generate_proof_hashes, hash_block,
    hash_data_blocks, verify_piece, verify_piece_layer, MerkleTree, MERKLE_BLOCK_SIZE,
};
pub use torrent::{File, Info, Metainfo, PieceHashes, PieceLayers, TorrentVersion};

#[cfg(test)]
mod tests;
