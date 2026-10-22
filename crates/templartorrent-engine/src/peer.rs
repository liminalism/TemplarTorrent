//! One peer actor per connection. The actor owns its bitfield, request pipeline
//! and choke/interest flags. It never touches the network directly: it emits
//! actions the transport drains, and it fences every block against the lease
//! generation so post-cancellation arrivals are dropped, not applied.
use std::collections::VecDeque;

use templartorrent_core::model::TorrentId;

use crate::{event::PeerId, lease::Lease};

/// Default cap on simultaneously requested blocks per peer (reqq bound).
pub const DEFAULT_MAX_IN_FLIGHT: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerMessage {
    Interested,
    NotInterested,
    Choke,
    Unchoke,
    Request { piece: u32, begin: u32, length: u32 },
    Cancel { piece: u32, begin: u32, length: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerAction {
    Send(PeerMessage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingRequest {
    piece: u32,
    begin: u32,
    length: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockDisposition {
    /// Expected block: caller must confirm it after admission.
    Accept,
    /// Already received or never requested: drop silently.
    Duplicate,
    /// Length does not match the pending request: drop and count.
    Invalid,
    /// Predates the current generation: drop, the piece was re-leased.
    Stale,
}

#[derive(Debug, Clone)]
pub struct PeerActor {
    id: PeerId,
    torrent: TorrentId,
    generation: u64,
    bitfield: Vec<u64>,
    pending: VecDeque<PendingRequest>,
    max_in_flight: usize,
    am_interested: bool,
    peer_choking: bool,
}

impl PeerActor {
    pub fn new(id: PeerId, torrent: TorrentId, generation: u64, bitfield_words: usize) -> Self {
        Self {
            id,
            torrent,
            generation,
            bitfield: vec![0; bitfield_words],
            pending: VecDeque::new(),
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            am_interested: false,
            peer_choking: true,
        }
    }

    pub fn id(&self) -> PeerId {
        self.id
    }

    pub fn torrent(&self) -> TorrentId {
        self.torrent
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn bitfield(&self) -> &[u64] {
        &self.bitfield
    }

    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    pub fn is_productive(&self) -> bool {
        self.am_interested && !self.peer_choking
    }

    /// Adopt a fresh bitfield (replaces; the coordinator tracks deltas).
    pub fn set_bitfield(&mut self, bits: &[u64]) {
        if bits.len() == self.bitfield.len() {
            self.bitfield.copy_from_slice(bits);
        }
    }

    pub fn set_have(&mut self, piece: u32, piece_count: usize) {
        let index = piece as usize;
        if index < piece_count && index / 64 < self.bitfield.len() {
            self.bitfield[index / 64] |= 1u64 << (index % 64);
        }
    }

    pub fn on_choke(&mut self) {
        self.peer_choking = true;
    }

    pub fn on_unchoke(&mut self) {
        self.peer_choking = false;
    }

    /// Express interest only on change; returns the wire action when it flips.
    pub fn want(&mut self, peer_has_wanted: bool) -> Option<PeerAction> {
        if peer_has_wanted == self.am_interested {
            return None;
        }
        self.am_interested = peer_has_wanted;
        Some(PeerAction::Send(if peer_has_wanted {
            PeerMessage::Interested
        } else {
            PeerMessage::NotInterested
        }))
    }

    pub fn free_slots(&self) -> usize {
        self.max_in_flight.saturating_sub(self.pending.len())
    }

    /// Bound this peer's request pipeline to `max` blocks, clamped to
    /// `1..=DEFAULT_MAX_IN_FLIGHT`. Requests already in flight stay pending;
    /// the pipeline shrinks as they land. Used for local peer preference.
    pub fn set_max_in_flight(&mut self, max: usize) {
        self.max_in_flight = max.clamp(1, DEFAULT_MAX_IN_FLIGHT);
    }

    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight
    }

    pub fn has_pending(&self, piece: u32) -> bool {
        self.pending.iter().any(|p| p.piece == piece)
    }

    /// Request unrequested lease blocks up to the pipeline cap. Each block is
    /// offered to `admit` (rate budget) before it is recorded; the first
    /// refusal stops the fill for this round.
    pub fn request_blocks(
        &mut self,
        lease: &Lease,
        mut admit: impl FnMut(u32) -> bool,
    ) -> Vec<PeerAction> {
        let mut actions = Vec::new();
        if lease.torrent != self.torrent || lease.generation != self.generation {
            return actions;
        }
        for span in &lease.blocks {
            if self.pending.len() >= self.max_in_flight {
                break;
            }
            let already = self
                .pending
                .iter()
                .any(|p| p.piece == lease.piece && p.begin == span.begin);
            if already {
                continue;
            }
            if !admit(span.length) {
                break;
            }
            self.pending.push_back(PendingRequest {
                piece: lease.piece,
                begin: span.begin,
                length: span.length,
            });
            actions.push(PeerAction::Send(PeerMessage::Request {
                piece: lease.piece,
                begin: span.begin,
                length: span.length,
            }));
        }
        actions
    }

    /// Classify an arrival without mutating pipeline state.
    pub fn check_block(
        &self,
        piece: u32,
        begin: u32,
        length: usize,
        generation: u64,
    ) -> BlockDisposition {
        if generation != self.generation {
            return BlockDisposition::Stale;
        }
        let Ok(length) = u32::try_from(length) else {
            return BlockDisposition::Invalid;
        };
        match self
            .pending
            .iter()
            .find(|p| p.piece == piece && p.begin == begin)
        {
            None => BlockDisposition::Duplicate,
            Some(pending) if pending.length != length => BlockDisposition::Invalid,
            Some(_) => BlockDisposition::Accept,
        }
    }

    /// Confirm an accepted block, freeing one pipeline slot. Returns false if
    /// the request is no longer pending (caller raced a resync).
    pub fn confirm_block(&mut self, piece: u32, begin: u32) -> bool {
        if let Some(index) = self
            .pending
            .iter()
            .position(|p| p.piece == piece && p.begin == begin)
        {
            self.pending.remove(index);
            true
        } else {
            false
        }
    }

    pub fn cancel_piece(&mut self, piece: u32) -> Vec<PeerAction> {
        let mut actions = Vec::new();
        self.pending.retain(|request| {
            if request.piece != piece {
                return true;
            }
            actions.push(PeerAction::Send(PeerMessage::Cancel {
                piece,
                begin: request.begin,
                length: request.length,
            }));
            false
        });
        actions
    }

    pub fn cancel_all(&mut self, generation: u64) -> Vec<PeerAction> {
        self.generation = generation;
        self.pending
            .drain(..)
            .map(|request| {
                PeerAction::Send(PeerMessage::Cancel {
                    piece: request.piece,
                    begin: request.begin,
                    length: request.length,
                })
            })
            .collect()
    }

    /// Drop all pipeline state after a generation change or choke.
    pub fn resync(&mut self, generation: u64) {
        self.generation = generation;
        self.pending.clear();
    }
}
