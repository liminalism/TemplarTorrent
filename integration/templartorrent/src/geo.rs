//! Optional offline IP-to-country lookup for the Peers tab.
//!
//! Resolution is local and lazy: the user-supplied MaxMind DB (GeoLite2
//! Country or City) is opened on the first lookup, never at startup, and at
//! most [`GEO_CACHE_CAP`] results are cached, including misses. An absent or
//! unreadable database yields `--` without blocking downloads. No database is
//! bundled with this tree; see `--geoip-db` in [`crate::workflow::USAGE`].
//!
//! Local, private and reserved addresses are never misrepresented as a
//! country. No online lookup, reverse DNS or telemetry is performed.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

/// Maximum cached IP-to-country results, including misses.
pub const GEO_CACHE_CAP: usize = 4096;

/// Environment override for the database path. Beats the default search
/// paths; an explicit `--geoip-db` beats this.
pub const GEO_ENV_VAR: &str = "TEMPLARTORRENT_GEOIP_DB";

/// Default search paths, Country before City: both carry ISO codes and the
/// Country database is much smaller.
pub fn default_candidates() -> Vec<PathBuf> {
    candidates_for(data_home())
}

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
}

fn candidates_for(data_home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = data_home {
        let dir = home.join("templartorrent");
        out.push(dir.join("GeoLite2-Country.mmdb"));
        out.push(dir.join("GeoLite2-City.mmdb"));
    }
    for dir in ["/usr/share/GeoIP", "/var/lib/GeoIP"] {
        out.push(PathBuf::from(dir).join("GeoLite2-Country.mmdb"));
        out.push(PathBuf::from(dir).join("GeoLite2-City.mmdb"));
    }
    out
}

/// Resolve which database to use. An explicit `--geoip-db` path and the
/// environment variable are returned as-is so a bad path surfaces as a
/// visible load failure instead of silently falling back elsewhere.
pub fn resolve_db_path(explicit: Option<&Path>) -> Option<PathBuf> {
    let env = std::env::var_os(GEO_ENV_VAR).map(PathBuf::from);
    let fallback = default_candidates().into_iter().find(|path| path.is_file());
    select_db_path(explicit, env.as_deref(), fallback.as_deref())
}

/// Pure precedence: explicit flag, then environment, then the first
/// existing default candidate. Split out so tests never mutate the
/// process environment.
fn select_db_path(
    explicit: Option<&Path>,
    env: Option<&Path>,
    fallback: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Some(path) = env
        && !path.as_os_str().is_empty()
    {
        return Some(path.to_path_buf());
    }
    fallback.map(Path::to_path_buf)
}

/// True only for globally routable unicast addresses. Everything else —
/// loopback, private, link-local, documentation, reserved, multicast —
/// must render as `--`, never as a country.
pub fn is_routable_ip(ip: &IpAddr) -> bool {
    match *ip {
        IpAddr::V4(v4) => is_routable_v4(&v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_routable_v4(&v4),
            None => is_routable_v6(&v6),
        },
    }
}

fn is_routable_v4(ip: &Ipv4Addr) -> bool {
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
    {
        return false;
    }
    let o = ip.octets();
    // 0.0.0.0/8, 100.64.0.0/10 (shared), 192.0.0.0/24 (special purpose),
    // 198.18.0.0/15 (benchmarking), 240.0.0.0/4 (reserved).
    if o[0] == 0
        || (o[0] == 100 && o[1] & 0xc0 == 0x40)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || o[0] & 0xf0 == 0xf0
    {
        return false;
    }
    true
}

fn is_routable_v6(ip: &Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    let s = ip.segments();
    // fc00::/7 unique-local, fe80::/10 link-local, 2001:db8::/32
    // documentation, 2001:2::/48 benchmarking.
    if s[0] & 0xfe00 == 0xfc00
        || s[0] & 0xffc0 == 0xfe80
        || (s[0] == 0x2001 && s[1] == 0x0db8)
        || (s[0] == 0x2001 && s[1] == 0x0002)
    {
        return false;
    }
    true
}

/// Validate and normalize a two-letter ISO 3166-1 alpha-2 code.
pub fn iso_code_to_bytes(iso: &str) -> Option<[u8; 2]> {
    let bytes = iso.as_bytes();
    if bytes.len() == 2 && bytes.iter().all(|byte| byte.is_ascii_alphabetic()) {
        Some([bytes[0].to_ascii_uppercase(), bytes[1].to_ascii_uppercase()])
    } else {
        None
    }
}

pub struct GeoDb {
    path: Option<PathBuf>,
    reader: Option<maxminddb::Reader<Vec<u8>>>,
    attempted: bool,
    load_error: Option<String>,
    cache: HashMap<IpAddr, Option<[u8; 2]>>,
    order: VecDeque<IpAddr>,
}

impl GeoDb {
    /// Record the database path without opening it. The file is read on
    /// the first [`GeoDb::lookup`], after the window is already up.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            reader: None,
            attempted: false,
            load_error: None,
            cache: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// One-line database state for the Peers tab header.
    pub fn status_text(&self) -> String {
        let file = self
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("country database");
        if self.reader.is_some() {
            return format!("{file} loaded");
        }
        match (&self.path, &self.load_error) {
            (Some(_), Some(error)) => {
                let short: String = error.chars().take(72).collect();
                format!("{file} failed: {short}")
            }
            (Some(_), None) => format!("{file} configured"),
            (None, _) => "not loaded — pass --geoip-db FILE".into(),
        }
    }

    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Look up one IP, caching the result (or miss) in a bounded map.
    pub fn lookup(&mut self, ip: IpAddr) -> Option<[u8; 2]> {
        if let Some(hit) = self.cache.get(&ip) {
            return *hit;
        }
        let result = self.resolve_uncached(ip);
        self.insert(ip, result);
        result
    }

    fn resolve_uncached(&mut self, ip: IpAddr) -> Option<[u8; 2]> {
        if !is_routable_ip(&ip) {
            return None;
        }
        let reader = self.ensure_loaded()?;
        let found = reader.lookup(ip).ok()?;
        if !found.has_data() {
            return None;
        }
        let record: maxminddb::geoip2::Country = found.decode().ok()??;
        // Some records (e.g. 1.1.1.1) carry only the registered country.
        let iso = record
            .country
            .iso_code
            .or(record.registered_country.iso_code)?;
        iso_code_to_bytes(iso)
    }

    fn ensure_loaded(&mut self) -> Option<&maxminddb::Reader<Vec<u8>>> {
        if self.reader.is_none() && !self.attempted {
            self.attempted = true;
            if let Some(path) = &self.path {
                match maxminddb::Reader::open_readfile(path) {
                    Ok(reader) => self.reader = Some(reader),
                    Err(error) => self.load_error = Some(error.to_string()),
                }
            }
        }
        self.reader.as_ref()
    }

    fn insert(&mut self, ip: IpAddr, value: Option<[u8; 2]>) {
        if self.cache.contains_key(&ip) {
            return;
        }
        while self.cache.len() >= GEO_CACHE_CAP {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.cache.remove(&oldest);
        }
        self.order.push_back(ip);
        self.cache.insert(ip, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn special_addresses_never_count_as_routable() {
        for text in [
            "0.0.0.0",
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "::",
            "::1",
            "fe80::1",
            "fc00::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_routable_ip(&ip(text)), "{text}");
        }
        for text in ["8.8.8.8", "1.1.1.1", "9.20.30.40", "2001:4860:4860::8888"] {
            assert!(is_routable_ip(&ip(text)), "{text}");
        }
    }

    #[test]
    fn iso_codes_must_be_two_ascii_letters() {
        assert_eq!(iso_code_to_bytes("US"), Some(*b"US"));
        assert_eq!(iso_code_to_bytes("us"), Some(*b"US"));
        for bad in ["", "U", "USA", "U1", "12", "U-", "éx"] {
            assert_eq!(iso_code_to_bytes(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn missing_database_reports_failure_without_blocking() {
        let mut db = GeoDb::new(None);
        assert_eq!(db.lookup(ip("8.8.8.8")), None);
        assert!(
            db.status_text().contains("not loaded"),
            "{}",
            db.status_text()
        );
        let missing = PathBuf::from("/nonexistent/GeoLite2-Country.mmdb");
        let mut db = GeoDb::new(Some(missing));
        assert_eq!(db.lookup(ip("8.8.8.8")), None);
        assert!(db.status_text().contains("failed"), "{}", db.status_text());
        // A corrupt file is a failure, not a panic.
        let dir = std::env::temp_dir().join(format!("tt-geo-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let junk = dir.join("junk.mmdb");
        std::fs::write(&junk, b"not a maxmind database").unwrap();
        let mut db = GeoDb::new(Some(junk.clone()));
        assert_eq!(db.lookup(ip("8.8.8.8")), None);
        assert!(db.status_text().contains("failed"), "{}", db.status_text());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_counts_misses_and_stays_bounded() {
        let mut db = GeoDb::new(None);
        // 9.0.0.0/8 is public space, so every miss exercises the cache path.
        for n in 0..GEO_CACHE_CAP + 64 {
            let address = IpAddr::from([9, (n >> 8) as u8, n as u8, 1]);
            assert_eq!(db.lookup(address), None);
        }
        assert_eq!(db.cache_len(), GEO_CACHE_CAP);
    }

    #[test]
    fn explicit_path_beats_env_and_defaults() {
        let explicit = Path::new("/explicit.mmdb");
        let env = Path::new("/env.mmdb");
        let fallback = Path::new("/fallback.mmdb");
        assert_eq!(
            select_db_path(Some(explicit), Some(env), Some(fallback)),
            Some(explicit.to_path_buf())
        );
        assert_eq!(
            select_db_path(None, Some(env), Some(fallback)),
            Some(env.to_path_buf())
        );
        assert_eq!(
            select_db_path(None, None, Some(fallback)),
            Some(fallback.to_path_buf())
        );
        assert_eq!(select_db_path(None, None, None), None);
        // An empty environment value is ignored, not used.
        assert_eq!(
            select_db_path(None, Some(Path::new("")), Some(fallback)),
            Some(fallback.to_path_buf())
        );
        // resolve_db_path passes an explicit flag straight through.
        assert_eq!(
            resolve_db_path(Some(explicit)),
            Some(explicit.to_path_buf())
        );
    }

    #[test]
    fn default_candidates_prefer_country_over_city() {
        let home = PathBuf::from("/data");
        let candidates = candidates_for(Some(home.clone()));
        assert!(candidates.len() >= 6);
        assert_eq!(
            candidates[0],
            home.join("templartorrent").join("GeoLite2-Country.mmdb")
        );
        assert_eq!(
            candidates[1],
            home.join("templartorrent").join("GeoLite2-City.mmdb")
        );
        assert!(
            candidates
                .iter()
                .any(|path| path == Path::new("/usr/share/GeoIP/GeoLite2-Country.mmdb"))
        );
        let without_home = candidates_for(None);
        assert_eq!(without_home.len(), 4);
    }
}
