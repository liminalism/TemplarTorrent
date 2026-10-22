use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("piece hash mismatch for piece {0}")]
    HashMismatch(u32),

    #[error("invalid piece index: {0}")]
    InvalidPieceIndex(u32),

    #[error("invalid block offset: piece {piece}, offset {offset}")]
    InvalidBlockOffset { piece: u32, offset: u32 },

    #[error("file not found: {0}")]
    FileNotFound(String),

    #[error("disk full")]
    DiskFull,

    #[error("torrent not registered: {0}")]
    TorrentNotFound(String),

    #[error("path traversal detected in file path: {0}")]
    PathTraversal(String),
}
