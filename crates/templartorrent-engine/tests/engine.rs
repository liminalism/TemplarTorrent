//! Engine session tests: deterministic, fake clocks, no I/O or network.
//! Every test drives the public session API only.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use templartorrent_core::{
    model::{JobState, Limits, TorrentId},
    sequence::FileSpec,
};
use templartorrent_engine::{
    EngineError,
    event::PeerId,
    peer::{PeerAction, PeerMessage},
    session::{JobSpec, Session, piece_working_set},
    worker::{PieceReader, PieceSink, PieceVerifier, SinkError},
};

struct AcceptAll;
impl PieceVerifier for AcceptAll {
    fn verify(&self, _piece: u32, _data: &[u8]) -> bool {
        true
    }
}

struct RejectAll;
impl PieceVerifier for RejectAll {
    fn verify(&self, _piece: u32, _data: &[u8]) -> bool {
        false
    }
}

type StoredPieces = Arc<Mutex<HashMap<(u64, u32), Vec<u8>>>>;

#[derive(Clone, Default)]
struct SharedSink {
    pieces: StoredPieces,
}

impl PieceSink for SharedSink {
    fn write_piece(
        &mut self,
        torrent: TorrentId,
        piece: u32,
        data: &[u8],
    ) -> Result<(), SinkError> {
        self.pieces
            .lock()
            .unwrap()
            .insert((torrent.0, piece), data.to_vec());
        Ok(())
    }
}

impl PieceReader for SharedSink {
    fn read_piece(
        &self,
        torrent: TorrentId,
        piece: u32,
        _expected_len: usize,
    ) -> Result<Option<Vec<u8>>, SinkError> {
        Ok(self
            .pieces
            .lock()
            .unwrap()
            .get(&(torrent.0, piece))
            .cloned())
    }
}

struct RejectPiece(u32);
impl PieceVerifier for RejectPiece {
    fn verify(&self, piece: u32, _data: &[u8]) -> bool {
        piece != self.0
    }
}

struct FailOnPiece {
    inner: SharedSink,
    fail: u32,
}
impl PieceReader for FailOnPiece {
    fn read_piece(
        &self,
        torrent: TorrentId,
        piece: u32,
        expected_len: usize,
    ) -> Result<Option<Vec<u8>>, SinkError> {
        if piece == self.fail {
            return Err(SinkError::Io("test read failure".into()));
        }
        self.inner.read_piece(torrent, piece, expected_len)
    }
}

struct FailingSink;
impl PieceSink for FailingSink {
    fn write_piece(&mut self, _t: TorrentId, _p: u32, _d: &[u8]) -> Result<(), SinkError> {
        Err(SinkError::Io("test failure".into()))
    }
}

// Two files, 50_000 bytes total, 16_384-byte pieces -> 4 pieces.
fn spec() -> JobSpec {
    JobSpec {
        name: "demo".into(),
        files: vec![
            FileSpec {
                path: "a.bin".into(),
                length: 40_000,
                wanted: true,
            },
            FileSpec {
                path: "b.bin".into(),
                length: 10_000,
                wanted: true,
            },
        ],
        piece_length: 16_384,
        max_pieces: 64,
    }
}

fn session() -> Session {
    Session::new(
        Limits::default(),
        vec![Box::new(SharedSink::default())],
        16,
        0,
    )
    .unwrap()
}

fn session_with_sink(sink: SharedSink) -> Session {
    Session::new(Limits::default(), vec![Box::new(sink)], 16, 0).unwrap()
}

fn ready_peer(session: &mut Session, id: TorrentId) -> PeerId {
    let peer = session.connect(id).unwrap();
    session.set_peer_choking(id, peer, false).unwrap();
    session.note_bitfield(id, peer, vec![0b1111]).unwrap();
    peer
}

fn requests_of(
    session_report: &templartorrent_engine::session::SessionReport,
) -> Vec<(u32, u32, u32)> {
    session_report
        .outbox
        .iter()
        .filter_map(|entry| match &entry.action {
            PeerAction::Send(PeerMessage::Request {
                piece,
                begin,
                length,
            }) => Some((*piece, *begin, *length)),
            _ => None,
        })
        .collect()
}

#[test]
fn lease_round_trip_verifies_piece() {
    let mut session = session();
    let id = session.add(spec()).unwrap();
    let peer = ready_peer(&mut session, id);
    let report = session.update(0, &AcceptAll);
    let requests = requests_of(&report);
    // Every 16 KiB piece is one block; all four pipeline in the first round.
    assert_eq!(requests.len(), 4, "all pieces pipelined, got {requests:?}");
    let (piece, begin, length) = requests[0];
    // a.bin's pieces share rank and availability; the live picker's seeded
    // tie-break may start on any of them.
    assert!(piece < 3, "a.bin piece expected, got {piece}");
    assert_eq!((begin, length), (0, 16384));
    session
        .inject_block(id, peer, piece, begin, vec![7u8; length as usize])
        .unwrap();
    let report = session.update(0, &AcceptAll);
    assert_eq!(report.outcomes, 1);
    assert_eq!(report.dropped_stale, 0);
    let snap = session.snapshot();
    assert_eq!(snap.jobs[0].verified_bytes, 16384);
    // The written piece's permit is gone; the three still-pipelined pieces
    // keep exactly their working sets reserved.
    assert_eq!(
        snap.payload_budget_used,
        2 * piece_working_set(16384).unwrap() + piece_working_set(848).unwrap(),
        "permit released after write"
    );
}

#[test]
fn generation_fencing_drops_stale_work() {
    let mut session = session();
    let id = session.add(spec()).unwrap();
    let peer = ready_peer(&mut session, id);
    let report = session.update(0, &AcceptAll);
    let (piece, begin, length) = requests_of(&report)[0];
    // Queue the block, then change the wanted set before the round runs.
    session
        .inject_block(id, peer, piece, begin, vec![7u8; length as usize])
        .unwrap();
    session.set_wanted(id, 0, false).unwrap();
    let report = session.update(0, &AcceptAll);
    assert!(report.dropped_stale >= 1, "stale outcome must be dropped");
    assert_eq!(session.snapshot().jobs[0].verified_bytes, 0);
    // The surviving file still completes afterwards.
    session.set_wanted(id, 0, true).unwrap();
    for _ in 0..40 {
        let report = session.update(0, &AcceptAll);
        for (piece, begin, length) in requests_of(&report) {
            session
                .inject_block(id, peer, piece, begin, vec![7u8; length as usize])
                .unwrap();
        }
        if session.snapshot().jobs[0].state == JobState::Seeding {
            break;
        }
    }
    assert_eq!(session.snapshot().jobs[0].state, JobState::Seeding);
}

#[test]
fn budget_bound_rejects_oversized_admission() {
    let limits = Limits {
        payload_budget_bytes: 100,
        ..Limits::default()
    };
    let mut session = Session::new(limits, vec![Box::new(SharedSink::default())], 16, 0).unwrap();
    assert!(
        matches!(session.add(spec()), Err(EngineError::Unsupported(_))),
        "reject an impossible working set before sending any requests"
    );
}

#[test]
fn worker_cap_rejects_when_full() {
    use templartorrent_core::budget::ByteBudget;
    use templartorrent_engine::worker::{DiskWorker, WorkItem};

    let budget = ByteBudget::new(1 << 20);
    let item = |piece| WorkItem {
        torrent: TorrentId(9),
        piece,
        data: vec![1u8; 100],
        generation: 0,
        permit: budget.try_acquire(100).unwrap(),
        peer: None,
    };
    let mut worker = DiskWorker::new(Box::new(SharedSink::default()), 1);
    worker.submit(item(0)).unwrap();
    let returned = worker.submit(item(1)).unwrap_err();
    assert_eq!(returned.piece, 1, "rejected item handed back untouched");
    assert_eq!(returned.data.len(), 100);
    drop(returned);
    assert_eq!(budget.used(), 100, "only the queued item holds budget");
}

#[test]
fn rejected_blocks_release_piece_for_retry() {
    let mut session = session();
    let id = session.add(spec()).unwrap();
    let peer = ready_peer(&mut session, id);
    let report = session.update(0, &AcceptAll);
    let (piece, begin, length) = requests_of(&report)[0];
    session
        .inject_block(id, peer, piece, begin, vec![7u8; length as usize])
        .unwrap();
    let report = session.update(0, &RejectAll);
    assert_eq!(report.outcomes, 1);
    assert_eq!(
        session.snapshot().jobs[0].verified_bytes,
        0,
        "hash failure verifies nothing"
    );
    // The piece is pickable again and completes with good bytes.
    let report = session.update(0, &AcceptAll);
    assert!(!requests_of(&report).is_empty(), "rejected piece re-issued");
    let (piece, begin, length) = requests_of(&report)[0];
    session
        .inject_block(id, peer, piece, begin, vec![7u8; length as usize])
        .unwrap();
    session.update(0, &AcceptAll);
    assert_eq!(session.snapshot().jobs[0].verified_bytes, 16384);
}

#[test]
fn rejected_blocks_blame_supplying_peer() {
    let mut session = session();
    let id = session.add(spec()).unwrap();
    let peer = ready_peer(&mut session, id);
    let report = session.update(0, &AcceptAll);
    let (piece, begin, length) = requests_of(&report)[0];
    session
        .inject_block(id, peer, piece, begin, vec![7u8; length as usize])
        .unwrap();
    let report = session.update(0, &RejectAll);
    assert_eq!(report.outcomes, 1);
    assert_eq!(report.blamed.len(), 1, "one hash failure, one blame entry");
    assert_eq!(report.blamed[0].torrent, id);
    assert_eq!(report.blamed[0].peer, peer);
    assert_eq!(report.blamed[0].piece, piece);
    // Accepted bytes blame nobody.
    let report = session.update(0, &AcceptAll);
    let (piece, begin, length) = requests_of(&report)[0];
    session
        .inject_block(id, peer, piece, begin, vec![7u8; length as usize])
        .unwrap();
    let report = session.update(0, &AcceptAll);
    assert!(report.blamed.is_empty(), "verified bytes blame nobody");
}

#[test]
fn write_failure_marks_job_error_and_resume_recovers() {
    let mut session = Session::new(Limits::default(), vec![Box::new(FailingSink)], 16, 0).unwrap();
    let id = session.add(spec()).unwrap();
    let peer = ready_peer(&mut session, id);
    let report = session.update(0, &AcceptAll);
    let (piece, begin, length) = requests_of(&report)[0];
    session
        .inject_block(id, peer, piece, begin, vec![7u8; length as usize])
        .unwrap();
    let report = session.update(0, &AcceptAll);
    assert_eq!(session.snapshot().jobs[0].state, JobState::Error);
    assert!(matches!(
        report.errors.as_slice(),
        [EngineError::Storage(_)]
    ));
    session.resume(id).unwrap();
    assert_eq!(session.snapshot().jobs[0].state, JobState::Downloading);
}

#[test]
fn rate_cap_stalls_requests_until_refill() {
    let limits = Limits {
        download_kb_s: 1,
        ..Limits::default()
    };
    let mut session = Session::new(limits, vec![Box::new(SharedSink::default())], 16, 0).unwrap();
    let id = session.add(spec()).unwrap();
    let peer = ready_peer(&mut session, id);
    let report = session.update(0, &AcceptAll);
    assert_eq!(requests_of(&report).len(), 1, "first block fits the burst");
    let (piece, begin, length) = requests_of(&report)[0];
    session
        .inject_block(id, peer, piece, begin, vec![7u8; length as usize])
        .unwrap();
    session.update(0, &AcceptAll);
    // Bucket is dry: 1 kB/s needs ~16 s to afford the next 16 KiB block.
    let report = session.update(0, &AcceptAll);
    assert!(
        requests_of(&report).is_empty(),
        "requests stall on empty bucket"
    );
    let report = session.update(20_000_000_000, &AcceptAll);
    assert_eq!(requests_of(&report).len(), 1, "refill re-admits requests");
}

#[test]
fn end_to_end_transfer_completes_and_seeds() {
    let sink = SharedSink::default();
    let mut session = session_with_sink(sink.clone());
    let id = session.add(spec()).unwrap();
    let peer = ready_peer(&mut session, id);
    for _ in 0..40 {
        let report = session.update(0, &AcceptAll);
        for (piece, begin, length) in requests_of(&report) {
            session
                .inject_block(id, peer, piece, begin, vec![piece as u8; length as usize])
                .unwrap();
        }
        if session.snapshot().jobs[0].state == JobState::Seeding {
            break;
        }
    }
    let snap = session.snapshot();
    assert_eq!(snap.jobs[0].state, JobState::Seeding);
    assert_eq!(snap.jobs[0].wanted_bytes, 50_000);
    assert_eq!(snap.jobs[0].verified_bytes, 50_000);
    assert_eq!(snap.payload_budget_used, 0);
    let stored = sink.pieces.lock().unwrap();
    assert_eq!(stored.len(), 4, "all four pieces written");
    for piece in 0..4u32 {
        let bytes = &stored[&(id.0, piece)];
        let expected = if piece < 3 { 16384 } else { 848 };
        assert_eq!(bytes.len(), expected);
        assert!(
            bytes.iter().all(|&b| b == piece as u8),
            "exact bytes preserved"
        );
    }
}

#[test]
fn selection_changes_resettle_completion_without_a_new_piece() {
    let mut session = session();
    let id = session.add(spec()).unwrap();
    let peer = ready_peer(&mut session, id);
    // Only a.bin wanted: pieces 0..=2 (the boundary piece included).
    session.set_wanted(id, 1, false).unwrap();
    for _ in 0..40 {
        let report = session.update(0, &AcceptAll);
        for (piece, begin, length) in requests_of(&report) {
            session
                .inject_block(id, peer, piece, begin, vec![piece as u8; length as usize])
                .unwrap();
        }
        if session.snapshot().jobs[0].state == JobState::Seeding {
            break;
        }
    }
    let snap = session.snapshot();
    assert_eq!(snap.jobs[0].state, JobState::Seeding);
    assert_eq!(snap.jobs[0].wanted_bytes, 40_000);
    // Wanting b.bin again reopens the job without any network event.
    session.set_wanted(id, 1, true).unwrap();
    assert_eq!(session.snapshot().jobs[0].state, JobState::Downloading);
    assert_eq!(session.snapshot().jobs[0].wanted_bytes, 50_000);
    // Dropping it again completes immediately: nothing new was verified.
    session.set_wanted(id, 1, false).unwrap();
    assert_eq!(session.snapshot().jobs[0].state, JobState::Seeding);
    // A paused job keeps its state until resume re-evaluates it.
    session.pause(id).unwrap();
    session.set_wanted(id, 1, true).unwrap();
    assert_eq!(session.snapshot().jobs[0].state, JobState::Paused);
    session.resume(id).unwrap();
    assert_eq!(session.snapshot().jobs[0].state, JobState::Downloading);
}

#[test]
fn pause_resume_remove_lifecycle() {
    let mut session = session();
    let id = session.add(spec()).unwrap();
    let _peer = ready_peer(&mut session, id);
    session.pause(id).unwrap();
    assert_eq!(session.snapshot().jobs[0].state, JobState::Paused);
    let report = session.update(0, &AcceptAll);
    assert_eq!(report.leases_issued, 0, "paused jobs issue nothing");
    session.resume(id).unwrap();
    assert_eq!(session.snapshot().jobs[0].state, JobState::Downloading);
    assert_eq!(
        session.remove(id, true),
        Err(EngineError::Unsupported("data deletion")),
        "destructive removal is not wired yet"
    );
    session.remove(id, false).unwrap();
    assert!(session.snapshot().jobs.is_empty());
    assert_eq!(session.pause(id), Err(EngineError::UnknownJob));
}

#[test]
fn tracker_clock_round_trip() {
    let mut session = session();
    let id = session.add(spec()).unwrap();
    let tracker = session.add_tracker(id).unwrap();
    assert_eq!(tracker, 0);
    assert_eq!(session.poll_announce(id, 0, false).unwrap(), vec![0]);
    assert!(
        session.poll_announce(id, 0, false).unwrap().is_empty(),
        "no overlap while in flight"
    );
    session.tracker_success(id, 0, 0, 100, None).unwrap();
    assert!(
        session.poll_announce(id, 50, false).unwrap().is_empty(),
        "interval respected"
    );
    assert_eq!(session.poll_announce(id, 101, false).unwrap(), vec![0]);
    session.tracker_failure(id, 0, 200, 0).unwrap();
    assert!(
        session.poll_announce(id, 210, false).unwrap().is_empty(),
        "30 s backoff"
    );
    assert_eq!(session.poll_announce(id, 231, false).unwrap(), vec![0]);
}

#[test]
fn duplicate_and_invalid_blocks_are_dropped() {
    let mut session = session();
    let id = session.add(spec()).unwrap();
    let peer = ready_peer(&mut session, id);
    let report = session.update(0, &AcceptAll);
    let requests = requests_of(&report);
    let (piece, begin, length) = requests[0];
    session
        .inject_block(id, peer, piece, begin, vec![7u8; length as usize])
        .unwrap();
    session.update(0, &AcceptAll);
    // Same block again: no longer pending, so dropped as duplicate.
    session
        .inject_block(id, peer, piece, begin, vec![7u8; length as usize])
        .unwrap();
    // Wrong length for a still-pending request (the other pieces were
    // pipelined in the first round): invalid.
    let report = session.update(0, &AcceptAll);
    let mut stale = report.dropped_stale;
    let next = requests[1];
    session
        .inject_block(id, peer, next.0, next.1, vec![7u8; 1])
        .unwrap();
    let report = session.update(0, &AcceptAll);
    stale += report.dropped_stale;
    assert!(stale >= 2, "duplicate + invalid dropped, got {stale}");
}

#[test]
fn lease_expiry_releases_dead_piece() {
    let mut session = session();
    let id = session.add(spec()).unwrap();
    let peer = ready_peer(&mut session, id);
    let report = session.update(0, &AcceptAll);
    assert_eq!(report.leases_issued, 4, "one lease per small piece");
    // Peer goes silent: after the timeout the piece returns to the pool and
    // a later round leases it again.
    session.disconnect(id, peer).unwrap();
    let peer2 = session.connect(id).unwrap();
    session.set_peer_choking(id, peer2, false).unwrap();
    session.note_bitfield(id, peer2, vec![0b1111]).unwrap();
    let report = session.update(31_000_000_000, &AcceptAll);
    assert!(
        report.leases_issued >= 1,
        "expired lease re-issued, report: {report:?}"
    );
}

#[test]
fn resume_adopts_matching_pieces_without_rewrite() {
    let sink = SharedSink::default();
    let mut session = session_with_sink(sink.clone());
    let id = session.add(spec()).unwrap();
    {
        let mut stored = sink.pieces.lock().unwrap();
        stored.insert((id.0, 0), vec![0xAAu8; 16384]);
        stored.insert((id.0, 1), vec![0xAAu8; 16384]);
    }
    let report = session.recheck(id, &sink, &AcceptAll).unwrap();
    assert_eq!(report.verified, 2);
    assert_eq!(report.missing, 2);
    assert_eq!(report.checked, 2);
    assert!(!report.completed);
    assert_eq!(session.snapshot().jobs[0].verified_bytes, 32768);

    // Download the rest; resumed pieces must never be requested again.
    let peer = ready_peer(&mut session, id);
    let mut requested = Vec::new();
    for _ in 0..40 {
        let update = session.update(0, &AcceptAll);
        for (piece, begin, length) in requests_of(&update) {
            requested.push(piece);
            session
                .inject_block(id, peer, piece, begin, vec![piece as u8; length as usize])
                .unwrap();
        }
        if session.snapshot().jobs[0].state == JobState::Seeding {
            break;
        }
    }
    assert_eq!(session.snapshot().jobs[0].state, JobState::Seeding);
    assert!(
        !requested.contains(&0) && !requested.contains(&1),
        "resumed pieces re-requested: {requested:?}"
    );
    assert!(requested.contains(&2) && requested.contains(&3));
    let stored = sink.pieces.lock().unwrap();
    assert_eq!(stored.len(), 4);
    for piece in [0u32, 1] {
        assert!(
            stored[&(id.0, piece)].iter().all(|&b| b == 0xAA),
            "resumed piece {piece} rewritten"
        );
    }
    for piece in [2u32, 3] {
        assert!(
            stored[&(id.0, piece)].iter().all(|&b| b == piece as u8),
            "missing piece {piece} has wrong bytes"
        );
    }
}

#[test]
fn resume_rejects_corrupt_and_short_pieces_then_retries() {
    let sink = SharedSink::default();
    let mut session = session_with_sink(sink.clone());
    let id = session.add(spec()).unwrap();
    {
        let mut stored = sink.pieces.lock().unwrap();
        stored.insert((id.0, 0), vec![9u8; 16384]);
        stored.insert((id.0, 1), vec![9u8; 100]);
        stored.insert((id.0, 2), vec![9u8; 16384]);
    }
    let report = session.recheck(id, &sink, &RejectPiece(2)).unwrap();
    assert_eq!(report.verified, 1, "only piece 0: {report:?}");
    assert_eq!(report.failed, 2, "short + corrupt: {report:?}");
    assert_eq!(report.missing, 1);
    assert_eq!(report.checked, 3);
    assert_eq!(session.snapshot().jobs[0].verified_bytes, 16384);

    // Fix the short piece and accept the previously corrupt one.
    sink.pieces
        .lock()
        .unwrap()
        .insert((id.0, 1), vec![9u8; 16384]);
    let report = session.recheck(id, &sink, &AcceptAll).unwrap();
    assert_eq!(report.verified, 2, "pieces 1-2 now adopted: {report:?}");
    assert_eq!(report.missing, 1);
    assert_eq!(session.snapshot().jobs[0].verified_bytes, 16384 * 3);
}

#[test]
fn resume_full_match_seeds_without_network() {
    let sink = SharedSink::default();
    let mut session = session_with_sink(sink.clone());
    let id = session.add(spec()).unwrap();
    {
        let mut stored = sink.pieces.lock().unwrap();
        for piece in 0..3u32 {
            stored.insert((id.0, piece), vec![3u8; 16384]);
        }
        stored.insert((id.0, 3), vec![3u8; 848]);
    }
    let report = session.recheck(id, &sink, &AcceptAll).unwrap();
    assert_eq!(report.verified, 4);
    assert!(report.completed);
    let snap = session.snapshot();
    assert_eq!(snap.jobs[0].state, JobState::Seeding);
    assert_eq!(snap.jobs[0].verified_bytes, 50_000);

    let _peer = ready_peer(&mut session, id);
    let report = session.update(0, &AcceptAll);
    assert_eq!(report.leases_issued, 0, "seeding issues no leases");
    assert!(requests_of(&report).is_empty());
}

#[test]
fn resume_read_error_marks_error_and_keeps_progress() {
    let sink = SharedSink::default();
    let mut session = session_with_sink(sink.clone());
    let id = session.add(spec()).unwrap();
    sink.pieces
        .lock()
        .unwrap()
        .insert((id.0, 0), vec![5u8; 16384]);
    let failing = FailOnPiece {
        inner: sink.clone(),
        fail: 1,
    };
    let err = session.recheck(id, &failing, &AcceptAll).unwrap_err();
    assert!(matches!(err, EngineError::Storage(_)));
    assert_eq!(session.snapshot().jobs[0].state, JobState::Error);
    assert_eq!(
        session.snapshot().jobs[0].verified_bytes,
        16384,
        "piece 0 kept despite abort"
    );

    session.resume(id).unwrap();
    let report = session.recheck(id, &sink, &AcceptAll).unwrap();
    assert_eq!(report.verified, 0, "piece 0 already adopted: {report:?}");
    assert_eq!(report.skipped, 1);
    assert_eq!(report.missing, 3);
    assert_eq!(session.snapshot().jobs[0].state, JobState::Downloading);
}

#[test]
fn limits_reject_impossible_piece_working_set_without_losing_progress() {
    let limits = Limits {
        payload_budget_bytes: 100,
        ..Limits::default()
    };
    let sink = SharedSink::default();
    let mut session = session_with_sink(sink.clone());
    let id = session.add(spec()).unwrap();
    {
        let mut stored = sink.pieces.lock().unwrap();
        for piece in 0..3u32 {
            stored.insert((id.0, piece), vec![1u8; 16384]);
        }
        stored.insert((id.0, 3), vec![1u8; 848]);
    }
    assert!(matches!(
        session.set_limits(limits, 0),
        Err(EngineError::Unsupported(_))
    ));
    let report = session.recheck(id, &sink, &AcceptAll).unwrap();
    assert_eq!(
        report.verified, 4,
        "failed settings must preserve the previous budget"
    );
    assert_eq!(session.snapshot().jobs[0].verified_bytes, 50_000);
}

#[test]
fn upload_reserves_shared_memory_and_global_rate_using_fake_clock() {
    let sink = SharedSink::default();
    let mut session = session_with_sink(sink.clone());
    let id = session.add(spec()).unwrap();
    assert!(session.reserve_upload(id, 0, 0, 16384, 0).is_err());
    sink.pieces
        .lock()
        .unwrap()
        .insert((id.0, 0), vec![1; 16384]);
    session.recheck(id, &sink, &AcceptAll).unwrap();
    let limits = Limits {
        upload_kb_s: 1,
        ..Limits::default()
    };
    session.set_limits(limits.clone(), 0).unwrap();
    let small_memory = Limits {
        payload_budget_bytes: 16384 * 3,
        ..limits.clone()
    };
    session.set_limits(small_memory, 0).unwrap();
    assert!(
        session
            .reserve_upload(id, 0, 0, 16384, 0)
            .unwrap()
            .is_none()
    );
    session.set_limits(limits, 0).unwrap();
    let (_, permit) = session.reserve_upload(id, 0, 0, 16384, 0).unwrap().unwrap();
    assert_eq!(session.payload_stats().reserved_bytes, 16384 * 3 + 64);
    assert!(session.reserve_upload(id, 0, 0, 1, 0).unwrap().is_none());
    let second = session.add(spec()).unwrap();
    sink.pieces
        .lock()
        .unwrap()
        .insert((second.0, 0), vec![1; 16384]);
    session.recheck(second, &sink, &AcceptAll).unwrap();
    assert!(
        session
            .reserve_upload(second, 0, 0, 1, 0)
            .unwrap()
            .is_none(),
        "upload rate must be shared across jobs"
    );
    drop(permit);
    assert_eq!(session.payload_stats().reserved_bytes, 0);
    assert!(
        session
            .reserve_upload(id, 0, 0, 1000, 999_999_999)
            .unwrap()
            .is_none()
    );
    assert!(
        session
            .reserve_upload(id, 0, 0, 1000, 1_000_000_000)
            .unwrap()
            .is_some()
    );
    assert!(
        session
            .reserve_upload(id, 0, 16384, 1, 2_000_000_000)
            .is_err()
    );
    assert!(
        session
            .reserve_upload(id, 0, 0, 16385, 2_000_000_000)
            .is_err()
    );
    assert!(
        session
            .reserve_upload(id, u32::MAX, 0, 1, 2_000_000_000)
            .is_err()
    );
    session.pause(id).unwrap();
    assert!(
        session
            .reserve_upload(id, 0, 0, 1, 3_000_000_000)
            .unwrap()
            .is_none()
    );
    session.resume(id).unwrap();
    session.upload_read_failed(id, 0).unwrap();
    assert_eq!(session.snapshot().jobs[0].verified_bytes, 0);
    assert_eq!(session.snapshot().jobs[0].state, JobState::Error);
    assert!(
        session
            .reserve_upload(id, 0, 0, 1, 4_000_000_000)
            .unwrap()
            .is_none()
    );
}

#[test]
fn seed_slots_and_upload_accounting_do_not_claim_queued_bytes() {
    let sink = SharedSink::default();
    let mut session = session_with_sink(sink.clone());
    let id = session.add(spec()).unwrap();
    for (piece, len) in [(0, 16384), (1, 16384), (2, 16384), (3, 848)] {
        sink.pieces
            .lock()
            .unwrap()
            .insert((id.0, piece), vec![1; len]);
    }
    session.recheck(id, &sink, &AcceptAll).unwrap();
    let (_, permit) = session.reserve_upload(id, 3, 0, 848, 0).unwrap().unwrap();
    assert_eq!(session.snapshot().jobs[0].total_payload_uploaded, 0);
    session.uploaded(id, 848).unwrap();
    drop(permit);
    assert_eq!(session.snapshot().jobs[0].total_payload_uploaded, 848);
    let limits = Limits {
        seed_jobs: 0,
        ..Limits::default()
    };
    session.set_limits(limits, 0).unwrap();
    assert!(!session.upload_admitted(id));
    assert!(session.reserve_upload(id, 0, 0, 1, 0).unwrap().is_none());
}
