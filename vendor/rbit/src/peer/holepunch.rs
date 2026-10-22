//! BEP-55: Holepunch Extension
//!
//! This module implements UDP hole punching for NAT traversal,
//! allowing peers behind firewalls to establish direct connections
//! through a relay peer.
//!
//! [BEP-55]: http://bittorrent.org/beps/bep_0055.html

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

/// Extension name for holepunch in BEP-10 handshake.
pub const HOLEPUNCH_EXTENSION_NAME: &str = "ut_holepunch";

/// Default extension ID for holepunch (typically 4).
pub const HOLEPUNCH_DEFAULT_EXTENSION_ID: u8 = 4;

/// Timeout for holepunch attempts.
pub const HOLEPUNCH_TIMEOUT_SECS: u64 = 30;

/// Delay before retrying holepunch.
pub const HOLEPUNCH_RETRY_DELAY_SECS: u64 = 5;

/// Holepunch message types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HolepunchMessageType {
    /// Request to initiate a connection through a relay.
    Rendezvous = 0x00,
    /// Instruction to connect to a peer.
    Connect = 0x01,
    /// Error response.
    Error = 0x02,
}

impl TryFrom<u8> for HolepunchMessageType {
    type Error = HolepunchError;

    fn try_from(value: u8) -> Result<Self, HolepunchError> {
        match value {
            0x00 => Ok(HolepunchMessageType::Rendezvous),
            0x01 => Ok(HolepunchMessageType::Connect),
            0x02 => Ok(HolepunchMessageType::Error),
            _ => Err(HolepunchError::InvalidMessageType(value)),
        }
    }
}

/// Address type in holepunch messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HolepunchAddrType {
    /// IPv4 address (4 bytes).
    IPv4 = 0x00,
    /// IPv6 address (16 bytes).
    IPv6 = 0x01,
}

impl TryFrom<u8> for HolepunchAddrType {
    type Error = HolepunchError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(HolepunchAddrType::IPv4),
            0x01 => Ok(HolepunchAddrType::IPv6),
            _ => Err(HolepunchError::InvalidAddrType(value)),
        }
    }
}

impl From<&IpAddr> for HolepunchAddrType {
    fn from(addr: &IpAddr) -> Self {
        match addr {
            IpAddr::V4(_) => HolepunchAddrType::IPv4,
            IpAddr::V6(_) => HolepunchAddrType::IPv6,
        }
    }
}

/// Error codes for holepunch error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum HolepunchErrorCode {
    /// The target peer is not known.
    NoSuchPeer = 0x01,
    /// We are not connected to the target peer.
    NotConnected = 0x02,
    /// The target peer does not support holepunch.
    NoSupport = 0x03,
    /// Cannot holepunch to yourself.
    NoSelf = 0x04,
}

impl TryFrom<u32> for HolepunchErrorCode {
    type Error = HolepunchError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(HolepunchErrorCode::NoSuchPeer),
            0x02 => Ok(HolepunchErrorCode::NotConnected),
            0x03 => Ok(HolepunchErrorCode::NoSupport),
            0x04 => Ok(HolepunchErrorCode::NoSelf),
            _ => Err(HolepunchError::InvalidErrorCode(value)),
        }
    }
}

impl std::fmt::Display for HolepunchErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HolepunchErrorCode::NoSuchPeer => write!(f, "no such peer"),
            HolepunchErrorCode::NotConnected => write!(f, "not connected to peer"),
            HolepunchErrorCode::NoSupport => write!(f, "peer does not support holepunch"),
            HolepunchErrorCode::NoSelf => write!(f, "cannot holepunch to self"),
        }
    }
}

/// Errors that can occur during holepunch operations.
#[derive(Debug, Error)]
pub enum HolepunchError {
    /// Invalid message type.
    #[error("invalid message type: {0}")]
    InvalidMessageType(u8),

    /// Invalid address type.
    #[error("invalid address type: {0}")]
    InvalidAddrType(u8),

    /// Invalid error code.
    #[error("invalid error code: {0}")]
    InvalidErrorCode(u32),

    /// Message too short.
    #[error("message too short: expected at least {expected} bytes, got {actual}")]
    MessageTooShort { expected: usize, actual: usize },

    /// Holepunch failed with error code.
    #[error("holepunch error: {0}")]
    RemoteError(HolepunchErrorCode),

    /// Holepunch timed out.
    #[error("holepunch timed out")]
    Timeout,

    /// Connection failed.
    #[error("connection failed: {0}")]
    ConnectionFailed(String),
}

/// A holepunch protocol message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolepunchMessage {
    /// Message type.
    pub msg_type: HolepunchMessageType,
    /// Target address.
    pub addr: SocketAddr,
    /// Error code (only valid for Error messages).
    pub err_code: u32,
}

impl HolepunchMessage {
    /// Creates a new Rendezvous message.
    ///
    /// Sent by the initiator to a relay to request a connection to a target peer.
    pub fn rendezvous(target: SocketAddr) -> Self {
        Self {
            msg_type: HolepunchMessageType::Rendezvous,
            addr: target,
            err_code: 0,
        }
    }

    /// Creates a new Connect message.
    ///
    /// Sent by the relay to both the initiator and target to trigger connection attempts.
    pub fn connect(peer: SocketAddr) -> Self {
        Self {
            msg_type: HolepunchMessageType::Connect,
            addr: peer,
            err_code: 0,
        }
    }

    /// Creates a new Error message.
    pub fn error(addr: SocketAddr, code: HolepunchErrorCode) -> Self {
        Self {
            msg_type: HolepunchMessageType::Error,
            addr,
            err_code: code as u32,
        }
    }

    /// Encodes the message to binary format.
    ///
    /// Format:
    /// - 1 byte: message type
    /// - 1 byte: address type (0 = IPv4, 1 = IPv6)
    /// - 4 or 16 bytes: address (big-endian)
    /// - 2 bytes: port (big-endian)
    /// - 4 bytes: error code (big-endian)
    pub fn encode(&self) -> Bytes {
        let addr_type = HolepunchAddrType::from(&self.addr.ip());
        let is_ipv6 = matches!(addr_type, HolepunchAddrType::IPv6);

        let size = if is_ipv6 { 24 } else { 12 };
        let mut buf = BytesMut::with_capacity(size);

        buf.put_u8(self.msg_type as u8);
        buf.put_u8(addr_type as u8);

        match self.addr.ip() {
            IpAddr::V4(ip) => buf.put_slice(&ip.octets()),
            IpAddr::V6(ip) => buf.put_slice(&ip.octets()),
        }

        buf.put_u16(self.addr.port());
        buf.put_u32(self.err_code);

        buf.freeze()
    }

    /// Decodes a message from binary format.
    pub fn decode(data: &[u8]) -> Result<Self, HolepunchError> {
        if data.len() < 8 {
            return Err(HolepunchError::MessageTooShort {
                expected: 8,
                actual: data.len(),
            });
        }

        let mut buf = data;

        let msg_type = HolepunchMessageType::try_from(buf.get_u8())?;
        let addr_type = HolepunchAddrType::try_from(buf.get_u8())?;

        let expected_len = match addr_type {
            HolepunchAddrType::IPv4 => 12,
            HolepunchAddrType::IPv6 => 24,
        };

        if data.len() < expected_len {
            return Err(HolepunchError::MessageTooShort {
                expected: expected_len,
                actual: data.len(),
            });
        }

        let ip = match addr_type {
            HolepunchAddrType::IPv4 => {
                let mut octets = [0u8; 4];
                buf.copy_to_slice(&mut octets);
                IpAddr::V4(Ipv4Addr::from(octets))
            }
            HolepunchAddrType::IPv6 => {
                let mut octets = [0u8; 16];
                buf.copy_to_slice(&mut octets);
                IpAddr::V6(Ipv6Addr::from(octets))
            }
        };

        let port = buf.get_u16();
        let err_code = buf.get_u32();

        let addr = SocketAddr::new(ip, port);

        Ok(Self {
            msg_type,
            addr,
            err_code,
        })
    }

    /// Returns true if this is a Rendezvous message.
    pub fn is_rendezvous(&self) -> bool {
        self.msg_type == HolepunchMessageType::Rendezvous
    }

    /// Returns true if this is a Connect message.
    pub fn is_connect(&self) -> bool {
        self.msg_type == HolepunchMessageType::Connect
    }

    /// Returns true if this is an Error message.
    pub fn is_error(&self) -> bool {
        self.msg_type == HolepunchMessageType::Error
    }

    /// Returns the error code if this is an Error message.
    pub fn error_code(&self) -> Option<HolepunchErrorCode> {
        if self.is_error() {
            HolepunchErrorCode::try_from(self.err_code).ok()
        } else {
            None
        }
    }
}

/// State for holepunch operations on a peer connection.
#[derive(Debug, Clone, Default)]
pub struct HolepunchState {
    /// Whether the remote peer supports holepunch.
    pub supported: bool,
    /// The extension ID for holepunch (from handshake).
    pub extension_id: Option<u8>,
}

impl HolepunchState {
    /// Creates a new holepunch state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates state from an extension handshake.
    pub fn update_from_handshake(&mut self, extensions: &std::collections::BTreeMap<String, u8>) {
        if let Some(&id) = extensions.get(HOLEPUNCH_EXTENSION_NAME) {
            self.supported = true;
            self.extension_id = Some(id);
        } else {
            self.supported = false;
            self.extension_id = None;
        }
    }

    /// Returns true if holepunch is supported.
    pub fn is_supported(&self) -> bool {
        self.supported && self.extension_id.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_type_conversion() {
        assert_eq!(
            HolepunchMessageType::try_from(0x00).unwrap(),
            HolepunchMessageType::Rendezvous
        );
        assert_eq!(
            HolepunchMessageType::try_from(0x01).unwrap(),
            HolepunchMessageType::Connect
        );
        assert_eq!(
            HolepunchMessageType::try_from(0x02).unwrap(),
            HolepunchMessageType::Error
        );
        assert!(HolepunchMessageType::try_from(0x03).is_err());
    }

    #[test]
    fn test_addr_type_conversion() {
        assert_eq!(
            HolepunchAddrType::try_from(0x00).unwrap(),
            HolepunchAddrType::IPv4
        );
        assert_eq!(
            HolepunchAddrType::try_from(0x01).unwrap(),
            HolepunchAddrType::IPv6
        );
        assert!(HolepunchAddrType::try_from(0x02).is_err());
    }

    #[test]
    fn test_error_code_conversion() {
        assert_eq!(
            HolepunchErrorCode::try_from(0x01).unwrap(),
            HolepunchErrorCode::NoSuchPeer
        );
        assert_eq!(
            HolepunchErrorCode::try_from(0x02).unwrap(),
            HolepunchErrorCode::NotConnected
        );
        assert_eq!(
            HolepunchErrorCode::try_from(0x03).unwrap(),
            HolepunchErrorCode::NoSupport
        );
        assert_eq!(
            HolepunchErrorCode::try_from(0x04).unwrap(),
            HolepunchErrorCode::NoSelf
        );
        assert!(HolepunchErrorCode::try_from(0x05).is_err());
    }

    #[test]
    fn test_encode_decode_ipv4_rendezvous() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 6881);
        let msg = HolepunchMessage::rendezvous(addr);

        let encoded = msg.encode();
        assert_eq!(encoded.len(), 12);

        let decoded = HolepunchMessage::decode(&encoded).unwrap();
        assert_eq!(decoded.msg_type, HolepunchMessageType::Rendezvous);
        assert_eq!(decoded.addr, addr);
        assert_eq!(decoded.err_code, 0);
    }

    #[test]
    fn test_encode_decode_ipv6_connect() {
        let addr = SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0, 0, 0, 0, 0, 0x8888)),
            6881,
        );
        let msg = HolepunchMessage::connect(addr);

        let encoded = msg.encode();
        assert_eq!(encoded.len(), 24);

        let decoded = HolepunchMessage::decode(&encoded).unwrap();
        assert_eq!(decoded.msg_type, HolepunchMessageType::Connect);
        assert_eq!(decoded.addr, addr);
        assert_eq!(decoded.err_code, 0);
    }

    #[test]
    fn test_encode_decode_error() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 51413);
        let msg = HolepunchMessage::error(addr, HolepunchErrorCode::NotConnected);

        let encoded = msg.encode();
        let decoded = HolepunchMessage::decode(&encoded).unwrap();

        assert_eq!(decoded.msg_type, HolepunchMessageType::Error);
        assert_eq!(decoded.addr, addr);
        assert_eq!(decoded.err_code, HolepunchErrorCode::NotConnected as u32);
        assert_eq!(decoded.error_code(), Some(HolepunchErrorCode::NotConnected));
    }

    #[test]
    fn test_message_too_short() {
        let data = [0x00, 0x00]; // Only 2 bytes
        let result = HolepunchMessage::decode(&data);
        assert!(matches!(
            result,
            Err(HolepunchError::MessageTooShort { .. })
        ));
    }

    #[test]
    fn test_holepunch_state() {
        let mut state = HolepunchState::new();
        assert!(!state.is_supported());

        let mut extensions = std::collections::BTreeMap::new();
        extensions.insert("ut_holepunch".to_string(), 4);
        state.update_from_handshake(&extensions);

        assert!(state.is_supported());
        assert_eq!(state.extension_id, Some(4));
    }
}
