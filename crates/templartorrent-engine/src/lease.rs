//! One lease coordinator per torrent. It serializes piece decisions through the
//! dependency-free reference picker, tracks busy pieces and per-piece peer
//! availability, and fences every lease against the sequence generation so a
//! wanted-set or order change cannot apply stale work.
use std::collections::HashMap;

use templartorrent_core::{
    model::TorrentId,
    picker::PieceIndex,
    sequence::{Sequence, SequenceError},
};

use crate::{EngineError, event::PeerId};

/// BEP-3 block size cap. Every requested block is at most this large.
pub const MAX_BLOCK_BYTES: usize = 16384;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSpan {
    pub begin: u32,
    pub length: u32,
}

#[derive(Debug, Clone)]
pub struct Lease {
    pub torrent: TorrentId,
    pub piece: u32,
    pub blocks: Vec<BlockSpan>,
    pub generation: u64,
    pub issued_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseOutcome {
    /// Lease applied; payload says whether verification state changed.
    Applied(bool),
    /// Lease predates the current policy generation; newer work stays intact.
    Stale,
}

#[derive(Debug)]
pub struct LeaseCoordinator {
    torrent: TorrentId,
    sequence: Sequence,
    piece_length: u64,
    total: u64,
    busy: Vec<u64>,
    availability: Vec<u32>,
    peers: HashMap<PeerId, Vec<u64>>,
    outstanding: HashMap<u32, (PeerId, Lease)>,
    generation: u64,
    /// Bucketed pickable pieces; rebuilt lazily when the sequence ranks change.
    index: PieceIndex,
}

impl LeaseCoordinator {
    pub fn new(torrent: TorrentId, sequence: Sequence, piece_length: u64, total: u64) -> Self {
        let words = sequence.piece_count().div_ceil(64);
        let pieces = sequence.piece_count();
        let generation = sequence.generation();
        let busy = vec![0; words];
        let availability = vec![0; pieces];
        let index = PieceIndex::new(&sequence, &busy, &availability, torrent.0);
        Self {
            torrent,
            sequence,
            piece_length,
            total,
            busy,
            availability,
            peers: HashMap::new(),
            outstanding: HashMap::new(),
            generation,
            index,
        }
    }

    /// Ranks may have changed through `sequence_mut` (selection, order, file
    /// completion); the index follows the sequence generation lazily.
    fn sync_index(&mut self) {
        if self.index.generation() != self.sequence.generation() {
            self.index
                .rebuild(&self.sequence, &self.busy, &self.availability);
        }
    }

    fn index_pickable(&mut self, piece: usize) {
        if piece >= self.sequence.piece_count() {
            return;
        }
        let pickable = !self.sequence.is_verified(piece)
            && self.busy[piece / 64] & (1u64 << (piece % 64)) == 0;
        let rank = self.sequence.piece_ranks()[piece];
        self.index
            .set_pickable(piece, pickable, rank, self.availability[piece]);
    }

    /// Verification state of a piece was withdrawn (failed recheck, changed
    /// file); the piece is pickable again unless leased.
    pub fn invalidate(&mut self, piece: u32) -> Result<bool, EngineError> {
        let index = usize::try_from(piece)
            .map_err(|_| EngineError::Sequence(SequenceError::InvalidPiece))?;
        let changed = self.sequence.invalidate(index)?;
        self.sync_index();
        self.index_pickable(index);
        Ok(changed)
    }

    pub fn sequence(&self) -> &Sequence {
        &self.sequence
    }

    pub fn sequence_mut(&mut self) -> &mut Sequence {
        &mut self.sequence
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Policy changes fence leases; ordinary file promotion does not invalidate
    /// useful work already leased to another peer.
    pub fn cancel_all(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.outstanding.clear();
        self.busy.fill(0);
        self.index
            .rebuild(&self.sequence, &self.busy, &self.availability);
    }

    /// Remove a received span so pipeline refill never requests it again.
    pub fn confirm_block(
        &mut self,
        peer: PeerId,
        piece: u32,
        begin: u32,
        length: usize,
        now_ns: u64,
    ) -> bool {
        let Some((holder, lease)) = self.outstanding.get_mut(&piece) else {
            return false;
        };
        if *holder != peer || lease.generation != self.generation {
            return false;
        }
        let Some(index) = lease
            .blocks
            .iter()
            .position(|span| span.begin == begin && span.length as usize == length)
        else {
            return false;
        };
        lease.blocks.remove(index);
        lease.issued_ns = now_ns;
        true
    }

    /// Register a peer with an empty bitfield. Returns false if already known.
    pub fn add_peer(&mut self, peer: PeerId) -> bool {
        if self.peers.contains_key(&peer) {
            return false;
        }
        self.peers.insert(peer, vec![0; self.busy.len()]);
        true
    }

    /// Forget a peer: subtract its availability and release its leases.
    pub fn remove_peer(&mut self, peer: PeerId) {
        if let Some(bits) = self.peers.remove(&peer) {
            self.subtract_availability(&bits);
        }
        let mine: Vec<u32> = self
            .outstanding
            .iter()
            .filter_map(|(&piece, (holder, _))| (*holder == peer).then_some(piece))
            .collect();
        for piece in mine {
            self.release(piece);
        }
    }

    pub fn note_bitfield(&mut self, peer: PeerId, bits: &[u64]) -> Result<(), EngineError> {
        if bits.len() != self.busy.len() {
            return Err(EngineError::Sequence(SequenceError::ShapeMismatch));
        }
        if !self.peers.contains_key(&peer) {
            return Err(EngineError::UnknownPeer);
        }
        self.sync_index();
        let known = self.peers.get_mut(&peer).expect("checked above");
        for (i, word) in bits.iter().enumerate() {
            let added = word & !known[i];
            let mut added_bits = added;
            while added_bits != 0 {
                let bit = added_bits.trailing_zeros() as usize;
                added_bits &= added_bits - 1;
                let index = i * 64 + bit;
                if let Some(count) = self.availability.get_mut(index) {
                    *count = count.saturating_add(1);
                    self.index.set_availability(index, *count);
                }
            }
            known[i] |= word;
        }
        Ok(())
    }

    pub fn note_have(&mut self, peer: PeerId, piece: u32) -> Result<(), EngineError> {
        let index = usize::try_from(piece)
            .map_err(|_| EngineError::Sequence(SequenceError::InvalidPiece))?;
        if index >= self.sequence.piece_count() {
            return Err(EngineError::Sequence(SequenceError::InvalidPiece));
        }
        if !self.peers.contains_key(&peer) {
            return Err(EngineError::UnknownPeer);
        }
        self.sync_index();
        let known = self.peers.get_mut(&peer).expect("checked above");
        let mask = 1u64 << (index % 64);
        if known[index / 64] & mask == 0 {
            known[index / 64] |= mask;
            self.availability[index] = self.availability[index].saturating_add(1);
            self.index.set_availability(index, self.availability[index]);
        }
        Ok(())
    }

    /// Pick one piece for a peer and lease it. Returns None when nothing is
    /// currently eligible from that peer.
    pub fn pick_for(&mut self, peer: PeerId, now_ns: u64) -> Result<Option<Lease>, EngineError> {
        if !self.peers.contains_key(&peer) {
            return Err(EngineError::UnknownPeer);
        }
        self.sync_index();
        let bits = &self.peers[&peer];
        if bits.len() != self.busy.len() {
            return Err(EngineError::Sequence(SequenceError::ShapeMismatch));
        }
        let picked = self.index.pick(
            bits,
            &self.availability,
            self.sequence.piece_ranks(),
            self.sequence.verified_words(),
            &self.busy,
        );
        let Some(piece) = picked else {
            return Ok(None);
        };
        let piece_usize = piece as usize;
        self.busy[piece_usize / 64] |= 1u64 << (piece_usize % 64);
        self.index_pickable(piece_usize);
        let lease = Lease {
            torrent: self.torrent,
            piece,
            blocks: self.split_blocks(piece)?,
            generation: self.generation,
            issued_ns: now_ns,
        };
        self.outstanding.insert(piece, (peer, lease.clone()));
        Ok(Some(lease))
    }

    /// Apply a finished lease: stale generations release the piece without
    /// touching verification state.
    pub fn complete_lease(&mut self, lease: &Lease) -> Result<LeaseOutcome, EngineError> {
        if lease.torrent != self.torrent {
            return Err(EngineError::UnknownJob);
        }
        if lease.generation != self.generation {
            return Ok(LeaseOutcome::Stale);
        }
        self.release(lease.piece);
        let changed = self.sequence.mark_verified(lease.piece as usize)?;
        self.sync_index();
        self.index_pickable(lease.piece as usize);
        Ok(LeaseOutcome::Applied(changed))
    }

    /// Release leases older than `timeout_ns` so a dead peer cannot wedge a piece.
    pub fn expire(&mut self, now_ns: u64, timeout_ns: u64) -> Vec<(PeerId, u32)> {
        let stale: Vec<(PeerId, u32)> = self
            .outstanding
            .iter()
            .filter_map(|(&piece, (peer, lease))| {
                (now_ns.saturating_sub(lease.issued_ns) >= timeout_ns).then_some((*peer, piece))
            })
            .collect();
        for (_, piece) in &stale {
            self.release(*piece);
        }
        stale
    }

    /// The lease currently held by a peer, if any, so the session can re-offer
    /// spans that did not fit the pipeline cap last round.
    pub fn lease_of(&self, peer: PeerId) -> Option<&Lease> {
        self.outstanding
            .values()
            .find_map(|(holder, lease)| (*holder == peer).then_some(lease))
    }

    /// Every lease this peer holds, in piece order, so the session can
    /// re-offer unrequested spans of all of them before leasing more.
    pub fn leases_of(&self, peer: PeerId) -> Vec<Lease> {
        let mut leases: Vec<Lease> = self
            .outstanding
            .values()
            .filter(|(holder, _)| *holder == peer)
            .map(|(_, lease)| lease.clone())
            .collect();
        leases.sort_by_key(|lease| lease.piece);
        leases
    }

    /// Release a piece back to the pickable pool without verifying it.
    pub fn release_piece(&mut self, piece: u32) {
        self.release(piece);
    }

    pub fn outstanding_len(&self) -> usize {
        self.outstanding.len()
    }

    /// Expected on-disk length of one piece (short last piece handled).
    pub fn piece_len(&self, piece: u32) -> Option<usize> {
        let index = usize::try_from(piece).ok()?;
        if index >= self.sequence.piece_count() {
            return None;
        }
        let start = u64::from(piece).checked_mul(self.piece_length)?;
        let end = start.saturating_add(self.piece_length).min(self.total);
        usize::try_from(end.saturating_sub(start)).ok()
    }

    /// True when the piece is leased or busy. Recheck skips such pieces so a
    /// resume scan never marks work that is still flying on the wire.
    pub fn is_held(&self, piece: u32) -> bool {
        if self.outstanding.contains_key(&piece) {
            return true;
        }
        let index = piece as usize;
        index < self.sequence.piece_count() && self.busy[index / 64] & (1u64 << (index % 64)) != 0
    }

    /// Adopt one hash-verified resume piece. Call only after the bytes were
    /// read from storage and accepted by `PieceVerifier`; wrong-length or
    /// rejected bytes must never reach here.
    pub fn adopt_verified(&mut self, piece: u32) -> Result<bool, EngineError> {
        let index = usize::try_from(piece)
            .map_err(|_| EngineError::Sequence(SequenceError::InvalidPiece))?;
        let changed = self.sequence.mark_verified(index)?;
        self.sync_index();
        self.index_pickable(index);
        Ok(changed)
    }

    /// Pickable pieces the index currently holds, for diagnostics and tests.
    pub fn pickable_len(&self) -> usize {
        self.index.len()
    }

    fn release(&mut self, piece: u32) {
        let index = piece as usize;
        if index < self.sequence.piece_count() {
            self.busy[index / 64] &= !(1u64 << (index % 64));
            self.sync_index();
            self.index_pickable(index);
        }
        self.outstanding.remove(&piece);
    }

    fn split_blocks(&self, piece: u32) -> Result<Vec<BlockSpan>, EngineError> {
        let start = u64::from(piece)
            .checked_mul(self.piece_length)
            .ok_or(EngineError::LimitOverflow)?;
        let end = start.saturating_add(self.piece_length).min(self.total);
        let mut blocks = Vec::new();
        let mut offset = start;
        while offset < end {
            let length = (end - offset).min(MAX_BLOCK_BYTES as u64);
            let begin = u32::try_from(offset - start).map_err(|_| EngineError::LimitOverflow)?;
            let length = u32::try_from(length).map_err(|_| EngineError::LimitOverflow)?;
            blocks.push(BlockSpan { begin, length });
            offset += u64::from(length);
        }
        Ok(blocks)
    }

    fn subtract_availability(&mut self, bits: &[u64]) {
        self.sync_index();
        for (i, word) in bits.iter().enumerate() {
            let mut remaining = *word;
            while remaining != 0 {
                let bit = remaining.trailing_zeros() as usize;
                remaining &= remaining - 1;
                let index = i * 64 + bit;
                if let Some(count) = self.availability.get_mut(index) {
                    *count = count.saturating_sub(1);
                    self.index.set_availability(index, *count);
                }
            }
        }
    }
}
