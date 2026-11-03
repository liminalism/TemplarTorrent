//! Typed command/ack channel between the UI event loop and a live engine
//! session. The UI submits core [`Command`]s and receives one [`Ack`] per
//! sequence number, in order, never overwritten. The inbound queue is bounded:
//! a full queue refuses with [`QueueFull`] instead of freezing the UI.
//! Headless-testable: no window, no filesystem, no network.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use templartorrent_core::model::{Command, Limits, Snapshot, TorrentId};
#[cfg(test)]
use templartorrent_engine::session::JobSpec;
use templartorrent_engine::{
    EngineError,
    session::Session,
    worker::{PieceSink, SinkError},
};

/// Cap on commands waiting for dispatch. Hitting it means the UI submitted
/// faster than frames dispatch; the caller retries, nothing is dropped.
pub const MAX_QUEUED_COMMANDS: usize = 64;

/// Monotonic command identity, assigned at submit time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seq(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckOutcome {
    Accepted,
    Rejected { reason: String },
}

#[derive(Debug, Clone)]
pub struct Ack {
    pub seq: Seq,
    pub outcome: AckOutcome,
}

impl Ack {
    fn accepted(seq: Seq) -> Self {
        Self {
            seq,
            outcome: AckOutcome::Accepted,
        }
    }

    fn rejected(seq: Seq, reason: impl Into<String>) -> Self {
        Self {
            seq,
            outcome: AckOutcome::Rejected {
                reason: reason.into(),
            },
        }
    }

    #[cfg(test)]
    pub fn is_accepted(&self) -> bool {
        matches!(self.outcome, AckOutcome::Accepted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueFull;

type StoredPieces = Arc<Mutex<HashMap<(u64, u32), Vec<u8>>>>;

#[derive(Clone, Default)]
struct MemorySink {
    pieces: StoredPieces,
}

impl PieceSink for MemorySink {
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

fn ack_result(seq: Seq, result: Result<(), EngineError>) -> Ack {
    match result {
        Ok(()) => Ack::accepted(seq),
        Err(error) => Ack::rejected(seq, error.to_string()),
    }
}

/// UI-side handle over a live [`Session`]. Commands queue via [`LiveBridge::submit`]
/// and execute in order via [`LiveBridge::dispatch`]; telemetry flows separately
/// through [`LiveBridge::snapshot`], latest-wins.
pub struct LiveBridge {
    session: Session,
    queue: VecDeque<(Seq, Command)>,
    next_seq: u64,
    shutdown_requested: bool,
}

impl LiveBridge {
    pub fn new(limits: Limits) -> Result<Self, EngineError> {
        Ok(Self {
            session: Session::new(limits, vec![Box::new(MemorySink::default())], 16, 0)?,
            queue: VecDeque::new(),
            next_seq: 1,
            shutdown_requested: false,
        })
    }

    pub fn submit(&mut self, command: Command) -> Result<Seq, QueueFull> {
        if self.queue.len() >= MAX_QUEUED_COMMANDS {
            return Err(QueueFull);
        }
        let seq = Seq(self.next_seq);
        self.next_seq += 1;
        self.queue.push_back((seq, command));
        Ok(seq)
    }

    #[cfg(test)]
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    /// Test seam for placing jobs in the session. The `Add` command itself
    /// stays rejected until the add dialog wires metainfo plus storage.
    #[cfg(test)]
    pub fn add_job_direct(&mut self, spec: JobSpec) -> Result<TorrentId, EngineError> {
        self.session.add(spec)
    }

    /// Test seam for tracker admission. Discovery wiring arrives later.
    #[cfg(test)]
    pub fn add_tracker_direct(&mut self, id: TorrentId) -> Result<usize, EngineError> {
        self.session.add_tracker(id)
    }

    pub fn snapshot(&mut self) -> Snapshot {
        self.session.snapshot()
    }

    /// Execute every queued command in order, returning one ack per command.
    pub fn dispatch(&mut self, now_ns: u64) -> Vec<Ack> {
        let mut acks = Vec::with_capacity(self.queue.len());
        while let Some((seq, command)) = self.queue.pop_front() {
            acks.push(self.execute(seq, command, now_ns));
        }
        acks
    }

    fn execute(&mut self, seq: Seq, command: Command, now_ns: u64) -> Ack {
        match command {
            Command::Pause(id) => ack_result(seq, self.session.pause(id)),
            Command::Resume(id) => ack_result(seq, self.session.resume(id)),
            Command::Remove { id, delete_data } => {
                ack_result(seq, self.session.remove(id, delete_data))
            }
            Command::SetLimits(limits) => ack_result(seq, self.session.set_limits(limits, now_ns)),
            Command::SetFileOrder { id, file_indices } => {
                ack_result(seq, self.session.set_order(id, file_indices))
            }
            Command::SetFileOrderAutomatic(id) => {
                ack_result(seq, self.session.set_order_automatic(id))
            }
            Command::SetFileWanted {
                id,
                file_index,
                wanted,
            } => ack_result(seq, self.session.set_wanted(id, file_index, wanted)),
            Command::Reannounce { .. } => {
                Ack::rejected(seq, "tracker transport is not connected to this window yet")
            }
            Command::Inspect { .. } => Ack::accepted(seq),
            Command::Shutdown => {
                self.shutdown_requested = true;
                Ack::accepted(seq)
            }
            Command::Add { .. } => Ack::rejected(seq, "add-torrent arrives with the add dialog"),
            Command::SetPeerPreference { .. } => {
                Ack::rejected(seq, "peer preferences arrive with peer scoring")
            }
            Command::AddUrl { .. } | Command::Retry(_) | Command::Cancel(_) => {
                Ack::rejected(seq, "URL downloads live in the production client")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use templartorrent_core::{
        model::{InspectorTab, JobState, PeerPreference},
        sequence::FileSpec,
    };

    fn spec() -> JobSpec {
        JobSpec {
            name: "bridge-job".into(),
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

    fn bridge_with_job() -> (LiveBridge, TorrentId) {
        let mut bridge = LiveBridge::new(Limits::default()).unwrap();
        let id = bridge.add_job_direct(spec()).unwrap();
        (bridge, id)
    }

    #[test]
    fn live_commands_pause_resume_ack_in_order() {
        let (mut bridge, id) = bridge_with_job();
        let first = bridge.submit(Command::Pause(id)).unwrap();
        let second = bridge.submit(Command::Resume(id)).unwrap();
        assert!(first.0 < second.0);
        let acks = bridge.dispatch(0);
        assert_eq!(acks.len(), 2);
        assert_eq!((acks[0].seq, acks[1].seq), (first, second));
        assert!(acks.iter().all(Ack::is_accepted));
        assert_eq!(bridge.snapshot().jobs[0].state, JobState::Downloading);
    }

    #[test]
    fn live_commands_rejects_unknown_job() {
        let (mut bridge, _) = bridge_with_job();
        let seq = bridge.submit(Command::Pause(TorrentId(999))).unwrap();
        let acks = bridge.dispatch(0);
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].seq, seq);
        assert!(matches!(&acks[0].outcome, AckOutcome::Rejected { reason } if !reason.is_empty()));
    }

    #[test]
    fn live_commands_queue_full_is_busy() {
        let (mut bridge, id) = bridge_with_job();
        for _ in 0..MAX_QUEUED_COMMANDS {
            bridge.submit(Command::Pause(id)).unwrap();
        }
        assert_eq!(bridge.submit(Command::Pause(id)), Err(QueueFull));
        assert_eq!(bridge.pending(), MAX_QUEUED_COMMANDS);
        let acks = bridge.dispatch(0);
        assert_eq!(acks.len(), MAX_QUEUED_COMMANDS);
        assert!(
            acks.windows(2)
                .all(|pair| pair[0].seq.0 + 1 == pair[1].seq.0),
            "acks must preserve submit order"
        );
        assert_eq!(bridge.pending(), 0);
        bridge.submit(Command::Resume(id)).unwrap();
    }

    #[test]
    fn live_commands_remove_limits_wanted_order() {
        let (mut bridge, id) = bridge_with_job();
        bridge
            .submit(Command::SetLimits(Limits::default()))
            .unwrap();
        bridge
            .submit(Command::SetFileWanted {
                id,
                file_index: 1,
                wanted: false,
            })
            .unwrap();
        bridge
            .submit(Command::SetFileOrder {
                id,
                file_indices: vec![1, 0],
            })
            .unwrap();
        bridge
            .submit(Command::SetFileWanted {
                id,
                file_index: 9,
                wanted: true,
            })
            .unwrap();
        let acks = bridge.dispatch(0);
        assert_eq!(acks.len(), 4);
        assert!(acks[0].is_accepted());
        assert!(acks[1].is_accepted());
        assert!(acks[2].is_accepted());
        assert!(!acks[3].is_accepted(), "bad file index must reject");
        bridge
            .submit(Command::Remove {
                id,
                delete_data: true,
            })
            .unwrap();
        let acks = bridge.dispatch(0);
        assert!(!acks[0].is_accepted(), "data deletion is not wired yet");
        assert_eq!(bridge.snapshot().jobs.len(), 1);
        bridge
            .submit(Command::Remove {
                id,
                delete_data: false,
            })
            .unwrap();
        let acks = bridge.dispatch(0);
        assert!(acks[0].is_accepted());
        assert!(bridge.snapshot().jobs.is_empty());
    }

    #[test]
    fn live_commands_reannounce_requires_actual_transport() {
        let (mut bridge, id) = bridge_with_job();
        bridge.add_tracker_direct(id).unwrap();
        bridge
            .submit(Command::Reannounce {
                id,
                tracker_index: 0,
            })
            .unwrap();
        let acks = bridge.dispatch(0);
        assert!(
            matches!(&acks[0].outcome, AckOutcome::Rejected { reason } if reason.contains("tracker transport"))
        );
        bridge
            .submit(Command::Reannounce {
                id,
                tracker_index: 0,
            })
            .unwrap();
        let acks = bridge.dispatch(0);
        assert!(
            !acks[0].is_accepted(),
            "a clock admission is not a completed network announce"
        );
        bridge
            .submit(Command::Reannounce {
                id: TorrentId(999),
                tracker_index: 0,
            })
            .unwrap();
        let acks = bridge.dispatch(0);
        assert!(!acks[0].is_accepted(), "unknown job must reject");
    }

    #[test]
    fn live_commands_unsupported_paths_reject_explicitly() {
        let (mut bridge, id) = bridge_with_job();
        bridge
            .submit(Command::Add {
                source: "magnet:?xt=test".into(),
                destination: std::path::PathBuf::from("/tmp"),
                public_magnet_bootstrap: false,
            })
            .unwrap();
        bridge
            .submit(Command::SetPeerPreference {
                id,
                address: "127.0.0.1:6881".parse().unwrap(),
                preference: PeerPreference::Prefer,
            })
            .unwrap();
        bridge
            .submit(Command::Inspect {
                id: Some(id),
                tab: InspectorTab::Files,
                visible_range: 0..10,
            })
            .unwrap();
        bridge.submit(Command::Shutdown).unwrap();
        let acks = bridge.dispatch(0);
        assert_eq!(acks.len(), 4);
        assert!(
            matches!(&acks[0].outcome, AckOutcome::Rejected { reason } if reason.contains("add dialog"))
        );
        assert!(
            matches!(&acks[1].outcome, AckOutcome::Rejected { reason } if reason.contains("peer scoring"))
        );
        assert!(acks[2].is_accepted());
        assert!(acks[3].is_accepted());
        assert!(bridge.is_shutdown_requested());
    }
}
