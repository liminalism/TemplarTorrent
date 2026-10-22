//! Durable piece bitmap. A set bit may be adopted without hashing only when
//! this record carries a durability barrier and every file that piece spans
//! still has the device, inode, size and mtime captured after that barrier.
//! The bitmap is not a hash of the bytes: a changed or uncertain file is
//! rechecked, and a missing barrier trusts nothing.

use templartorrent_storage::FileStamp;

/// One file's identity at the moment the bitmap was made durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeFile {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
}

impl From<FileStamp> for ResumeFile {
    fn from(stamp: FileStamp) -> Self {
        Self {
            dev: stamp.dev,
            ino: stamp.ino,
            size: stamp.size,
            mtime_sec: stamp.mtime_sec,
            mtime_nsec: stamp.mtime_nsec,
        }
    }
}

impl ResumeFile {
    fn matches(self, live: FileStamp) -> bool {
        self.dev == live.dev
            && self.ino == live.ino
            && self.size == live.size
            && self.mtime_sec == live.mtime_sec
            && self.mtime_nsec == live.mtime_nsec
    }
}

/// Schema-versioned resume image. `bits` uses least-significant-bit-first
/// order: bit `piece % 8` of byte `piece / 8` is piece `piece`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRecord {
    pub pieces: usize,
    /// True only when every set bit's bytes were synced before this record
    /// was written. False trusts nothing.
    pub barrier: bool,
    pub bits: Vec<u8>,
    pub files: Vec<ResumeFile>,
}

impl ResumeRecord {
    /// Build a record from pieces the engine already accepted. The caller
    /// has synced payload bytes and inode fields before `stamps` were taken.
    pub fn checkpoint(piece_count: usize, verified_words: &[u64], stamps: &[FileStamp]) -> Self {
        Self {
            pieces: piece_count,
            barrier: true,
            bits: bits_from_verified_words(verified_words, piece_count),
            files: stamps.iter().copied().map(ResumeFile::from).collect(),
        }
    }
}

/// What one restore did with a resume record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeScan {
    /// Pieces marked verified from the bitmap, without reading them.
    pub trusted: usize,
    /// Pieces whose bytes were read and hashed.
    pub checked: usize,
    /// Checked pieces whose bytes were missing a match.
    pub failed: usize,
    /// Why the bitmap was not applied. `None` when it was applied, including
    /// when some files were simply rechecked.
    pub note: Option<&'static str>,
}

pub(crate) fn bits_from_verified_words(words: &[u64], piece_count: usize) -> Vec<u8> {
    let mut bits = vec![0u8; piece_count.div_ceil(8)];
    for piece in 0..piece_count {
        let Some(word) = words.get(piece / 64).copied() else {
            continue;
        };
        if word & (1u64 << (piece % 64)) != 0 {
            let byte = &mut bits[piece / 8];
            *byte |= 1u8 << (piece % 8);
        }
    }
    bits
}

/// Pieces whose bits may be adopted. `Err` means the record cannot be
/// applied (the caller hashes every piece and surfaces `reason`). A changed
/// file is not an error: pieces that touch it are absent from the `Ok` list.
pub(crate) fn trusted_pieces(
    piece_count: usize,
    piece_length: u64,
    file_lengths: &[u64],
    resume: &ResumeRecord,
    live: &[FileStamp],
) -> Result<Vec<u32>, &'static str> {
    if !resume.barrier {
        return Err("resume bitmap had no durability barrier");
    }
    let expected = piece_count.div_ceil(8);
    if resume.pieces != piece_count || resume.bits.len() != expected {
        return Err("resume bitmap does not match this torrent");
    }
    if !piece_count.is_multiple_of(8) {
        let used = piece_count % 8;
        if resume.bits[expected - 1] >> used != 0 {
            return Err("resume bitmap does not match this torrent");
        }
    }
    if resume.files.len() != file_lengths.len() || live.len() != file_lengths.len() {
        return Err("resume bitmap file list does not match this torrent");
    }
    let mut trusted = Vec::new();
    for piece in 0..piece_count {
        if resume.bits[piece / 8] & (1u8 << (piece % 8)) == 0 {
            continue;
        }
        let Some(files) = overlapping_files(piece as u32, piece_length, file_lengths) else {
            continue;
        };
        if files.is_empty() {
            continue;
        }
        let intact = files
            .iter()
            .all(|&index| resume.files[index].matches(live[index]));
        if intact {
            trusted.push(piece as u32);
        }
    }
    Ok(trusted)
}

/// Files whose byte range overlaps this piece. Empty when the piece lies
/// past the torrent or overflows. Zero-length files never overlap.
fn overlapping_files(piece: u32, piece_length: u64, lengths: &[u64]) -> Option<Vec<usize>> {
    let start = u64::from(piece).checked_mul(piece_length)?;
    let mut end = start.checked_add(piece_length)?;
    let mut total = 0u64;
    for length in lengths {
        total = total.checked_add(*length)?;
    }
    if start >= total {
        return Some(Vec::new());
    }
    end = end.min(total);
    let mut hit = Vec::new();
    let mut offset = 0u64;
    for (index, length) in lengths.iter().copied().enumerate() {
        let next = offset.checked_add(length)?;
        if length > 0 && start < next && offset < end {
            hit.push(index);
        }
        offset = next;
    }
    Some(hit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(ino: u64) -> FileStamp {
        FileStamp {
            dev: 1,
            ino,
            size: 10,
            mtime_sec: 20,
            mtime_nsec: 30,
        }
    }

    fn record(bits: Vec<u8>, files: Vec<ResumeFile>) -> ResumeRecord {
        ResumeRecord {
            pieces: 2,
            barrier: true,
            bits,
            files,
        }
    }

    #[test]
    fn unchanged_files_trust_every_set_bit() {
        let live = [stamp(7), stamp(8)];
        let resume = record(
            vec![0b0000_0011],
            live.into_iter().map(ResumeFile::from).collect(),
        );
        // 100 + 50 bytes, 80-byte pieces: piece 1 spans both files.
        let trusted = trusted_pieces(2, 80, &[100, 50], &resume, &live).unwrap();
        assert_eq!(trusted, vec![0, 1]);
    }

    #[test]
    fn a_changed_file_rechecks_only_the_pieces_that_touch_it() {
        let mut live = [stamp(7), stamp(8)];
        let saved: Vec<_> = live.into_iter().map(ResumeFile::from).collect();
        live[1].mtime_nsec += 1;
        let resume = record(vec![0b0000_0011], saved);
        let trusted = trusted_pieces(2, 80, &[100, 50], &resume, &live).unwrap();
        assert_eq!(trusted, vec![0], "piece 1 spans the changed file");
    }

    #[test]
    fn no_barrier_and_a_short_bitfield_trust_nothing() {
        let live = [stamp(7)];
        let mut resume = record(vec![0b0000_0001], vec![ResumeFile::from(live[0])]);
        resume.pieces = 1;
        resume.barrier = false;
        let reason = trusted_pieces(1, 80, &[100], &resume, &live).unwrap_err();
        assert!(reason.contains("barrier"), "{reason}");
        resume.barrier = true;
        resume.bits.clear();
        let reason = trusted_pieces(1, 80, &[100], &resume, &live).unwrap_err();
        assert!(reason.contains("does not match"), "{reason}");
    }

    #[test]
    fn spare_high_bits_and_a_file_count_mismatch_are_rejected() {
        let live = [stamp(7)];
        let mut one_piece = record(vec![0b1000_0001], vec![ResumeFile::from(live[0])]);
        one_piece.pieces = 1;
        let reason = trusted_pieces(1, 80, &[100], &one_piece, &live).unwrap_err();
        assert!(reason.contains("does not match"), "{reason}");
        let resume = record(vec![0b0000_0011], vec![ResumeFile::from(live[0])]);
        let reason = trusted_pieces(2, 80, &[60, 40], &resume, &live).unwrap_err();
        assert!(reason.contains("file list"), "{reason}");
    }

    #[test]
    fn verified_words_pack_lsb_first() {
        // Pieces 0 and 63 set in the first word, piece 64 in the second.
        let words = [1u64 | (1u64 << 63), 1u64];
        let bits = bits_from_verified_words(&words, 65);
        assert_eq!(bits.len(), 9);
        assert_eq!(bits[0] & 1, 1);
        assert_eq!(bits[7] & 0b1000_0000, 0b1000_0000);
        assert_eq!(bits[8], 1);
    }
}
