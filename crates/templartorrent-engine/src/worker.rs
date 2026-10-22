//! Bounded disk/hash workers. Each worker owns a queue with a hard cap: when it
//! is full, submissions are rejected and the caller keeps its bytes. Every
//! queued buffer travels with its byte-budget permit, which releases exactly
//! once the item is verified and written or dropped.
use std::collections::VecDeque;

use templartorrent_core::{budget::Permit, model::TorrentId};

use crate::event::PeerId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkError {
    Io(String),
    UnknownJob,
}

pub trait PieceVerifier {
    fn verify(&self, piece: u32, data: &[u8]) -> bool;

    /// Sessions with multiple torrents route hashes by stable job identity.
    /// Single-torrent verifiers keep the simpler deterministic test seam.
    fn verify_for(&self, _torrent: TorrentId, piece: u32, data: &[u8]) -> bool {
        self.verify(piece, data)
    }
}

pub trait PieceSink: Send {
    fn write_piece(&mut self, torrent: TorrentId, piece: u32, data: &[u8])
    -> Result<(), SinkError>;
}

/// Read path for resume/recheck. Returns `Ok(None)` when the piece is absent
/// or incomplete on disk. Implementations must validate file identity and
/// sizes before returning bytes; the session still hash-verifies every byte
/// via `PieceVerifier` before marking anything verified.
pub trait PieceReader {
    fn read_piece(
        &self,
        torrent: TorrentId,
        piece: u32,
        expected_len: usize,
    ) -> Result<Option<Vec<u8>>, SinkError>;
}

#[derive(Debug)]
pub struct WorkItem {
    pub torrent: TorrentId,
    pub piece: u32,
    pub data: Vec<u8>,
    pub generation: u64,
    pub permit: Permit,
    /// Supplier of these bytes, when they arrived over the wire. Carried so a
    /// hash failure attributes blame to exactly the peer that sent bad data.
    pub peer: Option<PeerId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkOutcome {
    Verified {
        torrent: TorrentId,
        piece: u32,
        generation: u64,
    },
    Rejected {
        torrent: TorrentId,
        piece: u32,
        generation: u64,
        peer: Option<PeerId>,
    },
    WriteFailed {
        torrent: TorrentId,
        piece: u32,
        generation: u64,
        error: SinkError,
    },
}

impl WorkOutcome {
    pub fn torrent(&self) -> TorrentId {
        match self {
            Self::Verified { torrent, .. }
            | Self::Rejected { torrent, .. }
            | Self::WriteFailed { torrent, .. } => *torrent,
        }
    }

    pub fn generation(&self) -> u64 {
        match self {
            Self::Verified { generation, .. }
            | Self::Rejected { generation, .. }
            | Self::WriteFailed { generation, .. } => *generation,
        }
    }

    pub fn piece(&self) -> u32 {
        match self {
            Self::Verified { piece, .. }
            | Self::Rejected { piece, .. }
            | Self::WriteFailed { piece, .. } => *piece,
        }
    }
}

pub struct DiskWorker {
    queue: VecDeque<WorkItem>,
    cap: usize,
    sink: Box<dyn PieceSink>,
}

impl DiskWorker {
    pub fn new(sink: Box<dyn PieceSink>, cap: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            cap: cap.max(1),
            sink,
        }
    }

    /// Queue one item, or hand it back untouched when full.
    pub fn submit(&mut self, item: WorkItem) -> Result<(), WorkItem> {
        if self.queue.len() >= self.cap {
            return Err(item);
        }
        self.queue.push_back(item);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Synchronous workers have no I/O outside poll; dropping queued items is
    /// sufficient to guarantee that cancelled jobs cannot write later.
    pub fn cancel(&mut self, torrent: TorrentId) {
        self.queue.retain(|item| item.torrent != torrent);
    }

    pub fn owned_bytes(&self) -> usize {
        self.queue.iter().map(|item| item.data.capacity()).sum()
    }

    /// Process exactly one head item: verify, then write. Stale generations
    /// are still reported; the session drops them by generation check.
    pub fn poll(&mut self, verifier: &dyn PieceVerifier) -> Option<WorkOutcome> {
        let item = self.queue.pop_front()?;
        if !verifier.verify_for(item.torrent, item.piece, &item.data) {
            return Some(WorkOutcome::Rejected {
                torrent: item.torrent,
                piece: item.piece,
                generation: item.generation,
                peer: item.peer,
            });
        }
        match self.sink.write_piece(item.torrent, item.piece, &item.data) {
            Ok(()) => Some(WorkOutcome::Verified {
                torrent: item.torrent,
                piece: item.piece,
                generation: item.generation,
            }),
            Err(error) => Some(WorkOutcome::WriteFailed {
                torrent: item.torrent,
                piece: item.piece,
                generation: item.generation,
                error,
            }),
        }
    }
}
