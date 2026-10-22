//! Application driver over rbit primitives and the deterministic session core.
//! Run on a network/disk thread, never the PixelKit thread. Call step to service
//! bounded actors. Discovery is deliberately injected: no tracker URL, x.pe,
//! DHT, PEX or other endpoint is contacted merely because metadata contains it.
//!
//! Release limitations: FileStore is not yet handle-relative/reparse hardened;
//! disk/hash work is synchronous but bounded and must run off the UI thread.
pub use crate::resume::{ResumeRecord, ResumeScan};
use crate::{
    metadata::{METADATA_BUDGET, fetch_metadata_budgeted},
    resume,
    torrent::{MAX_DRIVER_PIECES, ValidatedV1},
    unpack_bitfield,
};
use bytes::Bytes;
use rbit::{ExtensionHandshake, Handshake, InfoHash, Message, MetadataMessage, PeerConnection};
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use templartorrent_core::{
    budget::{ByteBudget, Permit},
    model::{JobState, Limits, PeerPreference, Snapshot, TorrentId},
};
use templartorrent_engine::{
    event::PeerId,
    peer::{DEFAULT_MAX_IN_FLIGHT, PeerAction, PeerMessage},
    session::{JobSpec, Session, SessionReport},
    worker::{PieceReader, PieceVerifier},
};
use templartorrent_storage::SharedStore;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
    time::timeout,
};

const EVENT_CAP: usize = 64;
const WIRE_CAP: usize = 32;
const CANDIDATE_CAP: usize = 32;
/// Distinct peer addresses with a non-default preference per job.
const PREFERENCE_CAP: usize = 256;
/// Request pipeline for an avoided peer; it stays usable for pieces nobody
/// else supplies but takes a smaller share.
const AVOID_PIPELINE: usize = 2;
/// How often the optimistic upload slot moves to another interested peer.
const OPTIMISTIC_INTERVAL_S: u64 = 30;
/// How often regular upload slots are re-ranked by measured rates. Between
/// evaluations the previous order holds, which is the hysteresis against churn.
const CHOKE_INTERVAL_S: u64 = 10;
/// Per-peer payload rates are sampled over this window and blended 3:1.
const RATE_WINDOW: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverError(pub &'static str);
impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for DriverError {}
type Result<T> = std::result::Result<T, DriverError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerOrigin {
    /// Stable identity of the original, permitted tracker that supplied this
    /// address. Private jobs use exactly one such tracker scope at a time.
    OriginalTracker(u64),
    /// Explicitly supplied by the user; requires public bootstrap opt-in while
    /// metadata is unknown. Never retained if metadata later proves private.
    UserProvided,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PeerSource {
    pub address: SocketAddr,
    pub origin: PeerOrigin,
}
// Peer addresses intentionally omitted from Debug/default diagnostics.
impl fmt::Debug for PeerSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerSource")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStatus {
    pub address: SocketAddr,
    pub client: String,
    pub remote_choked: bool,
    pub remote_interested: bool,
    pub we_choke: bool,
    pub preference: PeerPreference,
    /// Payload received from this peer, smoothed, bytes per second.
    pub down_bps: u64,
    /// Payload sent to this peer, smoothed, bytes per second.
    pub up_bps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerStatus {
    pub label: String,
    pub origin: &'static str,
    pub seeds: Option<u32>,
    pub peers: Option<u32>,
    pub manual_due_s: u64,
    pub last_error: Option<String>,
}

/// Bytes left reported to a tracker before the torrent length is known.
const UNKNOWN_LEFT: u64 = 1 << 62;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingData {
    Refuse,
    Recheck,
}

struct Tracked {
    url: String,
    origin: &'static str,
    origin_id: u64,
    local: bool,
    seeds: Option<u32>,
    peers: Option<u32>,
    last_error: Option<String>,
}
struct Job {
    torrent: ValidatedV1,
    sources: Vec<PeerSource>,
    private_tracker: Option<u64>,
    destination: PathBuf,
    queued: bool,
    last_error: Option<&'static str>,
    trackers: Vec<Tracked>,
    /// Local preference by peer IP, so a blocked peer cannot return on
    /// another ephemeral port. Normal entries are not stored.
    preferences: HashMap<IpAddr, PeerPreference>,
    /// Whether the user allowed public discovery for this magnet.
    bootstrap: bool,
    /// Pieces adopted from a resume bitmap on restore, without hashing.
    resume_trusted: usize,
    /// Pieces read and hashed on that restore scan.
    resume_checked: usize,
    /// Hashed pieces that did not match.
    resume_failed: usize,
    /// Why a supplied bitmap was not applied.
    resume_note: Option<&'static str>,
}

/// Everything needed to reinstall a job without the network: the metainfo
/// envelope, where it lives, what the user intended, and local policy.
/// Tracker URLs may carry passkeys; `Debug` redacts them.
#[derive(Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub info_hash: [u8; 20],
    /// `d4:info<raw>e`, accepted by [`Driver::magnet_from_torrent_bytes`].
    pub torrent: Vec<u8>,
    pub destination: PathBuf,
    pub paused: bool,
    pub public_bootstrap: bool,
    pub wanted: Vec<bool>,
    pub order: Vec<usize>,
    /// `(url, original)`; added trackers are re-derived from the public list.
    pub trackers: Vec<(String, bool)>,
    pub preferences: Vec<(IpAddr, PeerPreference)>,
}
impl fmt::Debug for JobRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobRecord")
            .field("torrent_bytes", &self.torrent.len())
            .field("destination", &self.destination)
            .field("paused", &self.paused)
            .field("public_bootstrap", &self.public_bootstrap)
            .field("wanted", &self.wanted)
            .field("order", &self.order)
            .field("trackers", &self.trackers.len())
            .field("preferences", &self.preferences.len())
            .finish_non_exhaustive()
    }
}
#[derive(Default)]
struct Jobs(HashMap<TorrentId, Job>);
impl PieceVerifier for Jobs {
    fn verify(&self, _piece: u32, _data: &[u8]) -> bool {
        false
    }
    fn verify_for(&self, id: TorrentId, piece: u32, data: &[u8]) -> bool {
        self.0
            .get(&id)
            .is_some_and(|job| job.torrent.verify(piece, data))
    }
}

struct Wire {
    torrent: TorrentId,
    source: PeerSource,
    sender: mpsc::Sender<WireCommand>,
    task: JoinHandle<()>,
    interested: bool,
    choked: bool,
    remote_choked: bool,
    client: String,
    uploads: VecDeque<(u32, u32, u32)>,
    uploading: bool,
    metadata_id: Option<u8>,
    /// Payload bytes in the current rate window and the smoothed rates.
    window_down: u64,
    window_up: u64,
    down_bps: u64,
    up_bps: u64,
}
struct WireCommand {
    message: Message,
    // Retained through encoding and the complete transport write.
    _permit: Option<Permit>,
    upload_bytes: u32,
    _owned: Option<OwnedPayload>,
}
impl From<Message> for WireCommand {
    fn from(message: Message) -> Self {
        Self {
            message,
            _permit: None,
            upload_bytes: 0,
            _owned: None,
        }
    }
}
#[derive(Default)]
struct UploadOwnership {
    current: AtomicUsize,
    high: AtomicUsize,
}
struct OwnedPayload {
    owner: Arc<UploadOwnership>,
    bytes: usize,
}
impl OwnedPayload {
    fn new(owner: &Arc<UploadOwnership>, bytes: usize) -> Self {
        let used = owner.current.fetch_add(bytes, Ordering::AcqRel) + bytes;
        owner.high.fetch_max(used, Ordering::Relaxed);
        Self {
            owner: Arc::clone(owner),
            bytes,
        }
    }
}
impl Drop for OwnedPayload {
    fn drop(&mut self) {
        self.owner.current.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
struct WireEvent {
    torrent: TorrentId,
    peer: PeerId,
    message: std::result::Result<Message, &'static str>,
    uploaded: u32,
}
struct Incoming {
    stream: TcpStream,
    hash: [u8; 20],
    address: SocketAddr,
}
struct Listener {
    address: SocketAddr,
    task: JoinHandle<()>,
    incoming: mpsc::Receiver<Incoming>,
}

#[derive(Debug, Default)]
pub struct DriverReport {
    pub work_outcomes: usize,
    pub completed: Vec<TorrentId>,
    pub hash_failures: usize,
    pub disconnected: usize,
    pub peer_errors: Vec<&'static str>,
}

pub struct Driver {
    session: Session,
    store: SharedStore,
    jobs: Jobs,
    wires: HashMap<PeerId, Wire>,
    events: mpsc::Receiver<WireEvent>,
    event_sender: mpsc::Sender<WireEvent>,
    metadata_budget: ByteBudget,
    peer_id: [u8; 20],
    started: Instant,
    limits: Limits,
    pending_failures: HashMap<PeerId, &'static str>,
    listener: Option<Listener>,
    upload_ownership: Arc<UploadOwnership>,
    allow_local_trackers: bool,
    public_trackers: Vec<String>,
    /// Current optimistic unchoke and when it was granted, in elapsed seconds.
    optimistic: Option<(PeerId, u64)>,
    /// Elapsed seconds of the last rotation decision.
    optimistic_rotated_s: u64,
    optimistic_interval_s: u64,
    /// Regular slot order from the last evaluation, and when it was made.
    choke_order: Vec<PeerId>,
    choke_evaluated_s: Option<u64>,
    rate_sampled: Instant,
}

impl Driver {
    pub fn new(limits: Limits) -> Result<Self> {
        Self::with_peer_id(limits, *rbit::PeerId::generate().as_bytes())
    }
    pub fn with_peer_id(limits: Limits, peer_id: [u8; 20]) -> Result<Self> {
        let store = SharedStore::new();
        let session = Session::new(limits.clone(), vec![Box::new(store.clone())], 8, 0)
            .map_err(|_| DriverError("invalid session limits"))?;
        let (event_sender, events) = mpsc::channel(EVENT_CAP);
        Ok(Self {
            session,
            store,
            jobs: Jobs::default(),
            wires: HashMap::new(),
            events,
            event_sender,
            metadata_budget: ByteBudget::new(METADATA_BUDGET),
            peer_id,
            started: Instant::now(),
            limits,
            pending_failures: HashMap::new(),
            listener: None,
            upload_ownership: Arc::default(),
            allow_local_trackers: false,
            public_trackers: Vec::new(),
            optimistic: None,
            optimistic_rotated_s: 0,
            optimistic_interval_s: OPTIMISTIC_INTERVAL_S,
            choke_order: Vec::new(),
            choke_evaluated_s: None,
            rate_sampled: Instant::now(),
        })
    }

    /// Fold the current window into each wire's smoothed rates once per
    /// [`RATE_WINDOW`]. A quiet window decays the rate toward zero.
    fn sample_rates(&mut self) {
        let elapsed = self.rate_sampled.elapsed();
        if elapsed < RATE_WINDOW {
            return;
        }
        let millis = u64::try_from(elapsed.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        for wire in self.wires.values_mut() {
            let down = wire.window_down.saturating_mul(1000) / millis;
            let up = wire.window_up.saturating_mul(1000) / millis;
            wire.down_bps = blend_rate(wire.down_bps, down);
            wire.up_bps = blend_rate(wire.up_bps, up);
            wire.window_down = 0;
            wire.window_up = 0;
        }
        self.rate_sampled = Instant::now();
    }

    /// Rotation period of the optimistic upload slot; tests shorten it.
    pub fn set_optimistic_interval_secs(&mut self, seconds: u64) {
        self.optimistic_interval_s = seconds.max(1);
    }

    /// Peer currently holding the optimistic slot, if any.
    pub fn optimistic_peer(&self) -> Option<SocketAddr> {
        self.optimistic
            .and_then(|(peer, _)| self.wires.get(&peer))
            .map(|wire| wire.source.address)
    }
    pub fn set_allow_local_trackers(&mut self, allow: bool) {
        self.allow_local_trackers = allow;
    }
    /// Configured public trackers. Applied only after metadata proves the
    /// torrent is public, capped at 12 including originals, and never allowed
    /// to target a local or private address.
    pub fn set_public_trackers(&mut self, urls: Vec<String>) {
        self.public_trackers = urls
            .into_iter()
            .take(crate::discovery::MAX_TRACKERS)
            .collect();
    }
    fn now_ns(&self) -> u64 {
        self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64
    }
    pub fn snapshot(&mut self) -> Snapshot {
        let mut snapshot = self.session.snapshot();
        let mut jobs = snapshot.jobs.to_vec();
        for job in &mut jobs {
            if self.jobs.0.get(&job.id).is_some_and(|record| record.queued) {
                job.state = JobState::Queued;
            } else if job.state == JobState::Seeding
                && (self.listener.is_none()
                    || !self.seed_admitted(job.id)
                    || self.limits.upload_peer_slots == 0)
            {
                job.state = JobState::Complete;
            }
        }
        snapshot.jobs = jobs.into();
        snapshot
    }
    /// Explanation for rows projected as Complete rather than Seeding.
    pub fn seeding_unavailable_reason(&self) -> &'static str {
        if self.listener.is_none() {
            "No incoming listener is configured"
        } else if self.limits.upload_peer_slots == 0 {
            "Upload peer slots are disabled"
        } else {
            "This job has no admitted seed slot"
        }
    }
    pub fn payload_stats(&self) -> templartorrent_engine::session::PayloadStats {
        self.session.payload_stats()
    }
    /// Disk-read Vec capacities plus queued/in-flight upload block bytes.
    /// Excludes rbit encoded frames and socket buffers; those appear in RSS.
    pub fn upload_owned_bytes(&self) -> (usize, usize) {
        (
            self.upload_ownership.current.load(Ordering::Acquire),
            self.upload_ownership.high.load(Ordering::Relaxed),
        )
    }
    pub fn metadata_reserved_bytes(&self) -> usize {
        self.metadata_budget.used()
    }
    pub fn connection_count(&self) -> usize {
        self.wires.len()
    }
    pub fn last_error(&self, id: TorrentId) -> Option<&'static str> {
        self.jobs.0.get(&id).and_then(|job| job.last_error)
    }
    pub fn destination(&self, id: TorrentId) -> Option<PathBuf> {
        self.jobs.0.get(&id).map(|job| job.destination.clone())
    }
    pub fn limits(&self) -> &Limits {
        &self.limits
    }
    pub fn elapsed_secs(&self) -> u64 {
        self.elapsed_s()
    }
    pub fn set_file_wanted(&mut self, id: TorrentId, file: usize, wanted: bool) -> Result<()> {
        if !self.jobs.0.contains_key(&id) {
            return Err(DriverError("unknown job"));
        }
        self.session
            .set_wanted(id, file, wanted)
            .map_err(|_| DriverError("file selection rejected"))
    }
    pub fn set_file_order(&mut self, id: TorrentId, order: Vec<usize>) -> Result<()> {
        if !self.jobs.0.contains_key(&id) {
            return Err(DriverError("unknown job"));
        }
        self.session
            .set_order(id, order)
            .map_err(|_| DriverError("file order rejected"))
    }
    pub fn file_layout(&self, id: TorrentId) -> Result<Vec<(String, u64)>> {
        // Layout only until the session's selected-file telemetry seam is wired.
        let job = self.jobs.0.get(&id).ok_or(DriverError("unknown job"))?;
        Ok(job
            .torrent
            .paths
            .iter()
            .map(|(parts, length)| (parts.join("/"), *length))
            .collect())
    }

    /// Explicit opt-in listener. At most eight routed sockets plus one
    /// five-second handshake probe wait outside the admitted connection pool.
    /// Unknown hashes and all private incoming connections are dropped without
    /// logs. Private uploads require outbound connections to peers from the
    /// selected original tracker; an inbound address cannot prove provenance.
    pub async fn listen(&mut self, address: SocketAddr) -> Result<SocketAddr> {
        if self.listener.is_some() {
            return Err(DriverError("listener already bound"));
        }
        let socket = TcpListener::bind(address)
            .await
            .map_err(|_| DriverError("listener bind failed"))?;
        let address = socket
            .local_addr()
            .map_err(|_| DriverError("listener address unavailable"))?;
        let (sender, incoming) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            while let Ok((stream, address)) = socket.accept().await {
                let probe = timeout(CONNECT_TIMEOUT, async {
                    let mut bytes = [0; 68];
                    loop {
                        let n = stream.peek(&mut bytes).await.map_err(|_| ())?;
                        if n == 0 {
                            return Err(());
                        }
                        if n == bytes.len() {
                            return Handshake::decode(&bytes).map_err(|_| ());
                        }
                        // peek preserves the handshake for rbit::accept. A
                        // partial prefix remains readable; avoid a spin loop.
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
                if let Ok(Ok(handshake)) = probe {
                    // Queue full: the failed send drops the socket.
                    let _ = sender.try_send(Incoming {
                        stream,
                        hash: handshake.info_hash,
                        address,
                    });
                }
            }
        });
        self.listener = Some(Listener {
            address,
            task,
            incoming,
        });
        Ok(address)
    }

    pub fn listening_address(&self) -> Option<SocketAddr> {
        self.listener.as_ref().map(|l| l.address)
    }

    fn seed_admitted(&self, id: TorrentId) -> bool {
        self.session.upload_admitted(id)
    }

    async fn accept_incoming(&mut self) {
        let incoming = self
            .listener
            .as_mut()
            .and_then(|l| l.incoming.try_recv().ok());
        let Some(incoming) = incoming else {
            return;
        };
        let id = self.jobs.0.iter().find_map(|(id, job)| {
            (matches!(job.torrent.meta.info_hash, InfoHash::V1(h) if h == incoming.hash)
                && job.private_tracker.is_none())
            .then_some(*id)
        });
        let Some(id) = id else {
            return;
        };
        if self.peer_preference(id, incoming.address) == PeerPreference::Block {
            return;
        }
        let state = self
            .snapshot()
            .jobs
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.state);
        if !matches!(state, Some(JobState::Downloading | JobState::Seeding))
            || self.wires.len() >= self.limits.connections_total
            || self.wires.values().filter(|w| w.torrent == id).count()
                >= self.limits.connections_per_job
        {
            return;
        }
        if let Ok(Ok(connection)) = timeout(
            CONNECT_TIMEOUT,
            PeerConnection::accept(incoming.stream, incoming.hash, self.peer_id),
        )
        .await
        {
            // No discovery authorization is inferred from incoming peers.
            let source = PeerSource {
                address: incoming.address,
                origin: PeerOrigin::UserProvided,
            };
            let _ = self.attach(id, source, connection, Vec::new());
        }
    }

    fn queue(&self, peer: PeerId, command: impl Into<WireCommand>) -> Result<()> {
        self.wires
            .get(&peer)
            .ok_or(DriverError("unknown peer"))?
            .sender
            .try_send(command.into())
            .map_err(|_| DriverError("peer command queue full or closed"))
    }

    /// Resolve from one explicitly permitted source. add_magnet borrows the
    /// coordinator for the resolution deadline; actor receive queues remain
    /// bounded while another job is being added. A later UI actor can run the
    /// resolution as a separate bounded work item without changing this seam.
    pub async fn add_magnet(
        &mut self,
        uri: &str,
        destination: &Path,
        existing: ExistingData,
        sources: &[PeerSource],
        public_bootstrap: bool,
    ) -> Result<TorrentId> {
        if uri.len() > 16 * 1024 {
            return Err(DriverError("magnet exceeds 16 KiB limit"));
        }
        let magnet = rbit::MagnetLink::parse(uri).map_err(|_| DriverError("invalid magnet"))?;
        let InfoHash::V1(hash) = magnet.info_hash else {
            return Err(DriverError("v2/hybrid magnets are not supported"));
        };
        if self
            .jobs
            .0
            .values()
            .any(|job| matches!(job.torrent.meta.info_hash, InfoHash::V1(h) if h == hash))
        {
            return Err(DriverError("torrent already exists"));
        }
        if sources.is_empty() || sources.len() > CANDIDATE_CAP {
            return Err(DriverError("provide 1..=32 permitted peer sources"));
        }
        if self.wires.len() >= self.limits.connections_total
            || self.limits.connections_per_job == 0
            || self.limits.download_jobs == 0
            || self
                .snapshot()
                .jobs
                .iter()
                .filter(|row| matches!(row.state, JobState::Downloading | JobState::Queued))
                .count()
                >= self.limits.download_jobs
        {
            return Err(DriverError("no connection or download slot available"));
        }
        let mut permitted = Vec::new();
        for source in sources {
            if (matches!(source.origin, PeerOrigin::OriginalTracker(_)) || public_bootstrap)
                && !permitted
                    .iter()
                    .any(|known: &PeerSource| known.address == source.address)
            {
                permitted.push(*source);
            }
        }
        if permitted.is_empty() {
            return Err(DriverError(
                "strict bootstrap requires an original-tracker source",
            ));
        }
        // Bounded fallback, including connect and metadata for all candidates.
        let resolution = timeout(Duration::from_secs(30), async {
            for source in &permitted {
                let Ok(Ok(mut connection)) = timeout(
                    CONNECT_TIMEOUT,
                    PeerConnection::connect(source.address, hash, self.peer_id),
                )
                .await
                else {
                    continue;
                };
                let Ok(metadata) =
                    fetch_metadata_budgeted(&mut connection, hash, &self.metadata_budget).await
                else {
                    continue;
                };
                return Ok((*source, connection, metadata));
            }
            Err(DriverError("no permitted peer supplied valid metadata"))
        })
        .await
        .map_err(|_| DriverError("magnet resolution deadline exceeded"))??;
        let (source, connection, metadata) = resolution;
        let (torrent, buffered) = ValidatedV1::from_fetched(metadata, hash, &self.metadata_budget)
            .map_err(DriverError)?;
        let private_tracker = if torrent.is_private() {
            match source.origin {
                PeerOrigin::OriginalTracker(id) => Some(id),
                PeerOrigin::UserProvided => {
                    return Err(DriverError(
                        "private metadata revoked public bootstrap; use its original tracker",
                    ));
                }
            }
        } else {
            None
        };
        permitted.retain(|candidate| {
            private_tracker.is_none_or(|id| candidate.origin == PeerOrigin::OriginalTracker(id))
        });
        // Admission precedes creation of any payload file.
        let id = self
            .session
            .add(JobSpec {
                name: torrent.name().into(),
                files: torrent.files(),
                piece_length: torrent.piece_length(),
                max_pieces: MAX_DRIVER_PIECES,
            })
            .map_err(|_| DriverError("torrent exceeds session admission limits"))?;
        let admitted =
            (|| {
                let root = prepare_destination(destination, existing)?;
                if self.jobs.0.values().any(|job| {
                    job.destination.starts_with(&root) || root.starts_with(&job.destination)
                }) {
                    return Err(DriverError("destination already owned by another job"));
                }
                self.store
                    .add_job(id, &root, &torrent.paths, torrent.piece_length())
                    .map_err(|_| DriverError("payload storage admission failed"))?;
                self.session
                    .recheck(id, &self.store, &torrent)
                    .map_err(|_| DriverError("resume recheck failed"))?;
                Ok(root)
            })();
        let root = match admitted {
            Ok(root) => root,
            Err(error) => {
                let _ = self.session.remove(id, false);
                let _ = self.store.close_job(id);
                return Err(error);
            }
        };
        self.jobs.0.insert(
            id,
            Job {
                torrent,
                sources: permitted,
                private_tracker,
                destination: root,
                queued: false,
                last_error: None,
                trackers: Vec::new(),
                preferences: HashMap::new(),
                bootstrap: public_bootstrap,
                resume_trusted: 0,
                resume_checked: 0,
                resume_failed: 0,
                resume_note: None,
            },
        );
        if self
            .session
            .snapshot()
            .jobs
            .iter()
            .any(|row| row.id == id && row.state == JobState::Seeding)
        {
            return Ok(id);
        }
        if let Err(error) = self.attach(id, source, connection, buffered) {
            self.session.remove(id, false).ok();
            self.store.close_job(id).ok();
            self.jobs.0.remove(&id);
            return Err(error);
        }
        Ok(id)
    }

    fn attach(
        &mut self,
        id: TorrentId,
        source: PeerSource,
        connection: PeerConnection,
        buffered: Vec<Message>,
    ) -> Result<()> {
        let peer = self
            .session
            .connect(id)
            .map_err(|_| DriverError("connection cap reached"))?;
        for message in buffered {
            if let Err(error) = self.deliver(id, peer, message) {
                self.session.disconnect(id, peer).ok();
                return Err(error);
            }
        }
        let pieces = self
            .jobs
            .0
            .get(&id)
            .ok_or(DriverError("unknown job"))?
            .torrent
            .piece_count();
        let (sender, receiver) = mpsc::channel(WIRE_CAP);
        let task = tokio::spawn(peer_actor(
            connection,
            id,
            peer,
            pieces,
            receiver,
            self.event_sender.clone(),
        ));
        self.wires.insert(
            peer,
            Wire {
                torrent: id,
                source,
                sender,
                task,
                interested: false,
                choked: true,
                remote_choked: true,
                client: "Unknown".into(),
                uploads: VecDeque::new(),
                uploading: false,
                metadata_id: None,
                window_down: 0,
                window_up: 0,
                down_bps: 0,
                up_bps: 0,
            },
        );
        let pipeline = pipeline_for(self.peer_preference(id, source.address));
        let _ = self.session.set_peer_pipeline(id, peer, pipeline);
        let initial = (|| {
            let words = self
                .session
                .verified_pieces(id)
                .map_err(|_| DriverError("unknown job"))?;
            let mut bits = vec![0u8; pieces.div_ceil(8)];
            for p in 0..pieces {
                if words[p / 64] & (1 << (p % 64)) != 0 {
                    bits[p / 8] |= 0x80 >> (p % 8);
                }
            }
            let mut handshake =
                ExtensionHandshake::with_extensions(&[("ut_metadata", crate::metadata::OUR_MD_ID)]);
            handshake.metadata_size = Some(self.jobs.0[&id].torrent.meta.raw_info().len() as i64);
            self.queue(
                peer,
                Message::Extended {
                    id: 0,
                    payload: handshake
                        .encode()
                        .map_err(|_| DriverError("metadata handshake failed"))?,
                },
            )?;
            self.queue(peer, Message::Bitfield(Bytes::from(bits)))
        })();
        if let Err(error) = initial {
            if let Some(wire) = self.wires.remove(&peer) {
                wire.task.abort();
            }
            self.session.disconnect(id, peer).ok();
            return Err(error);
        }
        Ok(())
    }

    pub async fn connect_peer(&mut self, id: TorrentId, source: PeerSource) -> Result<()> {
        let state = self
            .session
            .snapshot()
            .jobs
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.state)
            .ok_or(DriverError("unknown job"))?;
        if !matches!(state, JobState::Downloading | JobState::Seeding) {
            return Err(DriverError(
                "peer connection requires an admitted running job",
            ));
        }
        let job = self.jobs.0.get(&id).ok_or(DriverError("unknown job"))?;
        if job.preferences.get(&source.address.ip()) == Some(&PeerPreference::Block) {
            return Err(DriverError("peer address is blocked"));
        }
        if job.sources.len() >= CANDIDATE_CAP
            && !job
                .sources
                .iter()
                .any(|known| known.address == source.address)
        {
            return Err(DriverError("peer candidate cap reached"));
        }
        if job
            .private_tracker
            .is_some_and(|tracker| source.origin != PeerOrigin::OriginalTracker(tracker))
        {
            return Err(DriverError(
                "private job requires its selected original tracker",
            ));
        }
        if self
            .wires
            .values()
            .any(|wire| wire.torrent == id && wire.source.address == source.address)
        {
            return Ok(());
        }
        if self.wires.len() >= self.limits.connections_total
            || self
                .wires
                .values()
                .filter(|wire| wire.torrent == id)
                .count()
                >= self.limits.connections_per_job
        {
            return Err(DriverError("connection cap reached"));
        }
        let InfoHash::V1(hash) = job.torrent.meta.info_hash else {
            return Err(DriverError("unsupported torrent"));
        };
        let connection = timeout(
            CONNECT_TIMEOUT,
            PeerConnection::connect(source.address, hash, self.peer_id),
        )
        .await
        .map_err(|_| DriverError("peer handshake deadline exceeded"))?
        .map_err(|_| DriverError("peer connection failed"))?;
        let job = self.jobs.0.get_mut(&id).unwrap();
        if !job
            .sources
            .iter()
            .any(|known| known.address == source.address)
        {
            job.sources.push(source);
        }
        self.attach(id, source, connection, Vec::new())
    }

    pub async fn pause(&mut self, id: TorrentId) -> Result<()> {
        if !self.jobs.0.contains_key(&id) {
            return Err(DriverError("unknown job"));
        }
        self.jobs.0.get_mut(&id).unwrap().queued = false;
        self.stop_wires(id).await;
        self.session
            .pause(id)
            .map_err(|_| DriverError("unknown job"))?;
        Ok(())
    }
    pub async fn resume(&mut self, id: TorrentId) -> Result<()> {
        let sources = self
            .jobs
            .0
            .get(&id)
            .ok_or(DriverError("unknown job"))?
            .sources
            .clone();
        self.session
            .resume(id)
            .map_err(|_| DriverError("resume rejected"))?;
        self.jobs.0.get_mut(&id).unwrap().queued = false;
        if self
            .snapshot()
            .jobs
            .iter()
            .any(|row| row.id == id && matches!(row.state, JobState::Complete | JobState::Seeding))
        {
            return Ok(());
        }
        let rows = self.snapshot();
        let runnable: Vec<_> = rows
            .jobs
            .iter()
            .filter(|row| matches!(row.state, JobState::Downloading | JobState::Queued))
            .take(self.limits.download_jobs)
            .map(|row| row.id)
            .collect();
        if !runnable.contains(&id) {
            self.pause(id).await?;
            self.jobs.0.get_mut(&id).unwrap().queued = true;
            return Ok(());
        }
        if sources.is_empty()
            && self
                .jobs
                .0
                .get(&id)
                .is_some_and(|job| !job.trackers.is_empty())
        {
            // A restored job knows no peers yet; its trackers announce on
            // their normal schedule and supply them.
            return Ok(());
        }
        timeout(Duration::from_secs(30), async {
            for source in sources {
                if self.connect_peer(id, source).await.is_ok() {
                    return Ok(());
                }
            }
            Err(DriverError("no permitted reconnect source is available"))
        })
        .await
        .map_err(|_| DriverError("reconnect deadline exceeded"))?
    }
    pub async fn remove(&mut self, id: TorrentId, delete_data: bool) -> Result<()> {
        if !self.jobs.0.contains_key(&id) {
            return Err(DriverError("unknown job"));
        }
        // Refuse unsafe deletes before pause or session removal changes state.
        if delete_data {
            self.store
                .preflight_delete(id)
                .map_err(|_| DriverError("data deletion was refused"))?;
        }
        self.pause(id).await?;
        if delete_data && self.store.delete_job_data(id).is_err() {
            self.jobs.0.get_mut(&id).unwrap().last_error = Some("data deletion failed");
            return Err(DriverError("data deletion failed"));
        }
        self.session
            .remove(id, false)
            .map_err(|_| DriverError("remove rejected"))?;
        if !delete_data {
            self.store
                .close_job(id)
                .map_err(|_| DriverError("storage handle closure failed"))?;
        }
        self.jobs.0.remove(&id);
        Ok(())
    }
    pub async fn shutdown(&mut self) {
        if let Some(listener) = self.listener.take() {
            listener.task.abort();
            let _ = listener.task.await;
        }
        let ids: Vec<_> = self.jobs.0.keys().copied().collect();
        for id in ids {
            let _ = self.remove(id, false).await;
        }
    }
    async fn stop_wires(&mut self, id: TorrentId) {
        let peers: Vec<_> = self
            .wires
            .iter()
            .filter_map(|(p, w)| (w.torrent == id).then_some(*p))
            .collect();
        let mut tasks = Vec::new();
        for peer in &peers {
            if let Some(wire) = self.wires.remove(peer) {
                wire.task.abort();
                tasks.push(wire.task);
            }
        }
        for task in tasks {
            let _ = task.await;
        }
        self.purge_retired_events();
        for peer in peers {
            self.pending_failures.remove(&peer);
            self.session.disconnect(id, peer).ok();
        }
    }

    /// Once actors are joined, discard their payload frames before releasing
    /// the assembly reservations which covered those frames. Route surviving
    /// peers' messages straight into the bounded engine inbox, never an
    /// accumulating side queue. Failure handling is deferred to step.
    fn purge_retired_events(&mut self) {
        for _ in 0..EVENT_CAP {
            let Ok(event) = self.events.try_recv() else {
                break;
            };
            if event.uploaded != 0 {
                let _ = self.session.uploaded(event.torrent, event.uploaded);
                if let Some(wire) = self.wires.get_mut(&event.peer) {
                    wire.uploading = false;
                    wire.window_up = wire.window_up.saturating_add(u64::from(event.uploaded));
                }
            }
            if !self.wires.contains_key(&event.peer) {
                continue;
            }
            let error = match event.message {
                Ok(message) => self
                    .deliver(event.torrent, event.peer, message)
                    .err()
                    .map(|e| e.0),
                Err(error) => Some(error),
            };
            if let Some(error) = error {
                self.pending_failures.insert(event.peer, error);
            }
        }
    }

    async fn retire_peer(&mut self, peer: PeerId) {
        if let Some(wire) = self.wires.remove(&peer) {
            wire.task.abort();
            let _ = wire.task.await;
            self.purge_retired_events();
            self.session.disconnect(wire.torrent, peer).ok();
        }
        self.pending_failures.remove(&peer);
    }

    pub async fn set_limits(&mut self, limits: Limits) -> Result<()> {
        self.session
            .set_limits(limits.clone(), self.now_ns())
            .map_err(|_| DriverError("limits rejected"))?;
        self.limits = limits;
        // Preserve running intent separately from slot admission.
        let rows = self.snapshot();
        let mut admitted = 0usize;
        let mut resume = Vec::new();
        for row in rows.jobs.iter() {
            if matches!(row.state, JobState::Downloading | JobState::Queued) {
                if admitted >= self.limits.download_jobs {
                    self.pause(row.id).await?;
                    self.jobs.0.get_mut(&row.id).unwrap().queued = true;
                } else {
                    admitted += 1;
                    if row.state == JobState::Queued {
                        resume.push(row.id);
                    }
                }
            }
        }
        // Quiesce peers when reduced connection caps require it. Speed=0
        // continues to mean unlimited, never pause.
        let mut counts = HashMap::new();
        // Retain preferred peers first when caps shrink; avoided peers go first.
        let peers = self.peers_by_preference();
        let mut total = 0;
        for peer in peers {
            let wire = &self.wires[&peer];
            let count = counts.entry(wire.torrent).or_insert(0usize);
            if total >= self.limits.connections_total || *count >= self.limits.connections_per_job {
                self.retire_peer(peer).await;
            } else {
                total += 1;
                *count += 1;
            }
        }
        for id in resume {
            if let Err(error) = self.resume(id).await {
                self.jobs.0.get_mut(&id).unwrap().last_error = Some(error.0);
            }
        }
        let rows = self.session.snapshot();
        let mut active_uploads = 0;
        let mut peers: Vec<_> = self.wires.keys().copied().collect();
        peers.sort_by_key(|p| p.0);
        for peer in peers {
            let wire = &self.wires[&peer];
            let denied_seed = rows
                .jobs
                .iter()
                .any(|j| j.id == wire.torrent && j.state == JobState::Seeding)
                && !self.seed_admitted(wire.torrent);
            if denied_seed || (!wire.choked && active_uploads >= self.limits.upload_peer_slots) {
                // Joining actors also drops any admitted upload buffer, so
                // reducing slots acknowledges quiescence of excess writers.
                self.retire_peer(peer).await;
            } else if !wire.choked {
                active_uploads += 1;
            }
        }
        Ok(())
    }

    fn deliver(&mut self, id: TorrentId, peer: PeerId, message: Message) -> Result<()> {
        let count = self
            .jobs
            .0
            .get(&id)
            .ok_or(DriverError("unknown job"))?
            .torrent
            .piece_count();
        let result = match message {
            Message::Interested => {
                if let Some(w) = self.wires.get_mut(&peer) {
                    w.interested = true;
                }
                Ok(())
            }
            Message::NotInterested => {
                if let Some(w) = self.wires.get_mut(&peer) {
                    w.interested = false;
                    w.uploads.clear();
                }
                Ok(())
            }
            Message::Request {
                index,
                begin,
                length,
            } => {
                let wire = self
                    .wires
                    .get_mut(&peer)
                    .ok_or(DriverError("unknown peer"))?;
                if wire.choked || !wire.interested {
                    return Ok(());
                }
                if length == 0
                    || length > 16384
                    || index as usize >= count
                    || u64::from(begin) + u64::from(length)
                        > self.jobs.0[&id].torrent.piece_length()
                {
                    return Err(DriverError("invalid upload request"));
                }
                let request = (index, begin, length);
                if !wire.uploads.contains(&request) {
                    if wire.uploads.len() + usize::from(wire.uploading) >= 8 {
                        return Err(DriverError("upload request queue exceeded"));
                    }
                    wire.uploads.push_back(request);
                }
                Ok(())
            }
            Message::Cancel {
                index,
                begin,
                length,
            } => {
                if let Some(wire) = self.wires.get_mut(&peer) {
                    wire.uploads.retain(|r| *r != (index, begin, length));
                }
                // A write already admitted to the transport can race Cancel.
                Ok(())
            }
            Message::Extended {
                id: extension,
                payload,
            } => {
                self.metadata_request(peer, extension, &payload)?;
                Ok(())
            }
            Message::Bitfield(bits) => {
                let mut words = vec![0; count.div_ceil(64)];
                unpack_bitfield(&bits, count, &mut words)
                    .map_err(|_| DriverError("invalid peer bitfield"))?;
                self.session.note_bitfield(id, peer, words)
            }
            Message::Have { piece } => self.session.note_have(id, peer, piece),
            Message::HaveAll => {
                let mut words = vec![u64::MAX; count.div_ceil(64)];
                if let Some(last) = words.last_mut() {
                    *last &= u64::MAX >> ((64 - count % 64) % 64);
                }
                self.session.note_bitfield(id, peer, words)
            }
            Message::Unchoke => {
                if let Some(wire) = self.wires.get_mut(&peer) {
                    wire.remote_choked = false;
                }
                self.session.set_peer_choking(id, peer, false)
            }
            Message::Choke => {
                if let Some(wire) = self.wires.get_mut(&peer) {
                    wire.remote_choked = true;
                }
                self.session.set_peer_choking(id, peer, true)
            }
            Message::Piece { index, begin, data } => {
                if let Some(wire) = self.wires.get_mut(&peer) {
                    wire.window_down = wire.window_down.saturating_add(data.len() as u64);
                }
                self.session
                    .inject_block(id, peer, index, begin, data.to_vec())
            }
            _ => Ok(()),
        };
        result.map_err(|_| DriverError("peer event rejected"))
    }

    fn metadata_request(&mut self, peer: PeerId, extension: u8, payload: &[u8]) -> Result<()> {
        use crate::{metadata::OUR_MD_ID, preflight};
        if extension != 0 && extension != OUR_MD_ID {
            return Ok(());
        }
        if payload.len() > 16384 {
            return Err(DriverError("extension control too large"));
        }
        preflight::validate(payload).map_err(|_| DriverError("invalid extension control"))?;
        if extension == 0 {
            if let Ok(handshake) = ExtensionHandshake::decode(payload) {
                let name = crate::client_name(Some(&handshake));
                if let Some(wire) = self.wires.get_mut(&peer) {
                    wire.client = name;
                }
            }
            let id = preflight::field(payload, b"m")
                .and_then(|m| preflight::field(m, b"ut_metadata"))
                .and_then(preflight::integer)
                .unwrap_or(0);
            let id = u8::try_from(id).map_err(|_| DriverError("invalid metadata extension id"))?;
            if let Some(wire) = self.wires.get_mut(&peer) {
                wire.metadata_id = (id != 0).then_some(id);
            }
        } else if extension == OUR_MD_ID {
            let wire = self.wires.get(&peer).ok_or(DriverError("unknown peer"))?;
            let Some(remote_id) = wire.metadata_id else {
                return Err(DriverError("metadata not negotiated"));
            };
            if preflight::field(payload, b"msg_type").and_then(preflight::integer) != Some(0) {
                return Err(DriverError("unsupported metadata message"));
            }
            let piece = preflight::field(payload, b"piece")
                .and_then(preflight::integer)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(DriverError("invalid metadata index"))?;
            let raw = self.jobs.0[&wire.torrent].torrent.meta.raw_info();
            let start = (piece as usize)
                .checked_mul(rbit::METADATA_PIECE_SIZE)
                .filter(|n| *n < raw.len())
                .ok_or(DriverError("invalid metadata index"))?;
            let end = (start + rbit::METADATA_PIECE_SIZE).min(raw.len());
            let permit = self
                .session
                .reserve_control((end - start) * 3 + 256)
                .ok_or(DriverError("metadata send memory exhausted"))?;
            let data = MetadataMessage::data(piece, raw.len() as u32, raw.slice(start..end));
            let message = Message::Extended {
                id: remote_id,
                payload: data
                    .encode()
                    .map_err(|_| DriverError("metadata encoding failed"))?,
            };
            self.queue(
                peer,
                WireCommand {
                    message,
                    _permit: Some(permit),
                    upload_bytes: 0,
                    _owned: None,
                },
            )?;
        }
        Ok(())
    }

    /// Slot order: the optimistic peer first, then Prefer, Normal, Avoid in
    /// stable connection order. One slot inside the configured total rotates
    /// every interval to an interested peer that preference order would leave
    /// choked, so a single slot rotates rather than a second appearing. The
    /// first interval after start runs on preference alone.
    fn upload_order(&mut self, rows: &Snapshot) -> Vec<PeerId> {
        let ordered = self.regular_order(rows);
        let slots = self.limits.upload_peer_slots;
        if slots == 0 {
            self.optimistic = None;
            return ordered;
        }
        let now = self.elapsed_s();
        let eligible = |driver: &Self, peer: PeerId| {
            driver.wires.get(&peer).is_some_and(|wire| {
                wire.interested
                    && driver.session.upload_admitted(wire.torrent)
                    && rows.jobs.iter().any(|job| {
                        job.id == wire.torrent
                            && matches!(job.state, JobState::Downloading | JobState::Seeding)
                    })
            })
        };
        // A vanished or uninterested optimistic peer hands its slot back to
        // preference order until the next rotation; no early reassignment.
        if self
            .optimistic
            .is_some_and(|(peer, _)| !eligible(self, peer))
        {
            self.optimistic = None;
        }
        if now.saturating_sub(self.optimistic_rotated_s) >= self.optimistic_interval_s {
            self.optimistic_rotated_s = now;
            // Regular slots are the configured total minus the optimistic one.
            let regular: Vec<PeerId> = ordered
                .iter()
                .copied()
                .filter(|peer| eligible(self, *peer))
                .take(slots.saturating_sub(1))
                .collect();
            let last = self.optimistic.map(|(peer, _)| peer.0);
            let mut candidates: Vec<PeerId> = ordered
                .iter()
                .copied()
                .filter(|peer| eligible(self, *peer) && !regular.contains(peer))
                .collect();
            candidates.sort_unstable_by_key(|peer| peer.0);
            let next = match last {
                Some(last) => candidates
                    .iter()
                    .copied()
                    .find(|peer| peer.0 > last)
                    .or_else(|| candidates.first().copied()),
                None => candidates.first().copied(),
            };
            self.optimistic = next.map(|peer| (peer, now));
        }
        match self.optimistic {
            Some((peer, _)) => {
                let mut order = vec![peer];
                order.extend(ordered.into_iter().filter(|other| *other != peer));
                order
            }
            None => ordered,
        }
    }

    async fn service_uploads(&mut self) {
        let rows = self.session.snapshot();
        let peers = self.upload_order(&rows);
        let mut slots = 0;
        for peer in peers {
            let Some(wire) = self.wires.get(&peer) else {
                continue;
            };
            let id = wire.torrent;
            let eligible = wire.interested
                && self.session.upload_admitted(id)
                && rows.jobs.iter().any(|j| {
                    j.id == id && matches!(j.state, JobState::Downloading | JobState::Seeding)
                })
                && slots < self.limits.upload_peer_slots;
            if eligible {
                slots += 1;
            }
            let choked = !eligible;
            if wire.choked != choked {
                if self
                    .queue(
                        peer,
                        if choked {
                            Message::Choke
                        } else {
                            Message::Unchoke
                        },
                    )
                    .is_err()
                {
                    self.retire_peer(peer).await;
                    continue;
                }
                let wire = self.wires.get_mut(&peer).unwrap();
                wire.choked = choked;
                if choked {
                    wire.uploads.clear();
                }
            }
            let wire = &self.wires[&peer];
            if choked || wire.uploading {
                continue;
            }
            let Some(&(piece, begin, length)) = wire.uploads.front() else {
                continue;
            };
            let reservation = self
                .session
                .reserve_upload(id, piece, begin, length, self.now_ns());
            let (full, permit) = match reservation {
                Ok(Some(admitted)) => admitted,
                Ok(None) => continue,
                Err(_) => {
                    self.jobs.0.get_mut(&id).unwrap().last_error =
                        Some("invalid or unverified upload request");
                    self.retire_peer(peer).await;
                    continue;
                }
            };
            let data = self.store.read_piece(id, piece, full);
            let data = match data {
                Ok(Some(data)) if data.len() == full => data,
                _ => {
                    self.session.upload_read_failed(id, piece).ok();
                    self.jobs.0.get_mut(&id).unwrap().last_error =
                        Some("payload changed or failed during upload read; recheck required");
                    self.stop_wires(id).await;
                    continue;
                }
            };
            let disk_owner = OwnedPayload::new(&self.upload_ownership, data.capacity());
            if !self.jobs.verify_for(id, piece, &data) {
                drop(data);
                drop(disk_owner);
                self.session.upload_read_failed(id, piece).ok();
                self.jobs.0.get_mut(&id).unwrap().last_error =
                    Some("payload changed or failed during upload read; recheck required");
                self.stop_wires(id).await;
                continue;
            }
            let block_owner = OwnedPayload::new(&self.upload_ownership, length as usize);
            let message = Message::Piece {
                index: piece,
                begin,
                data: Bytes::copy_from_slice(
                    &data[begin as usize..begin as usize + length as usize],
                ),
            };
            drop(data);
            drop(disk_owner);
            if self
                .queue(
                    peer,
                    WireCommand {
                        message,
                        _permit: Some(permit),
                        upload_bytes: length,
                        _owned: Some(block_owner),
                    },
                )
                .is_err()
            {
                self.retire_peer(peer).await;
                continue;
            }
            let wire = self.wires.get_mut(&peer).unwrap();
            wire.uploads.pop_front();
            wire.uploading = true;
        }
    }

    async fn event(&mut self, event: WireEvent, report: &mut DriverReport) {
        if event.uploaded != 0 {
            let _ = self.session.uploaded(event.torrent, event.uploaded);
            if let Some(wire) = self.wires.get_mut(&event.peer) {
                wire.uploading = false;
                wire.window_up = wire.window_up.saturating_add(u64::from(event.uploaded));
            }
        }
        if !self.wires.contains_key(&event.peer) {
            return;
        }
        let error = match event.message {
            Ok(message) => self
                .deliver(event.torrent, event.peer, message)
                .err()
                .map(|e| e.0),
            Err(error) => Some(error),
        };
        if let Some(error) = error {
            if let Some(job) = self.jobs.0.get_mut(&event.torrent) {
                job.last_error = Some(error);
            }
            report.peer_errors.push(error);
            report.disconnected += 1;
            self.retire_peer(event.peer).await;
        }
    }
    async fn pump(&mut self, report: &mut DriverReport) -> Result<bool> {
        let SessionReport {
            verified,
            outbox,
            outcomes,
            completed,
            blamed,
            errors,
            ..
        } = self.session.update(self.now_ns(), &self.jobs);
        if !errors.is_empty() {
            return Err(DriverError("engine storage or scheduling failure"));
        }
        let changed = !outbox.is_empty() || outcomes != 0;
        report.work_outcomes += outcomes;
        report.completed.extend(completed.iter().copied());
        report.hash_failures += blamed.len();
        for (id, piece) in verified {
            for (peer, wire) in &self.wires {
                if wire.torrent == id && self.queue(*peer, Message::Have { piece }).is_err() {
                    self.pending_failures
                        .insert(*peer, "peer command queue full or closed");
                }
            }
        }
        for entry in outbox {
            let PeerAction::Send(action) = entry.action;
            let message = match action {
                PeerMessage::Interested => Message::Interested,
                PeerMessage::NotInterested => Message::NotInterested,
                PeerMessage::Choke => Message::Choke,
                PeerMessage::Unchoke => Message::Unchoke,
                PeerMessage::Request {
                    piece,
                    begin,
                    length,
                } => Message::Request {
                    index: piece,
                    begin,
                    length,
                },
                PeerMessage::Cancel {
                    piece,
                    begin,
                    length,
                } => Message::Cancel {
                    index: piece,
                    begin,
                    length,
                },
            };
            if self
                .wires
                .get(&entry.peer)
                .is_some_and(|wire| wire.sender.try_send(message.into()).is_err())
            {
                self.event(
                    WireEvent {
                        torrent: entry.torrent,
                        peer: entry.peer,
                        message: Err("peer command queue full or closed"),
                        uploaded: 0,
                    },
                    report,
                )
                .await;
            }
        }
        // With no listener configured preserve the diagnostic download-only
        // lifecycle. A listening driver keeps admitted complete jobs serving.
        for id in completed {
            if self.listener.is_none() || !self.seed_admitted(id) {
                self.stop_wires(id).await;
            }
        }
        self.service_uploads().await;
        Ok(changed)
    }

    /// Service at most EVENT_CAP ready wire events, then wait only when idle.
    /// This method never manufactures progress and has no dependency on rendering.
    pub async fn step(&mut self, wait: Duration) -> Result<DriverReport> {
        let mut report = DriverReport::default();
        self.sample_rates();
        self.accept_incoming().await;
        let failures = std::mem::take(&mut self.pending_failures);
        for (peer, error) in failures {
            if let Some(wire) = self.wires.get(&peer) {
                self.event(
                    WireEvent {
                        torrent: wire.torrent,
                        peer,
                        message: Err(error),
                        uploaded: 0,
                    },
                    &mut report,
                )
                .await;
            }
        }
        let mut any = false;
        for _ in 0..EVENT_CAP {
            let Ok(event) = self.events.try_recv() else {
                break;
            };
            any = true;
            self.event(event, &mut report).await;
        }
        let changed = self.pump(&mut report).await?;
        if !any
            && !changed
            && let Ok(Some(event)) =
                timeout(wait.min(Duration::from_millis(50)), self.events.recv()).await
        {
            self.event(event, &mut report).await;
            self.pump(&mut report).await?;
        }
        self.refresh_due_tracker().await;
        Ok(report)
    }

    fn elapsed_s(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    pub fn file_progress(
        &self,
        id: TorrentId,
    ) -> Result<Vec<templartorrent_engine::session::FileProgress>> {
        self.session
            .file_progress(id)
            .map_err(|_| DriverError("unknown job"))
    }

    pub fn peer_status(&self, id: TorrentId) -> Vec<PeerStatus> {
        let mut rows: Vec<_> = self
            .wires
            .values()
            .filter(|wire| wire.torrent == id)
            .map(|wire| PeerStatus {
                address: wire.source.address,
                client: wire.client.clone(),
                remote_choked: wire.remote_choked,
                remote_interested: wire.interested,
                we_choke: wire.choked,
                preference: self.peer_preference(id, wire.source.address),
                down_bps: wire.down_bps,
                up_bps: wire.up_bps,
            })
            .collect();
        rows.sort_by_key(|row| row.address);
        rows
    }

    pub fn info_hash(&self, id: TorrentId) -> Option<[u8; 20]> {
        match self.jobs.0.get(&id)?.torrent.meta.info_hash {
            InfoHash::V1(hash) => Some(hash),
            _ => None,
        }
    }

    /// Snapshot one job for persistence. `paused` is the user's intent: a job
    /// parked only by the download-slot cap is still running.
    pub fn export_job(&mut self, id: TorrentId) -> Result<JobRecord> {
        let state = self
            .session
            .snapshot()
            .jobs
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.state)
            .ok_or(DriverError("unknown job"))?;
        let progress = self
            .session
            .file_progress(id)
            .map_err(|_| DriverError("unknown job"))?;
        let order = self
            .session
            .file_order(id)
            .map_err(|_| DriverError("unknown job"))?;
        let job = self.jobs.0.get(&id).ok_or(DriverError("unknown job"))?;
        let InfoHash::V1(info_hash) = job.torrent.meta.info_hash else {
            return Err(DriverError("unsupported torrent"));
        };
        let raw = job.torrent.meta.raw_info();
        let mut torrent = Vec::with_capacity(raw.len() + 8);
        torrent.extend_from_slice(b"d4:info");
        torrent.extend_from_slice(raw);
        torrent.push(b'e');
        let mut preferences: Vec<_> = job
            .preferences
            .iter()
            .map(|(ip, preference)| (*ip, *preference))
            .collect();
        preferences.sort_by_key(|(ip, _)| *ip);
        Ok(JobRecord {
            info_hash,
            torrent,
            destination: job.destination.clone(),
            paused: state == JobState::Paused && !job.queued,
            public_bootstrap: job.bootstrap,
            wanted: progress.iter().map(|file| file.wanted).collect(),
            order,
            trackers: job
                .trackers
                .iter()
                .map(|tracker| (tracker.url.clone(), tracker.origin == "original"))
                .collect(),
            preferences,
        })
    }

    /// Pieces the engine has verified, plus file identity taken after a
    /// full sync. The barrier is set because verification runs only after
    /// `write_piece` has synced the bytes, and [`SharedStore::durable_identity`]
    /// syncs inode fields before the stamps in this record.
    pub fn export_resume(&self, id: TorrentId) -> Result<ResumeRecord> {
        let job = self.jobs.0.get(&id).ok_or(DriverError("unknown job"))?;
        let words = self
            .session
            .verified_pieces(id)
            .map_err(|_| DriverError("unknown job"))?;
        let stamps = self
            .store
            .durable_identity(id)
            .map_err(|_| DriverError("resume identity check failed"))?;
        if stamps.len() != job.torrent.paths.len() {
            return Err(DriverError("resume identity check failed"));
        }
        Ok(ResumeRecord::checkpoint(
            job.torrent.piece_count(),
            &words,
            &stamps,
        ))
    }

    /// How the most recent [`Driver::restore_job`] applied a resume bitmap.
    /// Jobs that were not restored report an empty scan.
    pub fn resume_scan(&self, id: TorrentId) -> Result<ResumeScan> {
        let job = self.jobs.0.get(&id).ok_or(DriverError("unknown job"))?;
        Ok(ResumeScan {
            trusted: job.resume_trusted,
            checked: job.resume_checked,
            failed: job.resume_failed,
            note: job.resume_note,
        })
    }

    fn plan_resume(
        &self,
        id: TorrentId,
        piece_count: usize,
        piece_length: u64,
        lengths: &[u64],
        resume: Option<&ResumeRecord>,
    ) -> (Vec<u32>, Option<&'static str>) {
        let Some(resume) = resume else {
            return (Vec::new(), None);
        };
        let live = match self.store.file_identity(id) {
            Ok(live) => live,
            Err(_) => return (Vec::new(), Some("resume identity check failed")),
        };
        match resume::trusted_pieces(piece_count, piece_length, lengths, resume, &live) {
            Ok(pieces) => (pieces, None),
            Err(reason) => (Vec::new(), Some(reason)),
        }
    }

    /// Reinstall a saved job from its stored metainfo. The destination is
    /// rechecked from disk; no peer is contacted. A resume bitmap, when
    /// supplied, skips the hash for pieces whose files still match the
    /// recorded identity and whose record carried a durability barrier.
    /// Saved trackers the job already knew are admitted (their hosts resolve)
    /// and announce on their normal schedule; a private job keeps only
    /// original trackers. An invalid saved selection or order is dropped and
    /// noted, never applied.
    pub async fn restore_job(&mut self, record: &JobRecord) -> Result<TorrentId> {
        self.restore_job_with_resume(record, None).await
    }

    /// [`Driver::restore_job`] with an optional resume bitmap. `None` hashes
    /// every piece, which is also what a missing bitmap file means.
    pub async fn restore_job_with_resume(
        &mut self,
        record: &JobRecord,
        resume: Option<&ResumeRecord>,
    ) -> Result<TorrentId> {
        let (torrent, hash) =
            ValidatedV1::from_torrent_bytes(&record.torrent, &self.metadata_budget)
                .map_err(DriverError)?;
        if hash != record.info_hash {
            return Err(DriverError("stored torrent does not match its record"));
        }
        if self
            .jobs
            .0
            .values()
            .any(|job| matches!(job.torrent.meta.info_hash, InfoHash::V1(h) if h == hash))
        {
            return Err(DriverError("torrent already exists"));
        }
        let id = self
            .session
            .add(JobSpec {
                name: torrent.name().into(),
                files: torrent.files(),
                piece_length: torrent.piece_length(),
                max_pieces: MAX_DRIVER_PIECES,
            })
            .map_err(|_| DriverError("torrent exceeds session admission limits"))?;
        let lengths: Vec<u64> = torrent.paths.iter().map(|(_, length)| *length).collect();
        let admitted =
            (|| {
                let root = prepare_destination(&record.destination, ExistingData::Recheck)?;
                if self.jobs.0.values().any(|job| {
                    job.destination.starts_with(&root) || root.starts_with(&job.destination)
                }) {
                    return Err(DriverError("destination already owned by another job"));
                }
                self.store
                    .add_job(id, &root, &torrent.paths, torrent.piece_length())
                    .map_err(|_| DriverError("payload storage admission failed"))?;
                let (trusted, note) = self.plan_resume(
                    id,
                    torrent.piece_count(),
                    torrent.piece_length(),
                    &lengths,
                    resume,
                );
                let trusted_count = if trusted.is_empty() {
                    0
                } else {
                    self.session
                        .adopt_trusted(id, &trusted)
                        .map_err(|_| DriverError("resume recheck failed"))?
                };
                let report = self
                    .session
                    .recheck(id, &self.store, &torrent)
                    .map_err(|_| DriverError("resume recheck failed"))?;
                Ok((root, trusted_count, report.checked, report.failed, note))
            })();
        let (root, resume_trusted, resume_checked, resume_failed, resume_note) = match admitted {
            Ok(root) => root,
            Err(error) => {
                let _ = self.session.remove(id, false);
                let _ = self.store.close_job(id);
                return Err(error);
            }
        };
        let mut last_error = resume_note;
        if record.wanted.len() == torrent.paths.len() {
            for (index, wanted) in record.wanted.iter().enumerate() {
                if !wanted && self.session.set_wanted(id, index, false).is_err() {
                    last_error = Some("saved file selection was invalid");
                }
            }
        } else if !record.wanted.is_empty() {
            last_error = Some("saved file selection was invalid");
        }
        if !record.order.is_empty() && self.session.set_order(id, record.order.clone()).is_err() {
            last_error = Some("saved file order was invalid");
        }
        let private = torrent.is_private();
        let mut trackers: Vec<Tracked> = Vec::new();
        let mut originals = 0u64;
        for (url, original) in record.trackers.iter().take(crate::discovery::MAX_TRACKERS) {
            if private && !*original {
                continue;
            }
            let Ok(admitted) =
                crate::discovery::admit_tracker(url, self.allow_local_trackers).await
            else {
                continue;
            };
            if trackers
                .iter()
                .any(|known| known.url == admitted.tracker.url)
            {
                continue;
            }
            let origin_id = if *original {
                originals += 1;
                originals
            } else {
                0
            };
            trackers.push(Tracked {
                url: admitted.tracker.url,
                origin: if *original { "original" } else { "added" },
                origin_id,
                local: admitted.local,
                seeds: None,
                peers: None,
                last_error: None,
            });
        }
        // A private job without an original tracker can never admit a peer.
        let private_tracker = private.then(|| {
            trackers
                .iter()
                .find(|tracker| tracker.origin == "original")
                .map_or(u64::MAX, |tracker| tracker.origin_id)
        });
        let mut preferences = HashMap::new();
        for (ip, preference) in record.preferences.iter().take(PREFERENCE_CAP) {
            if *preference != PeerPreference::Normal {
                preferences.insert(*ip, *preference);
            }
        }
        self.jobs.0.insert(
            id,
            Job {
                torrent,
                sources: Vec::new(),
                private_tracker,
                destination: root,
                queued: false,
                last_error,
                trackers: Vec::new(),
                preferences,
                bootstrap: record.public_bootstrap,
                resume_trusted,
                resume_checked,
                resume_failed,
                resume_note,
            },
        );
        for tracker in trackers {
            if self.session.add_tracker(id).is_err() {
                break;
            }
            if let Some(job) = self.jobs.0.get_mut(&id) {
                job.trackers.push(tracker);
            }
        }
        if record.paused {
            self.pause(id).await?;
            return Ok(id);
        }
        // Running intent still respects the download-slot cap.
        let rows = self.snapshot();
        let running = rows
            .jobs
            .iter()
            .filter(|row| row.id != id && row.state == JobState::Downloading)
            .count();
        let state = rows
            .jobs
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.state);
        if state == Some(JobState::Downloading) && running >= self.limits.download_jobs {
            self.pause(id).await?;
            self.jobs.0.get_mut(&id).unwrap().queued = true;
        }
        Ok(id)
    }

    /// Local preference for one peer address in one job. Keyed by IP, so an
    /// incoming peer cannot evade Block by reconnecting from another port.
    /// Block closes matching connections now, drops the address from the
    /// candidate list and refuses reconnection, incoming handshakes and
    /// tracker-supplied candidates. Avoid narrows the request pipeline and is
    /// released first when caps shrink; Prefer is retained first and unchoked
    /// first. Nothing here commands the remote side, and blocking a sole
    /// supplier can leave a job unable to complete.
    pub async fn set_peer_preference(
        &mut self,
        id: TorrentId,
        address: SocketAddr,
        preference: PeerPreference,
    ) -> Result<()> {
        let ip = address.ip();
        {
            let job = self.jobs.0.get_mut(&id).ok_or(DriverError("unknown job"))?;
            if preference == PeerPreference::Normal {
                job.preferences.remove(&ip);
            } else {
                if !job.preferences.contains_key(&ip) && job.preferences.len() >= PREFERENCE_CAP {
                    return Err(DriverError("peer preference table is full"));
                }
                job.preferences.insert(ip, preference);
            }
            if preference == PeerPreference::Block {
                job.sources.retain(|source| source.address.ip() != ip);
            }
        }
        // A manual preference applies now; the rate-ranking hysteresis must
        // not delay it.
        self.choke_evaluated_s = None;
        let peers: Vec<_> = self
            .wires
            .iter()
            .filter_map(|(peer, wire)| {
                (wire.torrent == id && wire.source.address.ip() == ip).then_some(*peer)
            })
            .collect();
        for peer in peers {
            if preference == PeerPreference::Block {
                self.retire_peer(peer).await;
            } else {
                let _ = self
                    .session
                    .set_peer_pipeline(id, peer, pipeline_for(preference));
            }
        }
        Ok(())
    }

    pub fn peer_preference(&self, id: TorrentId, address: SocketAddr) -> PeerPreference {
        self.jobs
            .0
            .get(&id)
            .and_then(|job| job.preferences.get(&address.ip()).copied())
            .unwrap_or(PeerPreference::Normal)
    }

    /// Current request pipeline cap for one connected peer, for diagnostics.
    pub fn peer_pipeline(&self, id: TorrentId, address: SocketAddr) -> Option<usize> {
        self.wires
            .iter()
            .find(|(_, wire)| wire.torrent == id && wire.source.address == address)
            .and_then(|(peer, _)| self.session.peer_pipeline(id, *peer).ok())
    }

    /// Regular slot order: preference first, then the peer that currently
    /// gives us the most verified payload (or, for a seeding job, takes the
    /// most), then connection order. Re-ranked every [`CHOKE_INTERVAL_S`];
    /// peers that arrived since are appended, so an order never churns on a
    /// single fast step.
    fn regular_order(&mut self, rows: &Snapshot) -> Vec<PeerId> {
        let now = self.elapsed_s();
        let due = self
            .choke_evaluated_s
            .is_none_or(|at| now.saturating_sub(at) >= CHOKE_INTERVAL_S);
        if due {
            let mut entries: Vec<(u8, u64, u64, PeerId)> = self
                .wires
                .iter()
                .map(|(peer, wire)| {
                    let seeding = rows
                        .jobs
                        .iter()
                        .any(|job| job.id == wire.torrent && job.state == JobState::Seeding);
                    let score = if seeding { wire.up_bps } else { wire.down_bps };
                    (
                        preference_rank(self.peer_preference(wire.torrent, wire.source.address)),
                        score,
                        peer.0,
                        *peer,
                    )
                })
                .collect();
            self.choke_order = rank_peers(&mut entries);
            self.choke_evaluated_s = Some(now);
            return self.choke_order.clone();
        }
        self.choke_order
            .retain(|peer| self.wires.contains_key(peer));
        let mut extra: Vec<PeerId> = self
            .wires
            .keys()
            .copied()
            .filter(|peer| !self.choke_order.contains(peer))
            .collect();
        extra.sort_unstable_by_key(|peer| peer.0);
        let mut order = self.choke_order.clone();
        order.extend(extra);
        order
    }

    /// Connected peers ordered Prefer, Normal, Avoid, then by connection id.
    fn peers_by_preference(&self) -> Vec<PeerId> {
        let mut peers: Vec<_> = self
            .wires
            .iter()
            .map(|(peer, wire)| {
                (
                    preference_rank(self.peer_preference(wire.torrent, wire.source.address)),
                    peer.0,
                    *peer,
                )
            })
            .collect();
        peers.sort_unstable_by_key(|(rank, order, _)| (*rank, *order));
        peers.into_iter().map(|(_, _, peer)| peer).collect()
    }

    pub fn tracker_status(&self, id: TorrentId) -> Result<Vec<TrackerStatus>> {
        let job = self.jobs.0.get(&id).ok_or(DriverError("unknown job"))?;
        let mut rows = Vec::with_capacity(job.trackers.len());
        for (index, tracker) in job.trackers.iter().enumerate() {
            let schedule = self
                .session
                .tracker_schedule(id, index)
                .map_err(|_| DriverError("unknown tracker"))?;
            rows.push(TrackerStatus {
                label: crate::discovery::redact_tracker_url(&tracker.url),
                origin: tracker.origin,
                seeds: tracker.seeds,
                peers: tracker.peers,
                manual_due_s: schedule.manual_due_s,
                last_error: tracker.last_error.clone(),
            });
        }
        Ok(rows)
    }

    /// Resolve peers from the magnet's own trackers, then install the job.
    /// Explicit `sources` are additional permitted endpoints. Configured public
    /// trackers are appended only after metadata proves the torrent is public.
    pub async fn add_magnet_with_discovery(
        &mut self,
        uri: &str,
        destination: &Path,
        existing: ExistingData,
        sources: &[PeerSource],
        public_bootstrap: bool,
    ) -> Result<TorrentId> {
        if uri.len() > 16 * 1024 {
            return Err(DriverError("magnet exceeds 16 KiB limit"));
        }
        let magnet = rbit::MagnetLink::parse(uri).map_err(|_| DriverError("invalid magnet"))?;
        let rbit::InfoHash::V1(hash) = magnet.info_hash else {
            return Err(DriverError("v2/hybrid magnets are not supported"));
        };
        let port = self
            .listening_address()
            .map(|address| address.port())
            .ok_or(DriverError("listening port is not configured"))?;
        let mut originals = Vec::new();
        for url in magnet.trackers.iter().take(crate::discovery::MAX_TRACKERS) {
            let Ok(admitted) =
                crate::discovery::admit_tracker(url, self.allow_local_trackers).await
            else {
                continue;
            };
            if originals
                .iter()
                .any(|known: &Tracked| known.url == admitted.tracker.url)
            {
                continue;
            }
            originals.push(Tracked {
                url: admitted.tracker.url,
                origin: "original",
                origin_id: originals.len() as u64 + 1,
                local: admitted.local,
                seeds: None,
                peers: None,
                last_error: None,
            });
        }
        let mut discovered = Vec::new();
        let mut announced = Vec::new();
        let peer_id = self.peer_id;
        for tracker in originals {
            let url = tracker.url.clone();
            match announce_detached(
                &url,
                self.allow_local_trackers,
                crate::announce::AnnounceRequest {
                    info_hash: &hash,
                    peer_id: &peer_id,
                    port,
                    uploaded: 0,
                    downloaded: 0,
                    left: UNKNOWN_LEFT,
                    event: rbit::TrackerEvent::Started,
                },
            )
            .await
            {
                Ok(response) => {
                    self.collect_peers(
                        &response,
                        tracker.local,
                        tracker.origin_id,
                        &mut discovered,
                    );
                    announced.push((tracker, Some(response)));
                }
                Err(_) => announced.push((tracker, None)),
            }
        }
        for source in sources {
            if discovered.len() >= CANDIDATE_CAP {
                break;
            }
            let allowed =
                matches!(source.origin, PeerOrigin::OriginalTracker(_)) || public_bootstrap;
            if allowed
                && !discovered
                    .iter()
                    .any(|known: &PeerSource| known.address == source.address)
            {
                discovered.push(*source);
            }
        }
        if discovered.is_empty() {
            return Err(DriverError(
                "no admissible tracker peers or explicit sources",
            ));
        }
        let id = self
            .add_magnet(uri, destination, existing, &discovered, public_bootstrap)
            .await?;
        let private_id = self.jobs.0.get(&id).and_then(|job| job.private_tracker);
        let mut kept = Vec::new();
        for (mut tracker, response) in announced {
            if private_id.is_some_and(|active| tracker.origin_id != active) {
                continue;
            }
            let index = self
                .session
                .add_tracker(id)
                .map_err(|_| DriverError("tracker table full"))?;
            let now = self.elapsed_s();
            if let Some(response) = response {
                let _ = self.session.tracker_success(
                    id,
                    index,
                    now,
                    response.interval,
                    response.min_interval,
                );
                tracker.seeds = response.complete;
                tracker.peers = response.incomplete;
            } else {
                let _ = self.session.tracker_failure(id, index, now, 0);
                tracker.last_error = Some("announce failed".into());
            }
            kept.push(tracker);
        }
        if let Some(job) = self.jobs.0.get_mut(&id) {
            job.trackers = kept;
        }
        self.augment_public(id).await;
        Ok(id)
    }

    pub fn magnet_from_torrent_bytes(bytes: &[u8]) -> Result<String> {
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(DriverError("torrent file exceeds 16 MiB limit"));
        }
        let meta =
            rbit::Metainfo::from_bytes(bytes).map_err(|_| DriverError("invalid torrent file"))?;
        let rbit::InfoHash::V1(hash) = meta.info_hash else {
            return Err(DriverError("v2/hybrid torrents are not supported"));
        };
        let mut magnet = format!("magnet:?xt=urn:btih:{}", hex_encode(&hash));
        for tracker in meta
            .trackers()
            .into_iter()
            .take(crate::discovery::MAX_TRACKERS)
        {
            let next = format!("{magnet}&tr={}", crate::discovery::percent_encode(&tracker));
            if next.len() > 16 * 1024 {
                break;
            }
            magnet = next;
        }
        Ok(magnet)
    }

    pub async fn reannounce(&mut self, id: TorrentId, index: usize) -> Result<()> {
        if !self.jobs.0.contains_key(&id) {
            return Err(DriverError("unknown job"));
        }
        let now = self.elapsed_s();
        let started = self
            .session
            .begin_announce(id, index, now, true)
            .map_err(|_| DriverError("unknown tracker"))?;
        if !started {
            return Err(DriverError("reannounce is still inside its cooldown"));
        }
        self.finish_announce(id, index, rbit::TrackerEvent::None)
            .await
    }

    async fn refresh_due_tracker(&mut self) {
        let now = self.elapsed_s();
        let mut choice = None;
        let jobs: Vec<_> = self.jobs.0.keys().copied().collect();
        for id in jobs {
            let Some(job) = self.jobs.0.get(&id) else {
                continue;
            };
            for index in 0..job.trackers.len() {
                let Ok(schedule) = self.session.tracker_schedule(id, index) else {
                    continue;
                };
                if !schedule.in_flight && now >= schedule.periodic_due_s {
                    choice = Some((id, index));
                    break;
                }
            }
            if choice.is_some() {
                break;
            }
        }
        let Some((id, index)) = choice else {
            return;
        };
        let Ok(true) = self.session.begin_announce(id, index, now, false) else {
            return;
        };
        let _ = self
            .finish_announce(id, index, rbit::TrackerEvent::None)
            .await;
    }

    async fn finish_announce(
        &mut self,
        id: TorrentId,
        index: usize,
        event: rbit::TrackerEvent,
    ) -> Result<()> {
        let Some(tracker) = self.jobs.0.get(&id).and_then(|job| job.trackers.get(index)) else {
            let _ = self.session.tracker_failure(id, index, self.elapsed_s(), 0);
            return Err(DriverError("unknown tracker"));
        };
        let url = tracker.url.clone();
        let local = tracker.local;
        let origin = if tracker.origin == "original" {
            PeerOrigin::OriginalTracker(tracker.origin_id)
        } else {
            PeerOrigin::UserProvided
        };
        match self.announce_job(id, &url, event).await {
            Ok(response) => {
                let _ = self.session.tracker_success(
                    id,
                    index,
                    self.elapsed_s(),
                    response.interval,
                    response.min_interval,
                );
                if let Some(tracker) = self
                    .jobs
                    .0
                    .get_mut(&id)
                    .and_then(|job| job.trackers.get_mut(index))
                {
                    tracker.seeds = response.complete;
                    tracker.peers = response.incomplete;
                    tracker.last_error = None;
                }
                self.ingest_peers(id, &response, origin, local).await;
                Ok(())
            }
            Err(error) => {
                let _ = self.session.tracker_failure(id, index, self.elapsed_s(), 0);
                if let Some(tracker) = self
                    .jobs
                    .0
                    .get_mut(&id)
                    .and_then(|job| job.trackers.get_mut(index))
                {
                    tracker.last_error = Some(error.0.to_string());
                }
                Err(error)
            }
        }
    }

    async fn announce_job(
        &mut self,
        id: TorrentId,
        url: &str,
        event: rbit::TrackerEvent,
    ) -> Result<rbit::AnnounceResponse> {
        let port = self
            .listening_address()
            .map(|address| address.port())
            .ok_or(DriverError("listening port is not configured"))?;
        let hash = match self.jobs.0.get(&id).map(|job| job.torrent.meta.info_hash) {
            Some(rbit::InfoHash::V1(hash)) => hash,
            _ => return Err(DriverError("unknown job")),
        };
        let row = self
            .snapshot()
            .jobs
            .iter()
            .find(|row| row.id == id)
            .cloned()
            .ok_or(DriverError("unknown job"))?;
        let peer_id = self.peer_id;
        announce_detached(
            url,
            self.allow_local_trackers,
            crate::announce::AnnounceRequest {
                info_hash: &hash,
                peer_id: &peer_id,
                port,
                uploaded: row.total_payload_uploaded,
                downloaded: row.total_payload_downloaded,
                left: row.wanted_bytes.saturating_sub(row.verified_bytes),
                event,
            },
        )
        .await
    }

    fn collect_peers(
        &self,
        response: &rbit::AnnounceResponse,
        local: bool,
        origin_id: u64,
        discovered: &mut Vec<PeerSource>,
    ) {
        for address in response.peers.iter().chain(response.peers6.iter()).copied() {
            if discovered.len() >= CANDIDATE_CAP {
                break;
            }
            if !peer_allowed(address, local) {
                continue;
            }
            if discovered.iter().any(|known| known.address == address) {
                continue;
            }
            discovered.push(PeerSource {
                address,
                origin: PeerOrigin::OriginalTracker(origin_id),
            });
        }
    }

    async fn ingest_peers(
        &mut self,
        id: TorrentId,
        response: &rbit::AnnounceResponse,
        origin: PeerOrigin,
        local: bool,
    ) {
        let addresses: Vec<_> = response
            .peers
            .iter()
            .chain(response.peers6.iter())
            .copied()
            .filter(|address| peer_allowed(*address, local))
            .filter(|address| self.peer_preference(id, *address) != PeerPreference::Block)
            .take(CANDIDATE_CAP)
            .collect();
        for address in addresses {
            let source = PeerSource { address, origin };
            self.remember_source(id, source);
            let _ = self.connect_peer(id, source).await;
        }
    }

    fn remember_source(&mut self, id: TorrentId, source: PeerSource) {
        let Some(job) = self.jobs.0.get_mut(&id) else {
            return;
        };
        if job.sources.len() >= CANDIDATE_CAP
            || job
                .sources
                .iter()
                .any(|known| known.address == source.address)
        {
            return;
        }
        job.sources.push(source);
    }

    async fn augment_public(&mut self, id: TorrentId) {
        let Some(job) = self.jobs.0.get(&id) else {
            return;
        };
        if job.torrent.is_private() || job.private_tracker.is_some() {
            return;
        }
        let existing: Vec<String> = job
            .trackers
            .iter()
            .map(|tracker| tracker.url.clone())
            .collect();
        let configured = self.public_trackers.clone();
        let mut allowed = Vec::new();
        for url in &configured {
            if crate::discovery::admit_tracker(url, false).await.is_ok() {
                allowed.push(url.clone());
            }
        }
        let planned = crate::discovery::plan_public_additions(&existing, &configured, |url| {
            allowed.iter().any(|known| known == url)
        });
        for url in planned {
            if self
                .jobs
                .0
                .get(&id)
                .is_some_and(|job| job.trackers.len() >= crate::discovery::MAX_TRACKERS)
            {
                break;
            }
            if self.session.add_tracker(id).is_err() {
                break;
            }
            if let Some(job) = self.jobs.0.get_mut(&id) {
                job.trackers.push(Tracked {
                    url,
                    origin: "added",
                    origin_id: 0,
                    local: false,
                    seeds: None,
                    peers: None,
                    last_error: None,
                });
            }
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        if let Some(listener) = &self.listener {
            listener.task.abort();
        }
        for wire in self.wires.values() {
            wire.task.abort();
        }
    }
}

/// Resolve and classify the tracker again, then announce over the
/// application-owned HTTP transport to the admitted address. Errors carry the
/// transport's reason so the Trackers tab can show it.
async fn announce_detached(
    url: &str,
    allow_local: bool,
    announce: crate::announce::AnnounceRequest<'_>,
) -> Result<rbit::AnnounceResponse> {
    let admitted = crate::discovery::admit_tracker(url, allow_local)
        .await
        .map_err(DriverError)?;
    let address = admitted
        .connect_address()
        .ok_or(DriverError("tracker host did not resolve"))?;
    crate::announce::announce(&admitted.tracker, address, &announce)
        .await
        .map_err(DriverError)
}

fn pipeline_for(preference: PeerPreference) -> usize {
    match preference {
        PeerPreference::Avoid => AVOID_PIPELINE,
        PeerPreference::Prefer | PeerPreference::Normal | PeerPreference::Block => {
            DEFAULT_MAX_IN_FLIGHT
        }
    }
}

/// Preference rank ascending, then measured score descending, then
/// connection order. Pure so the policy is testable without a clock.
fn rank_peers(entries: &mut [(u8, u64, u64, PeerId)]) -> Vec<PeerId> {
    entries
        .sort_unstable_by_key(|(rank, score, order, _)| (*rank, std::cmp::Reverse(*score), *order));
    entries.iter().map(|(_, _, _, peer)| *peer).collect()
}

fn blend_rate(previous: u64, instant: u64) -> u64 {
    if previous == 0 {
        instant
    } else {
        (previous.saturating_mul(3).saturating_add(instant)) / 4
    }
}

fn preference_rank(preference: PeerPreference) -> u8 {
    match preference {
        PeerPreference::Prefer => 0,
        PeerPreference::Normal => 1,
        PeerPreference::Avoid => 2,
        PeerPreference::Block => 3,
    }
}

fn peer_allowed(address: SocketAddr, local_tracker: bool) -> bool {
    if address.port() == 0 || address.ip().is_unspecified() {
        return false;
    }
    !crate::discovery::is_restricted_ip(address.ip()) || local_tracker
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn prepare_destination(path: &Path, existing: ExistingData) -> Result<PathBuf> {
    match existing {
        ExistingData::Refuse => std::fs::create_dir(path)
            .map_err(|_| DriverError("destination must be a new directory"))?,
        ExistingData::Recheck => {
            let meta = std::fs::symlink_metadata(path)
                .map_err(|_| DriverError("resume destination is missing"))?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(DriverError("resume destination must be a real directory"));
            }
        }
    }
    std::fs::canonicalize(path).map_err(|_| DriverError("destination validation failed"))
}

async fn peer_actor(
    mut connection: PeerConnection,
    torrent: TorrentId,
    peer: PeerId,
    pieces: usize,
    mut commands: mpsc::Receiver<WireCommand>,
    events: mpsc::Sender<WireEvent>,
) {
    let mut pending: HashMap<(u32, u32), u32> = HashMap::new();
    let result: std::result::Result<(), &'static str> = async {
        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { return Ok(()); };
                    let WireCommand { message, _permit, upload_bytes, _owned } = command;
                    match &message {
                        Message::Request { index, begin, length } => {
                            if pending.len() >= 8 && !pending.contains_key(&(*index, *begin)) { return Err("wire request pipeline exceeded"); }
                            pending.insert((*index, *begin), *length);
                        }
                        Message::Cancel { index, begin, .. } => { pending.remove(&(*index, *begin)); }
                        _ => {}
                    }
                    timeout(Duration::from_secs(5), connection.send(message)).await
                        .map_err(|_| "peer send stalled")?.map_err(|_| "peer send failed")?;
                    if upload_bytes != 0 {
                        events.send(WireEvent { torrent, peer, message: Ok(Message::KeepAlive), uploaded: upload_bytes })
                            .await.map_err(|_| "driver closed")?;
                    }
                    drop(_permit);
                    drop(_owned);
                }
                message = connection.receive() => {
                    let message = message.map_err(|_| "peer disconnected")?;
                    match &message {
                        Message::Piece { index, begin, data } => {
                            let Some(length) = pending.remove(&(*index, *begin)) else { continue; };
                            if data.len() != length as usize || data.len() > 16384 { return Err("invalid peer block length"); }
                        }
                        Message::Bitfield(bits) if bits.len() != pieces.div_ceil(8) => return Err("invalid peer bitfield size"),
                        Message::Port(_) | Message::Suggest { .. } | Message::AllowedFast { .. } | Message::Reject { .. } => continue,
                        _ => {}
                    }
                    events.send(WireEvent { torrent, peer, message: Ok(message), uploaded: 0 }).await.map_err(|_| "driver closed")?;
                }
            }
        }
    }.await;
    if let Err(error) = result {
        let _ = events
            .send(WireEvent {
                torrent,
                peer,
                message: Err(error),
                uploaded: 0,
            })
            .await;
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn regular_slots_rank_preference_then_rate_then_arrival() {
        let peer = |n: u64| PeerId(n);
        let mut entries = vec![
            (1, 50_000, 1, peer(1)),
            (0, 10, 2, peer(2)),
            (1, 90_000, 3, peer(3)),
            (2, 999_999, 4, peer(4)),
            (1, 90_000, 0, peer(0)),
        ];
        assert_eq!(
            rank_peers(&mut entries),
            vec![peer(2), peer(0), peer(3), peer(1), peer(4)]
        );
        assert_eq!(blend_rate(0, 400), 400);
        assert_eq!(blend_rate(400, 0), 300);
        assert_eq!(blend_rate(1_000, 2_000), 1_250);
    }
}
