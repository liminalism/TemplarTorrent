//! The engine session under an arbitrary sequence of peer behaviour: wrong
//! bitfield lengths, out-of-range haves, unsolicited, misaligned, short and
//! corrupt blocks, choke flips, selection and order changes, pause/resume and
//! clock jumps. Beyond not panicking, it checks that the sink only ever gets
//! the right bytes, that no piece verifies twice, and that owned payload bytes
//! stay inside the budget the session was built with.
#![no_main]
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use templartorrent_core::{
    model::{Limits, TorrentId},
    sequence::FileSpec,
};
use templartorrent_engine::{
    event::PeerId,
    peer::{PeerAction, PeerMessage},
    session::{JobSpec, Session},
    worker::{PieceSink, PieceVerifier, SinkError},
};

const BUDGET: usize = 4 * 1024 * 1024;

fn byte_at(offset: u64) -> u8 {
    (offset.wrapping_mul(31) ^ (offset >> 8)) as u8
}

struct Layout {
    piece_length: u64,
    total: u64,
}

impl Layout {
    fn expected(&self, piece: u32) -> Option<Vec<u8>> {
        let start = u64::from(piece).checked_mul(self.piece_length)?;
        if start >= self.total {
            return None;
        }
        let end = (start + self.piece_length).min(self.total);
        Some((start..end).map(byte_at).collect())
    }
}

impl PieceVerifier for Layout {
    fn verify(&self, piece: u32, data: &[u8]) -> bool {
        self.expected(piece).is_some_and(|bytes| bytes == data)
    }
}

type Written = Arc<Mutex<Vec<(u32, Vec<u8>)>>>;

struct Sink(Written);
impl PieceSink for Sink {
    fn write_piece(&mut self, _: TorrentId, piece: u32, data: &[u8]) -> Result<(), SinkError> {
        self.0.lock().unwrap().push((piece, data.to_vec()));
        Ok(())
    }
}

#[derive(Arbitrary, Debug)]
struct Input {
    files: Vec<u32>,
    large_pieces: bool,
    ops: Vec<Op>,
}

#[derive(Arbitrary, Debug)]
enum Op {
    Connect,
    Disconnect(u8),
    Choke(u8, bool),
    Pipeline(u8, u8),
    Bitfield(u8, Vec<u64>),
    FullBitfield(u8),
    Have(u8, u32),
    /// Answer the n-th outstanding request with a chosen defect.
    Answer(u8, u8, Defect),
    /// Bytes nobody asked for.
    Unsolicited(u8, u32, u32, u16),
    Wanted(u8, bool),
    Order(Vec<u8>),
    Pause,
    Resume,
    Tick(u32),
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum Defect {
    None,
    FlipByte(u16),
    Short,
    Long,
    Twice,
}

fn compact(data: Vec<u8>) -> Vec<u8> {
    data.into_boxed_slice().into_vec()
}

fuzz_target!(|input: Input| {
    let files: Vec<FileSpec> = input
        .files
        .iter()
        .take(6)
        .enumerate()
        .map(|(i, length)| FileSpec {
            path: format!("f{i}"),
            length: u64::from(*length % 150_000),
            wanted: true,
        })
        .collect();
    let piece_length = if input.large_pieces { 32_768 } else { 16_384 };
    let total: u64 = files.iter().map(|file| file.length).sum();
    let file_count = files.len();
    let layout = Layout {
        piece_length,
        total,
    };
    let written: Written = Arc::default();
    let limits = Limits {
        payload_budget_bytes: BUDGET,
        ..Limits::default()
    };
    let mut session = Session::new(limits, vec![Box::new(Sink(written.clone()))], 4, 0).unwrap();
    let Ok(id) = session.add(JobSpec {
        name: "fuzz".into(),
        files,
        piece_length,
        max_pieces: 64,
    }) else {
        return;
    };
    let pieces = total.div_ceil(piece_length) as u32;
    let mut peers: Vec<PeerId> = Vec::new();
    let mut outstanding: Vec<(PeerId, u32, u32, u32)> = Vec::new();
    let mut verified: HashSet<u32> = HashSet::new();
    let mut now: u64 = 0;
    let pick =
        |peers: &[PeerId], i: u8| (!peers.is_empty()).then(|| peers[i as usize % peers.len()]);

    for op in input.ops.into_iter().take(256) {
        match op {
            Op::Connect => {
                if let Ok(peer) = session.connect(id) {
                    peers.push(peer);
                }
            }
            Op::Disconnect(i) => {
                if let Some(peer) = pick(&peers, i) {
                    let _ = session.disconnect(id, peer);
                    peers.retain(|p| *p != peer);
                    outstanding.retain(|r| r.0 != peer);
                }
            }
            Op::Choke(i, choking) => {
                if let Some(peer) = pick(&peers, i) {
                    let _ = session.set_peer_choking(id, peer, choking);
                }
            }
            Op::Pipeline(i, max) => {
                if let Some(peer) = pick(&peers, i) {
                    let _ = session.set_peer_pipeline(id, peer, usize::from(max));
                }
            }
            Op::Bitfield(i, words) => {
                if let Some(peer) = pick(&peers, i) {
                    let words: Vec<u64> = words.into_iter().take(8).collect();
                    let _ = session.note_bitfield(id, peer, words);
                }
            }
            Op::FullBitfield(i) => {
                if let Some(peer) = pick(&peers, i) {
                    let _ = session.note_bitfield(id, peer, vec![u64::MAX; 1]);
                }
            }
            Op::Have(i, piece) => {
                if let Some(peer) = pick(&peers, i) {
                    let _ = session.note_have(id, peer, piece);
                }
            }
            Op::Answer(i, n, defect) => {
                let mine: Vec<usize> = outstanding
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| pick(&peers, i) == Some(r.0))
                    .map(|(k, _)| k)
                    .collect();
                if mine.is_empty() {
                    continue;
                }
                let (peer, piece, begin, length) =
                    outstanding.remove(mine[n as usize % mine.len()]);
                let Some(full) = layout.expected(piece) else {
                    continue;
                };
                let start = begin as usize;
                let end = start.saturating_add(length as usize).min(full.len());
                let mut data = full.get(start..end).unwrap_or_default().to_vec();
                match defect {
                    Defect::None | Defect::Twice => {}
                    Defect::FlipByte(at) if !data.is_empty() => {
                        let at = at as usize % data.len();
                        data[at] ^= 0x5a;
                    }
                    Defect::FlipByte(_) => {}
                    Defect::Short => {
                        data.pop();
                    }
                    Defect::Long => data.push(0),
                }
                let _ = session.inject_block(id, peer, piece, begin, compact(data.clone()));
                if matches!(defect, Defect::Twice) {
                    let _ = session.inject_block(id, peer, piece, begin, compact(data));
                }
            }
            Op::Unsolicited(i, piece, begin, length) => {
                if let Some(peer) = pick(&peers, i) {
                    let data = vec![0xa5; usize::from(length)];
                    let _ = session.inject_block(id, peer, piece, begin, compact(data));
                }
            }
            Op::Wanted(file, wanted) => {
                if file_count > 0 {
                    let _ = session.set_wanted(id, file as usize % file_count, wanted);
                }
            }
            Op::Order(order) => {
                let order: Vec<usize> = order.into_iter().take(8).map(usize::from).collect();
                let _ = session.set_order(id, order);
            }
            Op::Pause => {
                let _ = session.pause(id);
            }
            Op::Resume => {
                let _ = session.resume(id);
            }
            Op::Tick(ns) => {
                now = now.saturating_add(u64::from(ns));
                let report = session.update(now, &layout);
                for entry in &report.outbox {
                    match &entry.action {
                        PeerAction::Send(PeerMessage::Request {
                            piece,
                            begin,
                            length,
                        }) => outstanding.push((entry.peer, *piece, *begin, *length)),
                        PeerAction::Send(PeerMessage::Cancel {
                            piece,
                            begin,
                            length,
                        }) => outstanding.retain(|r| *r != (entry.peer, *piece, *begin, *length)),
                        _ => {}
                    }
                }
                outstanding.truncate(512);
                for (torrent, piece) in &report.verified {
                    assert_eq!(*torrent, id);
                    assert!(*piece < pieces, "verified piece {piece} of {pieces}");
                    assert!(verified.insert(*piece), "piece {piece} verified twice");
                }
            }
        }
        let stats = session.payload_stats();
        assert!(
            stats.owned_bytes() <= BUDGET && stats.reserved_bytes <= BUDGET,
            "owned {stats:?} over budget {BUDGET}"
        );
    }
    for (piece, data) in written.lock().unwrap().iter() {
        assert_eq!(
            layout.expected(*piece).as_deref(),
            Some(data.as_slice()),
            "sink got wrong bytes for piece {piece}"
        );
    }
});
