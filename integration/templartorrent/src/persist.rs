//! Session persistence under the XDG data directory: one `<hash>.job` text
//! record and one `<hash>.torrent` metainfo file per job, plus `settings`.
//! Each write goes to a temporary file, is synced, then renamed over the
//! target, so a crash leaves either the old or the new file. Reads are
//! bounded and a bad record is reported with a reason, never trusted.

use std::{
    fs,
    io::Write,
    net::IpAddr,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use templartorrent_core::model::{Limits, PeerPreference};
use templartorrent_rbit_adapter::driver::JobRecord;

pub const RECORD_HEADER: &str = "templartorrent-job 1";
pub const SETTINGS_HEADER: &str = "templartorrent-settings 1";
pub const MAX_RECORD_BYTES: u64 = 64 * 1024;
pub const MAX_TORRENT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TRACKERS: usize = 12;
const MAX_PREFERENCES: usize = 256;
const MAX_FILES: usize = 128;
const MAX_URL: usize = 2048;

#[derive(Debug, Clone)]
pub struct Settings {
    pub limits: Limits,
    pub allow_local_trackers: bool,
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

    pub fn remove_job(&self, hash: &[u8; 20]) -> Result<(), String> {
        for path in [self.job_path(hash), self.torrent_path(hash)] {
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
    )
}

pub fn decode_settings(text: &str) -> Result<Settings, String> {
    let mut lines = text.lines();
    if lines.next() != Some(SETTINGS_HEADER) {
        return Err("unknown settings version".into());
    }
    let mut limits = Limits::default();
    let mut allow_local_trackers = false;
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
            other => return Err(format!("unknown field {other}")),
        }
    }
    if limits.payload_budget_bytes == 0 {
        return Err("payload budget must be positive".into());
    }
    Ok(Settings {
        limits,
        allow_local_trackers,
    })
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
        };
        let decoded = decode_settings(&encode_settings(&settings)).unwrap();
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
            })
            .unwrap();
        assert!(store.load_settings().unwrap().unwrap().allow_local_trackers);
    }
}
