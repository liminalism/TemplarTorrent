use thiserror::Error;

/// Errors that can occur during peer communication.
#[derive(Debug, Error)]
pub enum PeerError {
    /// Network I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The peer sent an invalid handshake.
    #[error("invalid handshake")]
    InvalidHandshake,

    /// The peer's info hash doesn't match ours.
    #[error("info hash mismatch")]
    InfoHashMismatch,

    /// Received a malformed protocol message.
    #[error("invalid message: {0}")]
    InvalidMessage(String),

    /// Received an unknown message ID.
    #[error("invalid message id: {0}")]
    InvalidMessageId(u8),

    /// The connection was closed by the peer.
    #[error("connection closed")]
    ConnectionClosed,

    /// Operation timed out.
    #[error("timeout")]
    Timeout,

    /// Protocol violation by the peer.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Extension protocol error.
    #[error("extension error: {0}")]
    Extension(String),

    /// Error decoding bencode in extension messages.
    #[error("bencode error: {0}")]
    Bencode(#[from] crate::bencode::BencodeError),

    /// Bitfield length doesn't match expected piece count.
    #[error("bitfield length mismatch: expected {expected}, got {actual}")]
    BitfieldLengthMismatch { expected: usize, actual: usize },

    /// Invalid piece index received.
    #[error("invalid piece index: {0}")]
    InvalidPieceIndex(u32),
}
