//! Pure dialog and display rules. No window, network, or filesystem mutation.

use std::path::{Path, PathBuf};

use templartorrent_core::{
    model::{JobState, Limits, PeerPreference, TorrentRow, clean_label},
    sequence::{SKIP, natural_cmp},
};

pub const USAGE: &str = "\
usage: templartorrent [--font FILE] [--geoip-db FILE] [--allow-local-trackers] [--] [MAGNET | FILE.torrent]...

A second launch forwards each magnet or .torrent path to the running instance
and exits. The running instance opens the Add dialog; it does not start a
transfer until Add is confirmed.

Peer country lookup uses --geoip-db FILE, $TEMPLARTORRENT_GEOIP_DB, or
~/.local/share/templartorrent/GeoLite2-Country.mmdb. Without a database,
peers show -- for the country.

Jobs, limits and peer preferences are saved under
$XDG_DATA_HOME/templartorrent/session (default ~/.local/share) and restored
on the next launch after a disk recheck. Removing a job deletes its record.
";

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
    Run {
        font: Option<PathBuf>,
        geoip_db: Option<PathBuf>,
        allow_local_trackers: bool,
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
            "--" => positional = true,
            "--allow-local-trackers" => allow_local_trackers = true,
            "--font" => {
                let path = args.next().ok_or("missing path after --font")?;
                font = Some(PathBuf::from(path));
            }
            "--geoip-db" => {
                let path = args.next().ok_or("missing path after --geoip-db")?;
                geoip_db = Some(PathBuf::from(path));
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown argument {other}"));
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
            Err("font file was not found".into())
        };
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
    Err("no font was found; pass --font with a static TrueType file".into())
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
        .unwrap_or_else(|| "--".into())
}

pub fn preference_text(preference: PeerPreference) -> &'static str {
    match preference {
        PeerPreference::Prefer => "Prefer",
        PeerPreference::Normal => "Normal",
        PeerPreference::Avoid => "Avoid",
        PeerPreference::Block => "Block",
    }
}

/// Shown above the peer list. Preference is local: it changes which peers we
/// unchoke, keep and pipeline; it cannot make a remote client send faster.
pub const PREFERENCE_NOTE: [&str; 2] = [
    "Applies to the selected peer's IP address for this job.",
    "Block closes it and refuses reconnection; blocking the only supplier can stall completion.",
];

/// `$XDG_DATA_HOME/templartorrent` or `~/.local/share/templartorrent`.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from(".local/share"));
    base.join("templartorrent")
}

pub fn public_tracker_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
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
        return Err("magnet is empty");
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
        return Err("magnet is empty");
    }
    if text.len() > 16 * 1024 {
        return Err("magnet exceeds 16 KiB limit");
    }
    if text.contains(['\n', '\r', '\0']) {
        return Err("magnet must be one line");
    }
    if text.len() < 8 || !text.as_bytes()[..8].eq_ignore_ascii_case(b"magnet:?") {
        return Err("magnet must start with magnet:?");
    }
    let mut canonical = String::from("magnet:?");
    canonical.push_str(&text[8..]);
    Ok(canonical)
}

pub fn validate_magnet(text: &str) -> Result<(), &'static str> {
    normalize_magnet(text).map(|_| ())
}

pub fn validate_destination(text: &str) -> Result<(), &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Err("choose a destination folder");
    }
    if text.contains('\0') {
        return Err("destination contains a null");
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
    Ok(Limits {
        download_jobs: parse_usize(form.download_jobs, "download jobs", 10_000)?,
        seed_jobs: parse_usize(form.seed_jobs, "seed jobs", 10_000)?,
        upload_peer_slots: parse_usize(form.upload_slots, "upload peer slots", 10_000)?,
        connections_total: parse_usize(form.connections, "max connections", 100_000)?,
        connections_per_job: parse_usize(form.per_job, "max per job", 100_000)?,
        download_kb_s: parse_u64(form.down_kb, "download kB/s")?,
        upload_kb_s: parse_u64(form.up_kb, "upload kB/s")?,
        payload_budget_bytes: parse_memory(form.memory_mib)?,
    })
}

fn parse_usize(text: &str, label: &str, max: usize) -> Result<usize, &'static str> {
    let value = parse_u64(text, label)?;
    let value = usize::try_from(value).map_err(|_| too_large(label))?;
    if value > max {
        return Err(too_large(label));
    }
    Ok(value)
}

fn parse_u64(text: &str, label: &str) -> Result<u64, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Err(empty(label));
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(not_numeric(label));
    }
    text.parse().map_err(|_| too_large(label))
}

fn parse_memory(text: &str) -> Result<usize, &'static str> {
    let mib = parse_u64(text, "payload memory")?;
    if mib == 0 {
        return Err("payload memory must be at least 1 MiB");
    }
    if mib > 8192 {
        return Err("payload memory is limited to 8192 MiB");
    }
    Ok((mib as usize) * 1024 * 1024)
}

fn empty(label: &str) -> &'static str {
    match label {
        "download jobs" => "download jobs is empty",
        "seed jobs" => "seed jobs is empty",
        "upload peer slots" => "upload peer slots is empty",
        "max connections" => "max connections is empty",
        "max per job" => "max per job is empty",
        "download kB/s" => "download kB/s is empty",
        "upload kB/s" => "upload kB/s is empty",
        _ => "payload memory is empty",
    }
}

fn not_numeric(label: &str) -> &'static str {
    match label {
        "download jobs" => "download jobs must be a whole number",
        "seed jobs" => "seed jobs must be a whole number",
        "upload peer slots" => "upload peer slots must be a whole number",
        "max connections" => "max connections must be a whole number",
        "max per job" => "max per job must be a whole number",
        "download kB/s" => "download kB/s must be a whole number",
        "upload kB/s" => "upload kB/s must be a whole number",
        _ => "payload memory must be a whole number",
    }
}

fn too_large(label: &str) -> &'static str {
    match label {
        "download jobs" => "download jobs is too large",
        "seed jobs" => "seed jobs is too large",
        "upload peer slots" => "upload peer slots is too large",
        "max connections" => "max connections is too large",
        "max per job" => "max per job is too large",
        "download kB/s" => "download kB/s is too large",
        "upload kB/s" => "upload kB/s is too large",
        _ => "payload memory is too large",
    }
}

pub fn rank_label(rank: u32, wanted: bool, verified: u64, length: u64) -> String {
    if !wanted {
        return "Skipped".into();
    }
    if verified >= length {
        return "Complete".into();
    }
    if rank == SKIP {
        return "Skipped".into();
    }
    format!("Auto: {}", ordinal(rank + 1))
}

fn ordinal(n: u32) -> String {
    let suffix = match n % 100 {
        11..=13 => "th",
        _ => match n % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };
    format!("{n}{suffix}")
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

pub fn automatic_order(paths: &[impl AsRef<str>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..paths.len()).collect();
    order.sort_by(|&left, &right| {
        natural_cmp(paths[left].as_ref(), paths[right].as_ref()).then(left.cmp(&right))
    });
    order
}

pub fn rate_text(bytes_per_second: u64) -> String {
    if bytes_per_second == 0 {
        "—".into()
    } else {
        format!("{} kB/s", bytes_per_second / 1000)
    }
}

/// Block round trip for the peer row. Sub-millisecond samples stay visible.
pub fn rtt_text(rtt_us: Option<u64>) -> String {
    match rtt_us {
        None => "—".into(),
        Some(us) if us / 1000 == 0 => "<1 ms".into(),
        Some(us) => format!("{} ms", us / 1000),
    }
}

pub fn eta_text(row: &TorrentRow) -> String {
    if row.wanted_bytes == 0 || matches!(row.state, JobState::Paused | JobState::Queued) {
        return "—".into();
    }
    match row.eta_seconds() {
        None => "—".into(),
        Some(0) => "done".into(),
        Some(seconds) => format!("{seconds} s"),
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
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
        Some(percent) => format!("{percent}%"),
        None => "—".to_string(),
    };
    format!(
        "{}    {}    {}    {}    {} / {}    {:?}",
        clean_label(&row.name, 60),
        progress,
        rate_text(row.down_bps),
        rate_text(row.up_bps),
        row.connected_seeds,
        row.connected_peers,
        row.state
    )
}

pub fn speed_cap(kb_s: u64) -> String {
    if kb_s == 0 {
        "unlimited".into()
    } else {
        format!("{kb_s} kB/s")
    }
}

pub fn cooldown_text(now_s: u64, due_s: u64) -> String {
    if now_s >= due_s {
        "ready".into()
    } else {
        format!("cooldown {} s", due_s - now_s)
    }
}

pub fn app_title(jobs: &[TorrentRow]) -> String {
    let title = templartorrent_core::model::window_title(jobs);
    if title == "TemplarTorrent" {
        let seeding = jobs
            .iter()
            .filter(|job| job.state == JobState::Seeding)
            .count();
        if seeding > 0 {
            return format!("{seeding} seeding — TemplarTorrent");
        }
    }
    title
}

/// How often the displayed payload rate is recomputed.
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
            "payload memory must be at least 1 MiB"
        );
        form.memory_mib = "48";
        form.down_kb = "";
        assert_eq!(parse_limits(form).unwrap_err(), "download kB/s is empty");
        form.down_kb = "fast";
        assert!(parse_limits(form).unwrap_err().contains("whole number"));
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
        assert_eq!(rank_label(0, true, 0, 10), "Auto: 1st");
        assert_eq!(rank_label(1, true, 0, 10), "Auto: 2nd");
        assert_eq!(rank_label(2, true, 0, 10), "Auto: 3rd");
        assert_eq!(rank_label(10, true, 0, 10), "Auto: 11th");
        assert_eq!(rank_label(0, false, 0, 10), "Skipped");
        assert_eq!(rank_label(SKIP, true, 0, 10), "Skipped");
        assert_eq!(rank_label(SKIP, true, 10, 10), "Complete");
        let ranks = [1, 0, SKIP];
        assert_eq!(move_file(&ranks, 1, true), None);
        assert_eq!(move_file(&ranks, 0, true), Some(vec![0, 1, 2]));
        let paths = ["Episode 2", "Episode 10", "Episode 1"];
        assert_eq!(automatic_order(&paths), vec![2, 0, 1]);
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
            Launch::Help => panic!("help"),
        }
    }

    #[test]
    fn unknown_rates_and_eta_use_an_em_dash() {
        let idle = row(JobState::Downloading, 100, 10, 0);
        assert_eq!(rate_text(0), "—");
        assert_eq!(rtt_text(None), "—");
        assert_eq!(rtt_text(Some(0)), "<1 ms");
        assert_eq!(rtt_text(Some(999)), "<1 ms");
        assert_eq!(rtt_text(Some(5_000)), "5 ms");
        assert_eq!(eta_text(&idle), "—");
        assert!(job_line(&idle).contains("—"));
        let done = row(JobState::Complete, 100, 100, 0);
        assert_eq!(eta_text(&done), "done");
        assert_eq!(app_title(&[]), "TemplarTorrent");
        let seeding = row(JobState::Seeding, 100, 100, 0);
        assert_eq!(app_title(&[seeding]), "1 seeding — TemplarTorrent");
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
                payloads,
                font,
                geoip_db,
            } => {
                assert!(allow_local_trackers);
                assert!(font.is_none());
                assert!(geoip_db.is_none());
                assert_eq!(
                    payloads,
                    vec!["--font".to_string(), "magnet:?xt=urn:btih:abc".to_string()]
                );
            }
            Launch::Help => panic!("help"),
        }
    }

    #[test]
    fn geoip_db_flag_parses_and_requires_a_path() {
        let launch = parse_args(["--geoip-db".into(), "/data/country.mmdb".into()]).unwrap();
        match launch {
            Launch::Run { geoip_db, .. } => {
                assert_eq!(geoip_db, Some(PathBuf::from("/data/country.mmdb")));
            }
            Launch::Help => panic!("help"),
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
        assert_eq!(country_text(None), "--");
        assert_eq!(country_text(Some(*b"US")), "US");
    }

    #[test]
    fn preference_labels_match_the_four_choices() {
        assert_eq!(preference_text(PeerPreference::Prefer), "Prefer");
        assert_eq!(preference_text(PeerPreference::Normal), "Normal");
        assert_eq!(preference_text(PeerPreference::Avoid), "Avoid");
        assert_eq!(preference_text(PeerPreference::Block), "Block");
        assert!(PREFERENCE_NOTE[1].contains("stall completion"));
    }
}
