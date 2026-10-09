use std::cmp::Ordering;

pub const SKIP: u32 = u32::MAX;

/// Torrent wire order, NOT display order. Offsets are derived without reordering files.
#[derive(Clone, Debug)]
pub struct FileSpec {
    pub path: String,
    pub length: u64,
    pub wanted: bool,
}

/// Allocation-free natural comparison. Arbitrarily long digit runs never overflow.
/// ASCII case-insensitive comparison; the original UTF-8 bytes break ties deterministically.
/// This is not a Unicode locale collation implementation.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (aa, bb) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < aa.len() && j < bb.len() {
        if aa[i].is_ascii_digit() && bb[j].is_ascii_digit() {
            let (start_i, start_j) = (i, j);
            while i < aa.len() && aa[i].is_ascii_digit() {
                i += 1;
            }
            while j < bb.len() && bb[j].is_ascii_digit() {
                j += 1;
            }
            let (mut si, mut sj) = (start_i, start_j);
            while si < i && aa[si] == b'0' {
                si += 1;
            }
            while sj < j && bb[sj] == b'0' {
                sj += 1;
            }
            let order = (i - si)
                .cmp(&(j - sj))
                .then_with(|| aa[si..i].cmp(&bb[sj..j]));
            if order != Ordering::Equal {
                return order;
            }
        } else {
            let order = aa[i].to_ascii_lowercase().cmp(&bb[j].to_ascii_lowercase());
            if order != Ordering::Equal {
                return order;
            }
            i += 1;
            j += 1;
        }
    }
    (aa.len() - i).cmp(&(bb.len() - j)).then_with(|| aa.cmp(bb))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceError {
    ZeroPieceLength,
    SizeOverflow,
    TooManyPieces,
    TooManyFiles,
    InvalidPiece,
    InvalidFile,
    InvalidOrder,
    ShapeMismatch,
}
impl std::fmt::Display for SequenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SequenceError {}

#[derive(Debug)]
pub struct Sequence {
    files: Vec<FileSpec>,
    offsets: Vec<u64>,
    total: u64,
    piece_length: u64,
    order: Vec<usize>,
    ranks: Vec<u32>,
    piece_ranks: Vec<u32>,
    verified: Vec<u64>,
    file_verified_bytes: Vec<u64>,
    generation: u64,
}
impl Sequence {
    pub fn new(
        files: Vec<FileSpec>,
        piece_length: u64,
        max_pieces: usize,
    ) -> Result<Self, SequenceError> {
        if piece_length == 0 {
            return Err(SequenceError::ZeroPieceLength);
        }
        if files.len() >= SKIP as usize {
            return Err(SequenceError::TooManyFiles);
        }
        let mut offsets = Vec::with_capacity(files.len());
        let mut total = 0u64;
        for file in &files {
            offsets.push(total);
            total = total
                .checked_add(file.length)
                .ok_or(SequenceError::SizeOverflow)?;
        }
        let count = total.div_ceil(piece_length);
        if count > max_pieces as u64 || count > u32::MAX as u64 {
            return Err(SequenceError::TooManyPieces);
        }
        let count = usize::try_from(count).map_err(|_| SequenceError::TooManyPieces)?;
        let mut order: Vec<_> = (0..files.len()).collect();
        order.sort_by(|&a, &b| natural_cmp(&files[a].path, &files[b].path).then(a.cmp(&b)));
        let mut out = Self {
            ranks: vec![SKIP; files.len()],
            file_verified_bytes: vec![0; files.len()],
            files,
            offsets,
            total,
            piece_length,
            order,
            piece_ranks: vec![SKIP; count],
            verified: vec![0; count.div_ceil(64)],
            generation: 0,
        };
        out.rebuild();
        Ok(out)
    }
    pub fn files(&self) -> &[FileSpec] {
        &self.files
    }
    pub fn order(&self) -> &[usize] {
        &self.order
    }
    pub fn file_ranks(&self) -> &[u32] {
        &self.ranks
    }
    pub fn piece_ranks(&self) -> &[u32] {
        &self.piece_ranks
    }
    pub fn verified_words(&self) -> &[u64] {
        &self.verified
    }
    pub fn file_verified_bytes(&self) -> &[u64] {
        &self.file_verified_bytes
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn total_size(&self) -> u64 {
        self.total
    }
    pub fn piece_count(&self) -> usize {
        self.piece_ranks.len()
    }
    pub fn is_verified(&self, piece: usize) -> bool {
        piece < self.piece_count() && self.verified[piece / 64] & (1u64 << (piece % 64)) != 0
    }
    pub fn set_wanted(&mut self, file: usize, wanted: bool) -> Result<(), SequenceError> {
        self.files
            .get_mut(file)
            .ok_or(SequenceError::InvalidFile)?
            .wanted = wanted;
        self.rebuild();
        Ok(())
    }
    /// A complete permutation; UI rearranges rank, NEVER on-disk torrent offsets.
    pub fn set_order(&mut self, order: Vec<usize>) -> Result<(), SequenceError> {
        if order.len() != self.files.len() {
            return Err(SequenceError::InvalidOrder);
        }
        let mut seen = vec![false; self.files.len()];
        for &i in &order {
            if i >= seen.len() || seen[i] {
                return Err(SequenceError::InvalidOrder);
            }
            seen[i] = true;
        }
        self.order = order;
        self.rebuild();
        Ok(())
    }
    /// Call ONLY after successful hash verification and successful storage write.
    /// Returns whether this changed state; duplicate arrival must not double count.
    pub fn mark_verified(&mut self, piece: usize) -> Result<bool, SequenceError> {
        self.set_verified(piece, true)
    }
    /// Used after failed recheck or external file change. Completion can move backwards.
    pub fn invalidate(&mut self, piece: usize) -> Result<bool, SequenceError> {
        self.set_verified(piece, false)
    }
    fn set_verified(&mut self, piece: usize, have: bool) -> Result<bool, SequenceError> {
        if piece >= self.piece_count() {
            return Err(SequenceError::InvalidPiece);
        }
        if self.is_verified(piece) == have {
            return Ok(false);
        }
        let mask = 1u64 << (piece % 64);
        if have {
            self.verified[piece / 64] |= mask;
        } else {
            self.verified[piece / 64] &= !mask;
        }
        let start = piece as u64 * self.piece_length;
        let end = start.saturating_add(self.piece_length).min(self.total);
        let first = self
            .offsets
            .partition_point(|&x| x <= start)
            .saturating_sub(1);
        // Walk back over zero-length entries sharing a boundary.
        let mut i = first;
        while i > 0 && self.offsets[i] == self.offsets[i - 1] {
            i -= 1;
        }
        let mut completed_changed = false;
        while i < self.files.len() && self.offsets[i] < end {
            let file_end = self.offsets[i] + self.files[i].length;
            let overlap = file_end.min(end).saturating_sub(self.offsets[i].max(start));
            let before = self.file_verified_bytes[i] == self.files[i].length;
            if have {
                self.file_verified_bytes[i] += overlap;
            } else {
                self.file_verified_bytes[i] -= overlap;
            }
            let after = self.file_verified_bytes[i] == self.files[i].length;
            completed_changed |= before != after;
            i += 1;
        }
        if completed_changed {
            self.rebuild();
        }
        Ok(true)
    }
    fn rebuild(&mut self) {
        self.ranks.fill(SKIP);
        self.piece_ranks.fill(SKIP);
        let mut next = 0u32;
        for &i in &self.order {
            if self.files[i].wanted && self.file_verified_bytes[i] < self.files[i].length {
                self.ranks[i] = next;
                next += 1;
            }
        }
        for (i, file) in self.files.iter().enumerate() {
            if self.ranks[i] == SKIP || file.length == 0 {
                continue;
            }
            let first = (self.offsets[i] / self.piece_length) as usize;
            let end = ((self.offsets[i] + file.length - 1) / self.piece_length + 1) as usize;
            for rank in &mut self.piece_ranks[first..end] {
                *rank = (*rank).min(self.ranks[i]);
            }
        }
        self.generation = self.generation.wrapping_add(1);
    }
    /// Reference picker: earliest unfinished file available from THIS peer, then rarity.
    /// `busy` means all remaining blocks already reserved, not merely partially downloaded.
    /// Caller owns atomic block leasing, fair job arbitration and the payload byte budget.
    /// Bitmap convention here is LSB-first u64, unlike BEP-3 wire MSB-first bytes.
    pub fn pick(
        &self,
        peer: &[u64],
        busy: &[u64],
        availability: &[u32],
    ) -> Result<Option<u32>, SequenceError> {
        let words = self.piece_count().div_ceil(64);
        if peer.len() != words || busy.len() != words || availability.len() != self.piece_count() {
            return Err(SequenceError::ShapeMismatch);
        }
        let mut best: Option<(u32, u32, u32)> = None;
        for wi in 0..words {
            let mut eligible = peer[wi] & !self.verified[wi] & !busy[wi];
            while eligible != 0 {
                let bit = eligible.trailing_zeros() as usize;
                eligible &= eligible - 1;
                let index = wi * 64 + bit;
                if index >= self.piece_count() {
                    continue;
                }
                let rank = self.piece_ranks[index];
                if rank == SKIP {
                    continue;
                }
                let key = (rank, availability[index], index as u32);
                if best.is_none_or(|old| key < old) {
                    best = Some(key);
                }
            }
        }
        Ok(best.map(|(_, _, i)| i))
    }
}
