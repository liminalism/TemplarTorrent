use thiserror::Error;

#[derive(Debug, Error)]
pub enum DhtError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("bencode error: {0}")]
    Bencode(#[from] crate::bencode::BencodeError),

    #[error("invalid message: {0}")]
    InvalidMessage(String),

    #[error("invalid node id length")]
    InvalidNodeId,

    #[error("timeout")]
    Timeout,

    #[error("rate limited")]
    RateLimited,

    #[error("node not found")]
    NodeNotFound,
}
