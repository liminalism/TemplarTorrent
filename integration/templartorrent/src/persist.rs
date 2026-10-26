//! Session persistence under the XDG data directory: one `<hash>.job` text
//! record, one `<hash>.torrent` metainfo file and one `<hash>.resume` bitmap
//! per job, plus `settings`. Each write goes to a temporary file, is synced,
//! then renamed over the target, so a crash leaves either the old or the new
//! file. Reads are bounded and a bad record is reported with a reason, never
//! trusted. A missing resume file is not an error: the job is rechecked.

use std::{
    fs,
    io::Write,
    net::IpAddr,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use templartorrent_core::model::{Limits, PeerPreference};
use templartorrent_rbit_adapter::driver::PeerEncryption;
use templartorrent_rbit_adapter::driver::{JobRecord, ResumeRecord};
use templartorrent_rbit_adapter::resume::ResumeFile;
use templartorrent_rbit_adapter::torrent::{MAX_DRIVER_FILES, MAX_DRIVER_PIECES};

pub const RECORD_HEADER: &str = "templartorrent-job 1";
pub const RESUME_HEADER: &str = "templartorrent-resume 1";
pub const SETTINGS_HEADER: &str = "templartorrent-settings 1";
pub const UI_HEADER: &str = "templartorrent-ui 1";
/// 4096 files: a wanted bitstring and an order list of up to five digits
/// and a comma each, plus trackers and preferences.
pub const MAX_RECORD_BYTES: u64 = 256 * 1024;
pub const MAX_TORRENT_BYTES: u64 = 16 * 1024 * 1024;
/// 262144 pieces encode to 64 KiB of hex, plus one line of at most about
/// 100 bytes per file for 4096 files.
pub const MAX_RESUME_BYTES: u64 = 512 * 1024;
const MAX_TRACKERS: usize = 12;
const MAX_PREFERENCES: usize = 256;
const MAX_FILES: usize = MAX_DRIVER_FILES;
const MAX_URL: usize = 2048;

#[derive(Debug, Clone)]
pub struct Settings {
    pub limits: Limits,
    pub allow_local_trackers: bool,
    /// Peer listen port, TCP on IPv4 and IPv6. 0 means not chosen yet; the
    /// first launch picks one and saves it so port forwards keep working.
    pub listen_port: u16,
    /// MSE/PE policy for new peer connections.
    pub encryption: PeerEncryption,
    /// Map the listen port on the router (NAT-PMP, then UPnP). Off unless
    /// the user ticked it: explicit consent, DESIGN.md section 9.
    pub port_mapping: bool,
}

fn encryption_name(encryption: PeerEncryption) -> &'static str {
    match encryption {
        PeerEncryption::PreferEncrypted => "prefer",
        PeerEncryption::RequireEncrypted => "require",
        PeerEncryption::PlaintextOnly => "plaintext",
    }
}

/// Window geometry in logical pixels and the inspector pane's share of the
/// body. Out-of-range values are clamped on load, never trusted raw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UiState {
    pub width: u32,
    pub height: u32,
    pub split: f32,
}

impl Default for UiState {
    fn default() -> Self {
        // Transmission GTK's default main window; a larger opening leaves the
        // download list empty.
        Self {
            width: 300,
            height: 500,
            split: 0.38,
        }
    }
}

impl UiState {
    pub const MIN_WIDTH: u32 = 260;
    pub const MIN_HEIGHT: u32 = 320;
    const MAX_SIDE: u32 = 16_384;

    pub fn clamped(self) -> Self {
        Self {
            width: self.width.clamp(Self::MIN_WIDTH, Self::MAX_SIDE),
            height: self.height.clamp(Self::MIN_HEIGHT, Self::MAX_SIDE),
            split: if self.split.is_finite() {
                self.split.clamp(0.1, 0.9)
            } else {
                Self::default().split
            },
        }
    }
}

pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn ensure_dir(&self) -> Result<(), String> {
        fs::create_dir_all(&self.dir).map_err(|error| format!("session directory: {error}"))?;
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700));
        Ok(())
    }

    fn job_path(&self, hash: &[u8; 20]) -> PathBuf {
        self.dir.join(format!("{}.job", hex(hash)))
    }

    fn torrent_path(&self, hash: &[u8; 20]) -> PathBuf {
        self.dir.join(format!("{}.torrent", hex(hash)))
    }

    fn resume_path(&self, hash: &[u8; 20]) -> PathBuf {
        self.dir.join(format!("{}.resume", hex(hash)))
    }

    pub fn save_job(&self, record: &JobRecord) -> Result<(), String> {
        self.ensure_dir()?;
        if record.torrent.len() as u64 > MAX_TORRENT_BYTES {
            return Err("torrent exceeds the 16 MiB record limit".into());
        }
        write_atomic(&self.torrent_path(&record.info_hash), &record.torrent)?;
        write_atomic(
            &self.job_path(&record.info_hash),
            encode_record(record).as_bytes(),
        )
    }

    pub fn save_resume(&self, hash: &[u8; 20], resume: &ResumeRecord) -> Result<(), String> {
        self.ensure_dir()?;
        let text = encode_resume(resume);
        if text.len() as u64 > MAX_RESUME_BYTES {
            return Err("resume bitmap exceeds the 128 KiB record limit".into());
        }
        write_atomic(&self.resume_path(hash), text.as_bytes())
    }

    /// `Ok(None)` when no bitmap was saved. A present but unreadable file is
    /// an error so the caller can say why it fell back to a full recheck.
    pub fn load_resume(&self, hash: &[u8; 20]) -> Result<Option<ResumeRecord>, String> {
        let path = self.resume_path(hash);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("{}: {error}", path.display())),
            Ok(_) => {}
        }
        let text = read_bounded(&path, MAX_RESUME_BYTES)?;
        let text = String::from_utf8(text).map_err(|_| "resume bitmap is not UTF-8".to_string())?;
        decode_resume(&text).map(Some)
    }

    pub fn remove_job(&self, hash: &[u8; 20]) -> Result<(), String> {
        for path in [
            self.job_path(hash),
            self.torrent_path(hash),
            self.resume_path(hash),
        ] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
        Ok(())
    }

    /// Every readable record in name order, plus one line per skipped file.
    pub fn load_jobs(&self) -> (Vec<JobRecord>, Vec<String>) {
        let mut records = Vec::new();
        let mut errors = Vec::new();
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return (records, errors);
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_type()
                    .is_ok_and(|kind| kind.is_file() && !kind.is_symlink())
            })
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.ends_with(".job"))
            .collect();
        names.sort();
        for name in names {
            let stem = &name[..name.len() - 4];
            let Some(hash) = unhex(stem)
                .ok()
                .and_then(|bytes| <[u8; 20]>::try_from(bytes).ok())
            else {
                errors.push(format!("{name}: file name is not an info hash"));
                continue;
            };
            match self.load_job(&hash) {
                Ok(record) => records.push(record),
                Err(reason) => errors.push(format!("{name}: {reason}")),
            }
        }
        (records, errors)
    }

    fn load_job(&self, hash: &[u8; 20]) -> Result<JobRecord, String> {
        let torrent = read_bounded(&self.torrent_path(hash), MAX_TORRENT_BYTES)?;
        let text = read_bounded(&self.job_path(hash), MAX_RECORD_BYTES)?;
        let text = String::from_utf8(text).map_err(|_| "record is not UTF-8".to_string())?;
        decode_record(&text, *hash, torrent)
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<(), String> {
        self.ensure_dir()?;
        write_atomic(
            &self.dir.join("settings"),
            encode_settings(settings).as_bytes(),
        )
    }

    pub fn save_ui(&self, ui: &UiState) -> Result<(), String> {
        self.ensure_dir()?;
        write_atomic(&self.dir.join("ui"), encode_ui(&ui.clamped()).as_bytes())
    }

    /// Saved geometry, clamped; `None` when nothing was saved yet.
    pub fn load_ui(&self) -> Result<Option<UiState>, String> {
        let path = self.dir.join("ui");
        if !path.is_file() {
            return Ok(None);
        }
        let text = read_bounded(&path, MAX_RECORD_BYTES)?;
        let text = String::from_utf8(text).map_err(|_| "ui state is not UTF-8".to_string())?;
        decode_ui(&text).map(Some)
    }

    pub fn load_settings(&self) -> Result<Option<Settings>, String> {
        let path = self.dir.join("settings");
        if !path.is_file() {
            return Ok(None);
        }
        let text = read_bounded(&path, MAX_RECORD_BYTES)?;
        let text = String::from_utf8(text).map_err(|_| "settings are not UTF-8".to_string())?;
        decode_settings(&text).map(Some)
    }
}

pub fn encode_record(record: &JobRecord) -> String {
    let mut out = String::new();
    out.push_str(RECORD_HEADER);
    out.push('\n');
    out.push_str(&format!(
        "destination {}\n",
        hex(record.destination.as_os_str().as_bytes())
    ));
    out.push_str(&format!("paused {}\n", u8::from(record.paused)));
    out.push_str(&format!(
        "bootstrap {}\n",
        u8::from(record.public_bootstrap)
    ));
    if !record.wanted.is_empty() {
        let bits: String = record
            .wanted
            .iter()
            .map(|wanted| if *wanted { '1' } else { '0' })
            .collect();
        out.push_str(&format!("wanted {bits}\n"));
    }
    if !record.order.is_empty() {
        let order: Vec<String> = record.order.iter().map(|index| index.to_string()).collect();
        out.push_str(&format!("order {}\n", order.join(",")));
    }
    for (url, original) in &record.trackers {
        let origin = if *original { "original" } else { "added" };
        out.push_str(&format!("tracker {origin} {url}\n"));
    }
    for (ip, preference) in &record.preferences {
        out.push_str(&format!("pref {ip} {}\n", preference_name(*preference)));
    }
    out
}

pub fn decode_record(
    text: &str,
    info_hash: [u8; 20],
    torrent: Vec<u8>,
) -> Result<JobRecord, String> {
    let mut lines = text.lines();
    if lines.next() != Some(RECORD_HEADER) {
        return Err("unknown record version".into());
    }
    let mut destination = None;
    let mut paused = false;
    let mut public_bootstrap = false;
    let mut wanted = Vec::new();
    let mut order = Vec::new();
    let mut trackers = Vec::new();
    let mut preferences = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(' ')
            .ok_or_else(|| format!("malformed line: {line}"))?;
        match key {
            "destination" => {
                let bytes = unhex(value)?;
                let path = PathBuf::from(std::ffi::OsString::from_vec(bytes));
                if !path.is_absolute() {
                    return Err("destination must be absolute".into());
                }
                destination = Some(path);
            }
            "paused" => paused = flag(value)?,
            "bootstrap" => public_bootstrap = flag(value)?,
            "wanted" => {
                if value.len() > MAX_FILES {
                    return Err("too many files in selection".into());
                }
                wanted = value
                    .chars()
                    .map(|c| match c {
                        '1' => Ok(true),
                        '0' => Ok(false),
                        _ => Err("selection must be 0 or 1".to_string()),
                    })
                    .collect::<Result<_, _>>()?;
            }
            "order" => {
                order = value
                    .split(',')
                    .map(|part| {
                        part.parse::<usize>()
                            .map_err(|_| "order must be numeric".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if order.len() > MAX_FILES {
                    return Err("too many files in order".into());
                }
            }
            "tracker" => {
                let (origin, url) = value
                    .split_once(' ')
                    .ok_or_else(|| "tracker needs an origin and a URL".to_string())?;
                let original = match origin {
                    "original" => true,
                    "added" => false,
                    _ => return Err("tracker origin must be original or added".into()),
                };
                if url.is_empty() || url.len() > MAX_URL || url.contains(char::is_whitespace) {
                    return Err("tracker URL is invalid".into());
                }
                if trackers.len() >= MAX_TRACKERS {
                    return Err("too many trackers".into());
                }
                trackers.push((url.to_string(), original));
            }
            "pref" => {
                let (ip, name) = value
                    .split_once(' ')
                    .ok_or_else(|| "preference needs an address and a value".to_string())?;
                let ip: IpAddr = ip
                    .parse()
                    .map_err(|_| "preference address is invalid".to_string())?;
                let preference = match name {
                    "Prefer" => PeerPreference::Prefer,
                    "Normal" => PeerPreference::Normal,
                    "Avoid" => PeerPreference::Avoid,
                    "Block" => PeerPreference::Block,
                    _ => return Err("preference value is invalid".into()),
                };
                if preferences.len() >= MAX_PREFERENCES {
                    return Err("too many preferences".into());
                }
                preferences.push((ip, preference));
            }
            other => return Err(format!("unknown field {other}")),
        }
    }
    Ok(JobRecord {
        info_hash,
        torrent,
        destination: destination.ok_or_else(|| "destination missing".to_string())?,
        paused,
        public_bootstrap,
        wanted,
        order,
        trackers,
        preferences,
    })
}

pub fn encode_resume(resume: &ResumeRecord) -> String {
    let mut out = String::new();
    out.push_str(RESUME_HEADER);
    out.push('\n');
    out.push_str(&format!("pieces {}\n", resume.pieces));
    out.push_str(&format!("barrier {}\n", u8::from(resume.barrier)));
    out.push_str(&format!("bits {}\n", hex(&resume.bits)));
    for (index, file) in resume.files.iter().enumerate() {
        out.push_str(&format!(
            "file {index} {} {} {} {} {}\n",
            file.dev, file.ino, file.size, file.mtime_sec, file.mtime_nsec
        ));
    }
    out
}

pub fn decode_resume(text: &str) -> Result<ResumeRecord, String> {
    let mut lines = text.lines();
    if lines.next() != Some(RESUME_HEADER) {
        return Err("unknown resume version".into());
    }
    let mut pieces = None;
    let mut barrier = None;
    let mut bits = None;
    let mut files: Vec<Option<ResumeFile>> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(' ')
            .ok_or_else(|| format!("malformed line: {line}"))?;
        match key {
            "pieces" => {
                if pieces.is_some() {
                    return Err("duplicate pieces".into());
                }
                let count = value
                    .parse::<usize>()
                    .map_err(|_| "pieces must be numeric".to_string())?;
                if count > MAX_DRIVER_PIECES {
                    return Err("too many pieces".into());
                }
                pieces = Some(count);
            }
            "barrier" => {
                if barrier.is_some() {
                    return Err("duplicate barrier".into());
                }
                barrier = Some(flag(value)?);
            }
            "bits" => {
                if bits.is_some() {
                    return Err("duplicate bits".into());
                }
                bits = Some(unhex(value)?);
            }
            "file" => {
                let mut parts = value.split(' ');
                let index = parts
                    .next()
                    .ok_or_else(|| "resume file needs an index".to_string())?
                    .parse::<usize>()
                    .map_err(|_| "resume file index must be numeric".to_string())?;
                if index >= MAX_DRIVER_FILES {
                    return Err("too many resume files".into());
                }
                let number = |parts: &mut std::str::Split<'_, char>, label: &str| {
                    parts
                        .next()
                        .ok_or_else(|| format!("resume file needs {label}"))?
                        .parse::<u64>()
                        .map_err(|_| format!("resume file {label} must be numeric"))
                };
                let dev = number(&mut parts, "device")?;
                let ino = number(&mut parts, "inode")?;
                let size = number(&mut parts, "size")?;
                let mtime_sec = parts
                    .next()
                    .ok_or_else(|| "resume file needs mtime".to_string())?
                    .parse::<i64>()
                    .map_err(|_| "resume file mtime must be numeric".to_string())?;
                let mtime_nsec = parts
                    .next()
                    .ok_or_else(|| "resume file needs mtime_nsec".to_string())?
                    .parse::<i64>()
                    .map_err(|_| "resume file mtime_nsec must be numeric".to_string())?;
                if parts.next().is_some() {
                    return Err("resume file has trailing fields".into());
                }
                if !(0..1_000_000_000).contains(&mtime_nsec) {
                    return Err("resume file mtime_nsec is out of range".into());
                }
                if files.len() <= index {
                    files.resize_with(index + 1, || None);
                }
                if files[index].is_some() {
                    return Err("duplicate resume file".into());
                }
                files[index] = Some(ResumeFile {
                    dev,
                    ino,
                    size,
                    mtime_sec,
                    mtime_nsec,
                });
            }
            other => return Err(format!("unknown field {other}")),
        }
    }
    let pieces = pieces.ok_or_else(|| "pieces missing".to_string())?;
    let bits = bits.ok_or_else(|| "bits missing".to_string())?;
    let expected = pieces.div_ceil(8);
    if bits.len() != expected {
        return Err("resume bitfield length does not match the piece count".into());
    }
    if !pieces.is_multiple_of(8) && expected > 0 && bits[expected - 1] >> (pieces % 8) != 0 {
        return Err("resume bitfield has spare bits set".into());
    }
    if files.iter().any(Option::is_none) {
        return Err("resume file list has a gap".into());
    }
    Ok(ResumeRecord {
        pieces,
        // A missing barrier line trusts nothing. The driver rechecks.
        barrier: barrier.unwrap_or(false),
        bits,
        files: files.into_iter().flatten().collect(),
    })
}

pub fn encode_settings(settings: &Settings) -> String {
    let l = &settings.limits;
    format!(
        "{SETTINGS_HEADER}\ndownload_jobs {}\nseed_jobs {}\nupload_peer_slots {}\nconnections_total {}\nconnections_per_job {}\ndownload_kb_s {}\nupload_kb_s {}\npayload_budget_bytes {}\nallow_local_trackers {}\n",
        l.download_jobs,
        l.seed_jobs,
        l.upload_peer_slots,
        l.connections_total,
        l.connections_per_job,
        l.download_kb_s,
        l.upload_kb_s,
        l.payload_budget_bytes,
        u8::from(settings.allow_local_trackers)
    ) + &format!(
        "listen_port {}\npeer_encryption {}\nport_mapping {}\n",
        settings.listen_port,
        encryption_name(settings.encryption),
        u8::from(settings.port_mapping)
    )
}

pub fn decode_settings(text: &str) -> Result<Settings, String> {
    let mut lines = text.lines();
    if lines.next() != Some(SETTINGS_HEADER) {
        return Err("unknown settings version".into());
    }
    let mut limits = Limits::default();
    let mut allow_local_trackers = false;
    let mut listen_port = 0u16;
    let mut encryption = PeerEncryption::default();
    let mut port_mapping = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(' ')
            .ok_or_else(|| format!("malformed line: {line}"))?;
        let number = || {
            value
                .parse::<u64>()
                .map_err(|_| format!("{key} must be numeric"))
        };
        let count = || usize::try_from(number()?).map_err(|_| format!("{key} is too large"));
        match key {
            "download_jobs" => limits.download_jobs = count()?,
            "seed_jobs" => limits.seed_jobs = count()?,
            "upload_peer_slots" => limits.upload_peer_slots = count()?,
            "connections_total" => limits.connections_total = count()?,
            "connections_per_job" => limits.connections_per_job = count()?,
            "download_kb_s" => limits.download_kb_s = number()?,
            "upload_kb_s" => limits.upload_kb_s = number()?,
            "payload_budget_bytes" => limits.payload_budget_bytes = count()?,
            "allow_local_trackers" => allow_local_trackers = flag(value)?,
            "listen_port" => {
                listen_port = value
                    .parse()
                    .map_err(|_| "listen_port must be 0..=65535".to_string())?
            }
            "peer_encryption" => {
                encryption = match value {
                    "prefer" => PeerEncryption::PreferEncrypted,
                    "require" => PeerEncryption::RequireEncrypted,
                    "plaintext" => PeerEncryption::PlaintextOnly,
                    _ => return Err("peer_encryption must be prefer, require or plaintext".into()),
                }
            }
            "port_mapping" => port_mapping = flag(value)?,
            other => return Err(format!("unknown field {other}")),
        }
    }
    if limits.payload_budget_bytes == 0 {
        return Err("payload budget must be positive".into());
    }
    Ok(Settings {
        limits,
        allow_local_trackers,
        listen_port,
        encryption,
        port_mapping,
    })
}

pub fn encode_ui(ui: &UiState) -> String {
    format!(
        "{UI_HEADER}\nwidth {}\nheight {}\nsplit {:.4}\n",
        ui.width, ui.height, ui.split
    )
}

pub fn decode_ui(text: &str) -> Result<UiState, String> {
    let mut lines = text.lines();
    if lines.next() != Some(UI_HEADER) {
        return Err("unknown ui state version".into());
    }
    let mut ui = UiState::default();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(' ')
            .ok_or_else(|| format!("malformed line: {line}"))?;
        match key {
            "width" => {
                ui.width = value
                    .parse()
                    .map_err(|_| "width must be numeric".to_string())?
            }
            "height" => {
                ui.height = value
                    .parse()
                    .map_err(|_| "height must be numeric".to_string())?;
            }
            "split" => {
                ui.split = value
                    .parse()
                    .map_err(|_| "split must be numeric".to_string())?
            }
            other => return Err(format!("unknown field {other}")),
        }
    }
    Ok(ui.clamped())
}

fn preference_name(preference: PeerPreference) -> &'static str {
    match preference {
        PeerPreference::Prefer => "Prefer",
        PeerPreference::Normal => "Normal",
        PeerPreference::Avoid => "Avoid",
        PeerPreference::Block => "Block",
    }
}

fn flag(value: &str) -> Result<bool, String> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err("flag must be 0 or 1".into()),
    }
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

fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("hex length is odd".into());
    }
    // Two-byte slices below would split a multibyte char.
    if !text.is_ascii() {
        return Err("invalid hex".into());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|_| "invalid hex".to_string()))
        .collect()
}

fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let meta =
        fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!("{}: not a regular file", path.display()));
    }
    if meta.len() > max {
        return Err(format!("{}: exceeds {max} bytes", path.display()));
    }
    fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Temporary file, data sync, rename, directory sync: readers see the old or
/// the new content, never a torn file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "record path has no parent".to_string())?;
    let mut name = path
        .file_name()
        .ok_or_else(|| "record path has no name".to_string())?
        .to_os_string();
    name.push(".tmp");
    let tmp = parent.join(name);
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {error}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "templartorrent-persist-{tag}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn record() -> JobRecord {
        JobRecord {
            info_hash: [7; 20],
            torrent: b"d4:infod4:name1:x12:piece lengthi16384e6:pieces0:6:lengthi0eee".to_vec(),
            destination: PathBuf::from("/tmp/shows/sÉason 1"),
            paused: true,
            public_bootstrap: true,
            wanted: vec![true, false, true],
            order: vec![2, 0, 1],
            trackers: vec![
                ("http://tracker.example/announce?passkey=abc".into(), true),
                ("udp://open.example:6969/announce".into(), false),
            ],
            preferences: vec![
                ("10.0.0.9".parse().unwrap(), PeerPreference::Block),
                ("2001:db8::1".parse().unwrap(), PeerPreference::Avoid),
            ],
        }
    }

    #[test]
    fn record_roundtrips_through_text() {
        let original = record();
        let text = encode_record(&original);
        assert!(text.starts_with(RECORD_HEADER));
        let decoded = decode_record(&text, original.info_hash, original.torrent.clone()).unwrap();
        assert_eq!(decoded, original);
        let debug = format!("{decoded:?}");
        assert!(
            !debug.contains("passkey"),
            "Debug must not print tracker URLs"
        );
    }

    #[test]
    fn malformed_records_are_rejected_with_a_reason() {
        let base = encode_record(&record());
        for (mutation, reason) in [
            (
                base.replace(RECORD_HEADER, "templartorrent-job 9"),
                "version",
            ),
            (base.replace("paused 1", "paused yes"), "flag"),
            (base.replace("wanted 101", "wanted 1x1"), "0 or 1"),
            (base.replace("order 2,0,1", "order 2,a"), "numeric"),
            (
                base.replace("tracker original", "tracker guessed"),
                "origin",
            ),
            (
                base.replace("pref 10.0.0.9 Block", "pref nowhere Block"),
                "address",
            ),
            (format!("{base}colour blue\n"), "unknown field"),
            (
                base.lines()
                    .filter(|line| !line.starts_with("destination"))
                    .map(|line| format!("{line}\n"))
                    .collect(),
                "destination missing",
            ),
        ] {
            let error = decode_record(&mutation, [1; 20], Vec::new()).unwrap_err();
            assert!(error.contains(reason), "{reason}: {error}");
        }
        let relative = base.replace(
            &hex("/tmp/shows/sÉason 1".as_bytes()),
            &hex(b"relative/path"),
        );
        assert!(
            decode_record(&relative, [1; 20], Vec::new())
                .unwrap_err()
                .contains("absolute")
        );
    }

    #[test]
    fn settings_roundtrip_and_reject_zero_budget() {
        let settings = Settings {
            limits: Limits {
                download_jobs: 1,
                upload_kb_s: 250,
                ..Limits::default()
            },
            allow_local_trackers: true,
            listen_port: 51234,
            encryption: PeerEncryption::RequireEncrypted,
            port_mapping: true,
        };
        let decoded = decode_settings(&encode_settings(&settings)).unwrap();
        assert!(decoded.port_mapping);
        assert_eq!(decoded.listen_port, 51234);
        assert_eq!(decoded.encryption, PeerEncryption::RequireEncrypted);
        assert_eq!(decoded.limits.download_jobs, 1);
        assert_eq!(decoded.limits.upload_kb_s, 250);
        assert_eq!(decoded.limits.seed_jobs, Limits::default().seed_jobs);
        assert!(decoded.allow_local_trackers);
        let zero = encode_settings(&settings).replace(
            &format!(
                "payload_budget_bytes {}",
                settings.limits.payload_budget_bytes
            ),
            "payload_budget_bytes 0",
        );
        assert!(decode_settings(&zero).unwrap_err().contains("positive"));
    }

    #[test]
    fn port_mapping_defaults_off_and_rejects_bad_flags() {
        // Files written before the setting existed decode with it off.
        let old = format!("{SETTINGS_HEADER}\nlisten_port 51234\npeer_encryption prefer\n");
        assert!(!decode_settings(&old).unwrap().port_mapping);
        let on = decode_settings(&format!("{SETTINGS_HEADER}\nport_mapping 1\n")).unwrap();
        assert!(on.port_mapping);
        assert!(decode_settings(&format!("{SETTINGS_HEADER}\nport_mapping yes\n")).is_err());
        let off = Settings {
            limits: Limits::default(),
            allow_local_trackers: false,
            listen_port: 0,
            encryption: PeerEncryption::default(),
            port_mapping: false,
        };
        assert!(encode_settings(&off).contains("port_mapping 0\n"));
    }

    #[test]
    fn ui_state_roundtrips_and_clamps_hostile_values() {
        let ui = UiState {
            width: 1120,
            height: 760,
            split: 0.42,
        };
        let decoded = decode_ui(&encode_ui(&ui)).unwrap();
        assert_eq!(decoded.width, 1120);
        assert_eq!(decoded.height, 760);
        assert!((decoded.split - 0.42).abs() < 0.001);
        let hostile =
            decode_ui(&format!("{UI_HEADER}\nwidth 1\nheight 999999\nsplit 7\n")).unwrap();
        assert_eq!(hostile.width, UiState::MIN_WIDTH);
        assert_eq!(hostile.height, 16_384);
        assert!((hostile.split - 0.9).abs() < 0.001);
        assert!(decode_ui("templartorrent-ui 2\n").is_err());
        assert!(decode_ui(&format!("{UI_HEADER}\nwidth wide\n")).is_err());
        let dir = Dir::new("ui");
        let store = SessionStore::new(dir.0.join("session"));
        assert!(store.load_ui().unwrap().is_none());
        store.save_ui(&ui).unwrap();
        assert_eq!(store.load_ui().unwrap().unwrap().width, 1120);
    }

    #[test]
    fn store_writes_atomically_and_skips_bad_files() {
        let dir = Dir::new("store");
        let store = SessionStore::new(dir.0.join("session"));
        let record = record();
        store.save_job(&record).unwrap();
        let (loaded, errors) = store.load_jobs();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(loaded, vec![record.clone()]);
        assert!(
            !fs::read_dir(store.dir()).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")),
            "no temporary file survives a save"
        );
        // A second save replaces the content in place.
        let mut changed = record.clone();
        changed.paused = false;
        store.save_job(&changed).unwrap();
        assert_eq!(store.load_jobs().0, vec![changed]);
        // Corrupt text, a missing torrent file and a stray name are reported.
        fs::write(
            store.dir().join(format!("{}.job", hex(&[7; 20]))),
            "garbage",
        )
        .unwrap();
        fs::write(
            store.dir().join(format!("{}.job", hex(&[8; 20]))),
            RECORD_HEADER,
        )
        .unwrap();
        fs::write(store.dir().join("notes.job"), RECORD_HEADER).unwrap();
        let (loaded, errors) = store.load_jobs();
        assert!(loaded.is_empty());
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("version")));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("No such file") || e.contains("torrent"))
        );
        assert!(errors.iter().any(|e| e.contains("not an info hash")));
        // An oversized record is refused before it is parsed.
        fs::write(
            store.dir().join(format!("{}.job", hex(&[7; 20]))),
            vec![b'a'; MAX_RECORD_BYTES as usize + 1],
        )
        .unwrap();
        assert!(store.load_jobs().1.iter().any(|e| e.contains("exceeds")));
        store.remove_job(&[7; 20]).unwrap();
        store.remove_job(&[7; 20]).unwrap();
        assert!(
            !store
                .dir()
                .join(format!("{}.torrent", hex(&[7; 20])))
                .exists()
        );
        assert!(store.load_settings().unwrap().is_none());
        store
            .save_settings(&Settings {
                limits: Limits::default(),
                allow_local_trackers: true,
                listen_port: 0,
                encryption: PeerEncryption::default(),
                port_mapping: false,
            })
            .unwrap();
        assert!(store.load_settings().unwrap().unwrap().allow_local_trackers);
    }

    fn sample_resume() -> ResumeRecord {
        ResumeRecord {
            pieces: 9,
            barrier: true,
            bits: vec![0xff, 0x01],
            files: vec![
                ResumeFile {
                    dev: 1,
                    ino: 2,
                    size: 3,
                    mtime_sec: -4,
                    mtime_nsec: 5,
                },
                ResumeFile {
                    dev: 8,
                    ino: 9,
                    size: 10,
                    mtime_sec: 11,
                    mtime_nsec: 12,
                },
            ],
        }
    }

    #[test]
    fn resume_bitmap_roundtrips_and_rejects_a_bad_record() {
        let resume = sample_resume();
        let text = encode_resume(&resume);
        assert_eq!(decode_resume(&text).unwrap(), resume);
        let dropped: String = text
            .lines()
            .filter(|line| !line.starts_with("barrier"))
            .map(|line| format!("{line}\n"))
            .collect();
        let open = decode_resume(&dropped).unwrap();
        assert!(!open.barrier, "a missing barrier trusts nothing");
        for (mutation, reason) in [
            (
                text.replace(RESUME_HEADER, "templartorrent-resume 9"),
                "version",
            ),
            (text.replace("barrier 1", "barrier 2"), "flag"),
            (text.replace("bits ff01", "bits ff03"), "spare"),
            (text.replace("bits ff01", "bits ff"), "length"),
            (format!("{text}colour blue\n"), "unknown"),
            (format!("{text}file 3 1 2 3 4 5\n"), "gap"),
            (
                text.replace("file 0 1 2 3 -4 5", "file 0 1 2 3 -4 1000000000"),
                "nsec",
            ),
        ] {
            let error = decode_resume(&mutation).unwrap_err();
            assert!(error.contains(reason), "{reason}: {error}");
        }
        let dir = Dir::new("resume");
        let store = SessionStore::new(dir.0.join("session"));
        let hash = [7u8; 20];
        assert!(store.load_resume(&hash).unwrap().is_none());
        store.save_resume(&hash, &resume).unwrap();
        assert_eq!(store.load_resume(&hash).unwrap().unwrap(), resume);
        store.remove_job(&hash).unwrap();
        assert!(store.load_resume(&hash).unwrap().is_none());
    }

    #[test]
    fn hex_fields_with_multibyte_chars_are_refused() {
        // Found by fuzz/persist_records: four bytes, even length, but slicing
        // at byte 2 splits the char.
        let hex = "2\u{45e}f";
        assert_eq!(hex.len(), 4);
        let record = format!("{RECORD_HEADER}\ndestination {hex}\n");
        assert!(decode_record(&record, [7; 20], Vec::new()).is_err());
        let resume = format!("{RESUME_HEADER}\npieces 9\nbits {hex}\n");
        assert!(decode_resume(&resume).is_err());
    }
}
