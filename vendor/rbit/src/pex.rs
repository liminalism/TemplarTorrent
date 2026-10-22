//! Peer Exchange ([BEP-11]).
//!
//! Peer Exchange (PEX) allows connected peers to share information about
//! other peers they know about, reducing reliance on trackers.
//!
//! # Overview
//!
//! PEX works via the extension protocol ([BEP-10]). Peers periodically exchange
//! messages containing:
//! - **added** - New peers to connect to
//! - **dropped** - Peers that have disconnected
//!
//! # Examples
//!
//! ```
//! use rbit::pex::{PexMessage, PexPeer, PexFlags};
//! use std::net::SocketAddr;
//!
//! let mut msg = PexMessage::new();
//!
//! // Add a peer
//! let peer = PexPeer::new("192.168.1.100:6881".parse().unwrap());
//! msg.add_peer(peer);
//!
//! // Add a seeder with flags
//! let seeder = PexPeer::with_flags(
//!     "10.0.0.1:51413".parse().unwrap(),
//!     PexFlags { seed: true, ..Default::default() }
//! );
//! msg.add_peer(seeder);
//!
//! // Encode for transmission
//! let added = msg.encode_added();
//! let flags = msg.encode_added_flags();
//! ```
//!
//! # Peer Flags
//!
//! Each peer has optional flags indicating capabilities:
//! - `encryption` - Supports encryption
//! - `seed` - Is a seeder (has all pieces)
//! - `utp` - Supports uTP protocol
//! - `holepunch` - Supports NAT hole punching
//! - `connectable` - Accepts incoming connections
//!
//! [BEP-10]: http://bittorrent.org/beps/bep_0010.html
//! [BEP-11]: http://bittorrent.org/beps/bep_0011.html

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use bytes::{BufMut, Bytes, BytesMut};

/// A peer discovered via Peer Exchange.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PexPeer {
    /// The peer's socket address.
    pub addr: SocketAddr,
    /// Capability flags for this peer.
    pub flags: PexFlags,
}

/// Capability flags for a PEX peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PexFlags {
    /// Peer supports encrypted connections.
    pub encryption: bool,
    /// Peer is a seeder (has complete data).
    pub seed: bool,
    /// Peer supports uTP (micro Transport Protocol).
    pub utp: bool,
    /// Peer supports NAT hole punching.
    pub holepunch: bool,
    /// Peer accepts incoming connections.
    pub connectable: bool,
}

impl PexFlags {
    /// Decodes flags from a single byte.
    pub fn from_byte(b: u8) -> Self {
        Self {
            encryption: (b & 0x01) != 0,
            seed: (b & 0x02) != 0,
            utp: (b & 0x04) != 0,
            holepunch: (b & 0x08) != 0,
            connectable: (b & 0x10) != 0,
        }
    }

    /// Encodes flags to a single byte.
    pub fn to_byte(self) -> u8 {
        let mut b = 0u8;
        if self.encryption {
            b |= 0x01;
        }
        if self.seed {
            b |= 0x02;
        }
        if self.utp {
            b |= 0x04;
        }
        if self.holepunch {
            b |= 0x08;
        }
        if self.connectable {
            b |= 0x10;
        }
        b
    }
}

/// A PEX message containing peer updates.
///
/// Messages track both IPv4 and IPv6 peers separately.
#[derive(Debug, Clone, Default)]
pub struct PexMessage {
    /// IPv4 peers to add.
    pub added: Vec<PexPeer>,
    /// IPv6 peers to add.
    pub added6: Vec<PexPeer>,
    /// IPv4 peers that disconnected.
    pub dropped: Vec<SocketAddr>,
    /// IPv6 peers that disconnected.
    pub dropped6: Vec<SocketAddr>,
}

impl PexMessage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_peer(&mut self, peer: PexPeer) {
        match peer.addr {
            SocketAddr::V4(_) => self.added.push(peer),
            SocketAddr::V6(_) => self.added6.push(peer),
        }
    }

    pub fn drop_peer(&mut self, addr: SocketAddr) {
        match addr {
            SocketAddr::V4(_) => self.dropped.push(addr),
            SocketAddr::V6(_) => self.dropped6.push(addr),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.added6.is_empty()
            && self.dropped.is_empty()
            && self.dropped6.is_empty()
    }

    pub fn encode_added(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.added.len() * 6);
        for peer in &self.added {
            if let SocketAddr::V4(addr) = peer.addr {
                buf.put_slice(&addr.ip().octets());
                buf.put_u16(addr.port());
            }
        }
        buf.freeze()
    }

    pub fn encode_added_flags(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.added.len());
        for peer in &self.added {
            if peer.addr.is_ipv4() {
                buf.put_u8(peer.flags.to_byte());
            }
        }
        buf.freeze()
    }

    pub fn encode_added6(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.added6.len() * 18);
        for peer in &self.added6 {
            if let SocketAddr::V6(addr) = peer.addr {
                buf.put_slice(&addr.ip().octets());
                buf.put_u16(addr.port());
            }
        }
        buf.freeze()
    }

    pub fn encode_added6_flags(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.added6.len());
        for peer in &self.added6 {
            if peer.addr.is_ipv6() {
                buf.put_u8(peer.flags.to_byte());
            }
        }
        buf.freeze()
    }

    pub fn encode_dropped(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.dropped.len() * 6);
        for addr in &self.dropped {
            if let SocketAddr::V4(addr) = addr {
                buf.put_slice(&addr.ip().octets());
                buf.put_u16(addr.port());
            }
        }
        buf.freeze()
    }

    pub fn encode_dropped6(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.dropped6.len() * 18);
        for addr in &self.dropped6 {
            if let SocketAddr::V6(addr) = addr {
                buf.put_slice(&addr.ip().octets());
                buf.put_u16(addr.port());
            }
        }
        buf.freeze()
    }

    pub fn decode_added(data: &[u8], flags: &[u8]) -> Vec<PexPeer> {
        let mut peers = Vec::new();
        let mut i = 0;
        let mut flag_idx = 0;

        while i + 6 <= data.len() {
            let ip = Ipv4Addr::new(data[i], data[i + 1], data[i + 2], data[i + 3]);
            let port = u16::from_be_bytes([data[i + 4], data[i + 5]]);

            let peer_flags = if flag_idx < flags.len() {
                PexFlags::from_byte(flags[flag_idx])
            } else {
                PexFlags::default()
            };

            peers.push(PexPeer {
                addr: SocketAddr::V4(SocketAddrV4::new(ip, port)),
                flags: peer_flags,
            });

            i += 6;
            flag_idx += 1;
        }

        peers
    }

    pub fn decode_added6(data: &[u8], flags: &[u8]) -> Vec<PexPeer> {
        let mut peers = Vec::new();
        let mut i = 0;
        let mut flag_idx = 0;

        while i + 18 <= data.len() {
            let mut ip_bytes = [0u8; 16];
            ip_bytes.copy_from_slice(&data[i..i + 16]);
            let ip = Ipv6Addr::from(ip_bytes);
            let port = u16::from_be_bytes([data[i + 16], data[i + 17]]);

            let peer_flags = if flag_idx < flags.len() {
                PexFlags::from_byte(flags[flag_idx])
            } else {
                PexFlags::default()
            };

            peers.push(PexPeer {
                addr: SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)),
                flags: peer_flags,
            });

            i += 18;
            flag_idx += 1;
        }

        peers
    }

    pub fn decode_dropped(data: &[u8]) -> Vec<SocketAddr> {
        let mut addrs = Vec::new();
        let mut i = 0;

        while i + 6 <= data.len() {
            let ip = Ipv4Addr::new(data[i], data[i + 1], data[i + 2], data[i + 3]);
            let port = u16::from_be_bytes([data[i + 4], data[i + 5]]);
            addrs.push(SocketAddr::V4(SocketAddrV4::new(ip, port)));
            i += 6;
        }

        addrs
    }

    pub fn decode_dropped6(data: &[u8]) -> Vec<SocketAddr> {
        let mut addrs = Vec::new();
        let mut i = 0;

        while i + 18 <= data.len() {
            let mut ip_bytes = [0u8; 16];
            ip_bytes.copy_from_slice(&data[i..i + 16]);
            let ip = Ipv6Addr::from(ip_bytes);
            let port = u16::from_be_bytes([data[i + 16], data[i + 17]]);
            addrs.push(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)));
            i += 18;
        }

        addrs
    }
}

impl PexPeer {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            flags: PexFlags::default(),
        }
    }

    pub fn with_flags(addr: SocketAddr, flags: PexFlags) -> Self {
        Self { addr, flags }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pex_flags() {
        let flags = PexFlags {
            encryption: true,
            seed: false,
            utp: true,
            holepunch: false,
            connectable: true,
        };

        let byte = flags.to_byte();
        let decoded = PexFlags::from_byte(byte);

        assert_eq!(flags, decoded);
    }

    #[test]
    fn test_pex_encode_decode_v4() {
        let mut msg = PexMessage::new();
        msg.add_peer(PexPeer {
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 6881)),
            flags: PexFlags {
                encryption: true,
                ..Default::default()
            },
        });
        msg.add_peer(PexPeer {
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 51413)),
            flags: PexFlags {
                seed: true,
                ..Default::default()
            },
        });

        let added = msg.encode_added();
        let flags = msg.encode_added_flags();

        let decoded = PexMessage::decode_added(&added, &flags);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].addr, msg.added[0].addr);
        assert!(decoded[0].flags.encryption);
        assert_eq!(decoded[1].addr, msg.added[1].addr);
        assert!(decoded[1].flags.seed);
    }

    #[test]
    fn test_pex_dropped() {
        let mut msg = PexMessage::new();
        msg.drop_peer(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(192, 168, 1, 1),
            6881,
        )));

        let dropped = msg.encode_dropped();
        let decoded = PexMessage::decode_dropped(&dropped);

        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0], msg.dropped[0]);
    }
}
