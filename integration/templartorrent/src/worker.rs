//! Owns the rbit [`Driver`] on one current-thread runtime, off the UI thread.
//! `step` is never cancelled: dropping it mid-announce would leave a tracker
//! clock in flight. Commands are drained between steps.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use templartorrent_core::model::{Command, JobState, Limits, PeerPreference, Snapshot, TorrentId};
use templartorrent_rbit_adapter::driver::{Driver, ExistingData, PeerEncryption};
use templartorrent_rbit_adapter::http::{
    IDLE_TIMEOUT, UrlConfig, UrlDetail, UrlManager, UrlRecord,
};
use templartorrent_rbit_adapter::portmap;
use tokio::sync::{mpsc, watch};

use crate::geo::GeoDb;
use crate::persist::{SessionStore, Settings};
use crate::space;
use crate::text::{t, tf};
use crate::workflow;

const QUEUE: usize = 64;
/// Resume bits are rewritten on a command, on shutdown, or at this cadence
/// once verified bytes have changed. Not once per block.
const RESUME_BATCH: Duration = Duration::from_secs(10);

type WakeHook = Arc<dyn Fn() + Send + Sync>;
type WakeSlot = Arc<Mutex<Option<WakeHook>>>;

#[derive(Clone, PartialEq, Eq)]
struct JobMark {
    id: u64,
    state: JobState,
    wanted: u64,
    verified: u64,
    downloaded: u64,
    uploaded: u64,
    seeds: u32,
    peers: u32,
    down_bps: u64,
    up_bps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenState {
    Starting,
    Bound(SocketAddr),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub index: usize,
    pub path: String,
    pub length: u64,
    pub verified_bytes: u64,
    pub wanted: bool,
    pub rank: u32,
    /// BEP 47 pad file: kept for the order permutation, never shown.
    pub pad: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerView {
    pub address: String,
    pub socket: SocketAddr,
    pub preference: PeerPreference,
    pub client: String,
    pub down_bps: u64,
    pub up_bps: u64,
    /// Smoothed block round trip in microseconds, when a requested block has returned.
    pub rtt_us: Option<u64>,
    /// Hash failures attributed to this peer.
    pub hash_failures: u32,
    pub country: Option<[u8; 2]>,
    pub remote_choked: bool,
    pub remote_interested: bool,
    pub we_choke: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerView {
    pub label: String,
    pub origin: String,
    pub seeds: Option<u32>,
    pub peers: Option<u32>,
    pub manual_due_s: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ack {
    pub seq: u64,
    pub accepted: bool,
    pub message: String,
    pub entries: Vec<String>,
    pub magnet: Option<String>,
    pub space: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct SessionView {
    pub snapshot: Snapshot,
    pub limits: Limits,
    pub listen: ListenState,
    pub selected: Option<TorrentId>,
    pub destination: Option<String>,
    pub last_error: Option<String>,
    pub seeding_reason: String,
    pub files: Vec<FileRow>,
    pub peers: Vec<PeerView>,
    pub trackers: Vec<TrackerView>,
    pub geo_status: String,
    pub elapsed_s: u64,
    pub acks: Vec<Ack>,
    pub allow_local_trackers: bool,
    pub public_tracker_count: usize,
    pub public_tracker_path: String,
    pub note: Option<String>,
    /// DHT routing-table size; `None` when the DHT is off or failed.
    pub dht_nodes: Option<usize>,
    pub encryption: PeerEncryption,
    /// The saved router port-mapping consent.
    pub port_mapping: bool,
    /// What the router mapping is doing now, without the external address.
    pub port_mapping_status: String,
    /// The selected job's URL download state, when it is a URL job.
    pub url_detail: Option<UrlDetail>,
    pub allow_local_urls: bool,
}

impl SessionView {
    pub(crate) fn idle(limits: Limits, public_tracker_path: String) -> Self {
        Self {
            snapshot: Snapshot::default(),
            limits,
            listen: ListenState::Starting,
            selected: None,
            destination: None,
            last_error: None,
            seeding_reason: String::new(),
            files: Vec::new(),
            peers: Vec::new(),
            trackers: Vec::new(),
            geo_status: t("geo.starting").into(),
            elapsed_s: 0,
            acks: Vec::new(),
            allow_local_trackers: false,
            public_tracker_count: 0,
            public_tracker_path,
            note: None,
            dht_nodes: None,
            encryption: PeerEncryption::default(),
            port_mapping: false,
            port_mapping_status: port_mapping_text(&portmap::Status::Off),
            url_detail: None,
            allow_local_urls: false,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Visible {
    jobs: Vec<JobMark>,
    names: Vec<String>,
    budget: usize,
    limits: (usize, usize, usize, usize, usize, u64, u64, usize),
    listen: ListenState,
    selected: Option<u64>,
    destination: Option<String>,
    last_error: Option<String>,
    seeding_reason: String,
    files: Vec<FileRow>,
    peers: Vec<PeerView>,
    trackers: Vec<TrackerView>,
    geo_status: String,
    elapsed_s: u64,
    acks: Vec<Ack>,
    allow_local_trackers: bool,
    public_tracker_count: usize,
    note: Option<String>,
    dht_nodes: Option<usize>,
    encryption: PeerEncryption,
    port_mapping: bool,
    port_mapping_status: String,
    url_detail: Option<UrlDetail>,
    allow_local_urls: bool,
}

pub fn same_visible(left: &SessionView, right: &SessionView) -> bool {
    let mut left_visible = visible(left);
    let mut right_visible = visible(right);
    if !cooling(left) && !cooling(right) {
        left_visible.elapsed_s = 0;
        right_visible.elapsed_s = 0;
    }
    left_visible == right_visible
}

fn cooling(view: &SessionView) -> bool {
    view.trackers
        .iter()
        .any(|tracker| tracker.manual_due_s > view.elapsed_s)
}

fn visible(view: &SessionView) -> Visible {
    Visible {
        jobs: view
            .snapshot
            .jobs
            .iter()
            .map(|job| JobMark {
                id: job.id.0,
                state: job.state,
                wanted: job.wanted_bytes,
                verified: job.verified_bytes,
                downloaded: job.total_payload_downloaded,
                uploaded: job.total_payload_uploaded,
                seeds: job.connected_seeds,
                peers: job.connected_peers,
                down_bps: job.down_bps,
                up_bps: job.up_bps,
            })
            .collect(),
        names: view
            .snapshot
            .jobs
            .iter()
            .map(|job| job.name.to_string())
            .collect(),
        budget: view.snapshot.payload_budget_used,
        limits: (
            view.limits.download_jobs,
            view.limits.seed_jobs,
            view.limits.upload_peer_slots,
            view.limits.connections_total,
            view.limits.connections_per_job,
            view.limits.download_kb_s,
            view.limits.upload_kb_s,
            view.limits.payload_budget_bytes,
        ),
        listen: view.listen.clone(),
        selected: view.selected.map(|id| id.0),
        destination: view.destination.clone(),
        last_error: view.last_error.clone(),
        seeding_reason: view.seeding_reason.clone(),
        files: view.files.clone(),
        peers: view.peers.clone(),
        trackers: view.trackers.clone(),
        geo_status: view.geo_status.clone(),
        elapsed_s: view.elapsed_s,
        acks: view.acks.clone(),
        allow_local_trackers: view.allow_local_trackers,
        public_tracker_count: view.public_tracker_count,
        note: view.note.clone(),
        dht_nodes: view.dht_nodes,
        encryption: view.encryption,
        port_mapping: view.port_mapping,
        port_mapping_status: view.port_mapping_status.clone(),
        url_detail: view.url_detail.clone(),
        allow_local_urls: view.allow_local_urls,
    }
}

pub struct WorkerConfig {
    pub limits: Limits,
    pub allow_local_trackers: bool,
    /// URL downloads may reach loopback, private and link-local addresses.
    pub allow_local_urls: bool,
    pub public_trackers: Vec<String>,
    pub public_tracker_path: PathBuf,
    pub geo_db_path: Option<PathBuf>,
    pub listen: SocketAddr,
    /// Where job records and settings live; `None` disables persistence.
    pub session_dir: Option<PathBuf>,
}

pub enum Request {
    Command {
        seq: u64,
        command: Command,
    },
    ListDir {
        seq: u64,
        path: PathBuf,
    },
    ReadTorrent {
        seq: u64,
        path: PathBuf,
    },
    Space {
        seq: u64,
        path: PathBuf,
    },
    AllowLocal(bool),
    /// Save the URL address allowance; applies to the next Add.
    AllowLocalUrls(bool),
    /// Save a new listen port; the listener rebinds on the next launch.
    SetListenPort(u16),
    /// Apply and save the MSE/PE policy for new connections.
    SetEncryption(PeerEncryption),
    /// Save the router port-mapping consent and start or remove the mapping.
    SetPortMapping(bool),
    ReloadTrackers,
}

pub struct WorkerHandle {
    tx: mpsc::Sender<Request>,
    stop: watch::Sender<bool>,
    view: Arc<Mutex<SessionView>>,
    selected: Arc<Mutex<Option<TorrentId>>>,
    waker: WakeSlot,
    thread: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    pub fn start(config: WorkerConfig) -> Result<Self, String> {
        let limits = config.limits.clone();
        let path = config.public_tracker_path.display().to_string();
        let view = Arc::new(Mutex::new(SessionView::idle(limits, path)));
        let selected = Arc::new(Mutex::new(None));
        let waker = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel(QUEUE);
        let (stop, stop_rx) = watch::channel(false);
        let shared = Shared {
            view: Arc::clone(&view),
            selected: Arc::clone(&selected),
            waker: Arc::clone(&waker),
        };
        let thread = std::thread::Builder::new()
            .name("templartorrent-driver".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("current-thread runtime");
                runtime.block_on(run_session(config, rx, stop_rx, shared));
            })
            .map_err(|error| tf("worker.thread_failed", &[("detail", &error)]))?;
        Ok(Self {
            tx,
            stop,
            view,
            selected,
            waker,
            thread: Some(thread),
        })
    }

    pub fn try_request(&self, request: Request) -> Result<(), &'static str> {
        self.tx.try_send(request).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => t("status.queue_full"),
            mpsc::error::TrySendError::Closed(_) => t("status.channel_closed"),
        })
    }

    pub fn view(&self) -> SessionView {
        lock(&self.view).clone()
    }

    pub fn set_selected(&self, id: Option<TorrentId>) {
        *lock(&self.selected) = id;
    }

    pub fn set_waker(&self, wake: WakeHook) {
        *lock(&self.waker) = Some(wake);
    }

    pub fn wait_ack(&self, seq: u64, timeout: Duration) -> Option<Ack> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(ack) = self.view().acks.iter().find(|ack| ack.seq == seq).cloned() {
                return Some(ack);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    pub fn shutdown(&mut self) {
        let _ = self.stop.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Shared {
    view: Arc<Mutex<SessionView>>,
    selected: Arc<Mutex<Option<TorrentId>>>,
    waker: WakeSlot,
}

enum Engine {
    Live {
        driver: Box<Driver>,
        /// URL jobs, spending from the driver's budget and download rate.
        urls: Box<UrlManager>,
        listen_error: Option<String>,
    },
    Down(&'static str),
}

/// A manager on the driver's shared limits.
fn url_manager(driver: &Driver, allow_local: bool) -> Box<UrlManager> {
    let (rate, origin) = driver.download_rate();
    Box::new(UrlManager::new(UrlConfig {
        budget: driver.payload_budget(),
        rate,
        origin,
        allow_local,
        tls: None,
        idle: IDLE_TIMEOUT,
        max_active: driver.limits().download_jobs,
    }))
}

struct Memory {
    allow_local: bool,
    allow_local_urls: bool,
    public_trackers: Vec<String>,
    public_path: PathBuf,
    geo: GeoDb,
    acks: Vec<Ack>,
    note: Option<String>,
    rates: std::collections::HashMap<u64, workflow::RateCarry>,
    rated_at: Option<Instant>,
    store: Option<SessionStore>,
    resume_flushed_at: Instant,
    resume_verified: u64,
    /// Saved peer listen port; 0 until a port is chosen.
    listen_port: u16,
    encryption: PeerEncryption,
    port_mapping: bool,
    /// `.torrent` files the Add dialog read, keyed by the magnet it was shown.
    /// Adding that magnet uses these bytes, so no metadata is fetched.
    torrent_files: std::collections::VecDeque<(String, Vec<u8>)>,
}

/// `.torrent` files kept between reading and Add.
const TORRENT_FILE_CACHE: usize = 4;

/// The listen address for this launch. An explicit port wins; otherwise the
/// saved port; otherwise, for an unspecified (production) address, a random
/// port from the dynamic range, saved by the caller. Loopback test configs
/// keep port 0.
fn choose_listen(config: SocketAddr, saved: u16) -> SocketAddr {
    if config.port() != 0 {
        return config;
    }
    if saved != 0 {
        return SocketAddr::new(config.ip(), saved);
    }
    if !config.ip().is_unspecified() {
        return config;
    }
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    let port = 49_152 + (hasher.finish() % 16_000) as u16;
    SocketAddr::new(config.ip(), port)
}

async fn run_session(
    config: WorkerConfig,
    mut rx: mpsc::Receiver<Request>,
    stop: watch::Receiver<bool>,
    shared: Shared,
) {
    let mut memory = Memory {
        allow_local: config.allow_local_trackers,
        allow_local_urls: config.allow_local_urls,
        public_trackers: config.public_trackers.clone(),
        public_path: config.public_tracker_path.clone(),
        geo: GeoDb::new(config.geo_db_path),
        acks: Vec::new(),
        note: None,
        rates: std::collections::HashMap::new(),
        rated_at: None,
        store: config.session_dir.clone().map(SessionStore::new),
        resume_flushed_at: Instant::now(),
        resume_verified: 0,
        listen_port: 0,
        encryption: PeerEncryption::default(),
        port_mapping: false,
        torrent_files: std::collections::VecDeque::new(),
    };
    let saved_port = memory
        .store
        .as_ref()
        .and_then(|store| store.load_settings().ok().flatten())
        .map_or(0, |settings| settings.listen_port);
    let listen = choose_listen(config.listen, saved_port);
    let mut engine = match Driver::new(config.limits) {
        Ok(mut driver) => {
            driver.set_allow_local_trackers(config.allow_local_trackers);
            driver.set_public_trackers(config.public_trackers);
            match driver.listen(listen).await {
                Ok(bound) => {
                    if config.listen.ip().is_unspecified() {
                        memory.listen_port = bound.port();
                        // Production only: loopback test sessions never
                        // contact the public DHT.
                        if let Err(error) = driver.enable_dht(bound.port()).await {
                            memory.note = Some(tf("note.dht_unavailable", &[("detail", &error.0)]));
                        }
                    }
                    let urls = url_manager(&driver, config.allow_local_urls);
                    Engine::Live {
                        driver: Box::new(driver),
                        urls,
                        listen_error: None,
                    }
                }
                Err(error) => {
                    let urls = url_manager(&driver, config.allow_local_urls);
                    Engine::Live {
                        driver: Box::new(driver),
                        urls,
                        listen_error: Some(error.0.to_string()),
                    }
                }
            }
        }
        Err(error) => Engine::Down(error.0),
    };
    let mut last = publish(&mut engine, &mut memory, &shared);
    restore_session(&mut engine, &mut memory).await;
    apply_port_mapping(&mut engine, &mut memory).await;
    // Keep the first launch's port, so a router forward stays valid.
    if memory.listen_port != 0
        && memory.listen_port != saved_port
        && let Engine::Live { driver, .. } = &engine
        && let Err(error) = save_settings(driver, &memory)
    {
        memory.note = Some(tf("note.session_not_saved", &[("detail", &error)]));
    }
    loop {
        if *stop.borrow() {
            break;
        }
        let mut halt = false;
        loop {
            match rx.try_recv() {
                Ok(request) => {
                    if dispatch(&mut engine, &mut memory, &shared, request).await {
                        halt = true;
                        break;
                    }
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    halt = true;
                    break;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
            }
        }
        last = publish_if_changed(&mut engine, &mut memory, &shared, last);
        if halt || *stop.borrow() {
            break;
        }
        match &mut engine {
            Engine::Live { driver, urls, .. } => {
                urls.tick();
                match driver.step(Duration::from_millis(200)).await {
                    Ok(report) => {
                        if let Some(store) = memory.store.as_ref() {
                            for id in report.resolved {
                                if let Err(error) = save_job(driver, store, id) {
                                    memory.note =
                                        Some(tf("note.session_not_saved", &[("detail", &error)]));
                                }
                            }
                        }
                    }
                    Err(error) => {
                        memory.note = Some(tf("note.driver_error", &[("detail", &error.0)]));
                    }
                }
                urls.tick();
                // Names, sizes, completions and failures are saved as they
                // arrive; offsets wait for the synced checkpoint below.
                if let Some(store) = memory.store.as_ref()
                    && let Err(error) = save_urls(store, &urls.take_dirty())
                {
                    memory.note = Some(tf("note.urls_not_saved", &[("detail", &error)]));
                }
            }
            Engine::Down(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
        last = publish_if_changed(&mut engine, &mut memory, &shared, last);
        maybe_flush_resumes(&mut engine, &mut memory).await;
    }
    if let Engine::Live { driver, urls, .. } = &mut engine {
        let flush_error = memory
            .store
            .as_ref()
            .and_then(|store| flush_resumes(driver, store).err());
        // URL tasks stop at an await point, their files are synced, and the
        // durable offsets are saved; running intent survives for the next launch.
        let records = urls.shutdown().await;
        if let Some(store) = memory.store.as_ref()
            && let Err(error) = save_urls(store, &records)
        {
            memory.note = Some(tf("note.urls_not_saved", &[("detail", &error)]));
        }
        driver.shutdown().await;
        if let Some(error) = flush_error {
            memory.note = Some(tf("note.resume_not_saved", &[("detail", &error)]));
        }
    }
}

fn save_urls(store: &SessionStore, records: &[UrlRecord]) -> Result<(), String> {
    for record in records {
        store.save_url(record)?;
    }
    Ok(())
}

/// Apply saved settings, then reinstall every saved job through a disk
/// recheck. Nothing here contacts a peer; saved trackers announce on their
/// own schedule once the driver steps. Skipped records are reported once.
async fn restore_session(engine: &mut Engine, memory: &mut Memory) {
    let Some(store) = &memory.store else {
        return;
    };
    let Engine::Live { driver, urls, .. } = engine else {
        return;
    };
    let mut notes = Vec::new();
    match store.load_settings() {
        Ok(Some(settings)) => {
            memory.allow_local = settings.allow_local_trackers;
            driver.set_allow_local_trackers(settings.allow_local_trackers);
            memory.allow_local_urls = settings.allow_local_urls;
            urls.set_allow_local(settings.allow_local_urls);
            memory.encryption = settings.encryption;
            driver.set_peer_encryption(settings.encryption);
            memory.port_mapping = settings.port_mapping;
            urls.set_max_active(settings.limits.download_jobs);
            if driver.set_limits(settings.limits).await.is_err() {
                notes.push(t("note.limits_rejected").to_string());
            }
        }
        Ok(None) => {}
        Err(error) => notes.push(tf("note.settings_not_loaded", &[("detail", &error)])),
    }
    let (records, errors) = store.load_jobs();
    let mut restored = 0usize;
    let mut skipped = errors.len();
    let mut first_skip = errors.into_iter().next();
    for record in &records {
        let resume = match store.load_resume(&record.info_hash) {
            Ok(resume) => resume,
            Err(error) => {
                notes.push(tf("note.resume_ignored", &[("detail", &error)]));
                None
            }
        };
        match driver
            .restore_job_with_resume(record, resume.as_ref())
            .await
        {
            Ok(id) => {
                restored += 1;
                if let Ok(scan) = driver.resume_scan(id)
                    && let Some(reason) = scan.note
                {
                    notes.push(tf("note.resume_not_trusted", &[("detail", &reason)]));
                }
            }
            Err(error) => {
                skipped += 1;
                first_skip.get_or_insert_with(|| tf("note.job_skipped", &[("detail", &error.0)]));
            }
        }
    }
    let (url_records, url_errors) = store.load_urls();
    skipped += url_errors.len();
    if first_skip.is_none() {
        first_skip = url_errors.into_iter().next();
    }
    for record in url_records {
        match urls.restore(record) {
            Ok(_) => restored += 1,
            Err(error) => {
                skipped += 1;
                first_skip.get_or_insert_with(|| tf("note.url_job_skipped", &[("detail", &error)]));
            }
        }
    }
    if skipped > 0 {
        notes.push(tf(
            "note.restored_skipped",
            &[
                ("restored", &restored),
                ("skipped", &skipped),
                ("reason", &first_skip.unwrap_or_default()),
            ],
        ));
    } else if restored > 0 {
        notes.push(tf("note.restored", &[("restored", &restored)]));
    }
    if !notes.is_empty() {
        memory.note = Some(notes.join(". "));
    }
}

/// Start or remove the router mapping to match the saved consent. Needs a
/// bound listener; the driver refuses a listener the router cannot reach.
async fn apply_port_mapping(engine: &mut Engine, memory: &mut Memory) {
    let Engine::Live { driver, .. } = engine else {
        return;
    };
    if !memory.port_mapping {
        driver.disable_port_mapping().await;
    } else if let Err(error) = driver.enable_port_mapping() {
        memory.note = Some(tf("note.port_mapping_unavailable", &[("detail", &error.0)]));
    }
}

fn save_job(driver: &mut Driver, store: &SessionStore, id: TorrentId) -> Result<(), String> {
    // A magnet without metadata has no record yet; it is saved once its
    // metadata is installed (`DriverReport::resolved`).
    if driver.awaiting_metadata(id) {
        return Ok(());
    }
    let record = driver.export_job(id).map_err(|error| error.0.to_string())?;
    store.save_job(&record)?;
    match driver.export_resume(id) {
        Ok(resume) => store.save_resume(&record.info_hash, &resume),
        Err(error) => Err(error.0.to_string()),
    }
}

/// Write every live job and its resume bitmap. The bitmap is produced after
/// payload handles have been synced, and this returns before those handles
/// are closed.
fn flush_resumes(driver: &mut Driver, store: &SessionStore) -> Result<(), String> {
    let ids: Vec<TorrentId> = driver.snapshot().jobs.iter().map(|job| job.id).collect();
    for id in ids {
        save_job(driver, store, id)?;
    }
    Ok(())
}

async fn maybe_flush_resumes(engine: &mut Engine, memory: &mut Memory) {
    if memory.resume_flushed_at.elapsed() < RESUME_BATCH {
        return;
    }
    let Engine::Live { driver, urls, .. } = engine else {
        return;
    };
    // URL offsets: sync first, then save what moved.
    let changed = urls.checkpoint().await;
    if let Some(store) = memory.store.as_ref()
        && let Err(error) = save_urls(store, &changed)
    {
        memory.note = Some(tf("note.urls_not_saved", &[("detail", &error)]));
    }
    let verified = driver
        .snapshot()
        .jobs
        .iter()
        .map(|job| job.verified_bytes)
        .sum();
    memory.resume_flushed_at = Instant::now();
    if verified == memory.resume_verified {
        return;
    }
    let saved = if let Some(store) = memory.store.as_ref() {
        flush_resumes(driver, store)
    } else {
        return;
    };
    match saved {
        Ok(()) => memory.resume_verified = verified,
        Err(error) => memory.note = Some(tf("note.resume_not_saved", &[("detail", &error)])),
    }
}

fn save_settings(driver: &Driver, memory: &Memory) -> Result<(), String> {
    let Some(store) = &memory.store else {
        return Ok(());
    };
    store.save_settings(&Settings {
        limits: driver.limits().clone(),
        allow_local_trackers: memory.allow_local,
        listen_port: memory.listen_port,
        encryption: memory.encryption,
        port_mapping: memory.port_mapping,
        allow_local_urls: memory.allow_local_urls,
    })
}

/// After an accepted command, persist what it changed. A failed save is
/// reported in the status note; the live state is already applied.
fn persist_after(
    engine: &mut Engine,
    memory: &mut Memory,
    command: &Command,
    added: Option<TorrentId>,
    removed: Option<Removed>,
) {
    let Engine::Live { driver, urls, .. } = engine else {
        return;
    };
    let Some(store) = &memory.store else {
        return;
    };
    let result = match command {
        Command::Add { .. } => match added {
            Some(id) => save_job(driver, store, id),
            None => Ok(()),
        },
        Command::AddUrl { .. } | Command::Retry(_) | Command::Cancel(_) => {
            save_urls(store, &urls.take_dirty())
        }
        Command::Pause(id) | Command::Resume(id) if UrlManager::is_url_id(*id) => {
            save_urls(store, &urls.take_dirty())
        }
        Command::Pause(id)
        | Command::Resume(id)
        | Command::SetFileOrder { id, .. }
        | Command::SetFileOrderAutomatic(id)
        | Command::SetFileWanted { id, .. }
        | Command::SetPeerPreference { id, .. } => save_job(driver, store, *id),
        Command::Remove { .. } => match removed {
            Some(Removed::Torrent(hash)) => store.remove_job(&hash),
            Some(Removed::Url(key)) => store.remove_url(&key),
            None => Ok(()),
        },
        Command::SetLimits(_) => save_settings(driver, memory),
        Command::Reannounce { .. } | Command::Inspect { .. } | Command::Shutdown => Ok(()),
    };
    if let Err(error) = result {
        memory.note = Some(tf("note.session_not_saved", &[("detail", &error)]));
    }
}

/// What a Remove took away, so its record can go too.
enum Removed {
    Torrent([u8; 20]),
    Url([u8; 20]),
}

fn publish_if_changed(
    engine: &mut Engine,
    memory: &mut Memory,
    shared: &Shared,
    last: SessionView,
) -> SessionView {
    let next = capture(engine, memory, shared);
    if same_visible(&last, &next) {
        return last;
    }
    *lock(&shared.view) = next.clone();
    let wake = lock(&shared.waker).clone();
    if let Some(wake) = wake {
        wake();
    }
    next
}

fn publish(engine: &mut Engine, memory: &mut Memory, shared: &Shared) -> SessionView {
    let next = capture(engine, memory, shared);
    *lock(&shared.view) = next.clone();
    let wake = lock(&shared.waker).clone();
    if let Some(wake) = wake {
        wake();
    }
    next
}

fn capture(engine: &mut Engine, memory: &mut Memory, shared: &Shared) -> SessionView {
    let selected = *lock(&shared.selected);
    let path = memory.public_path.display().to_string();
    match engine {
        Engine::Down(reason) => SessionView {
            snapshot: Snapshot::default(),
            limits: Limits::default(),
            listen: ListenState::Failed((*reason).to_string()),
            selected: None,
            destination: None,
            last_error: None,
            seeding_reason: String::new(),
            files: Vec::new(),
            peers: Vec::new(),
            trackers: Vec::new(),
            geo_status: memory.geo.status_text(),
            elapsed_s: 0,
            acks: memory.acks.clone(),
            allow_local_trackers: memory.allow_local,
            public_tracker_count: memory.public_trackers.len(),
            public_tracker_path: path,
            note: Some((*reason).to_string()),
            dht_nodes: None,
            encryption: memory.encryption,
            port_mapping: memory.port_mapping,
            port_mapping_status: port_mapping_text(&portmap::Status::Off),
            url_detail: None,
            allow_local_urls: memory.allow_local_urls,
        },
        Engine::Live {
            driver,
            urls,
            listen_error,
        } => {
            let mut snapshot = driver.snapshot();
            if !urls.is_empty() {
                let mut jobs = snapshot.jobs.to_vec();
                jobs.extend(urls.rows());
                snapshot.jobs = jobs.into();
            }
            apply_rates(&mut snapshot, memory);
            let selected = selected.filter(|id| snapshot.jobs.iter().any(|job| job.id == *id));
            let listen = match (driver.listening_address(), listen_error.clone()) {
                (Some(address), _) => ListenState::Bound(address),
                (None, Some(error)) => ListenState::Failed(error),
                (None, None) => ListenState::Starting,
            };
            let url_detail = selected.and_then(|id| urls.detail(id));
            let (files, peers, trackers, destination, last_error) = match selected {
                Some(_) if url_detail.is_some() => (
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    url_detail.as_ref().map(|detail| detail.destination.clone()),
                    url_detail.as_ref().and_then(|detail| detail.error.clone()),
                ),
                Some(id) => (
                    file_rows(driver, id),
                    peer_rows(driver, id, &mut memory.geo),
                    tracker_rows(driver, id),
                    driver
                        .destination(id)
                        .map(|path| path.display().to_string()),
                    driver.last_error(id).map(str::to_string),
                ),
                None => (Vec::new(), Vec::new(), Vec::new(), None, None),
            };
            SessionView {
                limits: driver.limits().clone(),
                elapsed_s: driver.elapsed_secs(),
                seeding_reason: driver.seeding_unavailable_reason().to_string(),
                snapshot,
                listen,
                selected,
                destination,
                last_error,
                files,
                peers,
                trackers,
                geo_status: memory.geo.status_text(),
                acks: memory.acks.clone(),
                allow_local_trackers: memory.allow_local,
                public_tracker_count: memory.public_trackers.len(),
                public_tracker_path: path,
                note: memory.note.clone(),
                dht_nodes: driver.dht_nodes(),
                encryption: memory.encryption,
                port_mapping: memory.port_mapping,
                port_mapping_status: port_mapping_text(&driver.port_mapping_status()),
                url_detail,
                allow_local_urls: memory.allow_local_urls,
            }
        }
    }
}

fn apply_rates(snapshot: &mut Snapshot, memory: &mut Memory) {
    let now = Instant::now();
    let dt_ms = memory
        .rated_at
        .map(|at| {
            let millis = now.saturating_duration_since(at).as_millis();
            u64::try_from(millis).unwrap_or(u64::MAX)
        })
        .unwrap_or(0);
    memory.rated_at = Some(now);
    let mut jobs = snapshot.jobs.to_vec();
    let mut live = std::collections::HashMap::with_capacity(jobs.len());
    for job in &mut jobs {
        let next = match memory.rates.get(&job.id.0).copied() {
            Some(previous) => workflow::advance_rate(
                previous,
                job.total_payload_downloaded,
                job.total_payload_uploaded,
                dt_ms,
            ),
            None => workflow::RateCarry {
                downloaded: job.total_payload_downloaded,
                uploaded: job.total_payload_uploaded,
                down_bps: 0,
                up_bps: 0,
                window_ms: 0,
            },
        };
        job.down_bps = next.down_bps;
        job.up_bps = next.up_bps;
        live.insert(job.id.0, next);
    }
    memory.rates = live;
    snapshot.jobs = jobs.into();
}

fn file_rows(driver: &Driver, id: TorrentId) -> Vec<FileRow> {
    driver
        .file_progress(id)
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(index, file)| FileRow {
            index,
            path: file.path,
            length: file.length,
            verified_bytes: file.verified_bytes,
            wanted: file.wanted,
            rank: file.rank,
            pad: driver.is_pad_file(id, index),
        })
        .collect()
}

fn peer_rows(driver: &Driver, id: TorrentId, geo: &mut GeoDb) -> Vec<PeerView> {
    driver
        .peer_status(id)
        .into_iter()
        .map(|peer| {
            let country = geo.lookup(peer.address.ip());
            PeerView {
                address: peer.address.to_string(),
                socket: peer.address,
                preference: peer.preference,
                client: peer.client,
                down_bps: peer.down_bps,
                up_bps: peer.up_bps,
                rtt_us: peer.rtt_us,
                hash_failures: peer.hash_failures,
                country,
                remote_choked: peer.remote_choked,
                remote_interested: peer.remote_interested,
                we_choke: peer.we_choke,
            }
        })
        .collect()
}

fn tracker_rows(driver: &Driver, id: TorrentId) -> Vec<TrackerView> {
    driver
        .tracker_status(id)
        .unwrap_or_default()
        .into_iter()
        .map(|tracker| TrackerView {
            label: tracker.label,
            // The driver's origin is a token; the words come from the file.
            origin: match tracker.origin {
                "original" => t("trackers.origin_original").to_string(),
                "added" => t("trackers.origin_added").to_string(),
                other => other.to_string(),
            },
            seeds: tracker.seeds,
            peers: tracker.peers,
            manual_due_s: tracker.manual_due_s,
            last_error: tracker.last_error,
        })
        .collect()
}

async fn dispatch(
    engine: &mut Engine,
    memory: &mut Memory,
    shared: &Shared,
    request: Request,
) -> bool {
    match request {
        Request::Command { seq, command } => {
            let stop = matches!(command, Command::Shutdown);
            let removed = match (&command, &*engine) {
                (Command::Remove { id, .. }, Engine::Live { driver, urls, .. }) => {
                    if UrlManager::is_url_id(*id) {
                        urls.key(*id).map(Removed::Url)
                    } else {
                        driver.info_hash(*id).map(Removed::Torrent)
                    }
                }
                _ => None,
            };
            let torrent = match &command {
                Command::Add { source, .. } => memory
                    .torrent_files
                    .iter()
                    .find(|(magnet, _)| magnet == source)
                    .map(|(_, bytes)| bytes.clone()),
                _ => None,
            };
            let result = run_command(engine, shared, &command, torrent.as_deref()).await;
            if let Ok(added) = &result {
                persist_after(engine, memory, &command, *added, removed);
            }
            push_ack(
                &mut memory.acks,
                Ack {
                    seq,
                    accepted: result.is_ok(),
                    message: result.err().unwrap_or_default(),
                    entries: Vec::new(),
                    magnet: None,
                    space: None,
                },
            );
            stop
        }
        Request::ListDir { seq, path } => {
            let (accepted, message, entries) = match list_dir(&path) {
                Ok(entries) => (true, String::new(), entries),
                Err(message) => (false, message, Vec::new()),
            };
            push_ack(
                &mut memory.acks,
                Ack {
                    seq,
                    accepted,
                    message,
                    entries,
                    magnet: None,
                    space: None,
                },
            );
            false
        }
        Request::ReadTorrent { seq, path } => {
            let (accepted, message, magnet) = match read_torrent(&path) {
                Ok((magnet, bytes)) => {
                    memory.torrent_files.retain(|(known, _)| *known != magnet);
                    if memory.torrent_files.len() >= TORRENT_FILE_CACHE {
                        memory.torrent_files.pop_front();
                    }
                    memory.torrent_files.push_back((magnet.clone(), bytes));
                    (true, String::new(), Some(magnet))
                }
                Err(message) => (false, message, None),
            };
            push_ack(
                &mut memory.acks,
                Ack {
                    seq,
                    accepted,
                    message,
                    entries: Vec::new(),
                    magnet,
                    space: None,
                },
            );
            false
        }
        Request::Space { seq, path } => {
            push_ack(
                &mut memory.acks,
                Ack {
                    seq,
                    accepted: true,
                    message: String::new(),
                    entries: Vec::new(),
                    magnet: None,
                    space: space::available_bytes(&path),
                },
            );
            false
        }
        Request::AllowLocal(allow) => {
            memory.allow_local = allow;
            if let Engine::Live { driver, .. } = engine {
                driver.set_allow_local_trackers(allow);
                if let Err(error) = save_settings(driver, memory) {
                    memory.note = Some(tf("note.session_not_saved", &[("detail", &error)]));
                }
            }
            false
        }
        Request::AllowLocalUrls(allow) => {
            memory.allow_local_urls = allow;
            if let Engine::Live { driver, urls, .. } = engine {
                urls.set_allow_local(allow);
                if let Err(error) = save_settings(driver, memory) {
                    memory.note = Some(tf("note.session_not_saved", &[("detail", &error)]));
                }
            }
            false
        }
        Request::SetEncryption(encryption) => {
            memory.encryption = encryption;
            if let Engine::Live { driver, .. } = engine {
                driver.set_peer_encryption(encryption);
                if let Err(error) = save_settings(driver, memory) {
                    memory.note = Some(tf("note.session_not_saved", &[("detail", &error)]));
                }
            }
            false
        }
        Request::SetPortMapping(on) => {
            memory.port_mapping = on;
            apply_port_mapping(engine, memory).await;
            if let Engine::Live { driver, .. } = engine
                && let Err(error) = save_settings(driver, memory)
            {
                memory.note = Some(tf("note.session_not_saved", &[("detail", &error)]));
            }
            false
        }
        Request::SetListenPort(port) => {
            memory.listen_port = port;
            if let Engine::Live { driver, .. } = engine {
                memory.note = Some(match save_settings(driver, memory) {
                    Ok(()) => tf("note.listen_port_restart", &[("port", &port)]),
                    Err(error) => tf("note.session_not_saved", &[("detail", &error)]),
                });
            }
            false
        }
        Request::ReloadTrackers => {
            memory.public_trackers = workflow::load_public_trackers(&memory.public_path);
            if let Engine::Live { driver, .. } = engine {
                driver.set_public_trackers(memory.public_trackers.clone());
            }
            false
        }
    }
}

/// `Ok(Some(id))` only for an accepted Add, so the new job can be saved.
/// `torrent` holds the `.torrent` bytes behind an Add's magnet when the Add
/// dialog read a file; the job is then added from them without BEP-9.
async fn run_command(
    engine: &mut Engine,
    shared: &Shared,
    command: &Command,
    torrent: Option<&[u8]>,
) -> Result<Option<TorrentId>, String> {
    let Engine::Live { driver, urls, .. } = engine else {
        return Err(t("worker.driver_not_started").into());
    };
    // Text from the URL manager and the driver is wrapped in a client
    // message, so the sentence around it comes from the messages file.
    let url_failed = |error: String| tf("worker.url_add_failed", &[("detail", &error)]);
    let failed = |error: String| tf("worker.command_failed", &[("detail", &error)]);
    match command {
        Command::AddUrl {
            url,
            destination,
            sha256,
        } => {
            return urls
                .add(url, destination, *sha256)
                .await
                .map(Some)
                .map_err(url_failed);
        }
        Command::Pause(id) if UrlManager::is_url_id(*id) => {
            return urls.pause(*id).await.map(|()| None).map_err(failed);
        }
        Command::Resume(id) if UrlManager::is_url_id(*id) => {
            return urls.resume(*id).map(|()| None).map_err(failed);
        }
        Command::Retry(id) => {
            if !UrlManager::is_url_id(*id) {
                return Err(t("worker.retry_url_only").into());
            }
            return urls.retry(*id).map(|()| None).map_err(failed);
        }
        Command::Cancel(id) => {
            if !UrlManager::is_url_id(*id) {
                return Err(t("worker.cancel_url_only").into());
            }
            return urls.cancel(*id).await.map(|()| None).map_err(failed);
        }
        Command::Remove { id, delete_data } if UrlManager::is_url_id(*id) => {
            return urls
                .remove(*id, *delete_data)
                .await
                .map(|_| None)
                .map_err(failed);
        }
        Command::SetLimits(limits) => urls.set_max_active(limits.download_jobs),
        _ => {}
    }
    let outcome = match command {
        Command::Add {
            source,
            destination,
            public_magnet_bootstrap,
        } => {
            let existing = if destination.is_dir() {
                ExistingData::Recheck
            } else if destination.exists() {
                return Err(t("worker.destination_not_dir").into());
            } else {
                ExistingData::Refuse
            };
            if let Some(bytes) = torrent {
                return driver
                    .add_torrent(bytes, destination, existing)
                    .await
                    .map(Some)
                    .map_err(|error| tf("worker.add_failed", &[("detail", &error.0)]));
            }
            // The job is listed at once, fetching metadata in the background;
            // the step loop saves its record when the metadata is installed.
            return driver
                .start_magnet_with_discovery(
                    source,
                    destination,
                    existing,
                    &[],
                    *public_magnet_bootstrap,
                )
                .map(Some)
                .map_err(|error| tf("worker.add_failed", &[("detail", &error.0)]));
        }
        Command::Pause(id) => driver.pause(*id).await,
        Command::Resume(id) => driver.resume(*id).await,
        Command::Remove { id, delete_data } => driver.remove(*id, *delete_data).await,
        Command::SetLimits(limits) => driver.set_limits(limits.clone()).await,
        Command::SetFileOrder { id, file_indices } => {
            driver.set_file_order(*id, file_indices.clone())
        }
        Command::SetFileOrderAutomatic(id) => driver.set_file_order_automatic(*id),
        Command::SetFileWanted {
            id,
            file_index,
            wanted,
        } => driver.set_file_wanted(*id, *file_index, *wanted),
        Command::Reannounce { id, tracker_index } => driver.reannounce(*id, *tracker_index).await,
        Command::SetPeerPreference {
            id,
            address,
            preference,
        } => driver.set_peer_preference(*id, *address, *preference).await,
        Command::Inspect { id, .. } => {
            *lock(&shared.selected) = *id;
            Ok(())
        }
        Command::Shutdown => Ok(()),
        Command::AddUrl { .. } | Command::Retry(_) | Command::Cancel(_) => {
            unreachable!("routed to the URL manager above")
        }
    };
    outcome
        .map(|()| None)
        .map_err(|error| failed(error.0.to_string()))
}

/// The router mapping state for the Settings dialog, without the external
/// address. The method names are protocol names and stay as written.
fn port_mapping_text(status: &portmap::Status) -> String {
    match status {
        portmap::Status::Off => t("settings.mapping_state_off").into(),
        portmap::Status::Probing => t("settings.mapping_state_probing").into(),
        portmap::Status::Mapped {
            method,
            port,
            external,
            ..
        } => {
            let method = match method {
                portmap::Method::NatPmp => "NAT-PMP",
                portmap::Method::Upnp => "UPnP",
            };
            let values: &[(&str, &dyn std::fmt::Display)] = &[("method", &method), ("port", port)];
            let private = external.is_some_and(|address| {
                !crate::geo::is_routable_ip(&std::net::IpAddr::V4(*address.ip()))
            });
            if private {
                tf("settings.mapping_state_mapped_private", values)
            } else {
                tf("settings.mapping_state_mapped", values)
            }
        }
        portmap::Status::Failed(reason) => {
            tf("settings.mapping_state_failed", &[("detail", reason)])
        }
    }
}

fn push_ack(acks: &mut Vec<Ack>, ack: Ack) {
    acks.push(ack);
    if acks.len() > 32 {
        acks.remove(0);
    }
}

fn list_dir(path: &Path) -> Result<Vec<String>, String> {
    let read = std::fs::read_dir(path).map_err(|_| t("add.browse_read_failed").to_string())?;
    let mut names = Vec::new();
    for entry in read {
        if names.len() == 200 {
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let kind = meta.file_type();
        if !kind.is_dir() || kind.is_symlink() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.contains('/') || name.contains('\0') {
            continue;
        }
        names.push(name.to_string());
    }
    names.sort_by(|left, right| {
        left.to_ascii_lowercase()
            .cmp(&right.to_ascii_lowercase())
            .then(left.cmp(right))
    });
    Ok(names)
}

fn read_torrent(path: &Path) -> Result<(String, Vec<u8>), String> {
    let torrent = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("torrent"));
    if !torrent {
        return Err(t("add.torrent_extension").into());
    }
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).map_err(|_| t("add.torrent_open_failed").to_string())?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| t("add.torrent_read_failed").to_string())?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(t("add.torrent_too_large").into());
    }
    let magnet = Driver::magnet_from_torrent_bytes(&bytes)
        .map_err(|error| tf("add.torrent_invalid", &[("detail", &error.0)]))?;
    Ok((magnet, bytes))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use templartorrent_core::model::{InspectorTab, PeerPreference};
    use templartorrent_rbit_adapter::driver::JobRecord;

    /// A message filled with `values`, cut before its first unfilled
    /// placeholder: the fixed text a note starts with.
    fn lead(key: &'static str, values: &[(&str, &dyn std::fmt::Display)]) -> String {
        let text = tf(key, values);
        match text.find('{') {
            Some(end) => text[..end].to_string(),
            None => text,
        }
    }

    fn rate_job(down_bps: u64) -> templartorrent_core::model::TorrentRow {
        templartorrent_core::model::TorrentRow {
            id: TorrentId(1),
            name: std::sync::Arc::from("transfer"),
            state: JobState::Downloading,
            wanted_bytes: 1_000,
            verified_bytes: 10,
            total_payload_downloaded: 10,
            total_payload_uploaded: 0,
            down_bps,
            up_bps: 0,
            connected_seeds: 0,
            connected_peers: 1,
            estimated_seeds: None,
            estimated_peers: None,
        }
    }

    fn config() -> WorkerConfig {
        WorkerConfig {
            limits: Limits::default(),
            allow_local_trackers: false,
            allow_local_urls: true,
            public_trackers: Vec::new(),
            public_tracker_path: PathBuf::from("public-trackers.txt"),
            geo_db_path: None,
            listen: "127.0.0.1:0".parse().unwrap(),
            session_dir: None,
        }
    }

    #[test]
    fn snapshot_sequence_alone_does_not_change_the_visible_fingerprint() {
        let mut left = SessionView::idle(Limits::default(), "trackers.txt".into());
        let mut right = left.clone();
        right.snapshot.sequence = 40;
        right.elapsed_s = 9;
        assert!(same_visible(&left, &right));
        let mut slow = SessionView::idle(Limits::default(), "trackers.txt".into());
        let mut fast = slow.clone();
        slow.snapshot.jobs = vec![rate_job(0)].into();
        fast.snapshot.jobs = vec![rate_job(2_000)].into();
        assert!(!same_visible(&slow, &fast));
        let tracker = TrackerView {
            label: "tracker".into(),
            origin: "original".into(),
            seeds: None,
            peers: None,
            manual_due_s: 20,
            last_error: None,
        };
        left.trackers.push(tracker.clone());
        right.trackers.push(tracker);
        left.elapsed_s = 3;
        right.elapsed_s = 4;
        assert!(!same_visible(&left, &right));
    }

    #[test]
    fn capture_without_a_user_geo_database_uses_the_bundled_one() {
        let mut memory = Memory {
            allow_local: false,
            allow_local_urls: false,
            public_trackers: Vec::new(),
            public_path: PathBuf::from("public-trackers.txt"),
            geo: GeoDb::new(None),
            acks: Vec::new(),
            note: None,
            rates: std::collections::HashMap::new(),
            rated_at: None,
            store: None,
            resume_flushed_at: Instant::now(),
            resume_verified: 0,
            listen_port: 0,
            encryption: PeerEncryption::default(),
            port_mapping: false,
            torrent_files: std::collections::VecDeque::new(),
        };
        let shared = Shared {
            view: Arc::new(Mutex::new(SessionView::idle(
                Limits::default(),
                "trackers.txt".into(),
            ))),
            selected: Arc::new(Mutex::new(None)),
            waker: Arc::new(Mutex::new(None)),
        };
        let mut engine = Engine::Down("test");
        let view = capture(&mut engine, &mut memory, &shared);
        assert!(view.peers.is_empty());
        assert!(
            view.geo_status.contains(crate::geo::BUNDLED_ATTRIBUTION),
            "{}",
            view.geo_status
        );
    }

    #[test]
    fn live_commands_ack_limits_and_reject_bad_add() {
        let mut handle = WorkerHandle::start(config()).unwrap();
        let limits = Limits {
            download_jobs: 2,
            ..Limits::default()
        };
        handle
            .try_request(Request::Command {
                seq: 1,
                command: Command::SetLimits(limits),
            })
            .unwrap();
        let ack = handle
            .wait_ack(1, Duration::from_secs(5))
            .expect("limits ack");
        assert!(ack.accepted, "{}", ack.message);
        assert_eq!(handle.view().limits.download_jobs, 2);
        handle
            .try_request(Request::Command {
                seq: 2,
                command: Command::Add {
                    source: "not a magnet".into(),
                    destination: std::env::temp_dir(),
                    public_magnet_bootstrap: false,
                },
            })
            .unwrap();
        let ack = handle.wait_ack(2, Duration::from_secs(5)).expect("add ack");
        assert!(!ack.accepted, "{}", ack.message);
        assert!(
            ack.message.contains("invalid magnet"),
            "driver should reject the magnet, got {}",
            ack.message
        );
        handle
            .try_request(Request::Command {
                seq: 3,
                command: Command::Remove {
                    id: TorrentId(9),
                    delete_data: true,
                },
            })
            .unwrap();
        let ack = handle
            .wait_ack(3, Duration::from_secs(5))
            .expect("remove ack");
        assert!(
            ack.message.contains("unknown job"),
            "delete reaches the driver, got {}",
            ack.message
        );
        handle
            .try_request(Request::Command {
                seq: 4,
                command: Command::SetPeerPreference {
                    id: TorrentId(1),
                    address: "127.0.0.1:1".parse().unwrap(),
                    preference: PeerPreference::Avoid,
                },
            })
            .unwrap();
        let ack = handle
            .wait_ack(4, Duration::from_secs(5))
            .expect("preference ack");
        assert!(
            ack.message.contains("unknown job"),
            "preference reaches the driver, got {}",
            ack.message
        );
        handle
            .try_request(Request::Command {
                seq: 5,
                command: Command::Inspect {
                    id: None,
                    tab: InspectorTab::Details,
                    visible_range: 0..0,
                },
            })
            .unwrap();
        let ack = handle
            .wait_ack(5, Duration::from_secs(5))
            .expect("inspect ack");
        assert!(ack.accepted, "{}", ack.message);
        handle.shutdown();
    }

    /// Add returns as soon as the magnet is admitted: the row is listed while
    /// its tracker is still silent, and later commands are not held behind it.
    #[test]
    fn magnet_add_is_listed_at_once_and_does_not_block_other_commands() {
        // A loopback "tracker" that accepts and never answers.
        let tracker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tracker.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in tracker.incoming().flatten() {
                held.push(stream);
            }
        });
        let destination = std::env::temp_dir().join(format!(
            "templartorrent-worker-pending-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir(&destination);
        let mut handle = WorkerHandle::start(WorkerConfig {
            allow_local_trackers: true,
            ..config()
        })
        .unwrap();
        let magnet = format!(
            "magnet:?xt=urn:btih:{}&dn=Pending%20Fixture&tr=http%3A%2F%2F127.0.0.1%3A{port}%2Fannounce",
            "ab".repeat(20)
        );
        let started = Instant::now();
        handle
            .try_request(Request::Command {
                seq: 1,
                command: Command::Add {
                    source: magnet,
                    destination: destination.clone(),
                    public_magnet_bootstrap: false,
                },
            })
            .unwrap();
        let ack = handle.wait_ack(1, Duration::from_secs(5)).expect("add ack");
        assert!(ack.accepted, "{}", ack.message);
        // The tracker holds its announce for ANNOUNCE_TIMEOUT (10 s).
        assert!(started.elapsed() < Duration::from_secs(5));
        let row = loop {
            let view = handle.view();
            if let Some(row) = view
                .snapshot
                .jobs
                .iter()
                .find(|job| &*job.name == "Pending Fixture")
            {
                break row.clone();
            }
            assert!(started.elapsed() < Duration::from_secs(5), "row not listed");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(row.state, JobState::Metadata);
        assert_eq!(row.wanted_bytes, 0);
        handle
            .try_request(Request::Command {
                seq: 2,
                command: Command::SetLimits(Limits {
                    download_jobs: 2,
                    ..Limits::default()
                }),
            })
            .unwrap();
        let ack = handle
            .wait_ack(2, Duration::from_secs(5))
            .expect("limits ack");
        assert!(ack.accepted, "{}", ack.message);
        assert!(started.elapsed() < Duration::from_secs(5));
        handle
            .try_request(Request::Command {
                seq: 3,
                command: Command::Remove {
                    id: row.id,
                    delete_data: true,
                },
            })
            .unwrap();
        let ack = handle
            .wait_ack(3, Duration::from_secs(5))
            .expect("remove ack");
        assert!(ack.accepted, "{}", ack.message);
        assert!(started.elapsed() < Duration::from_secs(5));
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle
            .view()
            .snapshot
            .jobs
            .iter()
            .any(|job| job.id == row.id)
        {
            assert!(Instant::now() < deadline, "removed row still listed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!destination.exists());
        handle.shutdown();
    }

    #[test]
    fn restores_saved_jobs_and_settings_then_removes_the_record() {
        let base = std::env::temp_dir().join(format!(
            "templartorrent-worker-restore-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("job/owned-series")).unwrap();
        let data: Vec<u8> = (0..70_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let torrent = rbit::TorrentBuilder::new("owned-series")
            .version(rbit::TorrentVersion::V1)
            .piece_length(16384)
            .add_file("Episode02.bin", data[..20_000].to_vec())
            .add_file("Episode01.bin", data[20_000..].to_vec())
            .build()
            .unwrap();
        std::fs::write(base.join("job/owned-series/Episode02.bin"), &data[..20_000]).unwrap();
        std::fs::write(base.join("job/owned-series/Episode01.bin"), &data[20_000..]).unwrap();
        let meta = rbit::Metainfo::from_bytes(&torrent).unwrap();
        let rbit::InfoHash::V1(hash) = meta.info_hash else {
            panic!("fixture must be v1");
        };
        let store = SessionStore::new(base.join("session"));
        store
            .save_settings(&Settings {
                limits: Limits {
                    download_jobs: 2,
                    ..Limits::default()
                },
                allow_local_trackers: true,
                listen_port: 0,
                encryption: PeerEncryption::default(),
                port_mapping: false,
                allow_local_urls: false,
            })
            .unwrap();
        store
            .save_job(&JobRecord {
                info_hash: hash,
                torrent,
                destination: base.join("job"),
                paused: false,
                public_bootstrap: false,
                wanted: vec![true, true],
                order: vec![1, 0],
                tiers: Vec::new(),
                trackers: Vec::new(),
                preferences: vec![("10.0.0.9".parse().unwrap(), PeerPreference::Block)],
            })
            .unwrap();
        // A bad record sits beside it and must only produce a note.
        std::fs::write(
            store.dir().join(format!("{}.job", "ab".repeat(20))),
            "nonsense",
        )
        .unwrap();
        let mut handle = WorkerHandle::start(WorkerConfig {
            session_dir: Some(base.join("session")),
            ..config()
        })
        .unwrap();
        let start = Instant::now();
        let view = loop {
            let view = handle.view();
            if view.snapshot.jobs.len() == 1 && view.note.is_some() {
                break view;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "job never restored: {:?}",
                view.note
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let job = view.snapshot.jobs[0].clone();
        assert_eq!(&*job.name, "owned-series");
        assert!(
            matches!(job.state, JobState::Complete | JobState::Seeding),
            "{:?}",
            job.state
        );
        assert_eq!(job.verified_bytes, 70_000);
        assert_eq!(view.limits.download_jobs, 2);
        assert!(view.allow_local_trackers);
        let note = view.note.clone().unwrap();
        let restored = lead(
            "note.restored_skipped",
            &[("restored", &1), ("skipped", &1)],
        );
        assert!(note.contains(&restored), "{note}");
        handle
            .try_request(Request::Command {
                seq: 1,
                command: Command::SetLimits(Limits {
                    download_jobs: 4,
                    ..Limits::default()
                }),
            })
            .unwrap();
        let ack = handle.wait_ack(1, Duration::from_secs(5)).unwrap();
        assert!(ack.accepted, "{}", ack.message);
        assert_eq!(
            store.load_settings().unwrap().unwrap().limits.download_jobs,
            4
        );
        handle
            .try_request(Request::Command {
                seq: 2,
                command: Command::Remove {
                    id: job.id,
                    delete_data: false,
                },
            })
            .unwrap();
        let ack = handle.wait_ack(2, Duration::from_secs(5)).unwrap();
        assert!(ack.accepted, "{}", ack.message);
        let (records, _) = store.load_jobs();
        assert!(records.is_empty(), "the record is gone after removal");
        assert!(base.join("job/owned-series/Episode01.bin").exists());
        handle.shutdown();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn shutdown_writes_a_resume_bitmap_the_next_launch_can_refuse() {
        let base = std::env::temp_dir().join(format!(
            "templartorrent-worker-resume-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("job/owned-series")).unwrap();
        let data: Vec<u8> = (0..70_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let torrent = rbit::TorrentBuilder::new("owned-series")
            .version(rbit::TorrentVersion::V1)
            .piece_length(16384)
            .add_file("Episode02.bin", data[..20_000].to_vec())
            .add_file("Episode01.bin", data[20_000..].to_vec())
            .build()
            .unwrap();
        std::fs::write(base.join("job/owned-series/Episode02.bin"), &data[..20_000]).unwrap();
        std::fs::write(base.join("job/owned-series/Episode01.bin"), &data[20_000..]).unwrap();
        let meta = rbit::Metainfo::from_bytes(&torrent).unwrap();
        let rbit::InfoHash::V1(hash) = meta.info_hash else {
            panic!("fixture must be v1");
        };
        let store = SessionStore::new(base.join("session"));
        store
            .save_job(&JobRecord {
                info_hash: hash,
                torrent,
                destination: base.join("job"),
                paused: false,
                public_bootstrap: false,
                wanted: vec![true, true],
                order: vec![1, 0],
                tiers: Vec::new(),
                trackers: Vec::new(),
                preferences: Vec::new(),
            })
            .unwrap();
        let mut handle = WorkerHandle::start(WorkerConfig {
            session_dir: Some(base.join("session")),
            ..config()
        })
        .unwrap();
        let start = Instant::now();
        loop {
            let view = handle.view();
            if view.snapshot.jobs.len() == 1
                && matches!(
                    view.snapshot.jobs[0].state,
                    JobState::Complete | JobState::Seeding
                )
            {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "job never restored: {:?}",
                view.note
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        handle.shutdown();
        let resume_path = store
            .dir()
            .join(format!("{}.resume", crate::persist::hex(&hash)));
        let text = std::fs::read_to_string(&resume_path).unwrap();
        assert!(text.starts_with("templartorrent-resume 1\n"), "{text}");
        assert!(text.contains("barrier 1\n"), "{text}");
        assert!(
            text.contains("bits 1f\n"),
            "five verified pieces, got {text}"
        );

        let mut handle = WorkerHandle::start(WorkerConfig {
            session_dir: Some(base.join("session")),
            ..config()
        })
        .unwrap();
        let start = Instant::now();
        let note = loop {
            let view = handle.view();
            if view.snapshot.jobs.len() == 1
                && matches!(
                    view.snapshot.jobs[0].state,
                    JobState::Complete | JobState::Seeding
                )
                && view
                    .note
                    .as_ref()
                    .is_some_and(|note| note.contains(&tf("note.restored", &[("restored", &1)])))
            {
                break view.note.unwrap();
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "trusted launch did not restore: {:?}",
                handle.view().note
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            !note.contains(&lead("note.resume_not_trusted", &[]))
                && !note.contains(&lead("note.resume_ignored", &[])),
            "{note}"
        );
        handle.shutdown();

        let text = std::fs::read_to_string(&resume_path)
            .unwrap()
            .replace("barrier 1", "barrier 0");
        std::fs::write(&resume_path, text).unwrap();
        let mut handle = WorkerHandle::start(WorkerConfig {
            session_dir: Some(base.join("session")),
            ..config()
        })
        .unwrap();
        let start = Instant::now();
        let note = loop {
            let view = handle.view();
            if view.snapshot.jobs.len() == 1
                && view
                    .note
                    .as_ref()
                    .is_some_and(|note| note.contains("barrier"))
            {
                break view.note.unwrap();
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "barrier refusal was not reported: {:?}",
                handle.view().note
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(note.contains("no durability barrier"), "{note}");
        assert!(
            matches!(
                handle.view().snapshot.jobs[0].state,
                JobState::Complete | JobState::Seeding
            ),
            "a refused bitmap still rechecks"
        );
        let id = handle.view().snapshot.jobs[0].id;
        handle
            .try_request(Request::Command {
                seq: 1,
                command: Command::Remove {
                    id,
                    delete_data: false,
                },
            })
            .unwrap();
        let ack = handle.wait_ack(1, Duration::from_secs(5)).unwrap();
        assert!(ack.accepted, "{}", ack.message);
        assert!(!resume_path.exists(), "removal deletes the bitmap");
        handle.shutdown();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A URL job goes through the same command channel as a torrent: it is
    /// added, shows in the snapshot, is saved as a record, restored on the
    /// next launch, and its record goes when it is removed.
    #[test]
    fn url_jobs_are_added_saved_restored_and_removed_through_the_worker() {
        use templartorrent_rbit_adapter::http::fixture::{self, Behaviour};
        let base =
            std::env::temp_dir().join(format!("templartorrent-worker-url-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 13 % 251) as u8).collect();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let server = runtime.block_on(fixture::start(Behaviour {
            body: data.clone(),
            ..Behaviour::default()
        }));
        let url = server.url("/worker.bin");
        let server_task = std::thread::spawn(move || {
            runtime.block_on(async {
                let _server = server;
                std::future::pending::<()>().await;
            });
        });
        let store = SessionStore::new(base.join("session"));
        let mut handle = WorkerHandle::start(WorkerConfig {
            session_dir: Some(base.join("session")),
            ..config()
        })
        .unwrap();
        handle
            .try_request(Request::Command {
                seq: 1,
                command: Command::AddUrl {
                    url: url.clone(),
                    destination: base.join("files"),
                    sha256: None,
                },
            })
            .unwrap();
        let ack = handle.wait_ack(1, Duration::from_secs(5)).expect("add ack");
        assert!(ack.accepted, "{}", ack.message);
        let start = Instant::now();
        let view = loop {
            let view = handle.view();
            if view
                .snapshot
                .jobs
                .iter()
                .any(|job| job.state == JobState::Complete)
            {
                break view;
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "never completed: {:?}",
                view.snapshot.jobs
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let job = view.snapshot.jobs[0].clone();
        assert!(UrlManager::is_url_id(job.id));
        assert_eq!(&*job.name, "worker.bin");
        assert_eq!(job.wanted_bytes, data.len() as u64);
        assert_eq!(std::fs::read(base.join("files/worker.bin")).unwrap(), data);
        handle.set_selected(Some(job.id));
        let start = Instant::now();
        loop {
            let view = handle.view();
            if let Some(detail) = view.url_detail {
                assert_eq!(detail.state, JobState::Complete);
                assert_eq!(
                    view.destination.as_deref(),
                    Some(detail.destination.as_str())
                );
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "no URL detail");
            std::thread::sleep(Duration::from_millis(10));
        }
        let (records, errors) = store.load_urls();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(records.len(), 1);
        assert!(records[0].complete);
        // Retry and Cancel refuse torrent ids and a complete URL job.
        handle
            .try_request(Request::Command {
                seq: 2,
                command: Command::Retry(TorrentId(1)),
            })
            .unwrap();
        let ack = handle.wait_ack(2, Duration::from_secs(5)).unwrap();
        assert!(
            !ack.accepted && ack.message.contains("only URL"),
            "{}",
            ack.message
        );
        handle
            .try_request(Request::Command {
                seq: 3,
                command: Command::Cancel(job.id),
            })
            .unwrap();
        let ack = handle.wait_ack(3, Duration::from_secs(5)).unwrap();
        assert!(
            !ack.accepted && ack.message.contains("complete"),
            "{}",
            ack.message
        );
        handle.try_request(Request::AllowLocalUrls(false)).unwrap();
        let start = Instant::now();
        while handle.view().allow_local_urls {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        handle.shutdown();
        assert!(!store.load_settings().unwrap().unwrap().allow_local_urls);

        let mut handle = WorkerHandle::start(WorkerConfig {
            session_dir: Some(base.join("session")),
            ..config()
        })
        .unwrap();
        let start = Instant::now();
        let restored = loop {
            let view = handle.view();
            if view.snapshot.jobs.len() == 1 {
                break view.snapshot.jobs[0].clone();
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{:?}", view.note);
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(restored.state, JobState::Complete);
        assert_eq!(&*restored.name, "worker.bin");
        // The saved allowance was off, so a second add of the loopback URL is refused.
        handle
            .try_request(Request::Command {
                seq: 1,
                command: Command::AddUrl {
                    url: url.clone(),
                    destination: base.join("other"),
                    sha256: None,
                },
            })
            .unwrap();
        let ack = handle.wait_ack(1, Duration::from_secs(5)).unwrap();
        assert!(
            !ack.accepted && ack.message.contains("local or private"),
            "{}",
            ack.message
        );
        handle
            .try_request(Request::Command {
                seq: 2,
                command: Command::Remove {
                    id: restored.id,
                    delete_data: false,
                },
            })
            .unwrap();
        let ack = handle.wait_ack(2, Duration::from_secs(5)).unwrap();
        assert!(ack.accepted, "{}", ack.message);
        assert!(store.load_urls().0.is_empty(), "the record is gone");
        assert!(
            base.join("files/worker.bin").exists(),
            "data kept by default"
        );
        handle.shutdown();
        drop(server_task);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The consent is saved and reapplied on the next launch. A loopback
    /// listener cannot be reached through the router, so the driver refuses
    /// it before sending anything (or finds no gateway at all).
    #[test]
    fn port_mapping_consent_is_saved_and_applied_on_launch() {
        let base = std::env::temp_dir().join(format!(
            "templartorrent-worker-portmap-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let store = SessionStore::new(base.join("session"));
        let start_worker = || {
            WorkerHandle::start(WorkerConfig {
                session_dir: Some(base.join("session")),
                ..config()
            })
            .unwrap()
        };
        let wait = |handle: &WorkerHandle, done: &dyn Fn(&SessionView) -> bool| {
            let start = Instant::now();
            loop {
                let view = handle.view();
                if done(&view) {
                    return view;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "{:?} {:?}",
                    view.port_mapping,
                    view.port_mapping_status
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let failed = lead("settings.mapping_state_failed", &[]);
        let refused =
            |view: &SessionView| view.port_mapping && view.port_mapping_status.starts_with(&failed);
        let off = t("settings.mapping_state_off");
        let mut handle = start_worker();
        let view = wait(&handle, &|view| {
            matches!(view.listen, ListenState::Bound(_))
        });
        assert!(!view.port_mapping, "off by default");
        assert_eq!(view.port_mapping_status, off);
        handle.try_request(Request::SetPortMapping(true)).unwrap();
        let view = wait(&handle, &refused);
        assert!(
            view.port_mapping_status.contains("listener is not bound")
                || view.port_mapping_status.contains("no IPv4"),
            "{}",
            view.port_mapping_status
        );
        assert!(store.load_settings().unwrap().unwrap().port_mapping);
        handle.shutdown();

        let mut handle = start_worker();
        wait(&handle, &refused);
        handle.try_request(Request::SetPortMapping(false)).unwrap();
        wait(&handle, &|view| {
            !view.port_mapping && view.port_mapping_status == off
        });
        assert!(!store.load_settings().unwrap().unwrap().port_mapping);
        handle.shutdown();
        let _ = std::fs::remove_dir_all(&base);
    }
}
