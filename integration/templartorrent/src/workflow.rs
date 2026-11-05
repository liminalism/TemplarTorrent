//! Pure dialog and display rules. No window, network, or filesystem mutation.

use std::path::{Path, PathBuf};

use templartorrent_core::{
    model::{JobState, Limits, PeerPreference, TorrentRow, clean_label},
    sequence::SKIP,
};

use crate::text::{t, tf};

#[cfg(windows)]
pub const FONT_CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\segoeui.ttf",
    r"C:\Windows\Fonts\arial.ttf",
    r"C:\Windows\Fonts\tahoma.ttf",
    r"C:\Windows\Fonts\verdana.ttf",
];

#[cfg(not(windows))]
pub const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
    "/usr/share/fonts/truetype/lato/Lato-Regular.ttf",
    "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/LiberationSans-Regular.ttf",
];

/// Matches the driver cap for configured public trackers.
pub const PUBLIC_TRACKER_CAP: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    Help,
    /// Windows: register as a magnet and .torrent handler for this user.
    Register,
    /// Windows: remove that registration.
    Unregister,
    Run {
        font: Option<PathBuf>,
        geoip_db: Option<PathBuf>,
        allow_local_trackers: bool,
        allow_local_urls: bool,
        payloads: Vec<String>,
    },
}

pub fn parse_args<I>(args: I) -> Result<Launch, String>
where
    I: IntoIterator<Item = String>,
{
    let mut font = None;
    let mut geoip_db = None;
    let mut allow_local_trackers = false;
    let mut allow_local_urls = false;
    let mut payloads = Vec::new();
    let mut positional = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if positional {
            push_payload(&mut payloads, arg);
            continue;
        }
        match arg.as_str() {
            "--help" | "-h" => return Ok(Launch::Help),
            "--register" => return Ok(Launch::Register),
            "--unregister" => return Ok(Launch::Unregister),
            "--" => positional = true,
            "--allow-local-trackers" => allow_local_trackers = true,
            "--allow-local-urls" => allow_local_urls = true,
            "--font" => {
                let path = args.next().ok_or(t("cli.missing_font_path"))?;
                font = Some(PathBuf::from(path));
            }
            "--geoip-db" => {
                let path = args.next().ok_or(t("cli.missing_geoip_path"))?;
                geoip_db = Some(PathBuf::from(path));
            }
            other if other.starts_with('-') => {
                return Err(tf("cli.unknown_argument", &[("argument", &other)]));
            }
            other => push_payload(&mut payloads, other.to_string()),
        }
    }
    let payloads = payloads
        .into_iter()
        .filter_map(|payload| {
            let trimmed = payload.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(normalize_magnet(&trimmed).unwrap_or(trimmed))
            }
        })
        .collect();
    Ok(Launch::Run {
        font,
        geoip_db,
        allow_local_trackers,
        allow_local_urls,
        payloads,
    })
}

/// A shell splits an unquoted magnet on `&`. Continuation arguments
/// (`dn=`, `tr=`, `xt.1=` and the other magnet parameters) join back onto
/// the magnet they belong to. A flag, another magnet, or a `.torrent` path
/// starts its own payload.
fn push_payload(payloads: &mut Vec<String>, arg: String) {
    if arg.is_empty() {
        return;
    }
    if let Some(last) = payloads.last_mut()
        && is_magnet_prefix(last)
        && is_magnet_continuation(&arg)
    {
        last.push('&');
        last.push_str(&arg);
        return;
    }
    payloads.push(arg);
}

fn is_magnet_prefix(text: &str) -> bool {
    let text = text.trim();
    text.len() >= 7 && text.as_bytes()[..7].eq_ignore_ascii_case(b"magnet:")
}

/// One layer of quotes or angle brackets, then a `magnet:` prefix. The
/// canonical form is [`normalize_magnet`].
fn looks_like_magnet(text: &str) -> bool {
    let mut text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() >= 2 {
        let (open, close) = (bytes[0], bytes[bytes.len() - 1]);
        if (open == b'"' && close == b'"')
            || (open == b'\'' && close == b'\'')
            || (open == b'<' && close == b'>')
        {
            text = text[1..text.len() - 1].trim();
        }
    }
    is_magnet_prefix(text)
}

fn is_magnet_continuation(arg: &str) -> bool {
    if arg.starts_with('-') || is_magnet_prefix(arg) {
        return false;
    }
    if Path::new(arg)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("torrent"))
    {
        return false;
    }
    let Some((key, _)) = arg.split_once('=') else {
        return false;
    };
    if key.is_empty() {
        return false;
    }
    matches!(
        magnet_param_base(key).to_ascii_lowercase().as_str(),
        "xt" | "dn" | "tr" | "ws" | "as" | "xs" | "kt" | "mt" | "x.pe" | "xl"
    )
}

fn magnet_param_base(key: &str) -> &str {
    if let Some((head, tail)) = key.rsplit_once('.')
        && !head.is_empty()
        && !tail.is_empty()
        && tail.bytes().all(|byte| byte.is_ascii_digit())
    {
        return head;
    }
    key
}

pub fn resolve_font(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok(path.to_path_buf())
        } else {
            Err(t("cli.font_not_found").into())
        };
    }
    #[cfg(windows)]
    if let Some(windir) = std::env::var_os("WINDIR").filter(|dir| !dir.is_empty()) {
        for name in ["segoeui.ttf", "arial.ttf", "tahoma.ttf", "verdana.ttf"] {
            let path = PathBuf::from(&windir).join("Fonts").join(name);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    for candidate in FONT_CANDIDATES {
        let path = Path::new(candidate);
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
    }
    if let Some(path) = font_from_fontconfig() {
        return Ok(path);
    }
    Err(t("cli.no_font").into())
}

/// Ask fontconfig for its default sans-serif face. This covers systems whose
/// fonts live outside [`FONT_CANDIDATES`]. Only TrueType files are accepted;
/// anything else falls through to the `--font` error above.
fn font_from_fontconfig() -> Option<PathBuf> {
    let output = std::process::Command::new("fc-match")
        .args(["sans-serif", "--format=%{file}\n"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.lines().next()?.trim();
    if path.is_empty() || !path.ends_with(".ttf") && !path.ends_with(".TTF") {
        return None;
    }
    let path = PathBuf::from(path);
    path.is_file().then_some(path)
}

/// Render an ISO country code for the Peers tab. Unknown is `--`, never a
/// fabricated code.
pub fn country_text(country: Option<[u8; 2]>) -> String {
    country
        .map(|code| String::from_utf8_lossy(&code).into_owned())
        .unwrap_or_else(|| t("peers.country_unknown").into())
}

pub fn preference_text(preference: PeerPreference) -> &'static str {
    match preference {
        PeerPreference::Prefer => t("peers.preference_prefer"),
        PeerPreference::Normal => t("peers.preference_normal"),
        PeerPreference::Avoid => t("peers.preference_avoid"),
        PeerPreference::Block => t("peers.preference_block"),
    }
}

/// Shown above the peer list. Preference is local: it changes which peers we
/// unchoke, keep and pipeline; it cannot make a remote client send faster.
pub fn preference_note() -> [&'static str; 2] {
    [t("peers.note_ip"), t("peers.note_block")]
}

/// The display name of a job state, for the job list and Details.
pub fn state_text(state: JobState) -> &'static str {
    match state {
        JobState::Queued => t("state.queued"),
        // A magnet whose info dictionary is still being fetched from peers.
        JobState::Metadata => t("state.metadata"),
        JobState::Checking => t("state.checking"),
        JobState::Downloading => t("state.downloading"),
        JobState::Complete => t("state.complete"),
        JobState::Seeding => t("state.seeding"),
        JobState::Paused => t("state.paused"),
        JobState::Removing => t("state.removing"),
        JobState::Error => t("state.error"),
    }
}

/// The user's home directory: `$HOME`, or `%USERPROFILE%` on Windows.
pub fn home_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    #[cfg(windows)]
    let home = home.or_else(|| std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()));
    home.map(PathBuf::from)
}

/// Base for per-user data: `$XDG_DATA_HOME`, else `%LOCALAPPDATA%` on
/// Windows, else `~/.local/share`.
pub fn data_home() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|base| !base.is_empty())
        .map(PathBuf::from);
    #[cfg(windows)]
    let base = base.or_else(|| {
        std::env::var_os("LOCALAPPDATA")
            .filter(|base| !base.is_empty())
            .map(PathBuf::from)
    });
    base.or_else(|| home_dir().map(|home| home.join(".local/share")))
}

/// `$XDG_DATA_HOME/templartorrent`, `%LOCALAPPDATA%/templartorrent` on
/// Windows, or `~/.local/share/templartorrent`.
pub fn data_dir() -> PathBuf {
    data_home()
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("templartorrent")
}

pub fn public_tracker_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|base| !base.is_empty())
        .map(PathBuf::from);
    #[cfg(windows)]
    let base = base.or_else(|| {
        std::env::var_os("APPDATA")
            .filter(|base| !base.is_empty())
            .map(PathBuf::from)
    });
    let base = base
        .or_else(|| home_dir().map(|home| home.join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("templartorrent").join("public-trackers.txt")
}

pub fn load_public_trackers(path: &Path) -> Vec<String> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    use std::io::Read;
    let mut text = String::new();
    if file.take(64 * 1024).read_to_string(&mut text).is_err() {
        return Vec::new();
    }
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .take(PUBLIC_TRACKER_CAP)
        .map(str::to_string)
        .collect()
}

/// Trim, drop one layer of quotes or angle brackets, and canonicalize the
/// `magnet:?` prefix. The query is left for the magnet parser.
pub fn normalize_magnet(text: &str) -> Result<String, &'static str> {
    let mut text = text.trim();
    if text.is_empty() {
        return Err(t("error.magnet_empty"));
    }
    let bytes = text.as_bytes();
    if bytes.len() >= 2 {
        let (open, close) = (bytes[0], bytes[bytes.len() - 1]);
        if (open == b'"' && close == b'"')
            || (open == b'\'' && close == b'\'')
            || (open == b'<' && close == b'>')
        {
            text = text[1..text.len() - 1].trim();
        }
    }
    if text.is_empty() {
        return Err(t("error.magnet_empty"));
    }
    if text.len() > 16 * 1024 {
        return Err(t("error.magnet_too_long"));
    }
    if text.contains(['\n', '\r', '\0']) {
        return Err(t("error.magnet_one_line"));
    }
    if text.len() < 8 || !text.as_bytes()[..8].eq_ignore_ascii_case(b"magnet:?") {
        return Err(t("error.magnet_prefix"));
    }
    let mut canonical = String::from("magnet:?");
    canonical.push_str(&text[8..]);
    Ok(canonical)
}

pub fn validate_magnet(text: &str) -> Result<(), &'static str> {
    normalize_magnet(text).map(|_| ())
}

/// What the Add dialog's source field holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddSource {
    Magnet(String),
    /// One URL or a batch of at most [`MAX_URL_BATCH`], each its own job.
    Urls(Vec<String>),
}

pub const MAX_URL_BATCH: usize = templartorrent_rbit_adapter::http::MAX_BATCH;

/// A magnet, or whitespace-separated http(s) URLs. Mixing the two, an
/// unknown scheme, duplicates and more than the batch limit are refused.
pub fn classify_source(text: &str) -> Result<AddSource, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(t("error.source_empty").into());
    }
    if looks_like_magnet(text) {
        // A pasted magnet may carry an unencoded space in its name; a second
        // link after whitespace is a batch, which magnets do not form.
        let another = text.split_whitespace().skip(1).any(|token| {
            is_magnet_prefix(token) || token.starts_with("http://") || token.starts_with("https://")
        });
        if another {
            return Err(t("error.magnet_mixed").into());
        }
        // Quoted, bracketed and differently cased prefixes become `magnet:?`.
        return normalize_magnet(text)
            .map(AddSource::Magnet)
            .map_err(str::to_string);
    }
    if text.len() > 64 * 1024 {
        return Err(t("error.url_list_too_long").into());
    }
    let mut urls: Vec<String> = Vec::new();
    for token in text.split_whitespace() {
        if token.starts_with("magnet:?") {
            return Err(t("error.magnet_mixed").into());
        }
        let parsed = templartorrent_rbit_adapter::http::parse_url(token)
            .map_err(|reason| tf("error.url_invalid", &[("url", &token), ("detail", &reason)]))?;
        if urls.contains(&parsed.url) {
            continue;
        }
        urls.push(parsed.url);
        if urls.len() > MAX_URL_BATCH {
            return Err(tf(
                "error.url_batch_too_large",
                &[("count", &MAX_URL_BATCH)],
            ));
        }
    }
    Ok(AddSource::Urls(urls))
}

/// Empty means no check; otherwise 64 hex digits.
pub fn parse_optional_sha256(text: &str) -> Result<Option<[u8; 32]>, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    templartorrent_rbit_adapter::http::parse_sha256(text)
        .map(Some)
        .map_err(|_| t("error.sha256_invalid"))
}

pub fn validate_destination(text: &str) -> Result<(), &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Err(t("error.destination_empty"));
    }
    if text.contains('\0') {
        return Err(t("error.destination_null"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub struct LimitForm<'a> {
    pub download_jobs: &'a str,
    pub seed_jobs: &'a str,
    pub upload_slots: &'a str,
    pub connections: &'a str,
    pub per_job: &'a str,
    pub down_kb: &'a str,
    pub up_kb: &'a str,
    pub memory_mib: &'a str,
}

pub fn parse_limits(form: LimitForm<'_>) -> Result<Limits, &'static str> {
    use LimitField::*;
    Ok(Limits {
        download_jobs: parse_usize(form.download_jobs, DownloadJobs, 10_000)?,
        seed_jobs: parse_usize(form.seed_jobs, SeedJobs, 10_000)?,
        upload_peer_slots: parse_usize(form.upload_slots, UploadSlots, 10_000)?,
        connections_total: parse_usize(form.connections, Connections, 100_000)?,
        connections_per_job: parse_usize(form.per_job, PerJob, 100_000)?,
        download_kb_s: parse_u64(form.down_kb, DownKb)?,
        upload_kb_s: parse_u64(form.up_kb, UpKb)?,
        payload_budget_bytes: parse_memory(form.memory_mib)?,
    })
}

/// Which Limits field a validation message is about.
#[derive(Clone, Copy)]
enum LimitField {
    DownloadJobs,
    SeedJobs,
    UploadSlots,
    Connections,
    PerJob,
    DownKb,
    UpKb,
    Memory,
}

fn parse_usize(text: &str, field: LimitField, max: usize) -> Result<usize, &'static str> {
    let value = parse_u64(text, field)?;
    let value = usize::try_from(value).map_err(|_| too_large(field))?;
    if value > max {
        return Err(too_large(field));
    }
    Ok(value)
}

fn parse_u64(text: &str, field: LimitField) -> Result<u64, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Err(empty(field));
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(not_numeric(field));
    }
    text.parse().map_err(|_| too_large(field))
}

fn parse_memory(text: &str) -> Result<usize, &'static str> {
    let mib = parse_u64(text, LimitField::Memory)?;
    if mib == 0 {
        return Err(t("limits.memory_too_small"));
    }
    if mib > 8192 {
        return Err(t("limits.memory_over_cap"));
    }
    Ok((mib as usize) * 1024 * 1024)
}

fn empty(field: LimitField) -> &'static str {
    match field {
        LimitField::DownloadJobs => t("limits.download_jobs_empty"),
        LimitField::SeedJobs => t("limits.seed_jobs_empty"),
        LimitField::UploadSlots => t("limits.upload_slots_empty"),
        LimitField::Connections => t("limits.connections_empty"),
        LimitField::PerJob => t("limits.per_job_empty"),
        LimitField::DownKb => t("limits.down_kb_empty"),
        LimitField::UpKb => t("limits.up_kb_empty"),
        LimitField::Memory => t("limits.memory_empty"),
    }
}

fn not_numeric(field: LimitField) -> &'static str {
    match field {
        LimitField::DownloadJobs => t("limits.download_jobs_not_number"),
        LimitField::SeedJobs => t("limits.seed_jobs_not_number"),
        LimitField::UploadSlots => t("limits.upload_slots_not_number"),
        LimitField::Connections => t("limits.connections_not_number"),
        LimitField::PerJob => t("limits.per_job_not_number"),
        LimitField::DownKb => t("limits.down_kb_not_number"),
        LimitField::UpKb => t("limits.up_kb_not_number"),
        LimitField::Memory => t("limits.memory_not_number"),
    }
}

fn too_large(field: LimitField) -> &'static str {
    match field {
        LimitField::DownloadJobs => t("limits.download_jobs_too_large"),
        LimitField::SeedJobs => t("limits.seed_jobs_too_large"),
        LimitField::UploadSlots => t("limits.upload_slots_too_large"),
        LimitField::Connections => t("limits.connections_too_large"),
        LimitField::PerJob => t("limits.per_job_too_large"),
        LimitField::DownKb => t("limits.down_kb_too_large"),
        LimitField::UpKb => t("limits.up_kb_too_large"),
        LimitField::Memory => t("limits.memory_too_large"),
    }
}

pub fn rank_label(rank: u32, wanted: bool, verified: u64, length: u64) -> String {
    if !wanted {
        return t("files.rank_skipped").into();
    }
    if verified >= length {
        return t("files.rank_complete").into();
    }
    if rank == SKIP {
        return t("files.rank_skipped").into();
    }
    tf("files.rank_auto", &[("position", &ordinal(rank + 1))])
}

fn ordinal(n: u32) -> String {
    let values: &[(&str, &dyn std::fmt::Display)] = &[("n", &n)];
    match n % 100 {
        11..=13 => tf("files.ordinal_th", values),
        _ => match n % 10 {
            1 => tf("files.ordinal_st", values),
            2 => tf("files.ordinal_nd", values),
            3 => tf("files.ordinal_rd", values),
            _ => tf("files.ordinal_th", values),
        },
    }
}

pub fn priority_order(ranks: &[u32]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..ranks.len()).collect();
    order.sort_by_key(|&index| (ranks[index], index));
    order
}

pub fn move_file(ranks: &[u32], index: usize, earlier: bool) -> Option<Vec<usize>> {
    let mut order = priority_order(ranks);
    let pos = order.iter().position(|&item| item == index)?;
    let next = if earlier {
        pos.checked_sub(1)?
    } else {
        let next = pos + 1;
        if next >= order.len() {
            return None;
        }
        next
    };
    order.swap(pos, next);
    Some(order)
}

/// The mark shown for a value that is unknown or not applicable.
pub fn unknown() -> &'static str {
    t("common.unknown")
}

/// A whole percentage, e.g. `42%`.
pub fn percent_text(percent: u64) -> String {
    tf("units.percent", &[("value", &percent)])
}

pub fn rate_text(bytes_per_second: u64) -> String {
    if bytes_per_second == 0 {
        unknown().into()
    } else {
        tf("units.kb_per_s", &[("value", &(bytes_per_second / 1000))])
    }
}

/// Block round trip for the peer row. Sub-millisecond samples stay visible.
pub fn rtt_text(rtt_us: Option<u64>) -> String {
    match rtt_us {
        None => unknown().into(),
        Some(us) if us / 1000 == 0 => t("units.ms_below_one").into(),
        Some(us) => tf("units.ms", &[("value", &(us / 1000))]),
    }
}

pub fn eta_text(row: &TorrentRow) -> String {
    if row.wanted_bytes == 0 || matches!(row.state, JobState::Paused | JobState::Queued) {
        return unknown().into();
    }
    match row.eta_seconds() {
        None => unknown().into(),
        Some(0) => t("units.eta_done").into(),
        Some(seconds) => tf("units.seconds", &[("value", &seconds)]),
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    let scaled = |unit: u64| format!("{:.1}", bytes as f64 / unit as f64);
    if bytes >= GIB {
        tf("units.gib", &[("value", &scaled(GIB))])
    } else if bytes >= MIB {
        tf("units.mib", &[("value", &scaled(MIB))])
    } else if bytes >= KIB {
        tf("units.kib", &[("value", &scaled(KIB))])
    } else {
        tf("units.bytes", &[("value", &bytes)])
    }
}

pub fn affected_bytes(lengths: &[u64]) -> u64 {
    lengths
        .iter()
        .fold(0, |sum, length| sum.saturating_add(*length))
}

pub fn job_line(row: &TorrentRow) -> String {
    let progress = match row
        .verified_bytes
        .saturating_mul(100)
        .checked_div(row.wanted_bytes)
    {
        Some(percent) => percent_text(percent),
        None => unknown().to_string(),
    };
    tf(
        "jobs.line",
        &[
            ("name", &clean_label(&row.name, 60)),
            ("progress", &progress),
            ("down", &rate_text(row.down_bps)),
            ("up", &rate_text(row.up_bps)),
            ("seeds", &row.connected_seeds),
            ("peers", &row.connected_peers),
            ("state", &state_text(row.state)),
        ],
    )
}

pub fn speed_cap(kb_s: u64) -> String {
    if kb_s == 0 {
        t("units.unlimited").into()
    } else {
        tf("units.kb_per_s", &[("value", &kb_s)])
    }
}

pub fn cooldown_text(now_s: u64, due_s: u64) -> String {
    if now_s >= due_s {
        t("trackers.ready").into()
    } else {
        tf("trackers.cooldown", &[("seconds", &(due_s - now_s))])
    }
}

/// The window title: the active download's name, a count of active jobs,
/// or of seeding jobs. Same rules as the core `window_title`, with every
/// word from the messages file.
pub fn app_title(jobs: &[TorrentRow]) -> String {
    let mut active = jobs.iter().filter(|job| {
        matches!(
            job.state,
            JobState::Downloading | JobState::Metadata | JobState::Checking
        )
    });
    if let Some(first) = active.next() {
        let others = active.count();
        return if others == 0 {
            tf(
                "title.one_active",
                &[("name", &clean_label(&first.name, 160))],
            )
        } else {
            tf("title.many_active", &[("count", &(others + 1))])
        };
    }
    let seeding = jobs
        .iter()
        .filter(|job| job.state == JobState::Seeding)
        .count();
    if seeding > 0 {
        return tf("title.seeding", &[("count", &seeding)]);
    }
    t("title.app").into()
}

// How often the displayed payload rate is recomputed.
pub const RATE_SAMPLE_MS: u64 = 3_000;

/// Smoothed payload rate carried between driver steps. Bytes accumulate until
/// [`RATE_SAMPLE_MS`] elapses, then each direction is blended from its delta.
/// A direction with no bytes in that window drops to zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateCarry {
    pub downloaded: u64,
    pub uploaded: u64,
    pub down_bps: u64,
    pub up_bps: u64,
    pub window_ms: u64,
}

pub fn advance_rate(previous: RateCarry, downloaded: u64, uploaded: u64, dt_ms: u64) -> RateCarry {
    let mut next = previous;
    next.window_ms = next.window_ms.saturating_add(dt_ms);
    if next.window_ms < RATE_SAMPLE_MS {
        return next;
    }
    let down_delta = downloaded.saturating_sub(previous.downloaded);
    let up_delta = uploaded.saturating_sub(previous.uploaded);
    next.down_bps = if down_delta == 0 {
        0
    } else {
        blend_rate(previous.down_bps, per_second(down_delta, next.window_ms))
    };
    next.up_bps = if up_delta == 0 {
        0
    } else {
        blend_rate(previous.up_bps, per_second(up_delta, next.window_ms))
    };
    next.downloaded = downloaded;
    next.uploaded = uploaded;
    next.window_ms = 0;
    next
}

fn per_second(delta: u64, elapsed_ms: u64) -> u64 {
    if elapsed_ms == 0 {
        0
    } else {
        u64::try_from(u128::from(delta) * 1000 / u128::from(elapsed_ms)).unwrap_or(u64::MAX)
    }
}

fn blend_rate(previous: u64, instant: u64) -> u64 {
    if previous == 0 {
        instant
    } else {
        u64::try_from((u128::from(previous) * 3 + u128::from(instant)) / 4).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use templartorrent_core::model::TorrentId;

    fn row(state: JobState, wanted: u64, verified: u64, down: u64) -> TorrentRow {
        TorrentRow {
            id: TorrentId(1),
            name: Arc::from("episode"),
            state,
            wanted_bytes: wanted,
            verified_bytes: verified,
            total_payload_downloaded: verified,
            total_payload_uploaded: 0,
            down_bps: down,
            up_bps: 0,
            connected_seeds: 1,
            connected_peers: 2,
            estimated_seeds: None,
            estimated_peers: None,
        }
    }

    #[test]
    fn limits_reject_empty_text_and_zero_memory() {
        let mut form = LimitForm {
            download_jobs: "3",
            seed_jobs: "3",
            upload_slots: "8",
            connections: "128",
            per_job: "48",
            down_kb: "0",
            up_kb: "0",
            memory_mib: "48",
        };
        let limits = parse_limits(form).unwrap();
        assert_eq!(limits.download_kb_s, 0);
        assert_eq!(limits.payload_budget_bytes, 48 * 1024 * 1024);
        form.memory_mib = "0";
        assert_eq!(
            parse_limits(form).unwrap_err(),
            t("limits.memory_too_small")
        );
        form.memory_mib = "48";
        form.down_kb = "";
        assert_eq!(parse_limits(form).unwrap_err(), t("limits.down_kb_empty"));
        form.down_kb = "fast";
        assert_eq!(
            parse_limits(form).unwrap_err(),
            t("limits.down_kb_not_number")
        );
        form.down_kb = "99999999999999999999999";
        assert_eq!(
            parse_limits(form).unwrap_err(),
            t("limits.down_kb_too_large")
        );
    }

    #[test]
    fn magnet_and_destination_rules() {
        assert!(validate_magnet("").is_err());
        assert!(validate_magnet("https://example").is_err());
        assert!(validate_magnet("magnet:?xt=urn:btih:aa").is_ok());
        assert!(validate_magnet("Magnet:?xt=urn:btih:aa").is_ok());
        assert_eq!(
            normalize_magnet("\"Magnet:?xt=urn:btih:aa\"").unwrap(),
            "magnet:?xt=urn:btih:aa"
        );
        assert!(validate_magnet(&format!("magnet:?{}", "a".repeat(20_000))).is_err());
        assert!(validate_destination("").is_err());
        assert!(validate_destination("/tmp/shows").is_ok());
    }

    #[test]
    fn ranks_and_order_follow_the_file_policy() {
        let auto = |key: &'static str, n: u32| {
            let position = tf(key, &[("n", &n)]);
            tf("files.rank_auto", &[("position", &position)])
        };
        assert_eq!(rank_label(0, true, 0, 10), auto("files.ordinal_st", 1));
        assert_eq!(rank_label(1, true, 0, 10), auto("files.ordinal_nd", 2));
        assert_eq!(rank_label(2, true, 0, 10), auto("files.ordinal_rd", 3));
        assert_eq!(rank_label(10, true, 0, 10), auto("files.ordinal_th", 11));
        assert_eq!(rank_label(20, true, 0, 10), auto("files.ordinal_st", 21));
        assert_eq!(rank_label(0, false, 0, 10), t("files.rank_skipped"));
        assert_eq!(rank_label(SKIP, true, 0, 10), t("files.rank_skipped"));
        assert_eq!(rank_label(SKIP, true, 10, 10), t("files.rank_complete"));
        let ranks = [1, 0, SKIP];
        assert_eq!(move_file(&ranks, 1, true), None);
        assert_eq!(move_file(&ranks, 0, true), Some(vec![0, 1, 2]));
    }

    #[test]
    fn smoothed_rates_wait_for_a_window_and_then_go_quiet() {
        let base = RateCarry {
            downloaded: 0,
            uploaded: 0,
            down_bps: 0,
            up_bps: 0,
            window_ms: 0,
        };
        let held = advance_rate(base, 100, 0, 100);
        assert_eq!(held.down_bps, 0);
        assert_eq!(held.downloaded, 0);
        assert_eq!(held.window_ms, 100);
        let still = advance_rate(held, 1_000, 50, 2_000);
        assert_eq!(still.down_bps, 0);
        assert_eq!(still.up_bps, 0);
        assert_eq!(still.downloaded, 0);
        assert_eq!(still.window_ms, 2_100);
        let rated = advance_rate(still, 1_000, 50, 900);
        assert_eq!(rated.down_bps, 333);
        assert_eq!(rated.up_bps, 16);
        assert_eq!(rated.downloaded, 1_000);
        assert_eq!(rated.uploaded, 50);
        assert_eq!(rated.window_ms, 0);
        let blended = advance_rate(rated, 4_000, 50, 3_000);
        assert_eq!(blended.down_bps, (333 * 3 + 1_000) / 4);
        assert_eq!(blended.up_bps, 0);
        let quiet = advance_rate(blended, 4_000, 50, 3_000);
        assert_eq!(quiet.down_bps, 0);
        assert_eq!(quiet.up_bps, 0);
    }

    #[test]
    fn split_magnet_arguments_rejoin() {
        let launch = parse_args([
            "magnet:?xt=urn:btih:abc".into(),
            "dn=Name".into(),
            "tr=http://tracker.example/announce".into(),
            "file.torrent".into(),
        ])
        .unwrap();
        match launch {
            Launch::Run { payloads, .. } => {
                assert_eq!(
                    payloads,
                    vec![
                        "magnet:?xt=urn:btih:abc&dn=Name&tr=http://tracker.example/announce"
                            .to_string(),
                        "file.torrent".to_string(),
                    ]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_rates_and_eta_use_an_em_dash() {
        let idle = row(JobState::Downloading, 100, 10, 0);
        assert_eq!(rate_text(0), unknown());
        assert_eq!(rtt_text(None), unknown());
        assert_eq!(rtt_text(Some(0)), t("units.ms_below_one"));
        assert_eq!(rtt_text(Some(999)), t("units.ms_below_one"));
        assert_eq!(rtt_text(Some(5_000)), tf("units.ms", &[("value", &5)]));
        assert_eq!(eta_text(&idle), unknown());
        assert!(job_line(&idle).contains(unknown()));
        let done = row(JobState::Complete, 100, 100, 0);
        assert_eq!(eta_text(&done), t("units.eta_done"));
        assert_eq!(app_title(&[]), t("title.app"));
        let seeding = row(JobState::Seeding, 100, 100, 0);
        assert_eq!(
            app_title(std::slice::from_ref(&seeding)),
            tf("title.seeding", &[("count", &1)])
        );
        let active = row(JobState::Downloading, 100, 10, 0);
        assert_eq!(
            app_title(&[active.clone(), seeding]),
            tf("title.one_active", &[("name", &"episode")])
        );
        assert_eq!(
            app_title(&[active.clone(), active]),
            tf("title.many_active", &[("count", &2)])
        );
        assert_eq!(format_bytes(512), tf("units.bytes", &[("value", &512)]));
        assert_eq!(
            format_bytes(3 * 1024 * 1024 / 2),
            tf("units.mib", &[("value", &"1.5")])
        );
    }

    #[test]
    fn args_keep_magnets_after_the_double_dash() {
        let launch = parse_args([
            "--allow-local-trackers".into(),
            "--".into(),
            "--font".into(),
            "magnet:?xt=urn:btih:abc".into(),
        ])
        .unwrap();
        match launch {
            Launch::Run {
                allow_local_trackers,
                allow_local_urls,
                payloads,
                font,
                geoip_db,
            } => {
                assert!(allow_local_trackers);
                assert!(!allow_local_urls);
                assert!(font.is_none());
                assert!(geoip_db.is_none());
                assert_eq!(
                    payloads,
                    vec!["--font".to_string(), "magnet:?xt=urn:btih:abc".to_string()]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sources_split_into_a_magnet_or_a_bounded_url_batch() {
        assert_eq!(
            classify_source("magnet:?xt=urn:btih:aa"),
            Ok(AddSource::Magnet("magnet:?xt=urn:btih:aa".into()))
        );
        assert_eq!(
            classify_source("\"Magnet:?xt=urn:btih:aa\""),
            Ok(AddSource::Magnet("magnet:?xt=urn:btih:aa".into()))
        );
        assert_eq!(
            classify_source(
                " https://a.example/f.iso\nhttp://b.example/g?x=1 https://a.example/f.iso "
            ),
            Ok(AddSource::Urls(vec![
                "https://a.example/f.iso".into(),
                "http://b.example/g?x=1".into()
            ]))
        );
        assert!(classify_source("").is_err());
        assert!(
            classify_source("ftp://a.example/f")
                .unwrap_err()
                .contains("scheme")
        );
        assert!(
            classify_source("magnet:?xt=urn:btih:aa http://b.example/g").unwrap_err()
                == t("error.magnet_mixed")
        );
        let many: Vec<String> = (0..MAX_URL_BATCH + 1)
            .map(|i| format!("http://h.example/{i}"))
            .collect();
        assert!(
            classify_source(&many.join(" ")).unwrap_err()
                == tf("error.url_batch_too_large", &[("count", &MAX_URL_BATCH)])
        );
        assert_eq!(parse_optional_sha256("  "), Ok(None));
        assert!(parse_optional_sha256(&"0".repeat(64)).unwrap().is_some());
        assert_eq!(parse_optional_sha256("abc"), Err(t("error.sha256_invalid")));
        let launch =
            parse_args(["--allow-local-urls".into(), "https://h.example/x".into()]).unwrap();
        assert!(matches!(
            launch,
            Launch::Run {
                allow_local_urls: true,
                ..
            }
        ));
    }

    #[test]
    fn geoip_db_flag_parses_and_requires_a_path() {
        let launch = parse_args(["--geoip-db".into(), "/data/country.mmdb".into()]).unwrap();
        match launch {
            Launch::Run { geoip_db, .. } => {
                assert_eq!(geoip_db, Some(PathBuf::from("/data/country.mmdb")));
            }
            other => panic!("{other:?}"),
        }
        assert!(parse_args(["--geoip-db".into()]).is_err());
    }

    #[test]
    fn explicit_missing_font_is_an_error() {
        let missing = Path::new("/nonexistent/font.ttf");
        assert!(resolve_font(Some(missing)).is_err());
    }

    #[test]
    fn unknown_countries_render_as_a_double_dash() {
        assert_eq!(country_text(None), t("peers.country_unknown"));
        assert_eq!(country_text(Some(*b"US")), "US");
    }

    #[test]
    fn preference_labels_match_the_four_choices() {
        assert_eq!(
            preference_text(PeerPreference::Prefer),
            t("peers.preference_prefer")
        );
        assert_eq!(
            preference_text(PeerPreference::Normal),
            t("peers.preference_normal")
        );
        assert_eq!(
            preference_text(PeerPreference::Avoid),
            t("peers.preference_avoid")
        );
        assert_eq!(
            preference_text(PeerPreference::Block),
            t("peers.preference_block")
        );
        assert_eq!(preference_note()[1], t("peers.note_block"));
    }
}
