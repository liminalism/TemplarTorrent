//! Incremental piece index for the live picker.
//!
//! [`crate::sequence::Sequence::pick`] scans every piece per call. This index
//! keeps each pickable piece (ranked, unverified, not busy) in a bucket keyed
//! by `(file rank, availability)`, so a pick walks buckets from the rarest
//! piece of the earliest unfinished file and stops at the first piece the
//! peer has. Maintenance is O(1) per changed piece and O(P) per rank rebuild.
//!
//! Contract against the oracle: for the same state, the picked piece has the
//! same `(rank, availability)` key as the oracle's pick, or both return
//! `None`. The tie-break inside a bucket is a seeded start offset, not the
//! lowest index, which DESIGN.md allows for the live engine.

use crate::sequence::{SKIP, Sequence};

const NONE: u32 = u32::MAX;
/// Availability values at or above this limit share the last bucket. More
/// than 1023 peers holding one piece is outside any configured peer cap.
pub const AVAILABILITY_BUCKETS: usize = 1024;
/// A peer holding fewer than one eighth of the pickable pieces is scanned by
/// its own bits instead of by bucket order.
const SPARSE_DIVISOR: usize = 8;

#[derive(Debug, Clone)]
pub struct PieceIndex {
    /// `buckets[rank][availability]` holds piece indices in arbitrary order.
    buckets: Vec<Vec<Vec<u32>>>,
    /// Position of a member piece inside its bucket, `NONE` otherwise.
    slot: Vec<u32>,
    /// Rank of a member piece, `SKIP` for a non-member.
    rank_of: Vec<u32>,
    /// Clamped availability bucket of a member piece.
    avail_of: Vec<u32>,
    members: usize,
    generation: u64,
    salt: u64,
}

impl PieceIndex {
    pub fn new(sequence: &Sequence, busy: &[u64], availability: &[u32], salt: u64) -> Self {
        let count = sequence.piece_count();
        let mut index = Self {
            buckets: Vec::new(),
            slot: vec![NONE; count],
            rank_of: vec![SKIP; count],
            avail_of: vec![0; count],
            members: 0,
            generation: sequence.generation(),
            salt,
        };
        index.rebuild(sequence, busy, availability);
        index
    }

    /// Sequence generation the ranks were last taken from.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.members
    }

    pub fn is_empty(&self) -> bool {
        self.members == 0
    }

    pub fn contains(&self, piece: usize) -> bool {
        self.slot.get(piece).is_some_and(|slot| *slot != NONE)
    }

    /// Recompute every bucket from the current ranks, verified bits, busy
    /// bits and availability. O(P); call after a rank change.
    pub fn rebuild(&mut self, sequence: &Sequence, busy: &[u64], availability: &[u32]) {
        for by_avail in &mut self.buckets {
            for bucket in by_avail {
                bucket.clear();
            }
        }
        self.slot.fill(NONE);
        self.rank_of.fill(SKIP);
        self.members = 0;
        let verified = sequence.verified_words();
        for (piece, &rank) in sequence.piece_ranks().iter().enumerate() {
            if rank == SKIP {
                continue;
            }
            let word = piece / 64;
            let mask = 1u64 << (piece % 64);
            let held = verified.get(word).is_some_and(|w| w & mask != 0)
                || busy.get(word).is_some_and(|w| w & mask != 0);
            if held {
                continue;
            }
            self.insert(piece, rank, availability.get(piece).copied().unwrap_or(0));
        }
        self.generation = sequence.generation();
    }

    /// Availability of one piece changed; a member moves bucket.
    pub fn set_availability(&mut self, piece: usize, availability: u32) {
        if !self.contains(piece) {
            return;
        }
        let clamped = clamp(availability);
        if self.avail_of[piece] == clamped {
            return;
        }
        let rank = self.rank_of[piece];
        self.remove(piece);
        self.insert(piece, rank, availability);
    }

    /// A piece became pickable (released or invalidated) or stopped being so
    /// (leased or verified). `rank` and `availability` describe it now.
    pub fn set_pickable(&mut self, piece: usize, pickable: bool, rank: u32, availability: u32) {
        if piece >= self.slot.len() {
            return;
        }
        if pickable && rank != SKIP {
            if !self.contains(piece) {
                self.insert(piece, rank, availability);
            }
        } else {
            self.remove(piece);
        }
    }

    fn insert(&mut self, piece: usize, rank: u32, availability: u32) {
        let clamped = clamp(availability);
        let rank_index = rank as usize;
        if self.buckets.len() <= rank_index {
            self.buckets.resize_with(rank_index + 1, Vec::new);
        }
        let by_avail = &mut self.buckets[rank_index];
        if by_avail.len() <= clamped as usize {
            by_avail.resize_with(clamped as usize + 1, Vec::new);
        }
        let bucket = &mut by_avail[clamped as usize];
        self.slot[piece] = bucket.len() as u32;
        bucket.push(piece as u32);
        self.rank_of[piece] = rank;
        self.avail_of[piece] = clamped;
        self.members += 1;
    }

    fn remove(&mut self, piece: usize) {
        let slot = self.slot[piece];
        if slot == NONE {
            return;
        }
        let bucket = &mut self.buckets[self.rank_of[piece] as usize][self.avail_of[piece] as usize];
        let last = bucket.len() - 1;
        if slot as usize != last {
            let moved = bucket[last];
            bucket.swap(slot as usize, last);
            self.slot[moved as usize] = slot;
        }
        bucket.pop();
        self.slot[piece] = NONE;
        self.rank_of[piece] = SKIP;
        self.members -= 1;
    }

    /// Lowest `(rank, availability)` piece the peer has. `peer`, `verified`
    /// and `busy` are LSB-first words of equal length; `availability` and
    /// `piece_ranks` are per piece. The index must be current for the ranks
    /// (see [`PieceIndex::generation`]).
    pub fn pick(
        &self,
        peer: &[u64],
        availability: &[u32],
        piece_ranks: &[u32],
        verified: &[u64],
        busy: &[u64],
    ) -> Option<u32> {
        let peer_count: usize = peer.iter().map(|w| w.count_ones() as usize).sum();
        if peer_count == 0 || self.members == 0 {
            return None;
        }
        if peer_count * SPARSE_DIVISOR < self.members {
            return pick_sparse(peer, availability, piece_ranks, verified, busy);
        }
        for by_avail in &self.buckets {
            for bucket in by_avail {
                let n = bucket.len();
                if n == 0 {
                    continue;
                }
                let start = (self.salt as usize) % n;
                for k in 0..n {
                    let piece = bucket[(start + k) % n] as usize;
                    if peer[piece / 64] & (1u64 << (piece % 64)) != 0 {
                        return Some(piece as u32);
                    }
                }
            }
        }
        None
    }
}

fn clamp(availability: u32) -> u32 {
    availability.min(AVAILABILITY_BUCKETS as u32 - 1)
}

/// Scan only the peer's own pieces: the reference key order restricted to
/// what this peer can supply.
fn pick_sparse(
    peer: &[u64],
    availability: &[u32],
    piece_ranks: &[u32],
    verified: &[u64],
    busy: &[u64],
) -> Option<u32> {
    let mut best: Option<(u32, u32, u32)> = None;
    for (wi, word) in peer.iter().enumerate() {
        let mut eligible =
            *word & !verified.get(wi).copied().unwrap_or(0) & !busy.get(wi).copied().unwrap_or(0);
        while eligible != 0 {
            let bit = eligible.trailing_zeros() as usize;
            eligible &= eligible - 1;
            let index = wi * 64 + bit;
            let Some(&rank) = piece_ranks.get(index) else {
                continue;
            };
            if rank == SKIP {
                continue;
            }
            let key = (rank, clamp(availability[index]), index as u32);
            if best.is_none_or(|old| key < old) {
                best = Some(key);
            }
        }
    }
    best.map(|(_, _, index)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequence::FileSpec;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    struct State {
        sequence: Sequence,
        busy: Vec<u64>,
        availability: Vec<u32>,
    }

    fn random_state(rng: &mut Rng) -> State {
        let files = 1 + rng.below(6) as usize;
        let piece_length = 1 << (10 + rng.below(4));
        let specs: Vec<FileSpec> = (0..files)
            .map(|i| FileSpec {
                path: format!("Part {} {i}", rng.below(40)),
                length: if rng.below(7) == 0 {
                    0
                } else {
                    rng.below(piece_length * 9) + 1
                },
                wanted: rng.below(4) != 0,
            })
            .collect();
        let mut sequence = Sequence::new(specs, piece_length, 4096).unwrap();
        let count = sequence.piece_count();
        for piece in 0..count {
            if rng.below(3) == 0 {
                sequence.mark_verified(piece).unwrap();
            }
        }
        let words = count.div_ceil(64);
        let mut busy = vec![0u64; words];
        for piece in 0..count {
            if rng.below(5) == 0 {
                busy[piece / 64] |= 1 << (piece % 64);
            }
        }
        let availability = (0..count).map(|_| rng.below(6) as u32).collect();
        State {
            sequence,
            busy,
            availability,
        }
    }

    fn random_peer(rng: &mut Rng, count: usize) -> Vec<u64> {
        let words = count.div_ceil(64);
        let density = rng.below(100);
        let mut peer = vec![0u64; words];
        for piece in 0..count {
            if rng.below(100) < density {
                peer[piece / 64] |= 1 << (piece % 64);
            }
        }
        peer
    }

    fn key(state: &State, piece: u32) -> (u32, u32) {
        (
            state.sequence.piece_ranks()[piece as usize],
            clamp(state.availability[piece as usize]),
        )
    }

    fn eligible(state: &State, peer: &[u64], piece: u32) -> bool {
        let p = piece as usize;
        let mask = 1u64 << (p % 64);
        peer[p / 64] & mask != 0
            && !state.sequence.is_verified(p)
            && state.busy[p / 64] & mask == 0
            && state.sequence.piece_ranks()[p] != SKIP
    }

    fn check_against_oracle(state: &State, index: &PieceIndex, peer: &[u64]) {
        let oracle = state
            .sequence
            .pick(peer, &state.busy, &state.availability)
            .unwrap();
        let live = index.pick(
            peer,
            &state.availability,
            state.sequence.piece_ranks(),
            state.sequence.verified_words(),
            &state.busy,
        );
        match (oracle, live) {
            (None, None) => {}
            (Some(expected), Some(got)) => {
                assert_eq!(key(state, expected), key(state, got));
                assert!(eligible(state, peer, got));
            }
            other => panic!("oracle and index disagree: {other:?}"),
        }
    }

    #[test]
    fn rebuilt_index_matches_oracle_key_for_random_states_and_peers() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..400 {
            let state = random_state(&mut rng);
            let index = PieceIndex::new(
                &state.sequence,
                &state.busy,
                &state.availability,
                rng.next(),
            );
            for _ in 0..8 {
                let peer = random_peer(&mut rng, state.sequence.piece_count());
                check_against_oracle(&state, &index, &peer);
            }
            let everything = vec![u64::MAX; state.sequence.piece_count().div_ceil(64)];
            check_against_oracle(&state, &index, &everything);
        }
    }

    #[test]
    fn incremental_updates_stay_equal_to_a_fresh_rebuild() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..200 {
            let mut state = random_state(&mut rng);
            let salt = rng.next();
            let mut index =
                PieceIndex::new(&state.sequence, &state.busy, &state.availability, salt);
            let count = state.sequence.piece_count();
            if count == 0 {
                continue;
            }
            for _ in 0..60 {
                let piece = rng.below(count as u64) as usize;
                let mask = 1u64 << (piece % 64);
                match rng.below(4) {
                    0 => {
                        state.availability[piece] = rng.below(6) as u32;
                        index.set_availability(piece, state.availability[piece]);
                    }
                    1 => {
                        // Lease or release.
                        state.busy[piece / 64] ^= mask;
                        let pickable = state.busy[piece / 64] & mask == 0
                            && !state.sequence.is_verified(piece);
                        index.set_pickable(
                            piece,
                            pickable,
                            state.sequence.piece_ranks()[piece],
                            state.availability[piece],
                        );
                    }
                    2 => {
                        // Verify or invalidate; a rank change may follow.
                        let before = state.sequence.generation();
                        if state.sequence.is_verified(piece) {
                            state.sequence.invalidate(piece).unwrap();
                        } else {
                            state.sequence.mark_verified(piece).unwrap();
                        }
                        if state.sequence.generation() != before {
                            index.rebuild(&state.sequence, &state.busy, &state.availability);
                        } else {
                            let pickable = !state.sequence.is_verified(piece)
                                && state.busy[piece / 64] & mask == 0;
                            index.set_pickable(
                                piece,
                                pickable,
                                state.sequence.piece_ranks()[piece],
                                state.availability[piece],
                            );
                        }
                    }
                    _ => {
                        let file = rng.below(state.sequence.files().len() as u64) as usize;
                        let wanted = !state.sequence.files()[file].wanted;
                        state.sequence.set_wanted(file, wanted).unwrap();
                        index.rebuild(&state.sequence, &state.busy, &state.availability);
                    }
                }
                let fresh =
                    PieceIndex::new(&state.sequence, &state.busy, &state.availability, salt);
                assert_eq!(fresh.len(), index.len());
                for p in 0..count {
                    assert_eq!(fresh.contains(p), index.contains(p), "piece {p}");
                    if fresh.contains(p) {
                        assert_eq!(fresh.rank_of[p], index.rank_of[p]);
                        assert_eq!(fresh.avail_of[p], index.avail_of[p]);
                    }
                }
                let peer = random_peer(&mut rng, count);
                check_against_oracle(&state, &index, &peer);
            }
        }
    }

    #[test]
    fn availability_beyond_the_bucket_limit_is_clamped_not_dropped() {
        let sequence = Sequence::new(
            vec![FileSpec {
                path: "x".into(),
                length: 64 * 1024,
                wanted: true,
            }],
            1024,
            64,
        )
        .unwrap();
        let availability = vec![u32::MAX; 64];
        let busy = vec![0u64];
        let index = PieceIndex::new(&sequence, &busy, &availability, 7);
        assert_eq!(index.len(), 64);
        let picked = index
            .pick(
                &[u64::MAX],
                &availability,
                sequence.piece_ranks(),
                sequence.verified_words(),
                &busy,
            )
            .unwrap();
        assert!(picked < 64);
    }
}
