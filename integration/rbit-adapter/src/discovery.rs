//! Tracker URL admission. Original magnet/torrent trackers may be loopback or
//! private only when the caller sets that allowance (tests and an explicit
//! user opt-in). Automatically added public trackers never may. Query strings
//! stay intact for announce and are stripped from any label.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

pub const MAX_TRACKERS: usize = 12;
const MAX_URL_LEN: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTracker {
    pub url: String,
    pub host: String,
    pub port: u16,
}

pub fn parse_tracker_url(raw: &str) -> Result<ParsedTracker, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_URL_LEN {
        return Err("tracker URL length is invalid");
    }
    if raw.contains([' ', '\n', '\r', '\t', '\0']) {
        return Err("tracker URL contains whitespace");
    }
    let (https, rest) = if let Some(rest) = raw.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = raw.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err("tracker scheme must be http or https");
    };
    if rest.contains('@') {
        return Err("tracker URL userinfo is rejected");
    }
    let (hostport, path) = match rest.find(['/', '?', '#']) {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if hostport.is_empty() {
        return Err("tracker host is missing");
    }
    let (host, port) = split_host_port(hostport, if https { 443 } else { 80 })?;
    if host.is_empty() {
        return Err("tracker host is missing");
    }
    let path = path.split('#').next().unwrap_or("");
    let url = format!(
        "{}://{hostport}{path}",
        if https { "https" } else { "http" }
    );
    if url.len() > MAX_URL_LEN {
        return Err("tracker URL length is invalid");
    }
    Ok(ParsedTracker { url, host, port })
}

fn split_host_port(hostport: &str, default_port: u16) -> Result<(String, u16), &'static str> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let end = rest.find(']').ok_or("tracker IPv6 host is missing ']'")?;
        let host = rest[..end].to_string();
        let after = &rest[end + 1..];
        if after.is_empty() {
            return Ok((host, default_port));
        }
        let port = after
            .strip_prefix(':')
            .ok_or("tracker host is invalid")?
            .parse::<u16>()
            .map_err(|_| "tracker port is invalid")?;
        if port == 0 {
            return Err("tracker port is invalid");
        }
        return Ok((host, port));
    }
    if let Some((host, port)) = hostport.rsplit_once(':')
        && !host.is_empty()
        && !host.contains(':')
    {
        let port = port.parse::<u16>().map_err(|_| "tracker port is invalid")?;
        if port == 0 {
            return Err("tracker port is invalid");
        }
        return Ok((host.to_string(), port));
    }
    Ok((hostport.to_string(), default_port))
}

pub fn redact_tracker_url(url: &str) -> String {
    match url.split_once('?') {
        Some((before, _)) if !before.is_empty() => format!("{before}?<redacted>"),
        _ => url.to_string(),
    }
}

pub fn is_restricted_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_restricted_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                is_restricted_v4(v4)
            } else {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || is_unique_local(v6)
                    || is_v6_link_local(v6)
            }
        }
    }
}

fn is_restricted_v4(v4: Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    v4.is_loopback()
        || v4.is_unspecified()
        || v4.is_multicast()
        || v4.is_broadcast()
        || v4.is_link_local()
        || a == 0
        || a == 10
        || a == 127
        || (a == 100 && (b & 0xc0) == 64)
        || (a == 172 && (b & 0xf0) == 16)
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || (a == 198 && (b & 0xfe) == 18)
        || a >= 224
}

fn is_unique_local(v6: Ipv6Addr) -> bool {
    (v6.octets()[0] & 0xfe) == 0xfc
}

fn is_v6_link_local(v6: Ipv6Addr) -> bool {
    let o = v6.octets();
    o[0] == 0xfe && (o[1] & 0xc0) == 0x80
}

pub fn classify_addresses(addresses: &[IpAddr], allow_local: bool) -> Result<(), &'static str> {
    if addresses.is_empty() {
        return Err("tracker host did not resolve");
    }
    let restricted = addresses.iter().any(|ip| is_restricted_ip(*ip));
    if restricted && !allow_local {
        return Err("tracker resolves to a local or private address");
    }
    if restricted && addresses.iter().any(|ip| !is_restricted_ip(*ip)) {
        return Err("tracker resolves to both public and private addresses");
    }
    Ok(())
}

pub async fn resolve_tracker(parsed: &ParsedTracker) -> Result<Vec<IpAddr>, &'static str> {
    if parsed.host.eq_ignore_ascii_case("localhost") {
        return Ok(vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ]);
    }
    if let Ok(ip) = parsed.host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    let lookup = if parsed.host.contains(':') {
        format!("[{}]:{}", parsed.host, parsed.port)
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let looked = tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host(&lookup))
        .await
        .map_err(|_| "tracker DNS lookup failed")?
        .map_err(|_| "tracker DNS lookup failed")?;
    let mut addresses = Vec::new();
    for address in looked {
        if !addresses.contains(&address.ip()) {
            addresses.push(address.ip());
        }
    }
    if addresses.is_empty() {
        return Err("tracker host did not resolve");
    }
    Ok(addresses)
}

pub struct AdmittedTracker {
    pub tracker: ParsedTracker,
    pub local: bool,
    /// Resolved and classified addresses; announces connect to one of these
    /// rather than resolving the host again.
    pub addresses: Vec<IpAddr>,
}

impl AdmittedTracker {
    /// The address an announce connects to: the first IPv4, else the first.
    pub fn connect_address(&self) -> Option<SocketAddr> {
        self.addresses
            .iter()
            .find(|ip| ip.is_ipv4())
            .or(self.addresses.first())
            .map(|ip| SocketAddr::new(*ip, self.tracker.port))
    }
}

pub async fn admit_tracker(url: &str, allow_local: bool) -> Result<AdmittedTracker, &'static str> {
    let parsed = parse_tracker_url(url)?;
    let addresses = resolve_tracker(&parsed).await?;
    classify_addresses(&addresses, allow_local)?;
    Ok(AdmittedTracker {
        local: addresses.iter().any(|ip| is_restricted_ip(*ip)),
        tracker: parsed,
        addresses,
    })
}

pub fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Append admitted public trackers that are not already present. `admitted`
/// is the caller's decision for a configured URL; local and duplicate URLs
/// are the caller's responsibility to reject inside that predicate. The result
/// never exceeds [`MAX_TRACKERS`] total entries.
pub fn plan_public_additions(
    existing: &[String],
    configured: &[String],
    mut admitted: impl FnMut(&str) -> bool,
) -> Vec<String> {
    let mut planned = Vec::new();
    for url in configured {
        if existing.len() + planned.len() >= MAX_TRACKERS {
            break;
        }
        if existing.iter().any(|known| known == url) || planned.iter().any(|known| known == url) {
            continue;
        }
        if admitted(url) {
            planned.push(url.clone());
        }
    }
    planned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_ranges_cover_local_and_documentation_space() {
        assert!(is_restricted_ip("127.0.0.1".parse().unwrap()));
        assert!(is_restricted_ip("10.1.2.3".parse().unwrap()));
        assert!(is_restricted_ip("192.168.0.1".parse().unwrap()));
        assert!(is_restricted_ip("169.254.1.1".parse().unwrap()));
        assert!(is_restricted_ip("100.64.0.1".parse().unwrap()));
        assert!(is_restricted_ip("203.0.113.9".parse().unwrap()));
        assert!(is_restricted_ip("::1".parse().unwrap()));
        assert!(is_restricted_ip("fe80::1".parse().unwrap()));
        assert!(is_restricted_ip("fd00::1".parse().unwrap()));
        assert!(!is_restricted_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_restricted_ip("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn public_additions_skip_duplicates_and_rejected_urls() {
        let existing = vec!["http://tracker.example/announce".into()];
        let configured = vec![
            "http://tracker.example/announce".into(),
            "http://127.0.0.1/announce".into(),
            "http://1.1.1.1/announce".into(),
        ];
        let planned =
            plan_public_additions(&existing, &configured, |url| !url.contains("127.0.0.1"));
        assert_eq!(planned, vec!["http://1.1.1.1/announce".to_string()]);
    }

    #[test]
    fn labels_strip_the_query_and_keep_the_endpoint() {
        assert_eq!(
            redact_tracker_url("http://tracker.example/announce?passkey=secret"),
            "http://tracker.example/announce?<redacted>"
        );
    }

    #[test]
    fn private_addresses_require_the_local_allowance() {
        let local = vec!["127.0.0.1".parse().unwrap()];
        assert!(classify_addresses(&local, false).is_err());
        assert!(classify_addresses(&local, true).is_ok());
        let mixed = vec!["1.1.1.1".parse().unwrap(), "10.0.0.1".parse().unwrap()];
        assert!(classify_addresses(&mixed, true).is_err());
    }
}
