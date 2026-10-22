use bytes::Bytes;

/// A bitfield representing which pieces a peer has.
///
/// Each bit represents whether a piece is available (1) or not (0).
/// Bits are numbered from the high bit of the first byte.
#[derive(Debug, Clone)]
pub struct Bitfield {
    bits: Vec<u8>,
    piece_count: usize,
}

impl Bitfield {
    /// Creates a new empty bitfield for the given number of pieces.
    pub fn new(piece_count: usize) -> Self {
        let byte_count = piece_count.div_ceil(8);
        Self {
            bits: vec![0; byte_count],
            piece_count,
        }
    }

    /// Creates a bitfield from raw bytes.
    pub fn from_bytes(bytes: Bytes, piece_count: usize) -> Self {
        let mut bits = bytes.to_vec();
        let expected_bytes = piece_count.div_ceil(8);

        if bits.len() < expected_bytes {
            bits.resize(expected_bytes, 0);
        }

        let mut bf = Self { bits, piece_count };
        bf.clear_spare_bits();
        bf
    }

    /// Creates a full bitfield (all pieces available).
    pub fn full(piece_count: usize) -> Self {
        let byte_count = piece_count.div_ceil(8);
        let mut bf = Self {
            bits: vec![0xFF; byte_count],
            piece_count,
        };
        bf.clear_spare_bits();
        bf
    }

    /// Returns true if the piece at the given index is available.
    pub fn has_piece(&self, index: usize) -> bool {
        if index >= self.piece_count {
            return false;
        }
        let byte_index = index / 8;
        let bit_index = 7 - (index % 8);
        (self.bits[byte_index] >> bit_index) & 1 == 1
    }

    /// Alias for `has_piece` for compatibility.
    pub fn has(&self, index: usize) -> bool {
        self.has_piece(index)
    }

    /// Sets the bit for the piece at the given index.
    pub fn set_piece(&mut self, index: usize) {
        if index >= self.piece_count {
            return;
        }
        let byte_index = index / 8;
        let bit_index = 7 - (index % 8);
        self.bits[byte_index] |= 1 << bit_index;
    }

    /// Alias for `set_piece` for compatibility.
    pub fn set(&mut self, index: usize) {
        self.set_piece(index)
    }

    /// Clears the bit for the piece at the given index.
    pub fn clear_piece(&mut self, index: usize) {
        if index >= self.piece_count {
            return;
        }
        let byte_index = index / 8;
        let bit_index = 7 - (index % 8);
        self.bits[byte_index] &= !(1 << bit_index);
    }

    /// Alias for `clear_piece` for compatibility.
    pub fn clear(&mut self, index: usize) {
        self.clear_piece(index)
    }

    /// Returns the number of pieces that are available.
    pub fn count(&self) -> usize {
        self.bits.iter().map(|b| b.count_ones() as usize).sum()
    }

    /// Alias for `count` for compatibility.
    pub fn count_ones(&self) -> usize {
        self.count()
    }

    /// Returns true if all pieces are available.
    pub fn is_complete(&self) -> bool {
        self.count() == self.piece_count
    }

    /// Returns true if no pieces are available.
    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|&b| b == 0)
    }

    /// Returns the total number of pieces.
    pub fn piece_count(&self) -> usize {
        self.piece_count
    }

    /// Alias for `piece_count` for compatibility.
    pub fn num_pieces(&self) -> usize {
        self.piece_count
    }

    /// Alias for `piece_count` for compatibility.
    pub fn len(&self) -> usize {
        self.piece_count
    }

    /// Returns the raw bytes of the bitfield.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    /// Converts the bitfield to owned bytes.
    pub fn to_bytes(&self) -> Bytes {
        Bytes::copy_from_slice(&self.bits)
    }

    /// Returns indices of pieces that the peer has but we don't.
    pub fn missing_pieces(&self, our_bitfield: &Bitfield) -> Vec<usize> {
        (0..self.piece_count)
            .filter(|&i| self.has_piece(i) && !our_bitfield.has_piece(i))
            .collect()
    }

    /// Returns indices of all pieces that are not available.
    pub fn missing(&self) -> Vec<usize> {
        (0..self.piece_count)
            .filter(|&i| !self.has_piece(i))
            .collect()
    }

    /// Returns indices of all available pieces.
    pub fn available_pieces(&self) -> Vec<usize> {
        (0..self.piece_count)
            .filter(|&i| self.has_piece(i))
            .collect()
    }

    /// Clears any spare bits in the last byte that don't correspond to pieces.
    fn clear_spare_bits(&mut self) {
        let spare = (self.bits.len() * 8) - self.piece_count;
        if spare > 0 && spare < 8 && !self.bits.is_empty() {
            let mask = 0xFFu8 << spare;
            let last = self.bits.len() - 1;
            self.bits[last] &= mask;
        }
    }
}
