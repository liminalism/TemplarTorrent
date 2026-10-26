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
use rbit::{
    ExtensionHandshake, Handshake, Message, MetadataMessage, PeerConnection,
    peer::{EncryptionPolicy, MseStream, mse},
};
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
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
    task::{JoinHandle, JoinSet},
    time::timeout,
};

const EVENT_CAP: usize = 64;
const WIRE_CAP: usize = 32;
/// Sources a magnet's metadata resolution may try before the job exists.
const CANDIDATE_CAP: usize = 32;
/// Remembered peer candidates per job (DESIGN.md section 5: 2,048).
const POOL_CAP: usize = 2048;
/// Outgoing connection attempts in flight across all jobs.
const HALF_OPEN_CAP: usize = 24;
/// First retry delay after a failed or closed connection; doubles per
/// consecutive failure up to [`RETRY_MAX_S`].
const RETRY_BASE_S: u64 = 30;
const RETRY_MAX_S: u64 = 1800;
/// Seconds between DHT get_peers lookups for one downloading job.
const DHT_INTERVAL_S: u64 = 300;
/// Retry delay after a lookup that returned no peers.
const DHT_EMPTY_RETRY_S: u64 = 30;
/// Bound on one DHT lookup, including its iterative queries.
const DHT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(60);
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
/// Outstanding request stamps kept per peer. Older stamps drop first.
const RTT_PENDING_CAP: usize = 32;
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

/// Message Stream Encryption (MSE/PE) on peer connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PeerEncryption {
    /// Outgoing: MSE offering RC4, redialled once in plaintext if the peer
    /// does not complete MSE. Incoming: plaintext or MSE, RC4 when offered.
    #[default]
    PreferEncrypted,
    /// RC4 only, both directions. Plaintext peers are refused.
    RequireEncrypted,
    /// No MSE: outgoing plaintext only, incoming MSE refused.
    PlaintextOnly,
}
impl PeerEncryption {
    fn policy(self) -> EncryptionPolicy {
        match self {
            Self::PreferEncrypted => EncryptionPolicy::PreferRc4,
            Self::RequireEncrypted => EncryptionPolicy::RequireRc4,
            Self::PlaintextOnly => EncryptionPolicy::PlaintextOnly,
        }
    }
}
/// What the listener's probe tasks read from the coordinator: the setting
/// and the v1 info hashes of public jobs, which are the MSE SKEYs it serves.
#[derive(Default)]
struct IncomingKeys {
    encryption: PeerEncryption,
    hashes: Vec<[u8; 20]>,
}
/// Concurrent incoming handshake probes; sockets past this are dropped.
const PROBE_CAP: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerOrigin {
    /// Stable identity of the original, permitted tracker that supplied this
    /// address. Private jobs use exactly one such tracker scope at a time.
    OriginalTracker(u64),
    /// Explicitly supplied by the user; requires public bootstrap opt-in while
    /// metadata is unknown. Never retained if metadata later proves private.
    UserProvided,
    /// Found by a DHT get_peers lookup (BEP 5). Public jobs only.
    Dht,
    /// Named by a connected peer's ut_pex message (BEP 11). Public jobs only.
    Pex,
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
    /// Smoothed block round trip in microseconds. Absent until a block we
    /// requested comes back.
    pub rtt_us: Option<u64>,
    /// Hash failures attributed to blocks this peer supplied.
    pub hash_failures: u32,
    /// Whether the wire negotiated MSE/PE RC4.
    pub encrypted: bool,
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
    /// Whether this tracker has already accepted a `started` announce.
    started: bool,
}
struct Job {
    torrent: ValidatedV1,
    /// Candidate pool, at most [`POOL_CAP`]. Every entry already passed the
    /// job's discovery policy when it was remembered.
    sources: Vec<PeerSource>,
    /// Consecutive failures and the earliest next attempt, elapsed seconds.
    retry: HashMap<SocketAddr, (u32, u64)>,
    /// Elapsed second the next DHT lookup is due.
    dht_due_s: u64,
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
    /// Block requests stamped when sent, oldest first.
    pending_rtt: Vec<SentRequest>,
    /// Smoothed solicited-block round trip, microseconds.
    rtt_us: Option<u64>,
    /// Attributed hash failures. They demote this peer and disconnect nobody.
    hash_failures: u32,
    /// The peer's ut_pex id, when it offered one and the job is public.
    pex_id: Option<u8>,
    /// Addresses our last PEX message to this peer listed, and when.
    pex_sent: Vec<SocketAddr>,
    pex_at_s: u64,
    encrypted: bool,
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
enum IncomingStream {
    Plain(TcpStream),
    /// MSE completed; positioned at the peer's BitTorrent handshake.
    Negotiated(Box<MseStream<TcpStream>>),
}
struct Incoming {
    stream: IncomingStream,
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
    encryption: PeerEncryption,
    incoming_keys: Arc<Mutex<IncomingKeys>>,
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
    /// Outgoing connection attempts in flight, by job and address.
    dialing: HashMap<(TorrentId, SocketAddr), JoinHandle<()>>,
    dial_sender: mpsc::Sender<Dialed>,
    dialed: mpsc::Receiver<Dialed>,
    /// Tracker announces in flight, by job and tracker index.
    announcing: HashMap<(TorrentId, usize), JoinHandle<()>>,
    announce_sender: mpsc::Sender<Announced>,
    announced: mpsc::Receiver<Announced>,
    /// The session's DHT node, its run and bootstrap tasks, and lookups.
    dht: Option<Arc<rbit::DhtServer>>,
    dht_tasks: Vec<JoinHandle<()>>,
    dht_lookups: HashMap<TorrentId, JoinHandle<()>>,
    dht_sender: mpsc::Sender<(TorrentId, Vec<SocketAddr>)>,
    dht_found: mpsc::Receiver<(TorrentId, Vec<SocketAddr>)>,
    pex_interval_s: u64,
}

/// One finished outgoing connection attempt.
struct Dialed {
    torrent: TorrentId,
    source: PeerSource,
    result: std::result::Result<PeerConnection, &'static str>,
}

/// One finished tracker announce.
struct Announced {
    torrent: TorrentId,
    index: usize,
    url: String,
    result: std::result::Result<rbit::AnnounceResponse, crate::announce::AnnounceFailure>,
}

impl Driver {
    pub fn new(limits: Limits) -> Result<Self> {
        Self::with_peer_id(limits, peer_id())
    }
    pub fn with_peer_id(limits: Limits, peer_id: [u8; 20]) -> Result<Self> {
        let store = SharedStore::new();
        let session = Session::new(limits.clone(), vec![Box::new(store.clone())], 8, 0)
            .map_err(|_| DriverError("invalid session limits"))?;
        let (event_sender, events) = mpsc::channel(EVENT_CAP);
        let (dial_sender, dialed) = mpsc::channel(HALF_OPEN_CAP);
        let (announce_sender, announced) = mpsc::channel(EVENT_CAP);
        let (dht_sender, dht_found) = mpsc::channel(EVENT_CAP);
        Ok(Self {
            session,
            store,
            jobs: Jobs::default(),
            wires: HashMap::new(),
            events,
            event_sender,
            metadata_budget: ByteBudget::new(METADATA_BUDGET),
            peer_id,
            encryption: PeerEncryption::default(),
            incoming_keys: Arc::default(),
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
            dialing: HashMap::new(),
            dial_sender,
            dialed,
            announcing: HashMap::new(),
            announce_sender,
            announced,
            dht: None,
            dht_tasks: Vec::new(),
            dht_lookups: HashMap::new(),
            dht_sender,
            dht_found,
            pex_interval_s: crate::pex::INTERVAL_S,
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
    /// Seconds between PEX messages to one peer; tests shorten it.
    pub fn set_pex_interval_secs(&mut self, seconds: u64) {
        self.pex_interval_s = seconds;
    }

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
        let job = self.jobs.0.get(&id).ok_or(DriverError("unknown job"))?;
        if wanted && job.torrent.is_pad(file) {
            return Err(DriverError("pad files are never downloaded on their own"));
        }
        self.session
            .set_wanted(id, file, wanted)
            .map_err(|_| DriverError("file selection rejected"))
    }
    /// Whether file `index` of job `id` is a BEP 47 pad file. The client
    /// hides these; they exist only to align real files to pieces.
    pub fn is_pad_file(&self, id: TorrentId, index: usize) -> bool {
        self.jobs
            .0
            .get(&id)
            .is_some_and(|job| job.torrent.is_pad(index))
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

    /// Explicit opt-in listener. At most eight routed sockets plus sixteen
    /// concurrent five-second handshake probes (plaintext detection and, for
    /// MSE, the whole encryption handshake) wait outside the admitted
    /// connection pool; sockets past that are dropped.
    /// Unknown hashes and all private incoming connections are dropped without
    /// logs. Private uploads require outbound connections to peers from the
    /// selected original tracker; an inbound address cannot prove provenance.
    pub async fn listen(&mut self, address: SocketAddr) -> Result<SocketAddr> {
        if self.listener.is_some() {
            return Err(DriverError("listener already bound"));
        }
        // `[::]` is dual-stack on Linux by default; a host without IPv6
        // falls back to IPv4 on the same port.
        let socket = match TcpListener::bind(address).await {
            Ok(socket) => socket,
            Err(_) if address.ip().is_unspecified() && address.is_ipv6() => TcpListener::bind(
                SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), address.port()),
            )
            .await
            .map_err(|_| DriverError("listener bind failed"))?,
            Err(_) => return Err(DriverError("listener bind failed")),
        };
        let address = socket
            .local_addr()
            .map_err(|_| DriverError("listener address unavailable"))?;
        let (sender, incoming) = mpsc::channel(8);
        let keys = Arc::clone(&self.incoming_keys);
        self.refresh_incoming_keys();
        let task = tokio::spawn(async move {
            // Dropping the set with the listener task aborts every probe.
            let mut probes = JoinSet::new();
            while let Ok((stream, address)) = socket.accept().await {
                while probes.try_join_next().is_some() {}
                if probes.len() >= PROBE_CAP {
                    continue;
                }
                let sender = sender.clone();
                let keys = Arc::clone(&keys);
                probes.spawn(async move {
                    let probe = timeout(CONNECT_TIMEOUT, route_incoming(stream, address, &keys));
                    if let Ok(Some(incoming)) = probe.await {
                        // Queue full: the failed send drops the socket.
                        let _ = sender.try_send(incoming);
                    }
                });
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

    /// Publish the setting and public v1 hashes to the listener's probes.
    fn refresh_incoming_keys(&self) {
        let hashes = self
            .jobs
            .0
            .values()
            .filter(|job| job.private_tracker.is_none())
            .map(|job| job.torrent.hash());
        let Ok(mut keys) = self.incoming_keys.lock() else {
            return;
        };
        keys.encryption = self.encryption;
        if !keys.hashes.iter().copied().eq(hashes.clone()) {
            keys.hashes = hashes.collect();
        }
    }

    async fn accept_incoming(&mut self) {
        self.refresh_incoming_keys();
        let incoming = self
            .listener
            .as_mut()
            .and_then(|l| l.incoming.try_recv().ok());
        let Some(mut incoming) = incoming else {
            return;
        };
        // A dual-stack listener reports IPv4 peers as ::ffff:a.b.c.d.
        incoming.address = SocketAddr::new(
            incoming.address.ip().to_canonical(),
            incoming.address.port(),
        );
        let id = self.jobs.0.iter().find_map(|(id, job)| {
            (job.torrent.hash() == incoming.hash && job.private_tracker.is_none()).then_some(*id)
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
        if self.encryption == PeerEncryption::RequireEncrypted
            && matches!(incoming.stream, IncomingStream::Plain(_))
        {
            return;
        }
        let peer_id = self.peer_id;
        let accepted = timeout(CONNECT_TIMEOUT, async {
            match incoming.stream {
                IncomingStream::Plain(stream) => {
                    PeerConnection::accept(stream, incoming.hash, peer_id).await
                }
                IncomingStream::Negotiated(stream) => {
                    PeerConnection::accept_with_stream(
                        *stream,
                        incoming.address,
                        incoming.hash,
                        peer_id,
                        false,
                    )
                    .await
                }
            }
        })
        .await;
        if let Ok(Ok(connection)) = accepted {
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
        let Some(hash) = magnet.info_hash.v1_hash().map(|hash| hash.0) else {
            return Err(DriverError("pure v2 magnets are not supported"));
        };
        if self.jobs.0.values().any(|job| job.torrent.hash() == hash) {
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
        // Up to METADATA_PARALLEL candidates at once; the first verified
        // metadata wins. The attempts are polled here rather than spawned,
        // so dropping this future drops every connection and permit at once.
        const METADATA_PARALLEL: usize = 4;
        type Attempt = std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Option<(
                            PeerSource,
                            PeerConnection,
                            crate::metadata::FetchedMetadata,
                        )>,
                    > + Send,
            >,
        >;
        let peer_id = self.peer_id;
        let encryption = self.encryption;
        let budget = self.metadata_budget.clone();
        let resolution = timeout(Duration::from_secs(60), async {
            let mut queue = permitted.iter().copied();
            let mut attempts: Vec<Attempt> = Vec::new();
            loop {
                while attempts.len() < METADATA_PARALLEL {
                    let Some(source) = queue.next() else {
                        break;
                    };
                    let budget = budget.clone();
                    attempts.push(Box::pin(async move {
                        let mut connection = dial_peer(source.address, hash, peer_id, encryption)
                            .await
                            .ok()?;
                        let metadata = fetch_metadata_budgeted(&mut connection, hash, &budget)
                            .await
                            .ok()?;
                        Some((source, connection, metadata))
                    }));
                }
                if attempts.is_empty() {
                    return Err(DriverError("no permitted peer supplied valid metadata"));
                }
                let finished = std::future::poll_fn(|cx| {
                    for index in 0..attempts.len() {
                        if let std::task::Poll::Ready(result) = attempts[index].as_mut().poll(cx) {
                            drop(attempts.swap_remove(index));
                            return std::task::Poll::Ready(result);
                        }
                    }
                    std::task::Poll::Pending
                })
                .await;
                if let Some(found) = finished {
                    return Ok(found);
                }
            }
        })
        .await
        .map_err(|_| DriverError("magnet resolution deadline exceeded"))??;
        let (source, connection, metadata) = resolution;
        let (torrent, buffered) = ValidatedV1::from_fetched(metadata, hash, &self.metadata_budget)
            .map_err(DriverError)?;
        let private_tracker = if torrent.is_private() {
            match source.origin {
                PeerOrigin::OriginalTracker(id) => Some(id),
                PeerOrigin::UserProvided | PeerOrigin::Dht | PeerOrigin::Pex => {
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
                    .add_job_with_pads(
                        id,
                        &root,
                        &torrent.paths,
                        &torrent.pads,
                        torrent.piece_length(),
                    )
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
                retry: HashMap::new(),
                dht_due_s: 0,
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
        let encrypted = connection.is_encrypted();
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
                pending_rtt: Vec::new(),
                rtt_us: None,
                hash_failures: 0,
                pex_id: None,
                pex_sent: Vec::new(),
                pex_at_s: 0,
                encrypted,
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
            // BEP 27: a private job never offers peer exchange.
            let mut handshake = if public_discovery(&self.jobs.0[&id]) {
                ExtensionHandshake::with_extensions(&[
                    ("ut_metadata", crate::metadata::OUR_MD_ID),
                    ("ut_pex", crate::pex::OUR_PEX_ID),
                ])
            } else {
                ExtensionHandshake::with_extensions(&[("ut_metadata", crate::metadata::OUR_MD_ID)])
            };
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

    /// Encryption for new peer connections, both directions. Existing
    /// connections keep what they negotiated.
    pub fn set_peer_encryption(&mut self, encryption: PeerEncryption) {
        self.encryption = encryption;
        self.refresh_incoming_keys();
    }
    pub fn peer_encryption(&self) -> PeerEncryption {
        self.encryption
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
        if job.sources.len() >= POOL_CAP
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
        let hash = job.torrent.hash();
        let connection = dial_peer(source.address, hash, self.peer_id, self.encryption)
            .await
            .map_err(DriverError)?;
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
        let has_trackers = self
            .jobs
            .0
            .get(&id)
            .is_some_and(|job| !job.trackers.is_empty());
        if sources.is_empty() && !has_trackers {
            return Err(DriverError("no permitted reconnect source is available"));
        }
        // Known candidates are dialed again right away; trackers announce on
        // their schedule and supply more. Neither waits here.
        if let Some(job) = self.jobs.0.get_mut(&id) {
            job.retry.clear();
        }
        self.fill_connections();
        Ok(())
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
        self.abort_job_tasks(id);
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
            self.note_retry(wire.torrent, wire.source.address, true);
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
                let now = self.now_ns();
                if let Some(wire) = self.wires.get_mut(&peer) {
                    wire.window_down = wire.window_down.saturating_add(data.len() as u64);
                    if let Some(sent) = take_request(&mut wire.pending_rtt, index, begin) {
                        let sample = now.saturating_sub(sent) / 1_000;
                        wire.rtt_us = Some(match wire.rtt_us {
                            Some(previous) => blend_rate(previous, sample),
                            None => sample,
                        });
                    }
                }
                self.session
                    .inject_block(id, peer, index, begin, data.to_vec())
            }
            _ => Ok(()),
        };
        result.map_err(|_| DriverError("peer event rejected"))
    }

    fn metadata_request(&mut self, peer: PeerId, extension: u8, payload: &[u8]) -> Result<()> {
        use crate::{metadata::OUR_MD_ID, pex::OUR_PEX_ID, preflight};
        if extension == OUR_PEX_ID {
            self.receive_pex(peer, payload);
            return Ok(());
        }
        if extension != 0 && extension != OUR_MD_ID {
            return Ok(());
        }
        if payload.len() > 16384 {
            return Err(DriverError("extension control too large"));
        }
        // Canonical bencode matters for metainfo, not here: some clients
        // send unsorted handshake keys. Such a message is ignored, and the
        // peer stays connected without the extension it could not describe.
        if preflight::validate(payload).is_err() {
            return Ok(());
        }
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
            let pex = preflight::field(payload, b"m")
                .and_then(|m| preflight::field(m, b"ut_pex"))
                .and_then(preflight::integer)
                .and_then(|id| u8::try_from(id).ok())
                .filter(|id| *id != 0);
            let public = self
                .wires
                .get(&peer)
                .and_then(|wire| self.jobs.0.get(&wire.torrent))
                .is_some_and(public_discovery);
            if let Some(wire) = self.wires.get_mut(&peer) {
                wire.metadata_id = (id != 0).then_some(id);
                wire.pex_id = pex.filter(|_| public);
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
        for entry in &blamed {
            if let Some(wire) = self.wires.get_mut(&entry.peer)
                && wire.torrent == entry.torrent
            {
                wire.hash_failures = wire.hash_failures.saturating_add(1);
            }
        }
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
                } => {
                    let now = self.now_ns();
                    if let Some(wire) = self.wires.get_mut(&entry.peer) {
                        note_request(&mut wire.pending_rtt, piece, begin, now);
                    }
                    Message::Request {
                        index: piece,
                        begin,
                        length,
                    }
                }
                PeerMessage::Cancel {
                    piece,
                    begin,
                    length,
                } => {
                    if let Some(wire) = self.wires.get_mut(&entry.peer) {
                        forget_request(&mut wire.pending_rtt, piece, begin);
                    }
                    Message::Cancel {
                        index: piece,
                        begin,
                        length,
                    }
                }
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
        self.collect_announces();
        self.refresh_due_trackers();
        self.collect_dht();
        self.refresh_dht();
        self.send_pex();
        self.collect_dials();
        self.fill_connections();
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
                rtt_us: wire.rtt_us,
                hash_failures: wire.hash_failures,
                encrypted: wire.encrypted,
            })
            .collect();
        rows.sort_by_key(|row| row.address);
        rows
    }

    pub fn info_hash(&self, id: TorrentId) -> Option<[u8; 20]> {
        Some(self.jobs.0.get(&id)?.torrent.hash())
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
        let info_hash = job.torrent.hash();
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
    /// full sync. The barrier is set because the verified set is read first
    /// and every piece in it was already written; [`SharedStore::durable_identity`]
    /// then syncs those bytes and the inode fields before the stamps in this
    /// record. Keep that order: a set read after the sync could name a piece
    /// that is still only in the page cache.
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
        if self.jobs.0.values().any(|job| job.torrent.hash() == hash) {
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
                    .add_job_with_pads(
                        id,
                        &root,
                        &torrent.paths,
                        &torrent.pads,
                        torrent.piece_length(),
                    )
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
                started: false,
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
                retry: HashMap::new(),
                dht_due_s: 0,
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

    /// Regular slot order: manual preference, then fewer attributed hash
    /// failures, then a peer who alone has a still-wanted piece, then higher
    /// payload rate, then a lower measured block round trip, then connection
    /// order. A seeding job uses upload rate and ignores download rate and
    /// download round trip. Re-ranked every [`CHOKE_INTERVAL_S`]; peers that
    /// arrived since are appended, so an order never churns on one fast step.
    fn regular_order(&mut self, rows: &Snapshot) -> Vec<PeerId> {
        let now = self.elapsed_s();
        let due = self
            .choke_evaluated_s
            .is_none_or(|at| now.saturating_sub(at) >= CHOKE_INTERVAL_S);
        if due {
            let mut entries: Vec<(SlotKey, PeerId)> = self
                .wires
                .iter()
                .map(|(peer, wire)| {
                    let seeding = rows
                        .jobs
                        .iter()
                        .any(|job| job.id == wire.torrent && job.state == JobState::Seeding);
                    let key = slot_key(SlotInput {
                        preference: preference_rank(
                            self.peer_preference(wire.torrent, wire.source.address),
                        ),
                        failures: wire.hash_failures,
                        sole: self.session.sole_wanted_supplier(wire.torrent, *peer),
                        seeding,
                        down_bps: wire.down_bps,
                        up_bps: wire.up_bps,
                        rtt_us: wire.rtt_us,
                        order: peer.0,
                    });
                    (key, *peer)
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
        let Some(hash) = magnet.info_hash.v1_hash().map(|hash| hash.0) else {
            return Err(DriverError("pure v2 magnets are not supported"));
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
                started: false,
            });
        }
        let mut discovered = Vec::new();
        let mut announced = Vec::new();
        let peer_id = self.peer_id;
        for mut tracker in originals {
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
                Err(failure) => {
                    tracker.last_error = Some(failure.detail);
                    announced.push((tracker, None));
                }
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
        // Trackers gave nothing: a public magnet may still find peers in the
        // DHT. Strict bootstrap never asks it.
        if discovered.is_empty()
            && public_bootstrap
            && let Some(dht) = self.dht.clone()
            && let Ok(Ok(peers)) = timeout(DHT_LOOKUP_TIMEOUT, dht.get_peers(hash)).await
        {
            for address in peers {
                if discovered.len() >= CANDIDATE_CAP {
                    break;
                }
                if peer_allowed(address, false)
                    && !discovered.iter().any(|known| known.address == address)
                {
                    discovered.push(PeerSource {
                        address,
                        origin: PeerOrigin::Dht,
                    });
                }
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
        let mut to_ingest = Vec::new();
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
                tracker.started = true;
                to_ingest.push((response, tracker.origin_id, tracker.local));
            } else {
                let _ = self.session.tracker_failure(id, index, now, 0);
                tracker
                    .last_error
                    .get_or_insert_with(|| "announce failed".into());
            }
            kept.push(tracker);
        }
        if let Some(job) = self.jobs.0.get_mut(&id) {
            job.trackers = kept;
        }
        // Metadata resolution used at most CANDIDATE_CAP of these peers; the
        // pool keeps the rest for the dialer.
        for (response, origin_id, local) in to_ingest {
            self.ingest_peers(id, &response, PeerOrigin::OriginalTracker(origin_id), local);
        }
        self.augment_public(id).await;
        Ok(id)
    }

    /// Add a job from `.torrent` bytes. The info dictionary is already in
    /// hand, so nothing is fetched from peers: the job is admitted, its files
    /// are prepared and rechecked, and its trackers announce from the next
    /// [`Driver::step`]. Private torrents keep their original trackers only.
    pub async fn add_torrent(
        &mut self,
        bytes: &[u8],
        destination: &Path,
        existing: ExistingData,
    ) -> Result<TorrentId> {
        let (torrent, hash) =
            ValidatedV1::from_torrent_bytes(bytes, &self.metadata_budget).map_err(DriverError)?;
        if self.jobs.0.values().any(|job| job.torrent.hash() == hash) {
            return Err(DriverError("torrent already exists"));
        }
        if self.limits.download_jobs == 0 || self.limits.connections_per_job == 0 {
            return Err(DriverError("no connection or download slot available"));
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
        let admitted =
            (|| {
                let root = prepare_destination(destination, existing)?;
                if self.jobs.0.values().any(|job| {
                    job.destination.starts_with(&root) || root.starts_with(&job.destination)
                }) {
                    return Err(DriverError("destination already owned by another job"));
                }
                self.store
                    .add_job_with_pads(
                        id,
                        &root,
                        &torrent.paths,
                        &torrent.pads,
                        torrent.piece_length(),
                    )
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
        let private = torrent.is_private();
        let mut trackers: Vec<Tracked> = Vec::new();
        // `torrent` was rebuilt from the info dictionary alone; the announce
        // keys live in the original file.
        let urls = rbit::Metainfo::from_bytes(bytes)
            .map(|meta| meta.trackers())
            .unwrap_or_default();
        for url in urls.into_iter().take(crate::discovery::MAX_TRACKERS) {
            let Ok(admitted) =
                crate::discovery::admit_tracker(&url, self.allow_local_trackers).await
            else {
                continue;
            };
            if trackers
                .iter()
                .any(|known| known.url == admitted.tracker.url)
            {
                continue;
            }
            trackers.push(Tracked {
                url: admitted.tracker.url,
                origin: "original",
                origin_id: trackers.len() as u64 + 1,
                local: admitted.local,
                seeds: None,
                peers: None,
                last_error: None,
                started: false,
            });
        }
        // A private job without an admissible tracker can never admit a peer.
        let private_tracker = private.then(|| trackers.first().map_or(u64::MAX, |t| t.origin_id));
        self.jobs.0.insert(
            id,
            Job {
                torrent,
                sources: Vec::new(),
                retry: HashMap::new(),
                dht_due_s: 0,
                private_tracker,
                destination: root,
                queued: false,
                last_error: None,
                trackers: Vec::new(),
                preferences: HashMap::new(),
                bootstrap: !private,
                resume_trusted: 0,
                resume_checked: 0,
                resume_failed: 0,
                resume_note: None,
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
        self.augment_public(id).await;
        // The download-slot cap still applies: queue rather than refuse.
        let rows = self.snapshot();
        let running = rows
            .jobs
            .iter()
            .filter(|row| row.id != id && row.state == JobState::Downloading)
            .count();
        let downloading = rows
            .jobs
            .iter()
            .any(|row| row.id == id && row.state == JobState::Downloading);
        if downloading && running >= self.limits.download_jobs {
            self.pause(id).await?;
            if let Some(job) = self.jobs.0.get_mut(&id) {
                job.queued = true;
            }
        }
        Ok(id)
    }

    pub fn magnet_from_torrent_bytes(bytes: &[u8]) -> Result<String> {
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(DriverError("torrent file exceeds 16 MiB limit"));
        }
        let meta =
            rbit::Metainfo::from_bytes(bytes).map_err(|_| DriverError("invalid torrent file"))?;
        let Some(hash) = meta.info_hash.v1_hash().map(|hash| hash.0) else {
            return Err(DriverError("pure v2 torrents are not supported"));
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

    /// Start a manual announce. The result lands in [`Driver::tracker_status`]
    /// once [`Driver::step`] collects it; this never waits on the network.
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
        self.spawn_announce(id, index)
    }

    /// Start every periodic announce that is due. Each runs in its own task.
    fn refresh_due_trackers(&mut self) {
        let now = self.elapsed_s();
        let mut due = Vec::new();
        for (id, job) in &self.jobs.0 {
            if job.queued {
                continue;
            }
            for index in 0..job.trackers.len() {
                if self.announcing.contains_key(&(*id, index)) {
                    continue;
                }
                let Ok(schedule) = self.session.tracker_schedule(*id, index) else {
                    continue;
                };
                if !schedule.in_flight && now >= schedule.periodic_due_s {
                    due.push((*id, index));
                }
            }
        }
        for (id, index) in due {
            let running = self.snapshot().jobs.iter().any(|row| {
                row.id == id
                    && matches!(
                        row.state,
                        JobState::Downloading | JobState::Seeding | JobState::Complete
                    )
            });
            if !running {
                continue;
            }
            if let Ok(true) = self.session.begin_announce(id, index, now, false) {
                let _ = self.spawn_announce(id, index);
            }
        }
    }

    fn spawn_announce(&mut self, id: TorrentId, index: usize) -> Result<()> {
        let port = self
            .listening_address()
            .map(|address| address.port())
            .ok_or(DriverError("listening port is not configured"));
        let hash = self
            .jobs
            .0
            .get(&id)
            .map(|job| job.torrent.hash())
            .ok_or(DriverError("unknown job"));
        let row = self
            .snapshot()
            .jobs
            .iter()
            .find(|row| row.id == id)
            .cloned()
            .ok_or(DriverError("unknown job"));
        let Some(tracker) = self
            .jobs
            .0
            .get_mut(&id)
            .and_then(|job| job.trackers.get_mut(index))
        else {
            let _ = self.session.tracker_failure(id, index, self.elapsed_s(), 0);
            return Err(DriverError("unknown tracker"));
        };
        let (port, hash, row) = match (port, hash, row) {
            (Ok(port), Ok(hash), Ok(row)) => (port, hash, row),
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                tracker.last_error = Some(error.0.to_string());
                let _ = self.session.tracker_failure(id, index, self.elapsed_s(), 0);
                return Err(error);
            }
        };
        let event = if tracker.started {
            rbit::TrackerEvent::None
        } else {
            rbit::TrackerEvent::Started
        };
        let url = tracker.url.clone();
        let allow_local = self.allow_local_trackers;
        let peer_id = self.peer_id;
        let sender = self.announce_sender.clone();
        let task = tokio::spawn(async move {
            let result = announce_detached(
                &url,
                allow_local,
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
            .await;
            let _ = sender
                .send(Announced {
                    torrent: id,
                    index,
                    url,
                    result,
                })
                .await;
        });
        self.announcing.insert((id, index), task);
        Ok(())
    }

    /// Apply finished announces. A result for a removed job, or for a
    /// tracker slot that now holds another URL, is dropped.
    fn collect_announces(&mut self) {
        while let Ok(done) = self.announced.try_recv() {
            self.announcing.remove(&(done.torrent, done.index));
            let now = self.elapsed_s();
            let Some(tracker) = self
                .jobs
                .0
                .get_mut(&done.torrent)
                .and_then(|job| job.trackers.get_mut(done.index))
                .filter(|tracker| tracker.url == done.url)
            else {
                continue;
            };
            let local = tracker.local;
            let origin = if tracker.origin == "original" {
                PeerOrigin::OriginalTracker(tracker.origin_id)
            } else {
                PeerOrigin::UserProvided
            };
            match done.result {
                Ok(response) => {
                    tracker.seeds = response.complete;
                    tracker.peers = response.incomplete;
                    tracker.last_error = None;
                    tracker.started = true;
                    let _ = self.session.tracker_success(
                        done.torrent,
                        done.index,
                        now,
                        response.interval,
                        response.min_interval,
                    );
                    self.ingest_peers(done.torrent, &response, origin, local);
                }
                Err(failure) => {
                    tracker.last_error = Some(failure.detail);
                    let _ = self
                        .session
                        .tracker_failure(done.torrent, done.index, now, 0);
                }
            }
        }
    }

    fn abort_job_tasks(&mut self, id: TorrentId) {
        if let Some(task) = self.dht_lookups.remove(&id) {
            task.abort();
        }
        self.dialing.retain(|(torrent, _), task| {
            let keep = *torrent != id;
            if !keep {
                task.abort();
            }
            keep
        });
        let mut aborted = Vec::new();
        self.announcing.retain(|(torrent, index), task| {
            let keep = *torrent != id;
            if !keep {
                task.abort();
                aborted.push(*index);
            }
            keep
        });
        // An aborted announce never reports; release its in-flight clock.
        let now = self.elapsed_s();
        for index in aborted {
            let _ = self.session.tracker_failure(id, index, now, 0);
        }
    }

    /// Whether `source` may be dialed for this job now.
    fn dialable(&self, id: TorrentId, job: &Job, source: &PeerSource, now: u64) -> bool {
        if job.preferences.get(&source.address.ip()) == Some(&PeerPreference::Block) {
            return false;
        }
        if job
            .private_tracker
            .is_some_and(|tracker| source.origin != PeerOrigin::OriginalTracker(tracker))
        {
            return false;
        }
        if job
            .retry
            .get(&source.address)
            .is_some_and(|(_, next)| *next > now)
        {
            return false;
        }
        !self.dialing.contains_key(&(id, source.address))
            && !self
                .wires
                .values()
                .any(|wire| wire.torrent == id && wire.source.address == source.address)
    }

    /// Dial remembered candidates for downloading jobs, within the global and
    /// per-job connection caps and [`HALF_OPEN_CAP`]. Never waits.
    fn fill_connections(&mut self) {
        let now = self.elapsed_s();
        let rows = self.snapshot();
        let mut plan = Vec::new();
        for row in rows.jobs.iter() {
            if row.state != JobState::Downloading {
                continue;
            }
            let Some(job) = self.jobs.0.get(&row.id) else {
                continue;
            };
            let hash = job.torrent.hash();
            let mut open = self.wires.values().filter(|w| w.torrent == row.id).count()
                + self.dialing.keys().filter(|(t, _)| *t == row.id).count();
            for source in &job.sources {
                if open >= self.limits.connections_per_job
                    || self.wires.len() + self.dialing.len() + plan.len()
                        >= self.limits.connections_total
                    || self.dialing.len() + plan.len() >= HALF_OPEN_CAP
                {
                    break;
                }
                if self.dialable(row.id, job, source, now)
                    && !plan
                        .iter()
                        .any(|(t, s, _): &(TorrentId, PeerSource, [u8; 20])| {
                            *t == row.id && s.address == source.address
                        })
                {
                    plan.push((row.id, *source, hash));
                    open += 1;
                }
            }
        }
        for (id, source, hash) in plan {
            let peer_id = self.peer_id;
            let encryption = self.encryption;
            let sender = self.dial_sender.clone();
            let task = tokio::spawn(async move {
                let result = dial_peer(source.address, hash, peer_id, encryption).await;
                let _ = sender
                    .send(Dialed {
                        torrent: id,
                        source,
                        result,
                    })
                    .await;
            });
            self.dialing.insert((id, source.address), task);
        }
    }

    /// Push the next attempt at `address` out by the backoff schedule.
    fn note_retry(&mut self, id: TorrentId, address: SocketAddr, failed: bool) {
        let now = self.elapsed_s();
        let Some(job) = self.jobs.0.get_mut(&id) else {
            return;
        };
        let entry = job.retry.entry(address).or_insert((0, 0));
        if failed {
            entry.0 = entry.0.saturating_add(1);
        }
        let shift = entry.0.saturating_sub(1).min(16);
        entry.1 = now.saturating_add((RETRY_BASE_S << shift).min(RETRY_MAX_S));
        if job.retry.len() > POOL_CAP {
            let known: std::collections::HashSet<_> =
                job.sources.iter().map(|source| source.address).collect();
            job.retry.retain(|address, _| known.contains(address));
        }
    }

    /// Admit finished dials. The job, its caps and the peer's preference are
    /// checked again: any of them may have changed while the dial ran.
    fn collect_dials(&mut self) {
        while let Ok(done) = self.dialed.try_recv() {
            self.dialing.remove(&(done.torrent, done.source.address));
            let connection = match done.result {
                Ok(connection) => connection,
                Err(_) => {
                    self.note_retry(done.torrent, done.source.address, true);
                    continue;
                }
            };
            let admissible = self.jobs.0.get(&done.torrent).is_some_and(|job| {
                job.preferences.get(&done.source.address.ip()) != Some(&PeerPreference::Block)
            }) && self.snapshot().jobs.iter().any(|row| {
                row.id == done.torrent
                    && matches!(row.state, JobState::Downloading | JobState::Seeding)
            }) && self.wires.len() < self.limits.connections_total
                && self
                    .wires
                    .values()
                    .filter(|wire| wire.torrent == done.torrent)
                    .count()
                    < self.limits.connections_per_job
                && !self.wires.values().any(|wire| {
                    wire.torrent == done.torrent && wire.source.address == done.source.address
                });
            if !admissible {
                continue;
            }
            if let Some(job) = self.jobs.0.get_mut(&done.torrent) {
                job.retry.remove(&done.source.address);
            }
            let _ = self.attach(done.torrent, done.source, connection, Vec::new());
        }
    }

    /// Join the mainline DHT on UDP `port` (the TCP listen port by
    /// convention; 0 picks one). Bootstrap runs in the background. Public
    /// downloading jobs then look up peers every [`DHT_INTERVAL_S`]. Lookups
    /// only: this node answers queries but does not announce_peer, so other
    /// peers find us through trackers and PEX rather than the DHT.
    pub async fn enable_dht(&mut self, port: u16) -> Result<u16> {
        if let Some(dht) = &self.dht {
            return Ok(dht.port());
        }
        let server = match rbit::DhtServer::bind(port).await {
            Ok(server) => server,
            Err(_) if port != 0 => rbit::DhtServer::bind(0)
                .await
                .map_err(|_| DriverError("DHT socket bind failed"))?,
            Err(_) => return Err(DriverError("DHT socket bind failed")),
        };
        let server = Arc::new(server);
        let run = Arc::clone(&server);
        self.dht_tasks.push(tokio::spawn(async move {
            let _ = run.run().await;
        }));
        let boot = Arc::clone(&server);
        self.dht_tasks.push(tokio::spawn(async move {
            // A cold node retries until its routing table has entries, then
            // widens it with random-target lookups: get_peers starts from the
            // closest known nodes, and a table of a dozen rarely reaches any.
            for attempt in 0..6u32 {
                let _ = timeout(Duration::from_secs(60), boot.bootstrap()).await;
                if boot.routing_table().node_count() >= 8 {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(10 << attempt.min(4))).await;
            }
            for _ in 0..30 {
                if boot.routing_table().node_count() >= 100 {
                    break;
                }
                let _ = timeout(
                    Duration::from_secs(20),
                    boot.find_node(rbit::NodeId::generate()),
                )
                .await;
            }
        }));
        let port = server.port();
        self.dht = Some(server);
        Ok(port)
    }

    /// Engine diagnostics: (leased pieces, requests in flight, peers we
    /// cannot request from, pickable pieces).
    pub fn lease_stats(&self, id: TorrentId) -> Option<(usize, usize, usize, usize)> {
        self.session.lease_stats(id)
    }

    /// Candidate pool size by origin: (tracker, DHT, PEX, user or incoming).
    pub fn candidate_counts(&self, id: TorrentId) -> (usize, usize, usize, usize) {
        let mut counts = (0, 0, 0, 0);
        for source in self.jobs.0.get(&id).map_or(&[][..], |job| &job.sources) {
            match source.origin {
                PeerOrigin::OriginalTracker(_) => counts.0 += 1,
                PeerOrigin::Dht => counts.1 += 1,
                PeerOrigin::Pex => counts.2 += 1,
                PeerOrigin::UserProvided => counts.3 += 1,
            }
        }
        counts
    }

    /// Routing-table size, or `None` without a DHT node.
    pub fn dht_nodes(&self) -> Option<usize> {
        self.dht
            .as_ref()
            .map(|dht| dht.routing_table().node_count())
    }

    /// Start due lookups for downloading public jobs once the routing table
    /// has nodes to ask.
    fn refresh_dht(&mut self) {
        let Some(dht) = &self.dht else {
            return;
        };
        if dht.routing_table().node_count() == 0 {
            return;
        }
        let now = self.elapsed_s();
        let rows = self.snapshot();
        let mut due = Vec::new();
        for row in rows.jobs.iter() {
            if row.state != JobState::Downloading || self.dht_lookups.contains_key(&row.id) {
                continue;
            }
            if let Some(job) = self.jobs.0.get(&row.id)
                && public_discovery(job)
                && now >= job.dht_due_s
            {
                due.push((row.id, job.torrent.hash()));
            }
        }
        for (id, hash) in due {
            if let Some(job) = self.jobs.0.get_mut(&id) {
                job.dht_due_s = now + DHT_INTERVAL_S;
            }
            let Some(dht) = self.dht.clone() else {
                return;
            };
            let sender = self.dht_sender.clone();
            let task = tokio::spawn(async move {
                let peers = timeout(DHT_LOOKUP_TIMEOUT, dht.get_peers(hash))
                    .await
                    .ok()
                    .and_then(|result| result.ok())
                    .unwrap_or_default();
                let _ = sender.send((id, peers)).await;
            });
            self.dht_lookups.insert(id, task);
        }
    }

    fn collect_dht(&mut self) {
        while let Ok((id, peers)) = self.dht_found.try_recv() {
            self.dht_lookups.remove(&id);
            if !self.jobs.0.get(&id).is_some_and(public_discovery) {
                continue;
            }
            // A cold routing table often finds nothing on the first try;
            // ask again soon instead of after the full interval.
            if peers.is_empty() {
                let retry = self.elapsed_s() + DHT_EMPTY_RETRY_S;
                if let Some(job) = self.jobs.0.get_mut(&id) {
                    job.dht_due_s = job.dht_due_s.min(retry);
                }
            }
            for address in peers.into_iter().take(POOL_CAP) {
                if peer_allowed(address, self.allow_local_trackers)
                    && self.peer_preference(id, address) != PeerPreference::Block
                {
                    self.remember_source(
                        id,
                        PeerSource {
                            address,
                            origin: PeerOrigin::Dht,
                        },
                    );
                }
            }
        }
    }

    /// Remember the added peers of one ut_pex message from `peer`.
    fn receive_pex(&mut self, peer: PeerId, payload: &[u8]) {
        let Some(id) = self.wires.get(&peer).map(|wire| wire.torrent) else {
            return;
        };
        if !self.jobs.0.get(&id).is_some_and(public_discovery) {
            return;
        }
        let Ok(added) = crate::pex::parse_added(payload) else {
            return;
        };
        for address in added {
            // Loopback, link-local and private addresses from strangers
            // only with the local-address setting, as for trackers.
            if peer_allowed(address, self.allow_local_trackers)
                && self.peer_preference(id, address) != PeerPreference::Block
            {
                self.remember_source(
                    id,
                    PeerSource {
                        address,
                        origin: PeerOrigin::Pex,
                    },
                );
            }
        }
    }

    /// Every [`crate::pex::INTERVAL_S`], tell each PEX-capable peer which of
    /// our dialed peers are connected and which have gone since last time.
    /// Incoming peers are not listed: their source port is not a listener.
    fn send_pex(&mut self) {
        let now = self.elapsed_s();
        let due: Vec<PeerId> = self
            .wires
            .iter()
            .filter(|(_, wire)| {
                wire.pex_id.is_some() && now >= wire.pex_at_s.saturating_add(self.pex_interval_s)
            })
            .map(|(peer, _)| *peer)
            .collect();
        for peer in due {
            let Some(wire) = self.wires.get(&peer) else {
                continue;
            };
            let id = wire.torrent;
            let Some(job) = self.jobs.0.get(&id) else {
                continue;
            };
            let current: Vec<SocketAddr> = self
                .wires
                .iter()
                .filter(|(other, w)| {
                    **other != peer
                        && w.torrent == id
                        && job
                            .sources
                            .iter()
                            .any(|source| source.address == w.source.address)
                })
                .map(|(_, w)| w.source.address)
                .take(crate::pex::MAX_SENT)
                .collect();
            let added: Vec<SocketAddr> = current
                .iter()
                .filter(|address| !wire.pex_sent.contains(address))
                .copied()
                .collect();
            let dropped: Vec<SocketAddr> = wire
                .pex_sent
                .iter()
                .filter(|address| !current.contains(address))
                .copied()
                .collect();
            let remote = wire.pex_id;
            if let Some(wire) = self.wires.get_mut(&peer) {
                wire.pex_at_s = now;
            }
            let Some(remote) = remote else {
                continue;
            };
            if added.is_empty() && dropped.is_empty() {
                continue;
            }
            let payload = Bytes::from(crate::pex::encode(&added, &dropped));
            if self
                .queue(
                    peer,
                    Message::Extended {
                        id: remote,
                        payload,
                    },
                )
                .is_ok()
                && let Some(wire) = self.wires.get_mut(&peer)
            {
                wire.pex_sent = current;
            }
        }
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

    fn ingest_peers(
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
            .take(POOL_CAP)
            .collect();
        // Remembered only; [`Driver::fill_connections`] dials them in the
        // background so an unreachable peer never stalls the step loop.
        for address in addresses {
            self.remember_source(id, PeerSource { address, origin });
        }
    }

    fn remember_source(&mut self, id: TorrentId, source: PeerSource) {
        let Some(job) = self.jobs.0.get_mut(&id) else {
            return;
        };
        if job
            .private_tracker
            .is_some_and(|tracker| source.origin != PeerOrigin::OriginalTracker(tracker))
        {
            return;
        }
        if job.sources.len() >= POOL_CAP
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
                    started: false,
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
        for task in self
            .dialing
            .values()
            .chain(self.announcing.values())
            .chain(self.dht_lookups.values())
            .chain(self.dht_tasks.iter())
        {
            task.abort();
        }
    }
}

/// Resolve and classify the tracker again, then announce once per address
/// family it resolved to (one IPv4, one IPv6) over the application-owned
/// HTTP(S) or UDP transport, merging the peers. Healthy if either family
/// answered. The failure's detail carries each family's reason (and a UDP
/// tracker's own message) so the Trackers tab can show it.
async fn announce_detached(
    url: &str,
    allow_local: bool,
    announce: crate::announce::AnnounceRequest<'_>,
) -> std::result::Result<rbit::AnnounceResponse, crate::announce::AnnounceFailure> {
    crate::announce::announce_tracker(url, allow_local, &announce).await
}

/// Azureus-style peer ID: `-TE` and the four-digit version (0.1.0 is 0100),
/// then rbit's twelve random bytes. Trackers and peers key client
/// identification off the prefix, so it must not borrow another client's.
pub const PEER_ID_PREFIX: &[u8; 8] = b"-TE0100-";

fn peer_id() -> [u8; 20] {
    let mut id = *rbit::PeerId::generate().as_bytes();
    id[..8].copy_from_slice(PEER_ID_PREFIX);
    id
}

fn pipeline_for(preference: PeerPreference) -> usize {
    match preference {
        PeerPreference::Avoid => AVOID_PIPELINE,
        PeerPreference::Prefer | PeerPreference::Normal | PeerPreference::Block => {
            DEFAULT_MAX_IN_FLIGHT
        }
    }
}

/// One block request waiting for its piece message.
struct SentRequest {
    piece: u32,
    begin: u32,
    sent_ns: u64,
}

/// Inputs to one upload-slot key. Seeding ignores `sole`, `down_bps` and `rtt_us`.
struct SlotInput {
    preference: u8,
    failures: u32,
    sole: bool,
    seeding: bool,
    down_bps: u64,
    up_bps: u64,
    rtt_us: Option<u64>,
    order: u64,
}

/// Ascending slot key. `rate` is reversed so a faster peer sorts earlier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SlotKey {
    preference: u8,
    failures: u32,
    /// 0 when this peer alone has a still-wanted piece. Seeding forces 1.
    rarity: u8,
    rate: std::cmp::Reverse<u64>,
    rtt_us: u64,
    order: u64,
}

fn slot_key(input: SlotInput) -> SlotKey {
    let (rate, rarity, rtt_us) = if input.seeding {
        (input.up_bps, 1, u64::MAX)
    } else {
        (
            input.down_bps,
            u8::from(!input.sole),
            input.rtt_us.unwrap_or(u64::MAX),
        )
    };
    SlotKey {
        preference: input.preference,
        failures: input.failures,
        rarity,
        rate: std::cmp::Reverse(rate),
        rtt_us,
        order: input.order,
    }
}

/// Preference, then failures, rarity, rate, round trip and arrival.
/// Pure so the policy is testable without a clock.
fn rank_peers(entries: &mut [(SlotKey, PeerId)]) -> Vec<PeerId> {
    entries.sort_unstable_by_key(|(key, _)| *key);
    entries.iter().map(|(_, peer)| *peer).collect()
}

fn note_request(pending: &mut Vec<SentRequest>, piece: u32, begin: u32, sent_ns: u64) {
    if let Some(found) = pending
        .iter_mut()
        .find(|request| request.piece == piece && request.begin == begin)
    {
        found.sent_ns = sent_ns;
        return;
    }
    if pending.len() >= RTT_PENDING_CAP {
        pending.remove(0);
    }
    pending.push(SentRequest {
        piece,
        begin,
        sent_ns,
    });
}

fn take_request(pending: &mut Vec<SentRequest>, piece: u32, begin: u32) -> Option<u64> {
    let index = pending
        .iter()
        .position(|request| request.piece == piece && request.begin == begin)?;
    Some(pending.remove(index).sent_ns)
}

fn forget_request(pending: &mut Vec<SentRequest>, piece: u32, begin: u32) {
    pending.retain(|request| request.piece != piece || request.begin != begin);
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

/// Whether DHT and PEX may serve this job: a public torrent whose user did
/// not ask for strict, tracker-only bootstrap.
fn public_discovery(job: &Job) -> bool {
    !job.torrent.is_private() && job.private_tracker.is_none() && job.bootstrap
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

/// Open one outgoing peer wire. Each attempt is bounded by CONNECT_TIMEOUT.
/// Under PreferEncrypted a failed or stalled MSE handshake is redialled once
/// in plaintext; nothing of the BitTorrent handshake was sent before it.
/// A refused TCP connect or a BitTorrent-level failure is not retried.
async fn dial_peer(
    address: SocketAddr,
    hash: [u8; 20],
    peer_id: [u8; 20],
    encryption: PeerEncryption,
) -> std::result::Result<PeerConnection, &'static str> {
    #[derive(PartialEq)]
    enum Stage {
        Connect,
        Encryption,
        Handshake,
    }
    let plaintext = async {
        timeout(
            CONNECT_TIMEOUT,
            PeerConnection::connect(address, hash, peer_id),
        )
        .await
        .map_err(|_| "peer handshake deadline exceeded")?
        .map_err(|_| "peer connection failed")
    };
    if encryption == PeerEncryption::PlaintextOnly {
        return plaintext.await;
    }
    let mut stage = Stage::Connect;
    let attempt = timeout(CONNECT_TIMEOUT, async {
        let tcp = TcpStream::connect(address)
            .await
            .map_err(|_| "peer connection failed")?;
        let _ = tcp.set_nodelay(true);
        stage = Stage::Encryption;
        let stream = mse::initiate(tcp, hash, encryption.policy())
            .await
            .map_err(|_| "peer encryption handshake failed")?;
        stage = Stage::Handshake;
        PeerConnection::connect_with_stream(stream, address, hash, peer_id, false)
            .await
            .map_err(|_| "peer connection failed")
    })
    .await
    .unwrap_or(Err("peer handshake deadline exceeded"));
    match attempt {
        Err(_) if stage == Stage::Encryption && encryption == PeerEncryption::PreferEncrypted => {
            plaintext.await
        }
        attempt => attempt,
    }
}

/// Classify one accepted socket and route it by info hash. A plaintext
/// BitTorrent handshake is only peeked, so rbit's accept reads it again;
/// anything else runs the MSE responder against the public job hashes.
/// The caller bounds the whole probe with CONNECT_TIMEOUT.
async fn route_incoming(
    stream: TcpStream,
    address: SocketAddr,
    keys: &Mutex<IncomingKeys>,
) -> Option<Incoming> {
    const PREFIX: &[u8; 20] = b"\x13BitTorrent protocol";
    let encryption = keys.lock().ok()?.encryption;
    let mut bytes = [0; 68];
    let plaintext = loop {
        let n = stream.peek(&mut bytes).await.ok()?;
        if n == 0 {
            return None;
        }
        let have = n.min(PREFIX.len());
        if bytes[..have] != PREFIX[..have] {
            break None;
        }
        if n == bytes.len() {
            break Some(Handshake::decode(&bytes).ok()?);
        }
        // A partial prefix remains readable; avoid a spin loop.
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    if let Some(handshake) = plaintext {
        return (encryption != PeerEncryption::RequireEncrypted).then_some(Incoming {
            stream: IncomingStream::Plain(stream),
            hash: handshake.info_hash,
            address,
        });
    }
    if encryption == PeerEncryption::PlaintextOnly {
        return None;
    }
    let _ = stream.set_nodelay(true);
    let accepted = mse::accept(stream, encryption.policy(), |req2| {
        let keys = keys.lock().ok()?;
        keys.hashes
            .iter()
            .find(|hash| mse::req2_hash(hash) == *req2)
            .copied()
    })
    .await
    .ok()?;
    match accepted {
        mse::Accepted::Encrypted { info_hash, stream } => Some(Incoming {
            stream: IncomingStream::Negotiated(Box::new(stream)),
            hash: info_hash,
            address,
        }),
        mse::Accepted::Plaintext(_) => None,
    }
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

    fn input(
        (preference, failures, sole, seeding, down_bps, up_bps, rtt_us, order): (
            u8,
            u32,
            bool,
            bool,
            u64,
            u64,
            Option<u64>,
            u64,
        ),
    ) -> SlotKey {
        slot_key(SlotInput {
            preference,
            failures,
            sole,
            seeding,
            down_bps,
            up_bps,
            rtt_us,
            order,
        })
    }

    #[test]
    fn slot_order_ranks_preference_failures_rarity_rate_and_rtt() {
        let peer = |n: u64| PeerId(n);
        let mut rate_only = vec![
            (input((1, 0, false, false, 50_000, 0, None, 1)), peer(1)),
            (input((0, 0, false, false, 10, 0, None, 2)), peer(2)),
            (input((1, 0, false, false, 90_000, 0, None, 3)), peer(3)),
            (input((2, 0, false, false, 999_999, 0, None, 4)), peer(4)),
            (input((1, 0, false, false, 90_000, 0, None, 0)), peer(0)),
        ];
        assert_eq!(
            rank_peers(&mut rate_only),
            vec![peer(2), peer(0), peer(3), peer(1), peer(4)]
        );

        let mut ranked = vec![
            (input((0, 0, false, false, 10, 0, Some(1), 7)), peer(7)),
            (input((1, 0, true, false, 1, 0, Some(9_000), 6)), peer(6)),
            (input((1, 0, false, false, 100, 0, Some(5_000), 5)), peer(5)),
            (
                input((1, 0, false, false, 100, 0, Some(10_000), 4)),
                peer(4),
            ),
            (input((1, 0, false, false, 100, 0, None, 3)), peer(3)),
            (
                input((1, 1, true, false, 1_000_000, 0, Some(1), 2)),
                peer(2),
            ),
            (
                input((2, 0, true, false, 1_000_000, 0, Some(1), 1)),
                peer(1),
            ),
        ];
        assert_eq!(
            rank_peers(&mut ranked),
            vec![
                peer(7),
                peer(6),
                peer(5),
                peer(4),
                peer(3),
                peer(2),
                peer(1)
            ]
        );
        assert!(
            input((1, 0, false, false, 100, 0, Some(0), 2))
                < input((1, 0, false, false, 100, 0, None, 1))
        );
        assert!(
            input((1, 0, false, true, 1, 500, None, 9))
                < input((1, 0, true, true, 9_000_000, 10, Some(1), 1))
        );
        assert_eq!(blend_rate(0, 400), 400);
        assert_eq!(blend_rate(400, 0), 300);
        assert_eq!(blend_rate(1_000, 2_000), 1_250);
    }

    #[test]
    fn slot_order_pending_rtt_replaces_and_caps() {
        let mut pending = Vec::new();
        note_request(&mut pending, 1, 0, 10);
        note_request(&mut pending, 1, 0, 25);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].sent_ns, 25);
        assert_eq!(take_request(&mut pending, 1, 0), Some(25));
        assert_eq!(take_request(&mut pending, 1, 0), None);
        forget_request(&mut pending, 1, 0);
        for index in 0..(RTT_PENDING_CAP + 5) {
            note_request(&mut pending, 0, index as u32, index as u64);
        }
        assert_eq!(pending.len(), RTT_PENDING_CAP);
        assert!(pending.iter().all(|request| request.begin >= 5));
        forget_request(&mut pending, 0, 5);
        assert_eq!(pending.len(), RTT_PENDING_CAP - 1);
        assert!(pending.iter().all(|request| request.begin != 5));
    }
}
