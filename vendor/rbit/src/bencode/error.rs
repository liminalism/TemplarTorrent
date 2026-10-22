use thiserror::Error;

/// Errors that can occur during bencode encoding or decoding.
///
/// # Examples
///
/// ```
/// use rbit::bencode::{decode, BencodeError};
///
/// // Truncated input
/// let result = decode(b"i42");
/// assert!(matches!(result, Err(BencodeError::UnexpectedEof)));
///
/// // Invalid integer (leading zeros)
/// let result = decode(b"i007e");
/// assert!(matches!(result, Err(BencodeError::InvalidInteger(_))));
///
/// // Trailing data
/// let result = decode(b"i42eextra");
/// assert!(matches!(result, Err(BencodeError::TrailingData)));
/// ```
#[derive(Debug, Error)]
pub enum BencodeError {
    /// Input ended before a complete value was parsed.
    #[error("unexpected end of input")]
    UnexpectedEof,

    /// Integer is malformed (e.g., has leading zeros, is empty, or overflows).
    #[error("invalid integer: {0}")]
    InvalidInteger(String),

    /// Byte string length prefix is not a valid number.
    #[error("invalid string length")]
    InvalidStringLength,

    /// Encountered an unexpected character while parsing.
    #[error("unexpected character: {0}")]
    UnexpectedChar(char),

    /// Extra data exists after the bencode value.
    #[error("trailing data after value")]
    TrailingData,

    /// Recursion limit (64 levels) exceeded to prevent stack overflow.
    #[error("nesting too deep")]
    NestingTooDeep,

    /// I/O error during encoding.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
