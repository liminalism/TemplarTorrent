use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// A peer returned from a tracker.
///
/// This struct includes the optional peer ID which may be present in
/// non-compact tracker responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The peer's socket address (IP and port).
    pub addr: SocketAddr,
    /// The peer's 20-byte ID (optional, not present in compact responses).
    pub peer_id: Option<[u8; 20]>,
}

impl Peer {
    /// Creates a new peer with just an address.
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            peer_id: None,
        }
    }

    /// Creates a new peer with address and peer ID.
    pub fn with_id(addr: SocketAddr, peer_id: [u8; 20]) -> Self {
        Self {
            addr,
            peer_id: Some(peer_id),
        }
    }

    /// Parses a peer from compact IPv4 format (6 bytes).
    ///
    /// Format: 4 bytes IP + 2 bytes port (big-endian).
    pub fn from_compact_v4(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 6 {
            return None;
        }
        let ip = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
        let port = u16::from_be_bytes([bytes[4], bytes[5]]);
        Some(Self {
            addr: SocketAddr::new(IpAddr::V4(ip), port),
            peer_id: None,
        })
    }

    /// Parses a peer from compact IPv6 format (18 bytes).
    ///
    /// Format: 16 bytes IP + 2 bytes port (big-endian).
    pub fn from_compact_v6(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 18 {
            return None;
        }
        let mut ip_bytes = [0u8; 16];
        ip_bytes.copy_from_slice(&bytes[..16]);
        let ip = Ipv6Addr::from(ip_bytes);
        let port = u16::from_be_bytes([bytes[16], bytes[17]]);
        Some(Self {
            addr: SocketAddr::new(IpAddr::V6(ip), port),
            peer_id: None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerEvent {
    None,
    Started,
    Stopped,
    Completed,
}

impl TrackerEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrackerEvent::None => "",
            TrackerEvent::Started => "started",
            TrackerEvent::Stopped => "stopped",
            TrackerEvent::Completed => "completed",
        }
    }

    pub fn as_udp_id(&self) -> u32 {
        match self {
            TrackerEvent::None => 0,
            TrackerEvent::Completed => 1,
            TrackerEvent::Started => 2,
            TrackerEvent::Stopped => 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnnounceResponse {
    pub interval: u32,
    pub min_interval: Option<u32>,
    pub complete: Option<u32>,
    pub incomplete: Option<u32>,
    pub peers: Vec<SocketAddr>,
    pub peers6: Vec<SocketAddr>,
    pub warning_message: Option<String>,
    pub tracker_id: Option<String>,
}

impl AnnounceResponse {
    pub fn new(interval: u32) -> Self {
        Self {
            interval,
            min_interval: None,
            complete: None,
            incomplete: None,
            peers: Vec::new(),
            peers6: Vec::new(),
            warning_message: None,
            tracker_id: None,
        }
    }

    pub fn all_peers(&self) -> impl Iterator<Item = &SocketAddr> {
        self.peers.iter().chain(self.peers6.iter())
    }
}

#[derive(Debug, Clone)]
pub struct ScrapeResponse {
    pub files: Vec<ScrapeFile>,
}

#[derive(Debug, Clone)]
pub struct ScrapeFile {
    pub info_hash: [u8; 20],
    pub complete: u32,
    pub incomplete: u32,
    pub downloaded: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct CompactPeer {
    pub ip: IpAddr,
    pub port: u16,
}

impl CompactPeer {
    pub fn from_v4_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 6 {
            return None;
        }
        let ip = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
        let port = u16::from_be_bytes([bytes[4], bytes[5]]);
        Some(Self {
            ip: IpAddr::V4(ip),
            port,
        })
    }

    pub fn from_v6_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 18 {
            return None;
        }
        let mut ip_bytes = [0u8; 16];
        ip_bytes.copy_from_slice(&bytes[..16]);
        let ip = Ipv6Addr::from(ip_bytes);
        let port = u16::from_be_bytes([bytes[16], bytes[17]]);
        Some(Self {
            ip: IpAddr::V6(ip),
            port,
        })
    }

    pub fn to_socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.ip, self.port)
    }
}

pub fn parse_compact_peers(data: &[u8]) -> Vec<SocketAddr> {
    data.chunks_exact(6)
        .filter_map(CompactPeer::from_v4_bytes)
        .map(|p| p.to_socket_addr())
        .collect()
}

#[cfg_attr(not(feature = "http-client"), allow(dead_code))]
pub fn parse_compact_peers6(data: &[u8]) -> Vec<SocketAddr> {
    data.chunks_exact(18)
        .filter_map(CompactPeer::from_v6_bytes)
        .map(|p| p.to_socket_addr())
        .collect()
}
