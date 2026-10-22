//! Complete-piece invariants with fake monotonic time; no socket or disk I/O.
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use templartorrent_core::{
    model::{JobState, Limits, TorrentId},
    sequence::FileSpec,
};
use templartorrent_engine::{
    event::PeerId,
    peer::{PeerAction, PeerMessage},
    session::{JobSpec, Session, SessionReport, piece_working_set},
    worker::{PieceSink, PieceVerifier, SinkError},
};

type Pieces = Arc<Mutex<HashMap<(u64, u32), Vec<u8>>>>;
#[derive(Clone, Default)]
struct Sink(Pieces);
impl PieceSink for Sink {
    fn write_piece(&mut self, id: TorrentId, piece: u32, data: &[u8]) -> Result<(), SinkError> {
        let previous = self.0.lock().unwrap().insert((id.0, piece), data.to_vec());
        assert!(previous.is_none(), "a piece must never be written twice");
        Ok(())
    }
}

fn bytes(piece: u32, begin: u32, length: u32) -> Vec<u8> {
    (begin..begin + length)
        .map(|offset| {
            (piece as u8)
                .wrapping_mul(13)
                .wrapping_add((offset / 16384) as u8)
                ^ (offset as u8).rotate_left(3)
        })
        .collect()
}
struct Exact;
impl PieceVerifier for Exact {
    fn verify(&self, piece: u32, data: &[u8]) -> bool {
        data == bytes(piece, 0, data.len() as u32)
    }
}

fn spec(length: u64, files: &[(&str, u64)]) -> JobSpec {
    JobSpec {
        name: "assembly".into(),
        piece_length: length,
        max_pieces: 64,
        files: files
            .iter()
            .map(|(path, length)| FileSpec {
                path: path.to_string(),
                length: *length,
                wanted: true,
            })
            .collect(),
    }
}
fn peer(session: &mut Session, id: TorrentId, bits: u64) -> PeerId {
    let peer = session.connect(id).unwrap();
    session.set_peer_choking(id, peer, false).unwrap();
    session.note_bitfield(id, peer, vec![bits]).unwrap();
    peer
}
fn requests(report: &SessionReport) -> Vec<(PeerId, u32, u32, u32)> {
    report
        .outbox
        .iter()
        .filter_map(|entry| match entry.action {
            PeerAction::Send(PeerMessage::Request {
                piece,
                begin,
                length,
            }) => Some((entry.peer, piece, begin, length)),
            _ => None,
        })
        .collect()
}
fn feed(session: &mut Session, id: TorrentId, requests: &[(PeerId, u32, u32, u32)]) {
    for &(peer, piece, begin, length) in requests.iter().rev() {
        session
            .inject_block(id, peer, piece, begin, bytes(piece, begin, length))
            .unwrap();
    }
}

#[test]
fn multi_block_pipeline_refills_out_of_order_with_short_tail_and_file_promotion() {
    const LENGTH: u64 = 256 * 1024;
    let sink = Sink::default();
    let mut session = Session::new(Limits::default(), vec![Box::new(sink.clone())], 1, 0).unwrap();
    // Wire order stays fixed; episode 1 spans piece 1 and the short piece 2.
    let id = session
        .add(spec(
            LENGTH,
            &[("episode02", LENGTH), ("episode01", LENGTH + 37)],
        ))
        .unwrap();
    peer(&mut session, id, 0b111);
    let mut seen = HashSet::new();
    let mut piece_order = Vec::new();
    let mut peak = 0;
    for _ in 0..30 {
        let report = session.update(0, &Exact);
        assert!(report.errors.is_empty(), "{report:?}");
        let batch = requests(&report);
        assert!(batch.len() <= 8, "pipeline must remain bounded");
        for &(_, piece, begin, _) in &batch {
            assert!(seen.insert((piece, begin)), "received block re-requested");
            if piece_order.last() != Some(&piece) {
                piece_order.push(piece);
            }
        }
        peak = peak.max(session.snapshot().payload_budget_used);
        feed(&mut session, id, &batch);
        if session.snapshot().jobs[0].state == JobState::Seeding {
            break;
        }
    }
    // Both episode01 pieces share rank 0 and availability 1; the live picker
    // may take them in either order (seeded tie-break), but episode02's piece
    // 0 must wait until episode01 is finished.
    assert_eq!(piece_order.len(), 3, "{piece_order:?}");
    assert_eq!(
        piece_order[2], 0,
        "episode01 finishes before episode02 starts"
    );
    assert_eq!(
        {
            let mut first_two = piece_order[..2].to_vec();
            first_two.sort_unstable();
            first_two
        },
        [1, 2]
    );
    assert_eq!(seen.len(), 33);
    // The next piece is leased while the current one's last blocks land, so
    // at most two piece working sets are reserved at once, never more.
    let working_set = piece_working_set(LENGTH as usize).unwrap();
    assert!(
        peak >= working_set && peak <= 2 * working_set,
        "peak {peak} outside one to two working sets of {working_set}"
    );
    let snap = session.snapshot();
    assert_eq!(snap.jobs[0].verified_bytes, 2 * LENGTH + 37);
    assert_eq!(snap.jobs[0].state, JobState::Seeding);
    assert_eq!(snap.payload_budget_used, 0);
    let written = sink.0.lock().unwrap();
    assert_eq!(written.len(), 3);
    for piece in 0..3 {
        let len = if piece == 2 { 37 } else { LENGTH as u32 };
        assert_eq!(written[&(id.0, piece)], bytes(piece, 0, len));
    }
}

#[test]
fn partial_piece_is_never_verified_or_written_and_duplicate_inbox_is_bounded() {
    let sink = Sink::default();
    let mut session = Session::new(Limits::default(), vec![Box::new(sink.clone())], 1, 0).unwrap();
    let id = session.add(spec(65536, &[("episode01", 65536)])).unwrap();
    let p = peer(&mut session, id, 1);
    let batch = requests(&session.update(0, &Exact));
    assert_eq!(batch.len(), 4);
    for _ in 0..1000 {
        session
            .inject_block(id, p, 0, 32768, bytes(0, 32768, 16384))
            .unwrap();
    }
    let ownership = session.payload_stats();
    assert_eq!(ownership.assembly_bytes, 65536);
    assert_eq!(ownership.inbox_bytes, 16384);
    assert_eq!(ownership.owned_bytes(), 81920);
    assert_eq!(ownership.reserved_bytes, 3 * 65536);
    // Wrong offset and wrong-length responses do not consume pending slots.
    session.inject_block(id, p, 0, 1, vec![0; 16384]).unwrap();
    session.inject_block(id, p, 0, 0, vec![0; 1]).unwrap();
    let report = session.update(0, &Exact);
    assert_eq!(report.dropped_stale, 1001);
    assert_eq!(report.outcomes, 0);
    assert!(
        requests(&report).is_empty(),
        "no re-request of received spans"
    );
    assert_eq!(session.snapshot().jobs[0].verified_bytes, 0);
    assert!(sink.0.lock().unwrap().is_empty());
    let rest: Vec<_> = batch.into_iter().filter(|r| r.2 != 32768).collect();
    feed(&mut session, id, &rest);
    let report = session.update(0, &Exact);
    assert_eq!(report.outcomes, 1);
    assert_eq!(session.snapshot().jobs[0].state, JobState::Seeding);
}

#[test]
fn memory_is_reserved_before_requests_and_repeated_settings_cannot_mint_capacity() {
    const LENGTH: usize = 65536;
    let limits = Limits {
        payload_budget_bytes: 3 * LENGTH,
        ..Limits::default()
    };
    let mut session = Session::new(limits.clone(), vec![Box::new(Sink::default())], 1, 0).unwrap();
    let id = session
        .add(spec(LENGTH as u64, &[("a", 2 * LENGTH as u64)]))
        .unwrap();
    let p1 = peer(&mut session, id, 3);
    let p2 = peer(&mut session, id, 3);
    let batch = requests(&session.update(0, &Exact));
    assert_eq!(batch.len(), 4);
    assert!(batch.iter().all(|r| r.0 == p1));
    assert_eq!(session.snapshot().payload_budget_used, 3 * LENGTH);
    feed(&mut session, id, &batch[..1]);
    session.update(0, &Exact);
    for _ in 0..20 {
        session.set_limits(limits.clone(), 0).unwrap();
        assert!(requests(&session.update(0, &Exact)).is_empty());
        assert_eq!(session.snapshot().payload_budget_used, 3 * LENGTH);
    }
    session.disconnect(id, p1).unwrap();
    assert_eq!(session.snapshot().payload_budget_used, 0);
    let batch = requests(&session.update(0, &Exact));
    assert_eq!(batch.len(), 4);
    // Either piece is a valid pick (same rank and availability); all four
    // requests must belong to one piece on the surviving peer.
    assert!(batch.iter().all(|r| r.0 == p2 && r.1 == batch[0].1));
}

#[test]
fn pause_cancels_partial_work_and_remove_discards_complete_backpressured_writes() {
    let sink = Sink::default();
    let mut session = Session::new(Limits::default(), vec![Box::new(sink.clone())], 1, 0).unwrap();
    let id = session.add(spec(16384, &[("a", 32768)])).unwrap();
    peer(&mut session, id, 3);
    peer(&mut session, id, 3);
    let batch = requests(&session.update(0, &Exact));
    feed(&mut session, id, &batch);
    session.pause(id).unwrap();
    assert_eq!(session.snapshot().payload_budget_used, 0);
    let report = session.update(0, &Exact);
    assert_eq!(requests(&report).len(), 0);
    assert_eq!(
        report.outbox.len(),
        2,
        "both pending requests get wire cancels"
    );
    assert!(sink.0.lock().unwrap().is_empty());
    session.resume(id).unwrap();
    let batch = requests(&session.update(0, &Exact));
    feed(&mut session, id, &batch);
    session.update(0, &Exact); // One write finishes, the other waits for worker space.
    assert_eq!(sink.0.lock().unwrap().len(), 1);
    session.remove(id, false).unwrap();
    session.update(0, &Exact);
    assert_eq!(session.snapshot().payload_budget_used, 0);
    assert_eq!(
        sink.0.lock().unwrap().len(),
        1,
        "removed job cannot write later"
    );
}

#[test]
fn file_promotion_preserves_other_peers_completed_work() {
    let sink = Sink::default();
    let mut session = Session::new(Limits::default(), vec![Box::new(sink.clone())], 2, 0).unwrap();
    let id = session
        .add(spec(16384, &[("episode01", 16384), ("episode02", 16384)]))
        .unwrap();
    peer(&mut session, id, 3);
    peer(&mut session, id, 3);
    let batch = requests(&session.update(0, &Exact));
    feed(&mut session, id, &batch);
    session.update(0, &Exact);
    let report = session.update(0, &Exact);
    assert_eq!(
        report.dropped_stale, 0,
        "promotion is not a user cancellation"
    );
    assert!(requests(&report).is_empty());
    assert_eq!(session.snapshot().jobs[0].state, JobState::Seeding);
    assert_eq!(sink.0.lock().unwrap().len(), 2);
}

#[test]
fn lease_timeout_cancels_pipeline_discards_partial_bytes_and_retries() {
    let sink = Sink::default();
    let mut session = Session::new(Limits::default(), vec![Box::new(sink.clone())], 1, 0).unwrap();
    let id = session.add(spec(65536, &[("a", 65536)])).unwrap();
    peer(&mut session, id, 1);
    let batch = requests(&session.update(0, &Exact));
    feed(&mut session, id, &batch[..1]);
    session.update(0, &Exact);
    let report = session.update(31_000_000_000, &Exact);
    let cancels = report
        .outbox
        .iter()
        .filter(|e| matches!(e.action, PeerAction::Send(PeerMessage::Cancel { .. })))
        .count();
    assert_eq!(cancels, 3);
    let retry = requests(&report);
    assert_eq!(
        retry.len(),
        4,
        "expiry must clear pending state as well as busy bits"
    );
    feed(&mut session, id, &retry);
    session.update(31_000_000_000, &Exact);
    assert_eq!(session.snapshot().jobs[0].state, JobState::Seeding);
    assert_eq!(session.snapshot().payload_budget_used, 0);
}

#[test]
fn low_rate_progress_refreshes_lease_until_complete() {
    let sink = Sink::default();
    let limits = Limits {
        download_kb_s: 1,
        ..Limits::default()
    };
    let mut session = Session::new(limits, vec![Box::new(sink)], 1, 0).unwrap();
    let id = session.add(spec(65536, &[("a", 65536)])).unwrap();
    peer(&mut session, id, 1);
    let mut requested = HashSet::new();
    for seconds in [0, 17, 34, 51, 68] {
        let report = session.update(seconds * 1_000_000_000, &Exact);
        let batch = requests(&report);
        for request in &batch {
            assert!(requested.insert(request.2), "slow piece restarted");
        }
        feed(&mut session, id, &batch);
        session.update(seconds * 1_000_000_000, &Exact);
    }
    assert_eq!(requested.len(), 4);
    assert_eq!(session.snapshot().jobs[0].state, JobState::Seeding);
}

#[test]
fn pause_preserves_queued_bitfield_and_have_connection_state() {
    let mut session =
        Session::new(Limits::default(), vec![Box::new(Sink::default())], 1, 0).unwrap();
    let id = session.add(spec(16384, &[("a", 32768)])).unwrap();
    let p = peer(&mut session, id, 1); // Bitfield still in the inbox.
    session.note_have(id, p, 1).unwrap();
    session.pause(id).unwrap();
    session.update(0, &Exact);
    session.resume(id).unwrap();
    for _ in 0..6 {
        let batch = requests(&session.update(0, &Exact));
        feed(&mut session, id, &batch);
    }
    assert_eq!(
        session.snapshot().jobs[0].state,
        JobState::Seeding,
        "pause must not forget a peer's initial wire availability"
    );
}

#[test]
fn oversized_incoming_allocation_cannot_hide_behind_a_small_block_length() {
    let mut session =
        Session::new(Limits::default(), vec![Box::new(Sink::default())], 1, 0).unwrap();
    let id = session.add(spec(16384, &[("a", 16384)])).unwrap();
    let p = peer(&mut session, id, 1);
    session.update(0, &Exact);
    let mut incoming = Vec::with_capacity(1 << 20);
    incoming.extend(bytes(0, 0, 16384));
    assert_eq!(
        session.inject_block(id, p, 0, 0, incoming),
        Err(templartorrent_engine::EngineError::NoCapacity)
    );
    assert_eq!(session.payload_stats().inbox_bytes, 0);
    session
        .inject_block(id, p, 0, 0, bytes(0, 0, 16384))
        .unwrap();
    session.update(0, &Exact);
    assert_eq!(session.snapshot().jobs[0].state, JobState::Seeding);
}

#[test]
fn active_piece_count_is_bounded_even_with_spare_byte_budget() {
    const PEERS: usize = templartorrent_engine::session::MAX_ACTIVE_PIECES + 1;
    let limits = Limits {
        connections_per_job: PEERS,
        ..Limits::default()
    };
    let mut session = Session::new(limits, vec![Box::new(Sink::default())], 1, 0).unwrap();
    let mut job = spec(1, &[("a", PEERS as u64)]);
    job.max_pieces = PEERS;
    let id = session.add(job).unwrap();
    for _ in 0..PEERS {
        let p = session.connect(id).unwrap();
        session.set_peer_choking(id, p, false).unwrap();
        session
            .note_bitfield(id, p, vec![u64::MAX; PEERS.div_ceil(64)])
            .unwrap();
    }
    let batch = requests(&session.update(0, &Exact));
    assert_eq!(batch.len(), PEERS - 1);
    assert_eq!(session.payload_stats().reserved_bytes, 3 * (PEERS - 1));
}
