use thiserror::Error;

#[derive(Debug, Error)]
pub enum TrackerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[cfg(feature = "http-client")]
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("bencode error: {0}")]
    Bencode(#[from] crate::bencode::BencodeError),

    #[error("tracker returned error: {0}")]
    TrackerError(String),

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("timeout")]
    Timeout,

    #[error("connection refused")]
    ConnectionRefused,

    #[error("invalid url: {0}")]
    InvalidUrl(String),

    #[error("unsupported protocol: {0}")]
    UnsupportedProtocol(String),
}
