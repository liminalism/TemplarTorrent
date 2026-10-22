use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Instant;

use rand::Rng as _;

use super::error::DhtError;

const MAX_FAILURES: u8 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub [u8; 20]);

impl NodeId {
    pub fn generate() -> Self {
        let mut id = [0u8; 20];
        rand::rng().fill(&mut id);
        Self(id)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DhtError> {
        if bytes.len() != 20 {
            return Err(DhtError::InvalidNodeId);
        }
        let mut id = [0u8; 20];
        id.copy_from_slice(bytes);
        Ok(Self(id))
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn distance(&self, other: &NodeId) -> [u8; 20] {
        let mut dist = [0u8; 20];
        for (i, d) in dist.iter_mut().enumerate() {
            *d = self.0[i] ^ other.0[i];
        }
        dist
    }

    pub fn bucket_index(&self, other: &NodeId) -> usize {
        let dist = self.distance(other);

        for (i, &byte) in dist.iter().enumerate() {
            if byte != 0 {
                let leading = byte.leading_zeros() as usize;
                return i * 8 + leading;
            }
        }

        159
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({:02x}{:02x}..)", self.0[0], self.0[1])
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: NodeId,
    pub addr: SocketAddr,
    pub last_seen: Instant,
    pub failures: u8,
}

impl Node {
    pub fn new(id: NodeId, addr: SocketAddr) -> Self {
        Self {
            id,
            addr,
            last_seen: Instant::now(),
            failures: 0,
        }
    }

    pub fn touch(&mut self) {
        self.last_seen = Instant::now();
        self.failures = 0;
    }

    pub fn fail(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    pub fn is_good(&self) -> bool {
        self.failures == 0 && self.last_seen.elapsed().as_secs() < 15 * 60
    }

    pub fn is_bad(&self) -> bool {
        self.failures >= MAX_FAILURES
    }

    pub fn from_compact(data: &[u8]) -> Option<Self> {
        if data.len() != 26 {
            return None;
        }

        let id = NodeId::from_bytes(&data[..20]).ok()?;
        let ip = Ipv4Addr::new(data[20], data[21], data[22], data[23]);
        let port = u16::from_be_bytes([data[24], data[25]]);

        Some(Self::new(id, SocketAddr::new(IpAddr::V4(ip), port)))
    }

    pub fn to_compact(&self) -> Option<[u8; 26]> {
        let mut compact = [0u8; 26];
        compact[..20].copy_from_slice(&self.id.0);

        match self.addr {
            SocketAddr::V4(v4) => {
                compact[20..24].copy_from_slice(&v4.ip().octets());
                compact[24..26].copy_from_slice(&v4.port().to_be_bytes());
                Some(compact)
            }
            SocketAddr::V6(_) => None,
        }
    }
}
