use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use rand::Rng as _;

const UNCHOKE_INTERVAL: Duration = Duration::from_secs(10);
const OPTIMISTIC_UNCHOKE_INTERVAL: Duration = Duration::from_secs(30);
const MAX_UNCHOKED: usize = 4;

/// The result of a choking decision for a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChokingDecision {
    /// Choke the peer (stop sending data).
    Choke,
    /// Unchoke the peer (allow data transfer).
    Unchoke,
    /// No change to the current choking state.
    NoChange,
}

impl ChokingDecision {
    /// Returns true if this decision changes the peer's state.
    pub fn is_change(&self) -> bool {
        !matches!(self, ChokingDecision::NoChange)
    }

    /// Returns true if this is an unchoke decision.
    pub fn is_unchoke(&self) -> bool {
        matches!(self, ChokingDecision::Unchoke)
    }

    /// Returns true if this is a choke decision.
    pub fn is_choke(&self) -> bool {
        matches!(self, ChokingDecision::Choke)
    }
}

#[derive(Debug, Clone, Default)]
pub struct PeerStats {
    pub downloaded: u64,
    pub uploaded: u64,
    pub download_rate: f64,
    pub upload_rate: f64,
    pub interested: bool,
    pub choking_us: bool,
    pub we_interested: bool,
    pub we_choking: bool,
    pub last_active: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChokingState {
    pub am_choking: bool,
    pub am_interested: bool,
    pub peer_choking: bool,
    pub peer_interested: bool,
}

impl Default for ChokingState {
    fn default() -> Self {
        Self {
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
        }
    }
}

pub struct ChokingAlgorithm {
    peers: HashMap<SocketAddr, PeerStats>,
    last_unchoke: Instant,
    last_optimistic: Instant,
    optimistic_peer: Option<SocketAddr>,
}

impl ChokingAlgorithm {
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
            last_unchoke: Instant::now(),
            last_optimistic: Instant::now(),
            optimistic_peer: None,
        }
    }

    pub fn add_peer(&mut self, addr: SocketAddr) {
        self.peers.insert(addr, PeerStats::default());
    }

    pub fn remove_peer(&mut self, addr: &SocketAddr) {
        self.peers.remove(addr);
        if self.optimistic_peer.as_ref() == Some(addr) {
            self.optimistic_peer = None;
        }
    }

    pub fn update_stats(&mut self, addr: &SocketAddr, stats: PeerStats) {
        if let Some(peer) = self.peers.get_mut(addr) {
            *peer = stats;
        }
    }

    pub fn compute_unchoke_decisions(&mut self, is_seed: bool) -> Vec<(SocketAddr, bool)> {
        let now = Instant::now();
        let mut decisions = Vec::new();

        if now.duration_since(self.last_unchoke) < UNCHOKE_INTERVAL {
            return decisions;
        }
        self.last_unchoke = now;

        let mut candidates: Vec<_> = self
            .peers
            .iter()
            .filter(|(_, stats)| stats.interested)
            .collect();

        if is_seed {
            candidates.sort_by(|(_, a), (_, b)| {
                b.upload_rate
                    .partial_cmp(&a.upload_rate)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        } else {
            candidates.sort_by(|(_, a), (_, b)| {
                b.download_rate
                    .partial_cmp(&a.download_rate)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }

        let mut unchoked_count = 0;
        let mut to_unchoke: Vec<SocketAddr> = Vec::new();

        for (addr, _) in candidates.iter().take(MAX_UNCHOKED - 1) {
            to_unchoke.push(**addr);
            unchoked_count += 1;
        }

        if now.duration_since(self.last_optimistic) >= OPTIMISTIC_UNCHOKE_INTERVAL {
            self.last_optimistic = now;

            let choked_interested: Vec<_> = self
                .peers
                .iter()
                .filter(|(addr, stats)| stats.interested && !to_unchoke.contains(addr))
                .map(|(addr, _)| *addr)
                .collect();

            if !choked_interested.is_empty() {
                let idx = rand::rng().random_range(0..choked_interested.len());
                self.optimistic_peer = Some(choked_interested[idx]);
            }
        }

        if let Some(opt) = self.optimistic_peer {
            if !to_unchoke.contains(&opt) && unchoked_count < MAX_UNCHOKED {
                to_unchoke.push(opt);
            }
        }

        for (addr, stats) in &self.peers {
            let should_unchoke = to_unchoke.contains(addr);
            if should_unchoke == stats.we_choking {
                decisions.push((*addr, should_unchoke));
            }
        }

        decisions
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }
}

impl Default for ChokingAlgorithm {
    fn default() -> Self {
        Self::new()
    }
}
