//! HTTP and HTTPS file downloads: URL jobs in the same queue as torrents
//! (DESIGN.md section 11).
//!
//! [`UrlManager`] owns every URL job on the driver's runtime thread. Each
//! running job is one Tokio task ([`job`]) that probes the URL with a Range
//! request, splits a ranged body into a few parts on their own connections,
//! charges every read buffer to the session's payload budget and every byte
//! to the session's download token bucket, writes to `<name>.part`, and
//! finishes with an optional SHA-256 check, a sync and a rename. The manager
//! is the only writer of a job's [`UrlRecord`]; the task reports through a
//! channel, and offsets reach the record only after the file was synced, so a
//! persisted offset never exceeds durable bytes.
//!
//! Nothing here is a torrent: no peer, tracker or DHT state is touched. The
//! shared pieces are the rustls configuration, the address admission rules,
//! the payload budget, the download rate and the id space.

pub mod fixture;
mod job;
pub mod transport;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use rustls::ClientConfig;
use sha1::{Digest, Sha1};
use templartorrent_core::{
    bandwidth::SharedBucket,
    budget::ByteBudget,
    model::{JobState, TorrentId, TorrentRow, clean_label},
};
use tokio::{sync::mpsc, task::JoinHandle};

pub use transport::{IDLE_TIMEOUT, MAX_REDIRECTS, MAX_URL_LEN, ParsedUrl, parse_url};

/// URL job ids live above every torrent id the engine will allocate.
pub const URL_ID_BIT: u64 = 1 << 63;
/// Most URLs one Add may carry.
pub const MAX_BATCH: usize = 16;
/// Parallel connections for a ranged body, by default and at most.
pub const DEFAULT_CONNECTIONS: u8 = 4;
pub const MAX_CONNECTIONS: u8 = 8;
/// A ranged body shorter than this is one connection.
pub const SPLIT_THRESHOLD: u64 = 1024 * 1024;
/// Read buffer per connection; each read reserves this from the payload budget.
pub const READ_CHUNK: usize = 64 * 1024;
/// Consecutive attempts without progress before a part gives up.
pub const MAX_ATTEMPTS: u32 = 5;
/// Backoff between attempts: this doubled per attempt, capped at 8 s.
pub const RETRY_BASE: Duration = Duration::from_millis(500);
pub const RETRY_CAP: Duration = Duration::from_secs(8);

/// A strong validator from the first response (RFC 9110 section 8.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validator {
    Etag(String),
    LastModified(String),
}

impl Validator {
    /// A quoted, non-weak ETag wins; otherwise `Last-Modified` when the
    /// response `Date` is at least 60 s later (section 8.8.2.2). A weak ETag
    /// or an undated response gives no validator and no resume.
    pub fn from_head(head: &transport::ResponseHead) -> Option<Self> {
        if let Some(etag) = head.header("etag") {
            let etag = etag.trim();
            return strong_etag(etag).then(|| Self::Etag(etag.to_string()));
        }
        let modified = head.header("last-modified")?.trim();
        let date = head.header("date")?.trim();
        let modified_s = parse_http_date(modified)?;
        let date_s = parse_http_date(date)?;
        (date_s - modified_s >= 60).then(|| Self::LastModified(modified.to_string()))
    }

    pub fn value(&self) -> &str {
        match self {
            Self::Etag(value) | Self::LastModified(value) => value,
        }
    }

    pub fn header_name(&self) -> &'static str {
        match self {
            Self::Etag(_) => "etag",
            Self::LastModified(_) => "last-modified",
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Etag(_) => "ETag",
            Self::LastModified(_) => "Last-Modified",
        }
    }
}

fn strong_etag(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2
        && bytes.first() == Some(&b'"')
        && bytes.last() == Some(&b'"')
        && bytes[1..bytes.len() - 1]
            .iter()
            .all(|b| *b == 0x21 || (0x23..=0x7e).contains(b))
}

/// IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) to seconds since the Unix
/// epoch. Other HTTP date forms are obsolete and give `None`.
pub fn parse_http_date(text: &str) -> Option<i64> {
    let mut parts = text.split_whitespace();
    let weekday = parts.next()?;
    if !weekday.ends_with(',') || weekday.len() != 4 {
        return None;
    }
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if parts.next()? != "GMT" || parts.next().is_some() {
        return None;
    }
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Days from civil, Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// One contiguous byte range of the file and how much of it has been
/// written. `end` is exclusive; `u64::MAX` means the size is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::len_without_is_empty)]
pub struct PartRecord {
    pub start: u64,
    pub end: u64,
    pub done: u64,
}

impl PartRecord {
    /// Bytes this part covers, or `None` while the size is unknown.
    pub fn len(&self) -> Option<u64> {
        (self.end != u64::MAX).then(|| self.end - self.start)
    }

    pub fn is_complete(&self) -> bool {
        self.len().is_some_and(|len| self.done >= len)
    }
}

/// Everything the session store keeps about one URL job. Produced and
/// consumed by [`UrlManager`]; the client crate only encodes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlRecord {
    /// SHA-1 of the URL and the destination, the record's file name.
    pub key: [u8; 20],
    pub url: String,
    pub destination: PathBuf,
    /// Known after the first response.
    pub name: Option<String>,
    pub size: Option<u64>,
    /// The server answered a Range request with 206 (resume and splitting
    /// are possible).
    pub ranged: bool,
    pub validator: Option<Validator>,
    pub parts: Vec<PartRecord>,
    pub sha256: Option<[u8; 32]>,
    pub paused: bool,
    pub complete: bool,
    pub error: Option<String>,
    pub connections: u8,
}

impl UrlRecord {
    pub fn new(url: &str, destination: PathBuf, sha256: Option<[u8; 32]>) -> Self {
        let mut hasher = Sha1::new();
        hasher.update(url.as_bytes());
        hasher.update([0]);
        hasher.update(destination.as_os_str().as_encoded_bytes());
        Self {
            key: hasher.finalize().into(),
            url: url.to_string(),
            destination,
            name: None,
            size: None,
            ranged: false,
            validator: None,
            parts: Vec::new(),
            sha256,
            paused: false,
            complete: false,
            error: None,
            connections: DEFAULT_CONNECTIONS,
        }
    }

    pub fn done_bytes(&self) -> u64 {
        self.parts.iter().map(|part| part.done).sum()
    }

    pub fn final_path(&self) -> Option<PathBuf> {
        self.name.as_ref().map(|name| self.destination.join(name))
    }

    pub fn part_path(&self) -> Option<PathBuf> {
        self.name
            .as_ref()
            .map(|name| self.destination.join(format!("{name}.part")))
    }

    /// Host and last path segment, until the server names the file.
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => match parse_url(&self.url) {
                Ok(parsed) => {
                    let segment = parsed.last_segment();
                    if segment.is_empty() {
                        parsed.host
                    } else {
                        format!(
                            "{} ({})",
                            sanitize_name(&segment).unwrap_or(segment),
                            parsed.host
                        )
                    }
                }
                Err(_) => "download".into(),
            },
        }
    }
}

/// Plan the parts of a job once its size and range support are known.
pub fn plan_parts(size: Option<u64>, split: bool, connections: u8) -> Vec<PartRecord> {
    let Some(size) = size else {
        return vec![PartRecord {
            start: 0,
            end: u64::MAX,
            done: 0,
        }];
    };
    let connections = u64::from(connections.clamp(1, MAX_CONNECTIONS));
    if !split || size < SPLIT_THRESHOLD || connections == 1 {
        return vec![PartRecord {
            start: 0,
            end: size,
            done: 0,
        }];
    }
    let count = connections.min(size.div_ceil(SPLIT_THRESHOLD)).max(1);
    let base = size / count;
    let extra = size % count;
    let mut parts = Vec::with_capacity(count as usize);
    let mut start = 0;
    for index in 0..count {
        let len = base + u64::from(index < extra);
        parts.push(PartRecord {
            start,
            end: start + len,
            done: 0,
        });
        start += len;
    }
    parts
}

/// A single path component, no control characters, not `.` or `..`, at
/// most 255 bytes. `None` when nothing usable remains.
pub fn sanitize_name(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let mut name: String = clean_label(last, 255)
        .chars()
        .filter(|c| *c != '\0')
        .collect();
    name = name.trim().to_string();
    while name.len() > 255 {
        name.pop();
    }
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(name)
}

/// `Content-Disposition` filename (RFC 6266, `filename*` preferred), then
/// the URL's last segment, then `download`.
pub fn derive_name(head: &transport::ResponseHead, url: &ParsedUrl) -> String {
    if let Some(value) = head.header("content-disposition")
        && let Some(name) = disposition_filename(value).and_then(|name| sanitize_name(&name))
    {
        return name;
    }
    if let Some(name) = sanitize_name(&url.last_segment()) {
        return name;
    }
    "download".into()
}

fn disposition_filename(value: &str) -> Option<String> {
    let mut plain = None;
    for parameter in value.split(';').skip(1) {
        let (key, raw) = parameter.split_once('=')?;
        let key = key.trim();
        let raw = raw.trim();
        if key.eq_ignore_ascii_case("filename*") {
            let encoded = raw
                .strip_prefix("UTF-8''")
                .or_else(|| raw.strip_prefix("utf-8''"))?;
            return Some(transport::percent_decode(encoded));
        }
        if key.eq_ignore_ascii_case("filename") {
            let unquoted = raw
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .unwrap_or(raw);
            plain = Some(unquoted.replace("\\\"", "\""));
        }
    }
    plain
}

/// What the task reports back to the manager.
pub(crate) enum Event {
    Described {
        id: TorrentId,
        name: String,
        size: Option<u64>,
        ranged: bool,
        validator: Option<Validator>,
        parts: Vec<PartRecord>,
    },
    Note {
        id: TorrentId,
        text: String,
    },
    Finished {
        id: TorrentId,
        result: Result<u64, String>,
        /// The temporary file was synced after the last write, so the live
        /// offsets may replace the record's.
        synced: bool,
    },
}

/// Live counters a running task updates; the manager reads them for rows
/// and checkpoints.
pub(crate) struct Progress {
    /// Bytes written per part, updated after each `write_at`.
    pub parts: Mutex<Vec<u64>>,
    pub active: AtomicUsize,
    pub received: AtomicU64,
    /// 0 transferring, 1 verifying the digest, 2 finishing.
    pub phase: AtomicU8,
    pub file: Mutex<Option<Arc<std::fs::File>>>,
}

impl Progress {
    fn new(parts: &[PartRecord]) -> Self {
        Self {
            parts: Mutex::new(parts.iter().map(|part| part.done).collect()),
            active: AtomicUsize::new(0),
            received: AtomicU64::new(0),
            phase: AtomicU8::new(0),
            file: Mutex::new(None),
        }
    }

    pub(crate) fn offsets(&self) -> Vec<u64> {
        lock(&self.parts).clone()
    }
}

enum Phase {
    Queued,
    Running {
        task: JoinHandle<()>,
        progress: Arc<Progress>,
    },
    Paused,
    Complete,
    Failed(String),
}

struct UrlJob {
    record: UrlRecord,
    phase: Phase,
    note: Option<String>,
    /// The record changed since the caller last took it.
    dirty: bool,
}

/// What the inspector shows for a URL job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlDetail {
    pub url: String,
    pub destination: String,
    pub file: Option<String>,
    pub size: Option<u64>,
    pub done: u64,
    pub ranged: bool,
    pub validator: Option<String>,
    pub parts: Vec<PartRecord>,
    pub connections: usize,
    pub sha256: Option<String>,
    pub error: Option<String>,
    pub note: Option<String>,
    pub state: JobState,
}

/// Settings the manager takes from the session.
pub struct UrlConfig {
    pub budget: ByteBudget,
    pub rate: SharedBucket,
    /// The instant the rate bucket's timestamps count from.
    pub origin: Instant,
    pub allow_local: bool,
    /// Replaces the public roots; tests trust a local CA.
    pub tls: Option<Arc<ClientConfig>>,
    /// Per-read deadline; production uses [`IDLE_TIMEOUT`].
    pub idle: Duration,
    /// Running URL jobs at once (the `Download jobs` setting).
    pub max_active: usize,
}

pub struct UrlManager {
    jobs: HashMap<TorrentId, UrlJob>,
    order: Vec<TorrentId>,
    next: u64,
    config: UrlConfig,
    events: mpsc::Receiver<Event>,
    sender: mpsc::Sender<Event>,
}

const EVENT_CAP: usize = 256;

impl UrlManager {
    pub fn new(config: UrlConfig) -> Self {
        let (sender, events) = mpsc::channel(EVENT_CAP);
        Self {
            jobs: HashMap::new(),
            order: Vec::new(),
            next: 1,
            config,
            events,
            sender,
        }
    }

    pub fn is_url_id(id: TorrentId) -> bool {
        id.0 & URL_ID_BIT != 0
    }

    pub fn contains(&self, id: TorrentId) -> bool {
        self.jobs.contains_key(&id)
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub fn set_allow_local(&mut self, allow: bool) {
        self.config.allow_local = allow;
    }

    pub fn set_max_active(&mut self, max_active: usize) {
        self.config.max_active = max_active;
    }

    fn allocate(&mut self) -> TorrentId {
        let id = TorrentId(URL_ID_BIT | self.next);
        self.next += 1;
        id
    }

    /// Validate the URL and destination, resolve and classify the host
    /// (nothing is sent), and queue the job. `destination` is created when
    /// missing, like a torrent destination.
    pub async fn add(
        &mut self,
        url: &str,
        destination: &Path,
        sha256: Option<[u8; 32]>,
    ) -> Result<TorrentId, String> {
        let parsed = parse_url(url).map_err(str::to_string)?;
        transport::admit_url(&parsed, self.config.allow_local)
            .await
            .map_err(str::to_string)?;
        let destination = prepare_destination(destination)?;
        let record = UrlRecord::new(&parsed.url, destination, sha256);
        if self.jobs.values().any(|job| job.record.key == record.key) {
            return Err("that URL is already queued for that folder".into());
        }
        let id = self.allocate();
        self.jobs.insert(
            id,
            UrlJob {
                record,
                phase: Phase::Queued,
                note: None,
                dirty: true,
            },
        );
        self.order.push(id);
        Ok(id)
    }

    /// Reinstall a saved job without contacting the server.
    pub fn restore(&mut self, record: UrlRecord) -> Result<TorrentId, String> {
        parse_url(&record.url).map_err(str::to_string)?;
        if !record.destination.is_absolute() {
            return Err("destination must be absolute".into());
        }
        if self.jobs.values().any(|job| job.record.key == record.key) {
            return Err("duplicate URL record".into());
        }
        let phase = if record.complete {
            Phase::Complete
        } else if let Some(error) = &record.error {
            Phase::Failed(error.clone())
        } else if record.paused {
            Phase::Paused
        } else {
            Phase::Queued
        };
        let id = self.allocate();
        self.jobs.insert(
            id,
            UrlJob {
                record,
                phase,
                note: None,
                dirty: false,
            },
        );
        self.order.push(id);
        Ok(id)
    }

    /// Drain task events and admit queued jobs. Call between driver steps.
    pub fn tick(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.apply(event);
        }
        let running = self
            .jobs
            .values()
            .filter(|job| matches!(job.phase, Phase::Running { .. }))
            .count();
        let mut slots = self.config.max_active.saturating_sub(running);
        let order = self.order.clone();
        for id in order {
            if slots == 0 {
                break;
            }
            if self
                .jobs
                .get(&id)
                .is_some_and(|job| matches!(job.phase, Phase::Queued))
            {
                self.start(id);
                slots -= 1;
            }
        }
    }

    fn start(&mut self, id: TorrentId) {
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        let progress = Arc::new(Progress::new(&job.record.parts));
        let context = job::Context {
            id,
            record: job.record.clone(),
            progress: Arc::clone(&progress),
            events: self.sender.clone(),
            budget: self.config.budget.clone(),
            rate: self.config.rate.clone(),
            origin: self.config.origin,
            allow_local: self.config.allow_local,
            tls: self.config.tls.clone(),
            idle: self.config.idle,
        };
        let task = tokio::spawn(job::run(context));
        job.phase = Phase::Running { task, progress };
        job.record.paused = false;
        job.record.error = None;
        job.dirty = true;
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Described {
                id,
                name,
                size,
                ranged,
                validator,
                parts,
            } => {
                if let Some(job) = self.jobs.get_mut(&id) {
                    job.record.name = Some(name);
                    job.record.size = size;
                    job.record.ranged = ranged;
                    job.record.validator = validator;
                    job.record.parts = parts;
                    job.dirty = true;
                }
            }
            Event::Note { id, text } => {
                if let Some(job) = self.jobs.get_mut(&id) {
                    job.note = Some(text);
                }
            }
            Event::Finished { id, result, synced } => {
                let Some(job) = self.jobs.get_mut(&id) else {
                    return;
                };
                if let Phase::Running { progress, .. } = &job.phase
                    && synced
                {
                    let offsets = progress.offsets();
                    for (part, done) in job.record.parts.iter_mut().zip(offsets) {
                        part.done = done;
                    }
                }
                match result {
                    Ok(size) => {
                        job.record.size = Some(size);
                        job.record.complete = true;
                        for part in &mut job.record.parts {
                            if part.end == u64::MAX {
                                part.end = size;
                            }
                            part.done = part.end - part.start;
                        }
                        job.phase = Phase::Complete;
                    }
                    Err(reason) => {
                        job.record.error = Some(reason.clone());
                        job.phase = Phase::Failed(reason);
                    }
                }
                job.dirty = true;
            }
        }
    }

    /// Stop the task, sync the file and fold the durable offsets into the
    /// record. The task is cancelled at an await point; a `write_at` in
    /// flight finishes first, so the offsets read afterwards are written.
    async fn halt(&mut self, id: TorrentId) -> Result<(), String> {
        let Some(job) = self.jobs.get_mut(&id) else {
            return Err("unknown job".into());
        };
        let phase = std::mem::replace(&mut job.phase, Phase::Paused);
        let Phase::Running { task, progress } = phase else {
            job.phase = phase;
            return Ok(());
        };
        task.abort();
        let _ = task.await;
        let offsets = progress.offsets();
        let file = lock(&progress.file).take();
        if let Some(file) = file {
            let synced = tokio::task::spawn_blocking(move || file.sync_data()).await;
            if !matches!(synced, Ok(Ok(()))) {
                // Unsynced bytes are not trusted: keep the old offsets.
                return Ok(());
            }
        }
        let job = self.jobs.get_mut(&id).ok_or("unknown job")?;
        for (part, done) in job.record.parts.iter_mut().zip(offsets) {
            part.done = done;
        }
        job.dirty = true;
        Ok(())
    }

    pub async fn pause(&mut self, id: TorrentId) -> Result<(), String> {
        match self.jobs.get(&id).map(|job| &job.phase) {
            None => return Err("unknown job".into()),
            Some(Phase::Running { .. } | Phase::Queued) => {}
            Some(Phase::Paused) => return Err("the job is already paused".into()),
            Some(Phase::Complete) => return Err("the job is complete".into()),
            Some(Phase::Failed(_)) => return Err("the job has stopped; use Retry".into()),
        }
        self.halt(id).await?;
        let job = self.jobs.get_mut(&id).ok_or("unknown job")?;
        job.phase = Phase::Paused;
        job.record.paused = true;
        job.dirty = true;
        Ok(())
    }

    pub fn resume(&mut self, id: TorrentId) -> Result<(), String> {
        let job = self.jobs.get_mut(&id).ok_or("unknown job")?;
        match job.phase {
            Phase::Paused => {
                job.phase = Phase::Queued;
                job.record.paused = false;
                job.dirty = true;
                Ok(())
            }
            Phase::Failed(_) => Err("the job has stopped; use Retry".into()),
            Phase::Complete => Err("the job is complete".into()),
            Phase::Queued | Phase::Running { .. } => Err("the job is not paused".into()),
        }
    }

    /// Restart a stopped job. Partial bytes stay and resume under the
    /// identity rules; the error is cleared.
    pub fn retry(&mut self, id: TorrentId) -> Result<(), String> {
        let job = self.jobs.get_mut(&id).ok_or("unknown job")?;
        match job.phase {
            Phase::Failed(_) | Phase::Paused => {
                job.phase = Phase::Queued;
                job.record.paused = false;
                job.record.error = None;
                job.note = None;
                job.dirty = true;
                Ok(())
            }
            Phase::Complete => Err("the job is complete".into()),
            Phase::Queued | Phase::Running { .. } => Err("the job is already running".into()),
        }
    }

    /// Stop a job and keep its partial file. Retry starts it again.
    pub async fn cancel(&mut self, id: TorrentId) -> Result<(), String> {
        match self.jobs.get(&id).map(|job| &job.phase) {
            None => return Err("unknown job".into()),
            Some(Phase::Running { .. } | Phase::Queued | Phase::Paused) => {}
            Some(Phase::Complete) => return Err("the job is complete".into()),
            Some(Phase::Failed(_)) => return Err("the job has already stopped".into()),
        }
        self.halt(id).await?;
        let job = self.jobs.get_mut(&id).ok_or("unknown job")?;
        let reason = "cancelled; the partial file was kept".to_string();
        job.record.error = Some(reason.clone());
        job.record.paused = false;
        job.phase = Phase::Failed(reason);
        job.dirty = true;
        Ok(())
    }

    /// Stop the job and forget it. With `delete_data` the final file and the
    /// temporary sibling are unlinked through the no-follow path; the folder
    /// stays. Returns the record key so the caller can delete its record.
    pub async fn remove(&mut self, id: TorrentId, delete_data: bool) -> Result<[u8; 20], String> {
        if !self.jobs.contains_key(&id) {
            return Err("unknown job".into());
        }
        self.halt(id).await?;
        let job = self.jobs.get(&id).ok_or("unknown job")?;
        let key = job.record.key;
        if delete_data {
            let root = &job.record.destination;
            for path in [job.record.part_path(), job.record.final_path()]
                .into_iter()
                .flatten()
            {
                templartorrent_storage::delete_owned_file(root, &path)
                    .map_err(|error| format!("data deletion refused: {error}"))?;
            }
        }
        self.jobs.remove(&id);
        self.order.retain(|known| *known != id);
        Ok(key)
    }

    /// Sync every running job's file and fold the durable offsets into its
    /// record. Returns the records that changed since the last call, for
    /// the session store.
    pub async fn checkpoint(&mut self) -> Vec<UrlRecord> {
        let running: Vec<(TorrentId, Arc<Progress>)> = self
            .jobs
            .iter()
            .filter_map(|(id, job)| match &job.phase {
                Phase::Running { progress, .. } => Some((*id, Arc::clone(progress))),
                _ => None,
            })
            .collect();
        for (id, progress) in running {
            let offsets = progress.offsets();
            let file = lock(&progress.file).clone();
            if let Some(file) = file {
                let synced = tokio::task::spawn_blocking(move || file.sync_data()).await;
                if !matches!(synced, Ok(Ok(()))) {
                    continue;
                }
            } else {
                continue;
            }
            if let Some(job) = self.jobs.get_mut(&id) {
                let changed = job
                    .record
                    .parts
                    .iter()
                    .zip(&offsets)
                    .any(|(part, done)| part.done != *done);
                if changed {
                    for (part, done) in job.record.parts.iter_mut().zip(offsets) {
                        part.done = done;
                    }
                    job.dirty = true;
                }
            }
        }
        self.take_dirty()
    }

    /// Records changed since the last take, without a sync.
    pub fn take_dirty(&mut self) -> Vec<UrlRecord> {
        let mut out = Vec::new();
        for id in &self.order {
            if let Some(job) = self.jobs.get_mut(id)
                && job.dirty
            {
                job.dirty = false;
                out.push(job.record.clone());
            }
        }
        out
    }

    pub fn record(&self, id: TorrentId) -> Option<&UrlRecord> {
        self.jobs.get(&id).map(|job| &job.record)
    }

    pub fn key(&self, id: TorrentId) -> Option<[u8; 20]> {
        self.jobs.get(&id).map(|job| job.record.key)
    }

    fn state(job: &UrlJob) -> JobState {
        match &job.phase {
            Phase::Queued => JobState::Queued,
            Phase::Running { progress, .. } => {
                if progress.phase.load(Ordering::Acquire) == 1 {
                    JobState::Checking
                } else {
                    JobState::Downloading
                }
            }
            Phase::Paused => JobState::Paused,
            Phase::Complete => JobState::Complete,
            Phase::Failed(_) => JobState::Error,
        }
    }

    fn done_bytes(job: &UrlJob) -> u64 {
        match &job.phase {
            Phase::Running { progress, .. } => progress.offsets().iter().sum(),
            Phase::Complete => job.record.size.unwrap_or_else(|| job.record.done_bytes()),
            _ => job.record.done_bytes(),
        }
    }

    /// Rows in add order, shaped like torrent rows. An unknown size is
    /// `wanted_bytes == 0`, which the UI renders as an em dash.
    pub fn rows(&self) -> Vec<TorrentRow> {
        self.order
            .iter()
            .filter_map(|id| self.jobs.get(id).map(|job| (*id, job)))
            .map(|(id, job)| {
                let done = Self::done_bytes(job);
                let active = match &job.phase {
                    Phase::Running { progress, .. } => progress.active.load(Ordering::Acquire),
                    _ => 0,
                };
                TorrentRow {
                    id,
                    name: Arc::from(job.record.label()),
                    state: Self::state(job),
                    wanted_bytes: job.record.size.unwrap_or(0),
                    verified_bytes: done,
                    total_payload_downloaded: done,
                    total_payload_uploaded: 0,
                    down_bps: 0,
                    up_bps: 0,
                    connected_seeds: 0,
                    connected_peers: u32::try_from(active).unwrap_or(u32::MAX),
                    estimated_seeds: None,
                    estimated_peers: None,
                }
            })
            .collect()
    }

    pub fn detail(&self, id: TorrentId) -> Option<UrlDetail> {
        let job = self.jobs.get(&id)?;
        let record = &job.record;
        let (parts, connections) = match &job.phase {
            Phase::Running { progress, .. } => (
                record
                    .parts
                    .iter()
                    .zip(progress.offsets())
                    .map(|(part, done)| PartRecord { done, ..*part })
                    .collect(),
                progress.active.load(Ordering::Acquire),
            ),
            _ => (record.parts.clone(), 0),
        };
        Some(UrlDetail {
            url: parse_url(&record.url)
                .map(|parsed| parsed.redacted())
                .unwrap_or_default(),
            destination: record.destination.display().to_string(),
            file: record.name.clone(),
            size: record.size,
            done: Self::done_bytes(job),
            ranged: record.ranged,
            validator: record
                .validator
                .as_ref()
                .map(|validator| format!("{} {}", validator.kind(), validator.value())),
            parts,
            connections,
            sha256: record.sha256.map(|digest| hex(&digest)),
            error: match &job.phase {
                Phase::Failed(reason) => Some(reason.clone()),
                _ => record.error.clone(),
            },
            note: job.note.clone(),
            state: Self::state(job),
        })
    }

    /// Kill every running task without a sync or a record update, the way a
    /// crash would. The records returned by the last checkpoint are what a
    /// restart has. Tests only.
    #[doc(hidden)]
    pub fn abort_all_for_test(&mut self) {
        for job in self.jobs.values_mut() {
            if let Phase::Running { task, .. } = &job.phase {
                task.abort();
                job.phase = Phase::Queued;
            }
        }
    }

    /// Stop every task, sync, and return every record for a final save.
    pub async fn shutdown(&mut self) -> Vec<UrlRecord> {
        let ids: Vec<TorrentId> = self.order.clone();
        for id in ids {
            if self
                .jobs
                .get(&id)
                .is_some_and(|job| matches!(job.phase, Phase::Running { .. }))
            {
                let _ = self.halt(id).await;
                if let Some(job) = self.jobs.get_mut(&id) {
                    // Running intent survives: the next launch resumes it.
                    job.phase = Phase::Queued;
                    job.record.paused = false;
                }
            }
        }
        self.order
            .iter()
            .filter_map(|id| self.jobs.get(id).map(|job| job.record.clone()))
            .collect()
    }
}

/// Like a torrent destination: an existing directory is used, a missing one
/// is created, a file is refused. The canonical absolute path is kept.
fn prepare_destination(path: &Path) -> Result<PathBuf, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(meta) if meta.file_type().is_symlink() => {
            if !path.is_dir() {
                return Err("destination must be a directory".into());
            }
        }
        Ok(_) => return Err("destination must be a directory".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(path)
                .map_err(|_| "destination folder could not be created".to_string())?;
        }
        Err(_) => return Err("destination validation failed".into()),
    }
    templartorrent_core::paths::canonical_dir(path)
        .map_err(|_| "destination validation failed".to_string())
}

pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

/// 64 hex digits to a digest; whitespace around it is ignored.
pub fn parse_sha256(text: &str) -> Result<[u8; 32], &'static str> {
    let text = text.trim();
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("SHA-256 must be 64 hexadecimal digits");
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|_| "SHA-256 must be 64 hexadecimal digits")?;
    }
    Ok(out)
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use transport::ResponseHead;

    fn head(pairs: &[(&str, &str)]) -> ResponseHead {
        ResponseHead {
            status: 200,
            headers: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn validators_need_a_strong_etag_or_a_settled_date() {
        assert_eq!(
            Validator::from_head(&head(&[("ETag", "\"abc\"")])),
            Some(Validator::Etag("\"abc\"".into()))
        );
        assert_eq!(Validator::from_head(&head(&[("ETag", "W/\"abc\"")])), None);
        assert_eq!(Validator::from_head(&head(&[("ETag", "abc")])), None);
        let fresh = head(&[
            ("Last-Modified", "Sun, 06 Nov 1994 08:49:37 GMT"),
            ("Date", "Sun, 06 Nov 1994 08:50:36 GMT"),
        ]);
        assert_eq!(Validator::from_head(&fresh), None, "59 s is too close");
        let settled = head(&[
            ("Last-Modified", "Sun, 06 Nov 1994 08:49:37 GMT"),
            ("Date", "Sun, 06 Nov 1994 08:50:37 GMT"),
        ]);
        assert_eq!(
            Validator::from_head(&settled),
            Some(Validator::LastModified(
                "Sun, 06 Nov 1994 08:49:37 GMT".into()
            ))
        );
        let undated = head(&[("Last-Modified", "Sun, 06 Nov 1994 08:49:37 GMT")]);
        assert_eq!(Validator::from_head(&undated), None);
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"), None);
    }

    #[test]
    fn parts_split_only_ranged_bodies_above_the_threshold() {
        assert_eq!(plan_parts(None, true, 4).len(), 1);
        assert_eq!(plan_parts(None, true, 4)[0].end, u64::MAX);
        assert_eq!(plan_parts(Some(SPLIT_THRESHOLD - 1), true, 4).len(), 1);
        assert_eq!(plan_parts(Some(10 * SPLIT_THRESHOLD), false, 4).len(), 1);
        let parts = plan_parts(Some(10 * SPLIT_THRESHOLD + 3), true, 4);
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0].start, 0);
        assert_eq!(parts[3].end, 10 * SPLIT_THRESHOLD + 3);
        for pair in parts.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
        assert_eq!(plan_parts(Some(2 * SPLIT_THRESHOLD), true, 8).len(), 2);
        assert_eq!(
            plan_parts(Some(100 * SPLIT_THRESHOLD), true, 200).len(),
            usize::from(MAX_CONNECTIONS)
        );
    }

    #[test]
    fn names_come_from_disposition_then_the_url_and_are_single_components() {
        let url = parse_url("http://files.example/dir/Report%20Q3.pdf?token=x").unwrap();
        assert_eq!(derive_name(&head(&[]), &url), "Report Q3.pdf");
        let attached = head(&[(
            "Content-Disposition",
            "attachment; filename=\"../../etc/passwd\"",
        )]);
        assert_eq!(derive_name(&attached, &url), "passwd");
        let star = head(&[(
            "Content-Disposition",
            "attachment; filename=\"plain.bin\"; filename*=UTF-8''na%C3%AFve.bin",
        )]);
        assert_eq!(derive_name(&star, &url), "naïve.bin");
        let dots = head(&[("Content-Disposition", "attachment; filename=\"..\"")]);
        assert_eq!(derive_name(&dots, &url), "Report Q3.pdf");
        let bare = parse_url("http://files.example/").unwrap();
        assert_eq!(derive_name(&head(&[]), &bare), "download");
        assert_eq!(sanitize_name("a\u{202e}b\nc"), Some("abc".into()));
        assert!(sanitize_name(&"x".repeat(300)).unwrap().len() <= 255);
    }

    #[test]
    fn records_key_on_url_and_destination_and_parse_digests() {
        let a = UrlRecord::new("http://h/f", PathBuf::from("/d"), None);
        let b = UrlRecord::new("http://h/f", PathBuf::from("/e"), None);
        assert_ne!(a.key, b.key);
        assert_eq!(a.label(), "f (h)");
        assert!(parse_sha256(&"a".repeat(64)).is_ok());
        assert!(parse_sha256(&"a".repeat(63)).is_err());
        assert!(parse_sha256(&"g".repeat(64)).is_err());
    }
}
