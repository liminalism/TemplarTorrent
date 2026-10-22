//! Session wiring: jobs in stable display order, one lease coordinator each,
//! a global arbiter, bounded workers and a fenced event inbox. All clocks are
//! caller-supplied; all queues are capped. Rbit transports and file-backed
//! storage drive these same seams in deterministic integration tests.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

use templartorrent_core::{
    model::{JobState, Limits, Snapshot, TorrentId, TorrentRow},
    sequence::{FileSpec, SKIP, Sequence, SequenceError},
    tracker::TrackerClock,
};

use crate::{
    EngineError,
    arbiter::SessionArbiter,
    event::{Event, EventBody, PeerId},
    lease::{LeaseCoordinator, LeaseOutcome},
    peer::PeerAction,
    peer::PeerActor,
    worker::{DiskWorker, PieceReader, PieceSink, PieceVerifier, WorkItem, WorkOutcome},
};

/// Hard cap on jobs per session.
pub const MAX_JOBS: usize = 4096;
/// Hard cap on queued inbound events.
pub const MAX_INBOX_EVENTS: usize = 4096;
/// Largest piece the scaffold will lease (keeps wire offsets in u32).
pub const MAX_PIECE_LENGTH: u64 = 64 << 20;
/// Lease lifetime; expired leases return to the pickable pool.
pub const LEASE_TIMEOUT_NS: u64 = 30_000_000_000;
pub const MAX_ACTIVE_PIECES: usize = 64;

/// Assembly plus all incoming block copies plus one transport copy. Reserved
/// before requests, retained through verification/write. Socket buffers and
/// untrusted upstream frame allocation remain outside this engine budget.
pub fn piece_working_set(length: usize) -> Option<usize> {
    length.checked_mul(3)
}

struct Assembly {
    item: WorkItem,
    remaining: usize,
}

/// Allocation capacities at a coordinator boundary, separate from reserved
/// copy headroom. Excludes fixture caches and upstream socket buffers.
#[derive(Debug, Default, Clone, Copy)]
pub struct PayloadStats {
    pub reserved_bytes: usize,
    pub reserved_high_watermark: usize,
    pub assembly_bytes: usize,
    pub inbox_bytes: usize,
    pub queued_write_bytes: usize,
}

impl PayloadStats {
    pub fn owned_bytes(&self) -> usize {
        self.assembly_bytes + self.inbox_bytes + self.queued_write_bytes
    }
}

#[derive(Debug, Clone)]
pub struct JobSpec {
    pub name: String,
    pub files: Vec<FileSpec>,
    pub piece_length: u64,
    pub max_pieces: usize,
}

#[derive(Debug, Clone)]
pub struct OutboxEntry {
    pub torrent: TorrentId,
    pub peer: PeerId,
    pub action: PeerAction,
}

/// One arrived block travelling the drain path. Bundled so the pipeline stays
/// within the argument-count lint without weakening any check.
struct BlockArrival {
    torrent: TorrentId,
    peer: Option<PeerId>,
    generation: u64,
    piece: u32,
    begin: u32,
    data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct BlameEntry {
    pub torrent: TorrentId,
    pub peer: PeerId,
    pub piece: u32,
}

#[derive(Debug, Default)]
pub struct SessionReport {
    pub verified: Vec<(TorrentId, u32)>,
    pub leases_issued: usize,
    pub outbox: Vec<OutboxEntry>,
    pub outcomes: usize,
    pub dropped_stale: usize,
    pub dropped_over_capacity: usize,
    pub completed: Vec<TorrentId>,
    pub connected: Vec<(TorrentId, PeerId)>,
    /// One entry per hash failure with an attributed supplier. Attribution is
    /// exact while each queued item holds one peer's bytes; per-peer scoring
    /// and quarantine consume this in later work.
    pub blamed: Vec<BlameEntry>,
    pub errors: Vec<EngineError>,
}

/// Outcome of one synchronous resume scan. `checked` counts pieces with bytes
/// present on disk; `verified` is the subset that hash-verified and was
/// adopted without any network fetch or rewrite.
#[derive(Debug, Default)]
pub struct RecheckReport {
    pub checked: usize,
    pub verified: usize,
    pub failed: usize,
    pub missing: usize,
    pub skipped: usize,
    pub completed: bool,
}

struct Job {
    id: TorrentId,
    name: Arc<str>,
    state: JobState,
    coordinator: LeaseCoordinator,
    peers: HashMap<PeerId, PeerActor>,
    trackers: Vec<TrackerClock>,
    downloaded_bytes: u64,
    uploaded_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackerSchedule {
    pub in_flight: bool,
    pub periodic_due_s: u64,
    pub manual_due_s: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProgress {
    pub path: String,
    pub length: u64,
    pub verified_bytes: u64,
    pub wanted: bool,
    pub rank: u32,
}

pub struct Session {
    jobs: Vec<Job>,
    next_torrent: u64,
    next_peer: u64,
    arbiter: SessionArbiter,
    workers: Vec<DiskWorker>,
    inbox: VecDeque<Event>,
    /// Pieces with bytes sitting in worker queues. Consulted before re-offer
    /// and re-admission so the confirm-to-verify window cannot duplicate work.
    verifying: HashSet<(u64, u32)>,
    assemblies: HashMap<(u64, u32), Assembly>,
    pending_outbox: Vec<OutboxEntry>,
    dropped_ingress: usize,
    snapshot_seq: u64,
}

impl Session {
    /// Only synced, hash-verified pieces are advertised to upload peers.
    pub fn verified_pieces(&self, id: TorrentId) -> Result<Vec<u64>, EngineError> {
        let job = self
            .jobs
            .iter()
            .find(|j| j.id == id)
            .ok_or(EngineError::UnknownJob)?;
        Ok(job.coordinator.sequence().verified_words().to_vec())
    }

    pub fn reserve_control(&self, bytes: usize) -> Option<templartorrent_core::budget::Permit> {
        self.arbiter.budget().try_acquire(bytes)
    }

    pub fn upload_admitted(&self, id: TorrentId) -> bool {
        let states: Vec<_> = self.jobs.iter().map(|j| (j.id, j.state)).collect();
        self.arbiter.admit(&states).contains(&id)
    }

    /// Reserve the full disk read, block copy and encoded wire copy before
    /// allocation. Bandwidth uses the same process-wide fake-clock arbiter as
    /// downloads. A failed reservation does not consume rate tokens.
    pub fn reserve_upload(
        &mut self,
        id: TorrentId,
        piece: u32,
        begin: u32,
        length: u32,
        now_ns: u64,
    ) -> Result<Option<(usize, templartorrent_core::budget::Permit)>, EngineError> {
        let job = self
            .jobs
            .iter()
            .find(|j| j.id == id)
            .ok_or(EngineError::UnknownJob)?;
        let states: Vec<_> = self.jobs.iter().map(|j| (j.id, j.state)).collect();
        if !matches!(job.state, JobState::Downloading | JobState::Seeding)
            || !self.arbiter.admit(&states).contains(&id)
        {
            return Ok(None);
        }
        let full = job
            .coordinator
            .piece_len(piece)
            .ok_or(EngineError::Unsupported("invalid upload piece"))?;
        if length == 0
            || length > 16384
            || (begin as usize)
                .checked_add(length as usize)
                .is_none_or(|end| end > full)
            || !job.coordinator.sequence().is_verified(piece as usize)
        {
            return Err(EngineError::Unsupported(
                "unverified or invalid upload span",
            ));
        }
        let bytes = full
            .checked_add(length as usize * 2 + 64)
            .ok_or(EngineError::LimitOverflow)?;
        let Some(permit) = self.arbiter.budget().try_acquire(bytes) else {
            return Ok(None);
        };
        if !self.arbiter.reserve_up(u64::from(length), now_ns) {
            return Ok(None);
        }
        Ok(Some((full, permit)))
    }

    /// Credit only complete successful transport sends, never queued writes.
    pub fn uploaded(&mut self, id: TorrentId, bytes: u32) -> Result<(), EngineError> {
        let job = self.job_mut(id)?;
        job.uploaded_bytes = job.uploaded_bytes.saturating_add(u64::from(bytes));
        Ok(())
    }

    /// External disk mutation discovered while serving invalidates progress and
    /// stops the job. A later explicit resume/recheck can recover safely.
    pub fn upload_read_failed(&mut self, id: TorrentId, piece: u32) -> Result<(), EngineError> {
        self.cancel_work(id)?;
        let job = self.job_mut(id)?;
        job.coordinator.invalidate(piece)?;
        job.state = JobState::Error;
        Ok(())
    }

    pub fn new(
        limits: Limits,
        sinks: Vec<Box<dyn PieceSink>>,
        worker_cap: usize,
        now_ns: u64,
    ) -> Result<Self, EngineError> {
        Ok(Self {
            jobs: Vec::new(),
            next_torrent: 1,
            next_peer: 1,
            arbiter: SessionArbiter::new(limits, now_ns)?,
            workers: sinks
                .into_iter()
                .map(|sink| DiskWorker::new(sink, worker_cap))
                .collect(),
            inbox: VecDeque::new(),
            verifying: HashSet::new(),
            assemblies: HashMap::new(),
            pending_outbox: Vec::new(),
            dropped_ingress: 0,
            snapshot_seq: 0,
        })
    }

    pub fn add(&mut self, spec: JobSpec) -> Result<TorrentId, EngineError> {
        if self.jobs.len() >= MAX_JOBS {
            return Err(EngineError::TooManyJobs);
        }
        if spec.piece_length == 0 || spec.piece_length > MAX_PIECE_LENGTH {
            return Err(EngineError::LimitOverflow);
        }
        let sequence = Sequence::new(spec.files, spec.piece_length, spec.max_pieces)?;
        let total = sequence.total_size();
        let largest = spec.piece_length.min(total) as usize;
        if piece_working_set(largest).is_none_or(|bytes| bytes > self.arbiter.budget().limit()) {
            return Err(EngineError::Unsupported(
                "piece assembly working set exceeds payload budget",
            ));
        }
        let id = TorrentId(self.next_torrent);
        self.next_torrent += 1;
        let state = if sequence.piece_ranks().iter().all(|rank| *rank == SKIP) {
            JobState::Seeding
        } else {
            JobState::Downloading
        };
        self.jobs.push(Job {
            id,
            name: Arc::from(spec.name.as_str()),
            state,
            coordinator: LeaseCoordinator::new(id, sequence, spec.piece_length, total),
            peers: HashMap::new(),
            trackers: Vec::new(),
            downloaded_bytes: 0,
            uploaded_bytes: 0,
        });
        Ok(id)
    }

    pub fn pause(&mut self, id: TorrentId) -> Result<(), EngineError> {
        let job = self.job_mut(id)?;
        if matches!(job.state, JobState::Downloading | JobState::Seeding) {
            job.state = JobState::Paused;
        }
        self.cancel_work(id)?;
        Ok(())
    }

    pub fn resume(&mut self, id: TorrentId) -> Result<(), EngineError> {
        let job = self.job_mut(id)?;
        if matches!(job.state, JobState::Paused | JobState::Error) {
            job.state = if Self::is_complete(job) {
                JobState::Seeding
            } else {
                JobState::Downloading
            };
        }
        Ok(())
    }

    /// Resume scan: adopt existing on-disk pieces that match this torrent.
    ///
    /// For each unverified, unheld piece the session reads bytes via `reader`,
    /// checks the exact expected length, hash-verifies via `verifier`, and only
    /// then marks the piece verified. Adopted pieces are never re-requested or
    /// rewritten. Missing, short, or corrupt pieces stay unverified for normal
    /// download. Call before connecting peers (or while paused) so file
    /// completion cannot fence active leases. One piece is held in memory at a
    /// time under the global payload budget; pieces that do not fit are
    /// skipped for a later scan. A storage read error marks the job `Error`
    /// and aborts the scan, keeping pieces adopted so far.
    pub fn recheck(
        &mut self,
        id: TorrentId,
        reader: &dyn PieceReader,
        verifier: &dyn PieceVerifier,
    ) -> Result<RecheckReport, EngineError> {
        let Some(index) = self.jobs.iter().position(|job| job.id == id) else {
            return Err(EngineError::UnknownJob);
        };
        let piece_count = self.jobs[index].coordinator.sequence().piece_count();
        let budget = self.arbiter.budget().clone();
        let mut report = RecheckReport::default();
        for piece in 0..piece_count as u32 {
            let skip = self.jobs[index]
                .coordinator
                .sequence()
                .is_verified(piece as usize)
                || self.verifying.contains(&(id.0, piece))
                || self.jobs[index].coordinator.is_held(piece);
            if skip {
                report.skipped += 1;
                continue;
            }
            let Some(expected_len) = self.jobs[index].coordinator.piece_len(piece) else {
                return Err(EngineError::Sequence(
                    templartorrent_core::sequence::SequenceError::InvalidPiece,
                ));
            };
            let Some(_permit) = budget.try_acquire(expected_len) else {
                report.skipped += 1;
                continue;
            };
            match reader.read_piece(id, piece, expected_len) {
                Err(error) => {
                    self.jobs[index].state = JobState::Error;
                    return Err(EngineError::Storage(error));
                }
                Ok(None) => report.missing += 1,
                Ok(Some(data)) => {
                    report.checked += 1;
                    if data.len() != expected_len || !verifier.verify(piece, &data) {
                        report.failed += 1;
                        continue;
                    }
                    if self.jobs[index].coordinator.adopt_verified(piece)? {
                        report.verified += 1;
                    } else {
                        report.skipped += 1;
                    }
                }
            }
        }
        if Self::is_complete(&self.jobs[index]) {
            report.completed = true;
            if self.jobs[index].state != JobState::Paused {
                self.jobs[index].state = JobState::Seeding;
            }
        }
        Ok(report)
    }

    /// Mark pieces verified without reading them. The caller has already
    /// decided their durable bytes match the torrent. Out-of-range pieces
    /// fail before any piece in `pieces` is adopted. Held or in-flight pieces
    /// are left alone. Returns how many pieces newly became verified.
    pub fn adopt_trusted(&mut self, id: TorrentId, pieces: &[u32]) -> Result<usize, EngineError> {
        let Some(index) = self.jobs.iter().position(|job| job.id == id) else {
            return Err(EngineError::UnknownJob);
        };
        let count = self.jobs[index].coordinator.sequence().piece_count();
        if pieces.iter().any(|piece| *piece as usize >= count) {
            return Err(EngineError::Sequence(SequenceError::InvalidPiece));
        }
        let mut adopted = 0;
        for &piece in pieces {
            if self.jobs[index]
                .coordinator
                .sequence()
                .is_verified(piece as usize)
                || self.verifying.contains(&(id.0, piece))
                || self.jobs[index].coordinator.is_held(piece)
            {
                continue;
            }
            if self.jobs[index].coordinator.adopt_verified(piece)? {
                adopted += 1;
            }
        }
        if Self::is_complete(&self.jobs[index]) && self.jobs[index].state != JobState::Paused {
            self.jobs[index].state = JobState::Seeding;
        }
        Ok(adopted)
    }

    /// Non-destructive removal only; data deletion arrives with safe storage.
    pub fn remove(&mut self, id: TorrentId, delete_data: bool) -> Result<(), EngineError> {
        if delete_data {
            return Err(EngineError::Unsupported("data deletion"));
        }
        let Some(index) = self.jobs.iter().position(|job| job.id == id) else {
            return Err(EngineError::UnknownJob);
        };
        self.cancel_work(id)?;
        self.inbox.retain(|event| event.torrent != id);
        let job = self.jobs.remove(index);
        for _ in 0..job.peers.len() {
            self.arbiter.close(id);
        }
        Ok(())
    }

    pub fn set_wanted(
        &mut self,
        id: TorrentId,
        file: usize,
        wanted: bool,
    ) -> Result<(), EngineError> {
        self.job_mut(id)?
            .coordinator
            .sequence_mut()
            .set_wanted(file, wanted)?;
        self.cancel_work(id)?;
        self.resettle_completion(id)
    }

    /// A selection change can finish a job whose remaining wanted files are
    /// already verified, or reopen a seeding job that now wants more. Paused
    /// and errored jobs keep their state; the next resume re-evaluates.
    fn resettle_completion(&mut self, id: TorrentId) -> Result<(), EngineError> {
        let job = self.job_mut(id)?;
        let complete = Self::is_complete(job);
        match job.state {
            JobState::Downloading if complete => job.state = JobState::Seeding,
            JobState::Seeding if !complete => job.state = JobState::Downloading,
            _ => {}
        }
        Ok(())
    }

    /// The current file-order permutation, as accepted by [`Session::set_order`].
    pub fn file_order(&self, id: TorrentId) -> Result<Vec<usize>, EngineError> {
        let job = self
            .jobs
            .iter()
            .find(|job| job.id == id)
            .ok_or(EngineError::UnknownJob)?;
        Ok(job.coordinator.sequence().order().to_vec())
    }

    pub fn set_order(&mut self, id: TorrentId, order: Vec<usize>) -> Result<(), EngineError> {
        self.job_mut(id)?
            .coordinator
            .sequence_mut()
            .set_order(order)?;
        self.cancel_work(id)
    }

    pub fn set_limits(&mut self, limits: Limits, now_ns: u64) -> Result<(), EngineError> {
        for job in &self.jobs {
            let largest = job.coordinator.piece_len(0).unwrap_or(0);
            if piece_working_set(largest).is_none_or(|bytes| bytes > limits.payload_budget_bytes) {
                return Err(EngineError::Unsupported(
                    "piece assembly working set exceeds payload budget",
                ));
            }
        }
        self.arbiter.set_limits(limits, now_ns)
    }

    fn cancel_work(&mut self, id: TorrentId) -> Result<(), EngineError> {
        let job = self.job_mut(id)?;
        job.coordinator.cancel_all();
        let generation = job.coordinator.generation();
        let mut actions = Vec::new();
        for (peer, actor) in &mut job.peers {
            actions.extend(
                actor
                    .cancel_all(generation)
                    .into_iter()
                    .map(|action| OutboxEntry {
                        torrent: id,
                        peer: *peer,
                        action,
                    }),
            );
        }
        self.pending_outbox.extend(actions);
        // Drop queued bytes before releasing their assembly reservations.
        let before = self.inbox.len();
        self.inbox
            .retain(|event| event.torrent != id || !matches!(event.body, EventBody::Block { .. }));
        self.dropped_ingress += before - self.inbox.len();
        for worker in &mut self.workers {
            worker.cancel(id);
        }
        self.verifying.retain(|(torrent, _)| *torrent != id.0);
        self.assemblies.retain(|(torrent, _), _| *torrent != id.0);
        Ok(())
    }

    pub fn add_tracker(&mut self, id: TorrentId) -> Result<usize, EngineError> {
        let job = self.job_mut(id)?;
        job.trackers.push(TrackerClock::new());
        Ok(job.trackers.len() - 1)
    }

    /// Admit one tracker announce. `false` means the cooldown or an in-flight
    /// request rejected it; the clock is unchanged in that case.
    pub fn begin_announce(
        &mut self,
        id: TorrentId,
        tracker: usize,
        now_s: u64,
        manual: bool,
    ) -> Result<bool, EngineError> {
        let job = self.job_mut(id)?;
        let clock = job
            .trackers
            .get_mut(tracker)
            .ok_or(EngineError::Unsupported("unknown tracker"))?;
        Ok(clock.begin(now_s, manual))
    }

    pub fn tracker_schedule(
        &self,
        id: TorrentId,
        tracker: usize,
    ) -> Result<TrackerSchedule, EngineError> {
        let job = self
            .jobs
            .iter()
            .find(|job| job.id == id)
            .ok_or(EngineError::UnknownJob)?;
        let clock = job
            .trackers
            .get(tracker)
            .ok_or(EngineError::Unsupported("unknown tracker"))?;
        Ok(TrackerSchedule {
            in_flight: clock.in_flight(),
            periodic_due_s: clock.next_due(),
            manual_due_s: clock.manual_due(),
        })
    }

    pub fn file_progress(&self, id: TorrentId) -> Result<Vec<FileProgress>, EngineError> {
        let job = self
            .jobs
            .iter()
            .find(|job| job.id == id)
            .ok_or(EngineError::UnknownJob)?;
        let sequence = job.coordinator.sequence();
        let verified = sequence.file_verified_bytes();
        let ranks = sequence.file_ranks();
        Ok(sequence
            .files()
            .iter()
            .enumerate()
            .map(|(index, file)| FileProgress {
                path: file.path.clone(),
                length: file.length,
                verified_bytes: verified[index],
                wanted: file.wanted,
                rank: ranks[index],
            })
            .collect())
    }

    /// Tracker indices whose announce clock admits an attempt right now.
    pub fn poll_announce(
        &mut self,
        id: TorrentId,
        now_s: u64,
        manual: bool,
    ) -> Result<Vec<usize>, EngineError> {
        let job = self.job_mut(id)?;
        let mut ready = Vec::new();
        for (index, clock) in job.trackers.iter_mut().enumerate() {
            if clock.begin(now_s, manual) {
                ready.push(index);
            }
        }
        Ok(ready)
    }

    pub fn tracker_success(
        &mut self,
        id: TorrentId,
        tracker: usize,
        now_s: u64,
        interval: u32,
        min_interval: Option<u32>,
    ) -> Result<(), EngineError> {
        let job = self.job_mut(id)?;
        if let Some(clock) = job.trackers.get_mut(tracker) {
            clock.success(now_s, interval, min_interval);
        }
        Ok(())
    }

    pub fn tracker_failure(
        &mut self,
        id: TorrentId,
        tracker: usize,
        now_s: u64,
        jitter_seconds: u64,
    ) -> Result<(), EngineError> {
        let job = self.job_mut(id)?;
        if let Some(clock) = job.trackers.get_mut(tracker) {
            clock.failure(now_s, jitter_seconds);
        }
        Ok(())
    }

    /// Register a peer connection. The transport behind it arrives later;
    /// until then the test harness drives events explicitly.
    pub fn connect(&mut self, torrent: TorrentId) -> Result<PeerId, EngineError> {
        if !self.jobs.iter().any(|job| job.id == torrent) {
            return Err(EngineError::UnknownJob);
        }
        if !self.arbiter.try_open(torrent) {
            return Err(EngineError::NoCapacity);
        }
        let peer = PeerId(self.next_peer);
        self.next_peer += 1;
        let job = self.job_mut(torrent)?;
        let words = job.coordinator.sequence().piece_count().div_ceil(64);
        let generation = job.coordinator.generation();
        job.coordinator.add_peer(peer);
        job.peers
            .insert(peer, PeerActor::new(peer, torrent, generation, words));
        Ok(peer)
    }

    /// Transport hook for remote choke/unchoke. Applies to the connection
    /// regardless of generation: choking is wire state, not lease state.
    pub fn set_peer_choking(
        &mut self,
        torrent: TorrentId,
        peer: PeerId,
        choking: bool,
    ) -> Result<(), EngineError> {
        let job = self.job_mut(torrent)?;
        let Some(actor) = job.peers.get_mut(&peer) else {
            return Err(EngineError::UnknownPeer);
        };
        if choking {
            actor.on_choke();
        } else {
            actor.on_unchoke();
        }
        Ok(())
    }

    /// Per-peer request pipeline cap, clamped to `1..=DEFAULT_MAX_IN_FLIGHT`.
    /// Local peer preference narrows an avoided peer without disconnecting it.
    pub fn set_peer_pipeline(
        &mut self,
        torrent: TorrentId,
        peer: PeerId,
        max: usize,
    ) -> Result<(), EngineError> {
        let job = self.job_mut(torrent)?;
        let actor = job.peers.get_mut(&peer).ok_or(EngineError::UnknownPeer)?;
        actor.set_max_in_flight(max);
        Ok(())
    }

    pub fn peer_pipeline(&self, torrent: TorrentId, peer: PeerId) -> Result<usize, EngineError> {
        let job = self
            .jobs
            .iter()
            .find(|job| job.id == torrent)
            .ok_or(EngineError::UnknownJob)?;
        job.peers
            .get(&peer)
            .map(|actor| actor.max_in_flight())
            .ok_or(EngineError::UnknownPeer)
    }

    pub fn disconnect(&mut self, torrent: TorrentId, peer: PeerId) -> Result<(), EngineError> {
        let job = self.job_mut(torrent)?;
        if job.peers.remove(&peer).is_none() {
            return Err(EngineError::UnknownPeer);
        }
        job.coordinator.remove_peer(peer);
        self.inbox
            .retain(|event| event.torrent != torrent || event.peer != Some(peer));
        self.assemblies
            .retain(|(id, _), assembly| *id != torrent.0 || assembly.item.peer != Some(peer));
        self.arbiter.close(torrent);
        Ok(())
    }

    pub fn note_bitfield(
        &mut self,
        torrent: TorrentId,
        peer: PeerId,
        bits: Vec<u64>,
    ) -> Result<(), EngineError> {
        let generation = self.event_generation(torrent, peer)?;
        self.push(Event {
            torrent,
            peer: Some(peer),
            generation,
            body: EventBody::Bitfield(bits),
        })
    }

    pub fn note_have(
        &mut self,
        torrent: TorrentId,
        peer: PeerId,
        piece: u32,
    ) -> Result<(), EngineError> {
        let generation = self.event_generation(torrent, peer)?;
        self.push(Event {
            torrent,
            peer: Some(peer),
            generation,
            body: EventBody::Have { piece },
        })
    }

    pub fn inject_block(
        &mut self,
        torrent: TorrentId,
        peer: PeerId,
        piece: u32,
        begin: u32,
        data: Vec<u8>,
    ) -> Result<(), EngineError> {
        let generation = self.event_generation(torrent, peer)?;
        // Reject unsolicited/duplicate bytes before they can occupy the inbox.
        // One queued copy per live span keeps aggregate arrivals within the
        // piece reservation acquired before the wire requests were emitted.
        let expected = self
            .jobs
            .iter()
            .find(|job| job.id == torrent)
            .and_then(|job| job.peers.get(&peer))
            .is_some_and(|actor| {
                actor.check_block(piece, begin, data.len(), generation)
                    == crate::peer::BlockDisposition::Accept
            });
        let queued = self.inbox.iter().any(|event| {
            event.torrent == torrent && event.peer == Some(peer)
                && matches!(&event.body, EventBody::Block { piece: p, begin: b, .. } if *p == piece && *b == begin)
        });
        if !expected || queued {
            self.dropped_ingress += 1;
            return Ok(());
        }
        if data.capacity() > data.len() {
            // Transport allocations belong to the caller; only compact block
            // allocations can transfer into the pre-reserved inbox.
            return Err(EngineError::NoCapacity);
        }
        self.push(Event {
            torrent,
            peer: Some(peer),
            generation,
            body: EventBody::Block { piece, begin, data },
        })
    }

    /// Drive one deterministic round: drain fenced events, issue leases within
    /// rate caps, expire stale leases and collect one outcome per worker.
    pub fn update(&mut self, now_ns: u64, verifier: &dyn PieceVerifier) -> SessionReport {
        let mut report = SessionReport {
            dropped_stale: std::mem::take(&mut self.dropped_ingress),
            outbox: std::mem::take(&mut self.pending_outbox),
            ..SessionReport::default()
        };
        // Resync peers to the live generation first: a wanted-set or order
        // change rebuilds the sequence, and pipelines from the old epoch must
        // clear before new events are judged.
        for job in &mut self.jobs {
            let generation = job.coordinator.generation();
            for actor in job.peers.values_mut() {
                if actor.generation() != generation {
                    actor.resync(generation);
                }
            }
        }
        for job in &mut self.jobs {
            for (peer, piece) in job.coordinator.expire(now_ns, LEASE_TIMEOUT_NS) {
                if let Some(actor) = job.peers.get_mut(&peer) {
                    report
                        .outbox
                        .extend(
                            actor
                                .cancel_piece(piece)
                                .into_iter()
                                .map(|action| OutboxEntry {
                                    torrent: job.id,
                                    peer,
                                    action,
                                }),
                        );
                }
                self.inbox.retain(|event| {
                    !(event.torrent == job.id
                        && matches!(&event.body, EventBody::Block { piece: p, .. } if *p == piece))
                });
                self.assemblies.remove(&(job.id.0, piece));
            }
        }
        self.drain(now_ns, &mut report);
        self.flush_assemblies();
        self.issue(now_ns, &mut report);
        let mut outcomes = Vec::new();
        for worker in &mut self.workers {
            if let Some(outcome) = worker.poll(verifier) {
                outcomes.push(outcome);
            }
        }
        for outcome in outcomes {
            report.outcomes += 1;
            self.apply_outcome(outcome, &mut report);
        }
        report
    }

    pub fn is_quiet(&self) -> bool {
        self.inbox.is_empty()
            && self.assemblies.is_empty()
            && self.workers.iter().all(|worker| worker.is_empty())
    }

    pub fn payload_stats(&self) -> PayloadStats {
        PayloadStats {
            reserved_bytes: self.arbiter.budget_used(),
            reserved_high_watermark: self.arbiter.budget_high_watermark(),
            assembly_bytes: self
                .assemblies
                .values()
                .map(|a| a.item.data.capacity())
                .sum(),
            inbox_bytes: self
                .inbox
                .iter()
                .map(|event| match &event.body {
                    EventBody::Block { data, .. } => data.capacity(),
                    _ => 0,
                })
                .sum(),
            queued_write_bytes: self.workers.iter().map(DiskWorker::owned_bytes).sum(),
        }
    }

    pub fn snapshot(&mut self) -> Snapshot {
        self.snapshot_seq += 1;
        let jobs: Vec<TorrentRow> = self
            .jobs
            .iter()
            .map(|job| {
                let sequence = job.coordinator.sequence();
                let wanted: u64 = sequence
                    .files()
                    .iter()
                    .filter(|f| f.wanted)
                    .map(|f| f.length)
                    .sum();
                TorrentRow {
                    id: job.id,
                    name: Arc::clone(&job.name),
                    state: job.state,
                    wanted_bytes: wanted,
                    verified_bytes: sequence.file_verified_bytes().iter().sum(),
                    total_payload_downloaded: job.downloaded_bytes,
                    total_payload_uploaded: job.uploaded_bytes,
                    down_bps: 0,
                    up_bps: 0,
                    connected_seeds: 0,
                    connected_peers: job.peers.len() as u32,
                    estimated_seeds: None,
                    estimated_peers: None,
                }
            })
            .collect();
        Snapshot {
            sequence: self.snapshot_seq,
            jobs: Arc::from(jobs),
            visible_peers: Arc::from(Vec::<templartorrent_core::model::PeerRow>::new()),
            payload_budget_used: self.arbiter.budget_used(),
        }
    }

    fn job_mut(&mut self, id: TorrentId) -> Result<&mut Job, EngineError> {
        self.jobs
            .iter_mut()
            .find(|job| job.id == id)
            .ok_or(EngineError::UnknownJob)
    }

    /// Generation stamped on new inbound events: the live coordinator
    /// generation, after validating the peer exists. Events that predate a
    /// later wanted-set change are fenced at drain time.
    fn event_generation(&mut self, torrent: TorrentId, peer: PeerId) -> Result<u64, EngineError> {
        let job = self.job_mut(torrent)?;
        if !job.peers.contains_key(&peer) {
            return Err(EngineError::UnknownPeer);
        }
        Ok(job.coordinator.generation())
    }

    fn push(&mut self, event: Event) -> Result<(), EngineError> {
        if self.inbox.len() >= MAX_INBOX_EVENTS {
            return Err(EngineError::QueueFull);
        }
        self.inbox.push_back(event);
        Ok(())
    }

    fn drain(&mut self, now_ns: u64, report: &mut SessionReport) {
        while let Some(event) = self.inbox.pop_front() {
            match event.body {
                EventBody::Connected => match self.connect(event.torrent) {
                    Ok(peer) => report.connected.push((event.torrent, peer)),
                    Err(error) => report.errors.push(error),
                },
                EventBody::Disconnected => {
                    if let Some(peer) = event.peer {
                        // Disconnection applies regardless of generation: a
                        // resync only clears pipeline state on the same wire.
                        if self.disconnect(event.torrent, peer).is_err() {
                            report.dropped_stale += 1;
                        }
                    }
                }
                EventBody::Have { piece } => {
                    if !self.apply_to_peer(
                        event.torrent,
                        event.peer,
                        event.generation,
                        report,
                        |coordinator, actor, piece_count| {
                            coordinator.note_have(actor.id(), piece)?;
                            actor.set_have(piece, piece_count);
                            Ok(())
                        },
                    ) {
                        continue;
                    }
                }
                EventBody::Bitfield(bits) => {
                    if !self.apply_to_peer(
                        event.torrent,
                        event.peer,
                        event.generation,
                        report,
                        |coordinator, actor, _| {
                            coordinator.note_bitfield(actor.id(), &bits)?;
                            actor.set_bitfield(&bits);
                            Ok(())
                        },
                    ) {
                        continue;
                    }
                }
                EventBody::Block { piece, begin, data } => {
                    self.apply_block(
                        BlockArrival {
                            torrent: event.torrent,
                            peer: event.peer,
                            generation: event.generation,
                            piece,
                            begin,
                            data,
                        },
                        now_ns,
                        report,
                    );
                }
            }
        }
    }

    /// Route a peer-scoped event with generation fencing. Returns false when
    /// the event was stale or its job/peer is gone (already counted).
    fn apply_to_peer(
        &mut self,
        torrent: TorrentId,
        peer: Option<PeerId>,
        _generation: u64,
        report: &mut SessionReport,
        apply: impl FnOnce(&mut LeaseCoordinator, &mut PeerActor, usize) -> Result<(), EngineError>,
    ) -> bool {
        let Some(peer) = peer else {
            report.dropped_stale += 1;
            return false;
        };
        let Ok(job) = self.job_mut(torrent) else {
            report.dropped_stale += 1;
            return false;
        };
        // Have/bitfield are wire connection state, independent of policy epochs.
        // Peer IDs are never reused, so a retired connection cannot update a new one.
        if !job.peers.contains_key(&peer) {
            report.dropped_stale += 1;
            return false;
        }
        let piece_count = job.coordinator.sequence().piece_count();
        let Job {
            coordinator, peers, ..
        } = job;
        let Some(actor) = peers.get_mut(&peer) else {
            report.dropped_stale += 1;
            return false;
        };
        if let Err(error) = apply(coordinator, actor, piece_count) {
            report.errors.push(error);
            return false;
        }
        true
    }

    fn apply_block(&mut self, arrival: BlockArrival, now_ns: u64, report: &mut SessionReport) {
        let Some(peer) = arrival.peer else {
            report.dropped_stale += 1;
            return;
        };
        // Classify first without mutating pipeline state.
        let disposition = self
            .jobs
            .iter()
            .find(|job| job.id == arrival.torrent)
            .and_then(|job| {
                job.peers.get(&peer).map(|actor| {
                    actor.check_block(
                        arrival.piece,
                        arrival.begin,
                        arrival.data.len(),
                        arrival.generation,
                    )
                })
            });
        match disposition {
            None => report.dropped_stale += 1,
            Some(crate::peer::BlockDisposition::Accept) => {
                self.admit_block(arrival, peer, now_ns, report)
            }
            Some(_) => report.dropped_stale += 1,
        }
    }

    fn admit_block(
        &mut self,
        arrival: BlockArrival,
        peer: PeerId,
        now_ns: u64,
        report: &mut SessionReport,
    ) {
        let BlockArrival {
            torrent,
            piece,
            begin,
            data,
            generation,
            ..
        } = arrival;
        if self.verifying.contains(&(torrent.0, piece)) {
            // Already queued for verification: a duplicate arrival racing the
            // worker. Drop without touching budget or pipeline state.
            report.dropped_stale += 1;
            return;
        }
        let key = (torrent.0, piece);
        let Some(assembly) = self.assemblies.get_mut(&key) else {
            report.dropped_stale += 1;
            return;
        };
        if assembly.item.peer != Some(peer) || assembly.item.generation != generation {
            report.dropped_stale += 1;
            return;
        }
        let start = begin as usize;
        let Some(end) = start
            .checked_add(data.len())
            .filter(|end| *end <= assembly.item.data.len())
        else {
            report.dropped_stale += 1;
            return;
        };
        let job = self.jobs.iter_mut().find(|job| job.id == torrent).unwrap();
        if !job
            .coordinator
            .confirm_block(peer, piece, begin, data.len(), now_ns)
        {
            report.dropped_stale += 1;
            return;
        }
        assembly.item.data[start..end].copy_from_slice(&data);
        assembly.remaining -= data.len();
        job.peers
            .get_mut(&peer)
            .unwrap()
            .confirm_block(piece, begin);
        job.downloaded_bytes += data.len() as u64;
    }

    fn flush_assemblies(&mut self) {
        // Deterministic key order; a full worker leaves the
        // complete assembly owned and charged for the next round.
        let mut ready: Vec<_> = self
            .assemblies
            .iter()
            .filter_map(|(key, assembly)| (assembly.remaining == 0).then_some(*key))
            .collect();
        ready.sort_unstable();
        for key in ready {
            let mut item = Some(self.assemblies.remove(&key).unwrap().item);
            for worker in &mut self.workers {
                if let Some(candidate) = item.take() {
                    match worker.submit(candidate) {
                        Ok(()) => break,
                        Err(returned) => item = Some(returned),
                    }
                }
            }
            if let Some(item) = item {
                self.assemblies.insert(key, Assembly { item, remaining: 0 });
            } else {
                self.verifying.insert(key);
            }
        }
    }

    fn issue(&mut self, now_ns: u64, report: &mut SessionReport) {
        let states: Vec<(TorrentId, JobState)> =
            self.jobs.iter().map(|job| (job.id, job.state)).collect();
        let admitted = self.arbiter.admit(&states);
        for id in admitted {
            let Some(index) = self.jobs.iter().position(|job| job.id == id) else {
                continue;
            };
            if self.jobs[index].state != JobState::Downloading {
                continue;
            }
            let mut peers: Vec<PeerId> = self.jobs[index].peers.keys().copied().collect();
            peers.sort_by_key(|peer| peer.0);
            for peer in peers {
                self.issue_for_peer(index, peer, now_ns, report);
            }
        }
    }

    fn issue_for_peer(
        &mut self,
        index: usize,
        peer: PeerId,
        now_ns: u64,
        report: &mut SessionReport,
    ) {
        // Interest first: a peer with anything we lack gets undivided attention.
        let wants = {
            let job = &self.jobs[index];
            let Some(actor) = job.peers.get(&peer) else {
                return;
            };
            let verified = job.coordinator.sequence().verified_words();
            actor
                .bitfield()
                .iter()
                .zip(verified.iter())
                .any(|(have, done)| have & !done != 0)
        };
        let action = self.jobs[index]
            .peers
            .get_mut(&peer)
            .and_then(|actor| actor.want(wants));
        if let Some(action) = action {
            report.outbox.push(OutboxEntry {
                torrent: self.jobs[index].id,
                peer,
                action,
            });
        }
        let productive = self.jobs[index]
            .peers
            .get(&peer)
            .is_some_and(|actor| actor.is_productive());
        if !productive {
            return;
        }
        // Re-offer every outstanding lease of this peer, then lease further
        // pieces while pipeline slots remain. Small pieces therefore pipeline
        // across pieces instead of one block per round trip; a large piece
        // already fills the pipeline, so nothing changes for it. Bounds stay
        // the pipeline cap, the global active-piece cap and the payload budget.
        let torrent = self.jobs[index].id;
        for lease in self.jobs[index].coordinator.leases_of(peer) {
            if self.free_slots(index, peer) == 0 {
                return;
            }
            if self.verifying.contains(&(torrent.0, lease.piece)) {
                continue;
            }
            let actions = self.jobs[index]
                .peers
                .get_mut(&peer)
                .map(|actor| {
                    actor.request_blocks(&lease, |length| {
                        self.arbiter.reserve_down(u64::from(length), now_ns)
                    })
                })
                .unwrap_or_default();
            report
                .outbox
                .extend(actions.into_iter().map(|action| OutboxEntry {
                    torrent,
                    peer,
                    action,
                }));
        }
        while self.free_slots(index, peer) > 0 {
            let lease = match self.jobs[index].coordinator.pick_for(peer, now_ns) {
                Ok(Some(lease)) => {
                    report.leases_issued += 1;
                    lease
                }
                Ok(None) => break,
                Err(error) => {
                    report.errors.push(error);
                    break;
                }
            };
            if self.verifying.contains(&(torrent.0, lease.piece)) {
                self.jobs[index].coordinator.release_piece(lease.piece);
                break;
            }
            let key = (torrent.0, lease.piece);
            if !self.assemblies.contains_key(&key) {
                if self.assemblies.len() + self.verifying.len() >= MAX_ACTIVE_PIECES {
                    self.jobs[index].coordinator.release_piece(lease.piece);
                    break;
                }
                let length = self.jobs[index].coordinator.piece_len(lease.piece).unwrap();
                let Some(permit) = piece_working_set(length)
                    .and_then(|bytes| self.arbiter.budget().try_acquire(bytes))
                else {
                    self.jobs[index].coordinator.release_piece(lease.piece);
                    break;
                };
                self.assemblies.insert(
                    key,
                    Assembly {
                        item: WorkItem {
                            torrent,
                            piece: lease.piece,
                            data: vec![0; length],
                            generation: lease.generation,
                            permit,
                            peer: Some(peer),
                        },
                        remaining: length,
                    },
                );
            }
            let actions = self.jobs[index]
                .peers
                .get_mut(&peer)
                .map(|actor| {
                    actor.request_blocks(&lease, |length| {
                        self.arbiter.reserve_down(u64::from(length), now_ns)
                    })
                })
                .unwrap_or_default();
            if actions.is_empty() {
                // The rate budget refused the first block: release only when
                // nothing for this piece is flying and nothing has landed.
                let flying = self.jobs[index]
                    .peers
                    .get(&peer)
                    .is_some_and(|actor| actor.has_pending(lease.piece));
                if !flying
                    && self
                        .assemblies
                        .get(&key)
                        .is_some_and(|assembly| assembly.remaining == assembly.item.data.len())
                {
                    self.jobs[index].coordinator.release_piece(lease.piece);
                    self.assemblies.remove(&key);
                }
                break;
            }
            report
                .outbox
                .extend(actions.into_iter().map(|action| OutboxEntry {
                    torrent,
                    peer,
                    action,
                }));
        }
    }

    fn free_slots(&self, index: usize, peer: PeerId) -> usize {
        self.jobs[index]
            .peers
            .get(&peer)
            .map(|actor| actor.free_slots())
            .unwrap_or(0)
    }

    fn apply_outcome(&mut self, outcome: WorkOutcome, report: &mut SessionReport) {
        let torrent = outcome.torrent();
        let generation = outcome.generation();
        let fresh = match self.job_mut(torrent) {
            Ok(job) => job.coordinator.generation() == generation,
            Err(_) => {
                self.verifying.remove(&(torrent.0, outcome.piece()));
                report.dropped_stale += 1;
                return;
            }
        };
        self.verifying.remove(&(torrent.0, outcome.piece()));
        if !fresh {
            report.dropped_stale += 1;
            return;
        }
        let Ok(job) = self.job_mut(torrent) else {
            report.dropped_stale += 1;
            return;
        };
        match outcome {
            WorkOutcome::Verified { piece, .. } => {
                let lease = crate::lease::Lease {
                    torrent,
                    piece,
                    blocks: Vec::new(),
                    generation,
                    issued_ns: 0,
                };
                match job.coordinator.complete_lease(&lease) {
                    Ok(LeaseOutcome::Applied(_)) => {
                        report.verified.push((torrent, piece));
                        if Self::is_complete(job) {
                            job.state = JobState::Seeding;
                            report.completed.push(torrent);
                        }
                    }
                    Ok(LeaseOutcome::Stale) => report.dropped_stale += 1,
                    Err(error) => report.errors.push(error),
                }
            }
            WorkOutcome::Rejected { piece, peer, .. } => {
                // Hash failed: attribute the supplier and make the piece
                // pickable again for a bounded retry.
                job.coordinator.release_piece(piece);
                if let Some(peer) = peer {
                    report.blamed.push(BlameEntry {
                        torrent,
                        peer,
                        piece,
                    });
                }
            }
            WorkOutcome::WriteFailed { piece, error, .. } => {
                job.coordinator.release_piece(piece);
                job.state = JobState::Error;
                report.errors.push(EngineError::Storage(error));
            }
        }
    }

    fn is_complete(job: &Job) -> bool {
        job.coordinator
            .sequence()
            .piece_ranks()
            .iter()
            .all(|&rank| rank == SKIP)
    }
}
