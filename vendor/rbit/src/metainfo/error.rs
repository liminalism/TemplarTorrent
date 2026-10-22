use thiserror::Error;

use crate::bencode::BencodeError;

/// Errors that can occur when parsing torrent files or magnet links.
#[derive(Debug, Error)]
pub enum MetainfoError {
    /// The torrent file contains invalid bencode.
    #[error("bencode error: {0}")]
    Bencode(#[from] BencodeError),

    /// A required field is missing from the torrent file.
    #[error("missing field: {0}")]
    MissingField(&'static str),

    /// A field has an invalid value or type.
    #[error("invalid field: {0}")]
    InvalidField(&'static str),

    /// The info hash has an invalid length (must be 20 or 32 bytes).
    #[error("invalid info hash length")]
    InvalidInfoHashLength,

    /// The magnet link is malformed.
    #[error("invalid magnet link: {0}")]
    InvalidMagnetLink(String),

    /// The torrent version is not supported.
    #[error("unsupported torrent version")]
    UnsupportedVersion,

    /// An I/O error occurred while reading the torrent file.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
