//! Metadata exchange extension (ut_metadata, BEP-9).
//!
//! This module implements the extension for exchanging torrent metadata
//! between peers. This is primarily used for magnet links where the
//! metadata needs to be fetched from peers.

use std::collections::BTreeMap;

use bytes::Bytes;

use super::error::PeerError;
use crate::bencode::{decode, encode, Value};

/// The size of a metadata piece (16 KB).
pub const METADATA_PIECE_SIZE: usize = 16384;

/// Message types for the ut_metadata extension (BEP-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataMessageType {
    /// Request a piece of metadata.
    Request = 0,
    /// Provide a piece of metadata.
    Data = 1,
    /// Reject a metadata request.
    Reject = 2,
}

impl MetadataMessageType {
    /// Converts a byte value to a message type.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(MetadataMessageType::Request),
            1 => Some(MetadataMessageType::Data),
            2 => Some(MetadataMessageType::Reject),
            _ => None,
        }
    }

    /// Converts the message type to a byte value.
    pub fn as_byte(&self) -> u8 {
        *self as u8
    }
}

/// A metadata exchange message (ut_metadata, BEP-9).
///
/// Used to request, send, or reject metadata pieces between peers.
#[derive(Debug, Clone)]
pub struct MetadataMessage {
    /// The message type (request, data, or reject).
    pub msg_type: MetadataMessageType,
    /// The piece index being requested/sent/rejected.
    pub piece: u32,
    /// The total size of the metadata (only present in data messages).
    pub total_size: Option<u32>,
    /// The actual metadata piece data (only present in data messages).
    pub data: Option<Bytes>,
}

impl MetadataMessage {
    /// Creates a new request message for a metadata piece.
    pub fn request(piece: u32) -> Self {
        Self {
            msg_type: MetadataMessageType::Request,
            piece,
            total_size: None,
            data: None,
        }
    }

    /// Creates a new data message with metadata piece content.
    pub fn data(piece: u32, total_size: u32, data: Bytes) -> Self {
        Self {
            msg_type: MetadataMessageType::Data,
            piece,
            total_size: Some(total_size),
            data: Some(data),
        }
    }

    /// Creates a new reject message for a metadata piece.
    pub fn reject(piece: u32) -> Self {
        Self {
            msg_type: MetadataMessageType::Reject,
            piece,
            total_size: None,
            data: None,
        }
    }

    /// Encodes the message to bytes for transmission.
    ///
    /// The format is a bencoded dictionary followed by optional raw data.
    pub fn encode(&self) -> Result<Bytes, PeerError> {
        let mut dict = BTreeMap::new();

        dict.insert(
            Bytes::from_static(b"msg_type"),
            Value::Integer(self.msg_type.as_byte() as i64),
        );

        dict.insert(
            Bytes::from_static(b"piece"),
            Value::Integer(self.piece as i64),
        );

        if let Some(total_size) = self.total_size {
            dict.insert(
                Bytes::from_static(b"total_size"),
                Value::Integer(total_size as i64),
            );
        }

        let encoded_dict = encode(&Value::Dict(dict))?;

        // For data messages, append the raw data after the bencoded dict
        if let Some(ref data) = self.data {
            let mut result = Vec::with_capacity(encoded_dict.len() + data.len());
            result.extend_from_slice(&encoded_dict);
            result.extend_from_slice(data);
            Ok(Bytes::from(result))
        } else {
            Ok(Bytes::from(encoded_dict))
        }
    }

    /// Decodes a metadata message from bytes.
    ///
    /// For data messages, the raw data follows the bencoded dictionary.
    pub fn decode(payload: &[u8]) -> Result<Self, PeerError> {
        // Find the end of the bencoded dictionary
        let dict_end = find_dict_end(payload)?;

        // Decode the dictionary portion
        let value = decode(&payload[..dict_end])?;
        let dict = value
            .as_dict()
            .ok_or_else(|| PeerError::Extension("expected dict".into()))?;

        let msg_type_byte =
            dict.get(b"msg_type".as_slice())
                .and_then(|v| v.as_integer())
                .ok_or_else(|| PeerError::Extension("missing msg_type".into()))? as u8;

        let msg_type = MetadataMessageType::from_byte(msg_type_byte)
            .ok_or_else(|| PeerError::Extension("invalid msg_type".into()))?;

        let piece =
            dict.get(b"piece".as_slice())
                .and_then(|v| v.as_integer())
                .ok_or_else(|| PeerError::Extension("missing piece".into()))? as u32;

        let total_size = dict
            .get(b"total_size".as_slice())
            .and_then(|v| v.as_integer())
            .map(|v| v as u32);

        // Extract data for Data messages
        let data = if msg_type == MetadataMessageType::Data && dict_end < payload.len() {
            Some(Bytes::copy_from_slice(&payload[dict_end..]))
        } else {
            None
        };

        Ok(Self {
            msg_type,
            piece,
            total_size,
            data,
        })
    }
}

/// Finds the end of a bencoded dictionary in the payload.
fn find_dict_end(payload: &[u8]) -> Result<usize, PeerError> {
    if payload.is_empty() || payload[0] != b'd' {
        return Err(PeerError::Extension("payload must start with 'd'".into()));
    }

    let mut depth = 0;
    let mut i = 0;

    while i < payload.len() {
        match payload[i] {
            b'd' | b'l' => {
                depth += 1;
                i += 1;
            }
            b'e' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return Ok(i);
                }
            }
            b'i' => {
                // Integer: i<number>e
                i += 1;
                while i < payload.len() && payload[i] != b'e' {
                    i += 1;
                }
                i += 1; // skip 'e'
            }
            b'0'..=b'9' => {
                // String: <length>:<data>
                let len_start = i;
                while i < payload.len() && payload[i] != b':' {
                    i += 1;
                }
                let len_str = std::str::from_utf8(&payload[len_start..i])
                    .map_err(|_| PeerError::Extension("invalid string length".into()))?;
                let len: usize = len_str
                    .parse()
                    .map_err(|_| PeerError::Extension("invalid string length".into()))?;
                i += 1; // skip ':'
                i += len; // skip string data
            }
            _ => {
                return Err(PeerError::Extension("invalid bencode".into()));
            }
        }
    }

    Err(PeerError::Extension("unterminated dict".into()))
}

/// Calculates the number of metadata pieces for a given metadata size.
pub fn metadata_piece_count(metadata_size: usize) -> usize {
    metadata_size.div_ceil(METADATA_PIECE_SIZE)
}

/// Calculates the size of a specific metadata piece.
pub fn metadata_piece_size(piece: u32, total_size: usize) -> usize {
    let offset = piece as usize * METADATA_PIECE_SIZE;
    if offset >= total_size {
        0
    } else {
        (total_size - offset).min(METADATA_PIECE_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_encode_decode() {
        let msg = MetadataMessage::request(5);
        let encoded = msg.encode().unwrap();
        let decoded = MetadataMessage::decode(&encoded).unwrap();

        assert_eq!(decoded.msg_type, MetadataMessageType::Request);
        assert_eq!(decoded.piece, 5);
        assert!(decoded.total_size.is_none());
        assert!(decoded.data.is_none());
    }

    #[test]
    fn test_data_encode_decode() {
        let data = Bytes::from(vec![1, 2, 3, 4, 5]);
        let msg = MetadataMessage::data(2, 1000, data.clone());
        let encoded = msg.encode().unwrap();
        let decoded = MetadataMessage::decode(&encoded).unwrap();

        assert_eq!(decoded.msg_type, MetadataMessageType::Data);
        assert_eq!(decoded.piece, 2);
        assert_eq!(decoded.total_size, Some(1000));
        assert_eq!(decoded.data, Some(data));
    }

    #[test]
    fn test_reject_encode_decode() {
        let msg = MetadataMessage::reject(10);
        let encoded = msg.encode().unwrap();
        let decoded = MetadataMessage::decode(&encoded).unwrap();

        assert_eq!(decoded.msg_type, MetadataMessageType::Reject);
        assert_eq!(decoded.piece, 10);
    }

    #[test]
    fn test_metadata_piece_count() {
        assert_eq!(metadata_piece_count(0), 0);
        assert_eq!(metadata_piece_count(1), 1);
        assert_eq!(metadata_piece_count(16384), 1);
        assert_eq!(metadata_piece_count(16385), 2);
        assert_eq!(metadata_piece_count(32768), 2);
        assert_eq!(metadata_piece_count(50000), 4);
    }
}
