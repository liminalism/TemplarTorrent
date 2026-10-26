//! Opt-in router port mapping: NAT-PMP (RFC 6886) first, then UPnP IGD
//! (SSDP discovery, the device description, and SOAP on a WANIPConnection or
//! WANPPPConnection service).
//!
//! DESIGN.md section 9 rules, as implemented here:
//! - Nothing is sent until the caller starts a [`PortMapper`], which the
//!   client does only when the user ticked the setting (default off).
//! - Every request goes to the IPv4 default gateway and nowhere else. NAT-PMP
//!   replies are accepted only from the gateway's NAT-PMP port; SSDP answers
//!   only from the gateway's address, and only when their LOCATION is
//!   `http://<gateway>[:port]/...` (no host names, no https, no redirects), so
//!   the description and control requests stay on the gateway too.
//! - Every network step has a deadline and every reply a size cap; the XML
//!   scan refuses DOCTYPEs, deep nesting and oversized documents.
//! - The mapping covers the listen port for TCP (peers) and UDP (the DHT
//!   shares the port). It is renewed before it lapses and removed when the
//!   mapper stops. If the process dies instead, the router drops it when the
//!   lease ends (2 h for NAT-PMP, 1 h for UPnP unless the router only
//!   supports permanent leases).
//!
//! The router's external address is kept in [`Status`] for callers, but
//! [`Status::describe`] never prints it.
use std::{
    fmt::Write as _,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::{oneshot, watch},
    task::JoinHandle,
    time::timeout,
};

pub const NATPMP_PORT: u16 = 5351;
pub const SSDP_TARGET: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
/// Lifetime requested from a NAT-PMP router (RFC 6886 recommends 7200 s).
pub const NATPMP_LIFETIME_S: u32 = 7200;
/// Lease requested from a UPnP router; 0 (permanent) only after error 725.
pub const UPNP_LEASE_S: u32 = 3600;
/// Largest accepted description or SOAP response, headers included.
pub const MAX_HTTP_BYTES: usize = 64 * 1024;
/// Largest accepted SSDP answer datagram.
pub const MAX_SSDP_BYTES: usize = 2048;
/// SSDP search targets, sent once each per search.
pub const SEARCH_TARGETS: [&str; 5] = [
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANIPConnection:2",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
];
/// Bound on [`PortMapper::stop`] in the driver: long enough for the
/// lifetime-0 and DeletePortMapping requests, short enough for shutdown.
pub const STOP_BOUND: Duration = Duration::from_secs(5);
const MAX_XML_DEPTH: usize = 32;
const MAX_XML_TAGS: usize = 4096;
const MAX_TEXT: usize = 512;
const MAX_SERVICES: usize = 4;
const MAX_PATH: usize = 512;
const MAPPING_NAME: &str = "TemplarTorrent";
const USER_AGENT: &str = "Linux UPnP/1.1 TemplarTorrent/0.1";
const WAN_IP: &str = "urn:schemas-upnp-org:service:WANIPConnection:";
const WAN_PPP: &str = "urn:schemas-upnp-org:service:WANPPPConnection:";

/// Where requests go and how long each step may take. Production uses
/// [`Config::default`]; tests point the gateway, the NAT-PMP port and the
/// SSDP target at loopback fixtures.
#[derive(Debug, Clone)]
pub struct Config {
    /// `None` reads the IPv4 default route.
    pub gateway: Option<Ipv4Addr>,
    pub natpmp_port: u16,
    pub ssdp_target: SocketAddrV4,
    pub natpmp: bool,
    pub upnp: bool,
    /// First NAT-PMP retransmission delay; it doubles per try (RFC 6886
    /// section 3.1 starts at 250 ms). Four tries total 3.75 s, well short of
    /// the RFC's nine.
    pub natpmp_initial: Duration,
    pub natpmp_tries: u32,
    /// How long to collect SSDP answers.
    pub ssdp_wait: Duration,
    /// Deadline for one HTTP exchange with the gateway, connect included.
    pub http_timeout: Duration,
    /// Floor between renewals, so a router granting tiny lifetimes cannot
    /// make the mapper spin.
    pub min_renew: Duration,
    /// Wait after a failed attempt before asking the router again.
    pub retry_after: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gateway: None,
            natpmp_port: NATPMP_PORT,
            ssdp_target: SSDP_TARGET,
            natpmp: true,
            upnp: true,
            natpmp_initial: Duration::from_millis(250),
            natpmp_tries: 4,
            ssdp_wait: Duration::from_secs(3),
            http_timeout: Duration::from_secs(5),
            min_renew: Duration::from_secs(60),
            retry_after: Duration::from_secs(600),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    NatPmp,
    Upnp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Off,
    Probing,
    Mapped {
        method: Method,
        /// External port the router granted for TCP.
        port: u16,
        /// The router's external address and port, when it reported one.
        external: Option<SocketAddrV4>,
        /// `None` for a permanent UPnP lease.
        expires: Option<Instant>,
    },
    Failed(String),
}

impl Status {
    /// Settings-dialog text. Never contains the external address.
    pub fn describe(&self) -> String {
        match self {
            Status::Off => "off".into(),
            Status::Probing => "asking the router".into(),
            Status::Mapped {
                method,
                port,
                external,
                ..
            } => {
                let method = match method {
                    Method::NatPmp => "NAT-PMP",
                    Method::Upnp => "UPnP",
                };
                let mut text = format!("mapped by {method} as external port {port} (TCP and UDP)");
                if external.is_some_and(|address| !is_public(*address.ip())) {
                    text.push_str(
                        "; the router's own external address is private, so peers may still not reach you",
                    );
                }
                text
            }
            Status::Failed(reason) => format!("not mapped: {reason}"),
        }
    }
}

/// Not private, shared (100.64/10), loopback, link-local or otherwise special.
fn is_public(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || (a == 100 && (64..128).contains(&b)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Udp,
    Tcp,
}

impl Protocol {
    fn natpmp_op(self) -> u8 {
        match self {
            Protocol::Udp => 1,
            Protocol::Tcp => 2,
        }
    }
    fn upnp_name(self) -> &'static str {
        match self {
            Protocol::Udp => "UDP",
            Protocol::Tcp => "TCP",
        }
    }
}

// ---------------------------------------------------------------------------
// Gateway

/// The lowest-metric IPv4 default route's gateway in `/proc/net/route`
/// format. Addresses there are the kernel's network-order words printed as
/// host integers, so the native byte order restores the octets.
pub fn parse_route_table(text: &str) -> Option<Ipv4Addr> {
    let mut best: Option<(u32, Ipv4Addr)> = None;
    for line in text.lines().skip(1).take(1024) {
        let fields: Vec<&str> = line.split_whitespace().take(8).collect();
        if fields.len() < 8 {
            continue;
        }
        let hex = |index: usize| u32::from_str_radix(fields[index], 16).ok();
        let (Some(destination), Some(gateway), Some(flags), Some(mask)) =
            (hex(1), hex(2), hex(3), hex(7))
        else {
            continue;
        };
        let Ok(metric) = fields[6].parse::<u32>() else {
            continue;
        };
        // RTF_UP | RTF_GATEWAY
        if destination != 0 || mask != 0 || flags & 0x3 != 0x3 {
            continue;
        }
        let gateway = Ipv4Addr::from(gateway.to_ne_bytes());
        if gateway.is_unspecified() || gateway.is_broadcast() || gateway.is_multicast() {
            continue;
        }
        if best.is_none_or(|(m, _)| metric < m) {
            best = Some((metric, gateway));
        }
    }
    best.map(|(_, gateway)| gateway)
}

/// This host's IPv4 default gateway. Linux only; elsewhere `None`.
pub fn default_gateway() -> Option<Ipv4Addr> {
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        let mut text = String::new();
        std::fs::File::open("/proc/net/route")
            .ok()?
            .take(256 * 1024)
            .read_to_string(&mut text)
            .ok()?;
        parse_route_table(&text)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Our address on the route to `gateway`. Connecting a UDP socket sends
/// nothing; it only selects the route and source address.
fn local_address(gateway: Ipv4Addr) -> Result<Ipv4Addr, &'static str> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|_| "no IPv4 socket for the gateway route")?;
    socket
        .connect((gateway, NATPMP_PORT))
        .map_err(|_| "no IPv4 route to the gateway")?;
    match socket.local_addr() {
        Ok(SocketAddr::V4(address)) if !address.ip().is_unspecified() => Ok(*address.ip()),
        _ => Err("no IPv4 address on the gateway route"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub gateway: Ipv4Addr,
    pub local: Ipv4Addr,
}

/// Gateway and our address on its route, after checking that `listen`
/// accepts connections on that address. Sends nothing.
pub fn resolve_route(config: &Config, listen: Option<SocketAddr>) -> Result<Route, &'static str> {
    let gateway = config
        .gateway
        .or_else(default_gateway)
        .ok_or("no IPv4 default gateway")?;
    let local = local_address(gateway)?;
    if let Some(listen) = listen {
        let reachable = match listen.ip() {
            ip if ip.is_unspecified() => true,
            IpAddr::V4(ip) => ip == local,
            IpAddr::V6(ip) => ip.to_ipv4_mapped() == Some(local),
        };
        if !reachable {
            return Err("the listener is not bound on the gateway's network");
        }
    }
    Ok(Route { gateway, local })
}

// ---------------------------------------------------------------------------
// NAT-PMP wire format

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatPmpReply {
    External {
        epoch: u32,
        address: Ipv4Addr,
    },
    Mapping {
        epoch: u32,
        protocol: Protocol,
        internal: u16,
        external: u16,
        lifetime: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatPmpError {
    /// Not a NAT-PMP reply, or truncated.
    Malformed(&'static str),
    /// The router answered with a non-zero result code.
    Refused(u16),
}

impl NatPmpError {
    pub fn reason(self) -> &'static str {
        match self {
            NatPmpError::Malformed(reason) => reason,
            NatPmpError::Refused(1) => "router does not support NAT-PMP version 0",
            NatPmpError::Refused(2) => "router refused the mapping (not authorized)",
            NatPmpError::Refused(3) => "router has no external network",
            NatPmpError::Refused(4) => "router is out of mapping resources",
            NatPmpError::Refused(5) => "router does not support this NAT-PMP request",
            NatPmpError::Refused(_) => "router returned an unknown NAT-PMP result",
        }
    }
}

pub fn natpmp_external_request() -> [u8; 2] {
    [0, 0]
}

/// Mapping request. A lifetime of 0 deletes; the RFC then wants the
/// suggested external port to be 0 as well.
pub fn natpmp_mapping_request(
    protocol: Protocol,
    internal: u16,
    external: u16,
    lifetime: u32,
) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[1] = protocol.natpmp_op();
    out[4..6].copy_from_slice(&internal.to_be_bytes());
    out[6..8].copy_from_slice(&external.to_be_bytes());
    out[8..12].copy_from_slice(&lifetime.to_be_bytes());
    out
}

/// One datagram from the gateway's NAT-PMP port. A non-zero result code is
/// checked before the length, since some routers truncate error replies.
pub fn parse_natpmp_reply(bytes: &[u8]) -> Result<NatPmpReply, NatPmpError> {
    if bytes.len() < 4 {
        return Err(NatPmpError::Malformed("NAT-PMP reply is truncated"));
    }
    if bytes[0] != 0 {
        return Err(NatPmpError::Malformed("reply is not NAT-PMP version 0"));
    }
    let op = bytes[1];
    if op < 128 {
        return Err(NatPmpError::Malformed("NAT-PMP datagram is a request"));
    }
    let result = u16::from_be_bytes([bytes[2], bytes[3]]);
    if result != 0 {
        return Err(NatPmpError::Refused(result));
    }
    let word =
        |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    match op {
        128 => {
            if bytes.len() < 12 {
                return Err(NatPmpError::Malformed("NAT-PMP reply is truncated"));
            }
            Ok(NatPmpReply::External {
                epoch: word(4),
                address: Ipv4Addr::new(bytes[8], bytes[9], bytes[10], bytes[11]),
            })
        }
        129 | 130 => {
            if bytes.len() < 16 {
                return Err(NatPmpError::Malformed("NAT-PMP reply is truncated"));
            }
            Ok(NatPmpReply::Mapping {
                epoch: word(4),
                protocol: if op == 129 {
                    Protocol::Udp
                } else {
                    Protocol::Tcp
                },
                internal: u16::from_be_bytes([bytes[8], bytes[9]]),
                external: u16::from_be_bytes([bytes[10], bytes[11]]),
                lifetime: word(12),
            })
        }
        _ => Err(NatPmpError::Malformed("unknown NAT-PMP reply opcode")),
    }
}

/// Send `request` and wait for the matching reply from exactly `gateway`,
/// retransmitting on the doubling schedule. Datagrams from anyone else, and
/// replies to other opcodes, are ignored.
async fn natpmp_exchange(
    socket: &UdpSocket,
    gateway: SocketAddrV4,
    request: &[u8],
    initial: Duration,
    tries: u32,
) -> Result<NatPmpReply, &'static str> {
    let expected = 128 + request[1];
    let mut wait = initial;
    let mut buffer = [0u8; 64];
    for _ in 0..tries.max(1) {
        if socket.send_to(request, gateway).await.is_err() {
            return Err("NAT-PMP request could not be sent");
        }
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(received) = timeout(remaining, socket.recv_from(&mut buffer)).await else {
                break;
            };
            let Ok((length, from)) = received else {
                break;
            };
            if from != SocketAddr::V4(gateway) {
                continue;
            }
            let bytes = &buffer[..length];
            if bytes.len() >= 2 && bytes[1] != expected {
                continue;
            }
            return parse_natpmp_reply(bytes).map_err(NatPmpError::reason);
        }
        wait = wait.saturating_mul(2);
    }
    Err("no NAT-PMP reply from the gateway")
}

// ---------------------------------------------------------------------------
// SSDP and URLs

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsdpResponse {
    pub location: String,
    pub search_target: String,
    /// The SERVER header, control characters removed, at most 120 chars.
    pub server: String,
}

fn search_request(target: SocketAddrV4, search: &str) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {target}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {search}\r\nUSER-AGENT: {USER_AGENT}\r\n\r\n"
    )
}

/// One SSDP answer: an HTTP/1.x 200 status and headers, no body.
pub fn parse_ssdp_response(bytes: &[u8]) -> Result<SsdpResponse, &'static str> {
    if bytes.len() > MAX_SSDP_BYTES {
        return Err("SSDP answer is too large");
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "SSDP answer is not UTF-8")?;
    let mut lines = text.split("\r\n").flat_map(|line| line.split('\n'));
    let status = lines.next().unwrap_or("");
    let mut parts = status.split_whitespace();
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") || parts.next() != Some("200") {
        return Err("SSDP answer is not an HTTP 200 response");
    }
    let mut location = None;
    let mut search_target = String::new();
    let mut server = String::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("location") {
            if location.is_some() {
                return Err("SSDP answer has two LOCATION headers");
            }
            location = Some(value);
        } else if name.eq_ignore_ascii_case("st") {
            search_target = clean(value, 200);
        } else if name.eq_ignore_ascii_case("server") {
            server = clean(value, 120);
        }
    }
    let location = location.ok_or("SSDP answer has no LOCATION")?;
    if location.is_empty() || location.len() > 256 || location.chars().any(char::is_control) {
        return Err("SSDP LOCATION is invalid");
    }
    Ok(SsdpResponse {
        location: location.to_string(),
        search_target,
        server,
    })
}

fn clean(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}

/// `http://` URL with an IPv4 literal host. Host names (which would need a
/// DNS lookup), user info, https and other schemes are refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpUrl {
    pub host: Ipv4Addr,
    pub port: u16,
    pub path: String,
}

pub fn parse_http_url(url: &str) -> Result<HttpUrl, &'static str> {
    let rest = url
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
        .map(|_| &url[7..])
        .ok_or("only http:// URLs are used")?;
    let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(split);
    if authority.contains('@') {
        return Err("URL user info is refused");
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or("URL port is invalid")?,
        ),
        None => (authority, 80),
    };
    let host = host
        .parse::<Ipv4Addr>()
        .map_err(|_| "URL host is not an IPv4 address")?;
    let path = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    check_path(&path)?;
    Ok(HttpUrl { host, port, path })
}

fn check_path(path: &str) -> Result<(), &'static str> {
    if path.len() > MAX_PATH
        || !path.starts_with('/')
        || path
            .bytes()
            .any(|b| !(0x21..0x7f).contains(&b) || matches!(b, b'"' | b'<' | b'>' | b'\\'))
    {
        return Err("URL path is invalid");
    }
    Ok(())
}

/// The SSDP LOCATION, admitted only when it names the gateway.
pub fn admit_location(location: &str, gateway: Ipv4Addr) -> Result<HttpUrl, &'static str> {
    let url = parse_http_url(location)
        .map_err(|_| "SSDP LOCATION is not an http:// URL to an IPv4 host")?;
    if url.host != gateway {
        return Err("SSDP LOCATION points to a host other than the gateway");
    }
    Ok(url)
}

/// A service's controlURL, relative to the LOCATION or absolute on the
/// gateway.
pub fn control_target(
    control: &str,
    location: &HttpUrl,
    gateway: Ipv4Addr,
) -> Result<HttpUrl, &'static str> {
    if control
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"))
    {
        let url = parse_http_url(control)?;
        if url.host != gateway {
            return Err("control URL points to a host other than the gateway");
        }
        return Ok(url);
    }
    if control.contains("://") {
        return Err("control URL is not http:// on the gateway");
    }
    let path = if control.starts_with('/') {
        control.to_string()
    } else {
        format!("/{control}")
    };
    check_path(&path)?;
    Ok(HttpUrl {
        host: location.host,
        port: location.port,
        path,
    })
}

// ---------------------------------------------------------------------------
// Bounded XML scan

enum Event<'a> {
    Open(&'a str),
    Close(&'a str),
    Text(&'a str),
}

/// Tags and text in document order, element names without namespace
/// prefixes. Comments and processing instructions are skipped; CDATA is
/// text; a DOCTYPE (entity declarations) is refused. Bounded in size,
/// nesting and tag count. Close tags are not matched against open tags.
fn scan_xml<'a>(xml: &'a str, mut visit: impl FnMut(Event<'a>)) -> Result<(), &'static str> {
    if xml.len() > MAX_HTTP_BYTES {
        return Err("XML exceeds 64 KiB");
    }
    let mut depth = 0usize;
    let mut tags = 0usize;
    let mut rest = xml;
    loop {
        let Some(open) = rest.find('<') else {
            if !rest.is_empty() {
                visit(Event::Text(rest));
            }
            return Ok(());
        };
        if open > 0 {
            visit(Event::Text(&rest[..open]));
        }
        rest = &rest[open..];
        tags += 1;
        if tags > MAX_XML_TAGS {
            return Err("XML has too many tags");
        }
        if let Some(body) = rest.strip_prefix("<!--") {
            let end = body.find("-->").ok_or("XML comment is unterminated")?;
            rest = &body[end + 3..];
            continue;
        }
        if let Some(body) = rest.strip_prefix("<![CDATA[") {
            let end = body.find("]]>").ok_or("XML CDATA is unterminated")?;
            visit(Event::Text(&body[..end]));
            rest = &body[end + 3..];
            continue;
        }
        if let Some(body) = rest.strip_prefix("<?") {
            let end = body.find("?>").ok_or("XML declaration is unterminated")?;
            rest = &body[end + 2..];
            continue;
        }
        if rest.starts_with("<!") {
            return Err("XML DOCTYPE is refused");
        }
        let end = rest.find('>').ok_or("XML tag is unterminated")?;
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        if let Some(name) = tag.strip_prefix('/') {
            depth = depth
                .checked_sub(1)
                .ok_or("XML close tag without an open tag")?;
            visit(Event::Close(local_name(name.trim())));
            continue;
        }
        let self_closing = tag.ends_with('/');
        let name = tag
            .trim_end_matches('/')
            .split(|c: char| c.is_ascii_whitespace())
            .next()
            .unwrap_or("");
        if name.is_empty() {
            return Err("XML tag has no name");
        }
        let name = local_name(name);
        visit(Event::Open(name));
        if self_closing {
            visit(Event::Close(name));
        } else {
            depth += 1;
            if depth > MAX_XML_DEPTH {
                return Err("XML is nested too deeply");
            }
        }
    }
}

fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn push_bounded(out: &mut String, text: &str) {
    for c in text.chars() {
        if out.len() >= MAX_TEXT {
            break;
        }
        out.push(c);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgdService {
    pub service_type: String,
    pub control_url: String,
}

/// WANIPConnection and WANPPPConnection services in document order, at
/// most four. Service types must look like a UPnP URN, because they are
/// echoed into the SOAPAction header and the request body.
pub fn parse_description(xml: &[u8]) -> Result<Vec<IgdService>, &'static str> {
    let xml = std::str::from_utf8(xml).map_err(|_| "description is not UTF-8")?;
    let mut services = Vec::new();
    let mut in_service = false;
    let mut field: Option<bool> = None; // Some(true) serviceType, Some(false) controlURL
    let mut service_type = String::new();
    let mut control = String::new();
    scan_xml(xml, |event| match event {
        Event::Open("service") => {
            in_service = true;
            field = None;
            service_type.clear();
            control.clear();
        }
        Event::Open("serviceType") if in_service => {
            field = Some(true);
            service_type.clear();
        }
        Event::Open("controlURL") if in_service => {
            field = Some(false);
            control.clear();
        }
        Event::Open(_) => field = None,
        Event::Text(text) => match field {
            Some(true) => push_bounded(&mut service_type, text),
            Some(false) => push_bounded(&mut control, text),
            None => {}
        },
        Event::Close("service") => {
            in_service = false;
            field = None;
            let kind = decode_entities(service_type.trim());
            let url = decode_entities(control.trim());
            if (kind.starts_with(WAN_IP) || kind.starts_with(WAN_PPP))
                && kind
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'.'))
                && !url.is_empty()
                && services.len() < MAX_SERVICES
            {
                services.push(IgdService {
                    service_type: kind,
                    control_url: url,
                });
            }
        }
        Event::Close(_) => field = None,
    })?;
    Ok(services)
}

/// Text of the first element named `name` (namespace prefix ignored).
pub fn soap_element(body: &str, name: &str) -> Option<String> {
    let mut inside = false;
    let mut done = false;
    let mut text = String::new();
    scan_xml(body, |event| {
        if done {
            return;
        }
        match event {
            Event::Open(open) if open == name => inside = true,
            Event::Text(part) if inside => push_bounded(&mut text, part),
            Event::Close(_) | Event::Open(_) if inside => done = true,
            _ => {}
        }
    })
    .ok()?;
    done.then(|| decode_entities(text.trim()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoapReply {
    Success(String),
    /// HTTP 500 with the UPnPError code when the body had one.
    Fault(Option<u16>),
}

/// A SOAP response's status and body. The framing (Content-Length,
/// chunked, no redirects) is the tracker client's; a 500 is reframed as
/// 200 to reuse it for the fault body.
pub fn parse_soap_response(raw: &[u8]) -> Result<SoapReply, &'static str> {
    let line_end = find(raw, b"\r\n").ok_or("gateway HTTP response is malformed")?;
    let status =
        std::str::from_utf8(&raw[..line_end]).map_err(|_| "gateway HTTP response is malformed")?;
    let code = status
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or("gateway HTTP response is malformed")?;
    match code {
        200 => {
            let body = http_body(raw)?;
            Ok(SoapReply::Success(
                String::from_utf8_lossy(&body).into_owned(),
            ))
        }
        500 => {
            if !status.starts_with("HTTP/1.") {
                return Err("gateway HTTP response is malformed");
            }
            let mut reframed = b"HTTP/1.1 200 OK".to_vec();
            reframed.extend_from_slice(&raw[line_end..]);
            let body = http_body(&reframed)?;
            let text = String::from_utf8_lossy(&body);
            Ok(SoapReply::Fault(
                soap_element(&text, "errorCode").and_then(|code| code.trim().parse().ok()),
            ))
        }
        _ => Err("gateway returned an unexpected HTTP status"),
    }
}

fn http_body(raw: &[u8]) -> Result<Vec<u8>, &'static str> {
    crate::announce::parse_http_response(raw).map_err(|error| match error {
        "tracker redirect refused" => "gateway redirect refused",
        "tracker returned a non-200 status" => "gateway returned a non-200 status",
        "tracker response exceeds 64 KiB" => "gateway response exceeds 64 KiB",
        _ => "gateway HTTP response is malformed",
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// HTTP to the gateway

/// Headers in, and the body length they announce present; lets the read
/// stop before EOF on servers that keep the connection open anyway.
fn response_complete(bytes: &[u8]) -> bool {
    let Some(end) = find(bytes, b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&bytes[..end]);
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value
                .trim()
                .parse::<usize>()
                .is_ok_and(|length| bytes.len() - end - 4 >= length);
        }
        if name.trim().eq_ignore_ascii_case("transfer-encoding") {
            return bytes.ends_with(b"\r\n0\r\n\r\n");
        }
    }
    false
}

async fn http_exchange(
    target: &HttpUrl,
    request: &[u8],
    deadline: Duration,
) -> Result<Vec<u8>, &'static str> {
    let exchange = async {
        let mut stream = TcpStream::connect(SocketAddrV4::new(target.host, target.port))
            .await
            .map_err(|_| "gateway HTTP connection failed")?;
        stream
            .write_all(request)
            .await
            .map_err(|_| "gateway HTTP request failed")?;
        let mut response = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|_| "gateway HTTP read failed")?;
            if n == 0 {
                break;
            }
            if response.len() + n > MAX_HTTP_BYTES {
                return Err("gateway response exceeds 64 KiB");
            }
            response.extend_from_slice(&chunk[..n]);
            if response_complete(&response) {
                break;
            }
        }
        Ok(response)
    };
    timeout(deadline, exchange)
        .await
        .map_err(|_| "gateway HTTP deadline exceeded")?
}

fn get_request(target: &HttpUrl) -> Vec<u8> {
    format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nUser-Agent: {USER_AGENT}\r\nAccept: text/xml\r\n\r\n",
        target.path, target.host, target.port
    )
    .into_bytes()
}

fn soap_request(target: &HttpUrl, service: &str, action: &str, args: &[(&str, String)]) -> Vec<u8> {
    let mut body = format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{service}\">"
    );
    for (name, value) in args {
        let _ = write!(body, "<{name}>{value}</{name}>");
    }
    let _ = write!(body, "</u:{action}></s:Body></s:Envelope>\r\n");
    let mut out = format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\nSOAPAction: \"{service}#{action}\"\r\nConnection: close\r\nUser-Agent: {USER_AGENT}\r\n\r\n",
        target.path,
        target.host,
        target.port,
        body.len()
    );
    out.push_str(&body);
    out.into_bytes()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoapFailure {
    Code(Option<u16>),
    Transport(&'static str),
}

impl SoapFailure {
    fn reason(self) -> String {
        match self {
            SoapFailure::Transport(reason) => reason.into(),
            SoapFailure::Code(Some(606)) => "router refused the mapping (not authorized)".into(),
            SoapFailure::Code(Some(718)) => "router already maps this port to another host".into(),
            SoapFailure::Code(Some(724)) => {
                "router requires equal internal and external ports".into()
            }
            SoapFailure::Code(Some(code)) => {
                format!("router refused the request (UPnP error {code})")
            }
            SoapFailure::Code(None) => "router refused the request".into(),
        }
    }
}

async fn soap_call(
    config: &Config,
    control: &HttpUrl,
    service: &str,
    action: &str,
    args: &[(&str, String)],
) -> Result<String, SoapFailure> {
    let request = soap_request(control, service, action, args);
    let raw = http_exchange(control, &request, config.http_timeout)
        .await
        .map_err(SoapFailure::Transport)?;
    match parse_soap_response(&raw).map_err(SoapFailure::Transport)? {
        SoapReply::Success(body) => Ok(body),
        SoapReply::Fault(code) => Err(SoapFailure::Code(code)),
    }
}

// ---------------------------------------------------------------------------
// Mapping procedures

#[derive(Debug, Clone)]
struct Upnp {
    control: HttpUrl,
    service: String,
}

#[derive(Debug, Clone)]
struct Mapping {
    method: Method,
    port: u16,
    external: Option<Ipv4Addr>,
    /// Granted lifetime in seconds; `None` is permanent.
    lifetime: Option<u32>,
    upnp: Option<Upnp>,
}

impl Mapping {
    fn status(&self) -> Status {
        Status::Mapped {
            method: self.method,
            port: self.port,
            external: self
                .external
                .filter(|ip| !ip.is_unspecified())
                .map(|ip| SocketAddrV4::new(ip, self.port)),
            expires: self
                .lifetime
                .map(|seconds| Instant::now() + Duration::from_secs(u64::from(seconds))),
        }
    }
    /// Half the granted lifetime; permanent leases are re-asserted every
    /// half hour in case the router restarted.
    fn renew_after(&self, config: &Config) -> Duration {
        let seconds = self.lifetime.unwrap_or(3600) / 2;
        Duration::from_secs(u64::from(seconds)).max(config.min_renew)
    }
}

/// What a cancelled attempt may have left on the router.
#[derive(Debug, Default)]
struct Progress {
    natpmp: Vec<Protocol>,
    upnp: Option<Upnp>,
    upnp_added: Vec<Protocol>,
}

/// Both protocols on `internal`, asking for `external`. On a partial
/// success the mapping that was made is removed again.
async fn natpmp_map(
    config: &Config,
    route: Route,
    internal: u16,
    external: u16,
    progress: &mut Progress,
) -> Result<Mapping, String> {
    let socket = UdpSocket::bind(SocketAddrV4::new(route.local, 0))
        .await
        .map_err(|_| "no UDP socket for NAT-PMP".to_string())?;
    let gateway = SocketAddrV4::new(route.gateway, config.natpmp_port);
    let exchange = |request: Vec<u8>| {
        let socket = &socket;
        async move {
            natpmp_exchange(
                socket,
                gateway,
                &request,
                config.natpmp_initial,
                config.natpmp_tries,
            )
            .await
        }
    };
    let address = match exchange(natpmp_external_request().to_vec()).await? {
        NatPmpReply::External { address, .. } => address,
        NatPmpReply::Mapping { .. } => return Err("NAT-PMP reply has the wrong opcode".into()),
    };
    let mut granted = Vec::new();
    for protocol in [Protocol::Tcp, Protocol::Udp] {
        let request = natpmp_mapping_request(protocol, internal, external, NATPMP_LIFETIME_S);
        let reply = exchange(request.to_vec()).await;
        let outcome = match reply {
            Ok(NatPmpReply::Mapping {
                protocol: answered,
                internal: mapped,
                external,
                lifetime,
                ..
            }) if answered == protocol && mapped == internal => {
                progress.natpmp.push(protocol);
                if lifetime == 0 || external == 0 {
                    Err("router granted an empty NAT-PMP mapping")
                } else {
                    Ok((external, lifetime))
                }
            }
            Ok(_) => Err("NAT-PMP reply is for another mapping"),
            Err(reason) => Err(reason),
        };
        match outcome {
            Ok(grant) => granted.push(grant),
            Err(reason) => {
                natpmp_delete(config, route, internal, &progress.natpmp).await;
                progress.natpmp.clear();
                return Err(reason.into());
            }
        }
    }
    Ok(Mapping {
        method: Method::NatPmp,
        port: granted[0].0,
        external: Some(address),
        lifetime: granted.iter().map(|(_, lifetime)| *lifetime).min(),
        upnp: None,
    })
}

/// Lifetime-0 requests, two tries each.
async fn natpmp_delete(config: &Config, route: Route, internal: u16, protocols: &[Protocol]) {
    if protocols.is_empty() {
        return;
    }
    let Ok(socket) = UdpSocket::bind(SocketAddrV4::new(route.local, 0)).await else {
        return;
    };
    let gateway = SocketAddrV4::new(route.gateway, config.natpmp_port);
    for protocol in protocols {
        let request = natpmp_mapping_request(*protocol, internal, 0, 0);
        let _ = natpmp_exchange(
            &socket,
            gateway,
            &request,
            config.natpmp_initial,
            config.natpmp_tries.min(2),
        )
        .await;
    }
}

/// Outcome of one SSDP search, with what was refused on the way.
#[derive(Debug, Default)]
pub struct SsdpOutcome {
    pub accepted: Option<(HttpUrl, SsdpResponse)>,
    /// Answers whose source was not the gateway.
    pub other_hosts: usize,
    /// Answers from the gateway that were refused, first reason kept.
    pub refused: Option<&'static str>,
    pub refused_count: usize,
}

async fn ssdp_search(config: &Config, route: Route) -> Result<SsdpOutcome, &'static str> {
    let socket = UdpSocket::bind(SocketAddrV4::new(route.local, 0))
        .await
        .map_err(|_| "no UDP socket for SSDP")?;
    // Binding the route's address makes Linux send the multicast out of the
    // gateway's interface. TTL 2 as UPnP 1.1 recommends.
    let _ = socket.set_multicast_ttl_v4(2);
    for search in SEARCH_TARGETS {
        let request = search_request(config.ssdp_target, search);
        socket
            .send_to(request.as_bytes(), config.ssdp_target)
            .await
            .map_err(|_| "SSDP search could not be sent")?;
    }
    let mut outcome = SsdpOutcome::default();
    let deadline = tokio::time::Instant::now() + config.ssdp_wait;
    let mut buffer = vec![0u8; MAX_SSDP_BYTES + 1];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(Ok((length, from))) = timeout(remaining, socket.recv_from(&mut buffer)).await else {
            break;
        };
        if from.ip() != IpAddr::V4(route.gateway) {
            outcome.other_hosts += 1;
            continue;
        }
        let admitted = parse_ssdp_response(&buffer[..length]).and_then(|response| {
            admit_location(&response.location, route.gateway).map(|url| (url, response))
        });
        match admitted {
            Ok(found) => {
                outcome.accepted = Some(found);
                break;
            }
            Err(reason) => {
                outcome.refused_count += 1;
                outcome.refused.get_or_insert(reason);
            }
        }
    }
    Ok(outcome)
}

async fn fetch_services(
    config: &Config,
    location: &HttpUrl,
) -> Result<Vec<IgdService>, &'static str> {
    let raw = http_exchange(location, &get_request(location), config.http_timeout).await?;
    let body = http_body(&raw)?;
    parse_description(&body)
}

async fn upnp_map(
    config: &Config,
    route: Route,
    port: u16,
    progress: &mut Progress,
) -> Result<Mapping, String> {
    let outcome = ssdp_search(config, route).await?;
    let Some((location, _)) = outcome.accepted else {
        return Err(outcome
            .refused
            .unwrap_or("no UPnP gateway answered the SSDP search")
            .into());
    };
    let services = fetch_services(config, &location).await?;
    if services.is_empty() {
        return Err("the gateway describes no WAN connection service".into());
    }
    let mut last = String::new();
    for service in services {
        let control = match control_target(&service.control_url, &location, route.gateway) {
            Ok(control) => control,
            Err(reason) => {
                last = reason.into();
                continue;
            }
        };
        let upnp = Upnp {
            control,
            service: service.service_type,
        };
        match upnp_add(config, route, port, &upnp, progress).await {
            Ok(mapping) => return Ok(mapping),
            Err(reason) => last = reason,
        }
    }
    Err(last)
}

/// AddPortMapping for TCP then UDP (lease 3600, or 0 after error 725), then
/// GetExternalIPAddress. A partial success is removed again.
async fn upnp_add(
    config: &Config,
    route: Route,
    port: u16,
    upnp: &Upnp,
    progress: &mut Progress,
) -> Result<Mapping, String> {
    progress.upnp = Some(upnp.clone());
    let mut lease = UPNP_LEASE_S;
    for protocol in [Protocol::Tcp, Protocol::Udp] {
        let add = |lease: u32| async move {
            let args = [
                ("NewRemoteHost", String::new()),
                ("NewExternalPort", port.to_string()),
                ("NewProtocol", protocol.upnp_name().to_string()),
                ("NewInternalPort", port.to_string()),
                ("NewInternalClient", route.local.to_string()),
                ("NewEnabled", "1".to_string()),
                ("NewPortMappingDescription", MAPPING_NAME.to_string()),
                ("NewLeaseDuration", lease.to_string()),
            ];
            soap_call(
                config,
                &upnp.control,
                &upnp.service,
                "AddPortMapping",
                &args,
            )
            .await
        };
        let mut result = add(lease).await;
        if result == Err(SoapFailure::Code(Some(725))) && lease != 0 {
            lease = 0;
            result = add(lease).await;
        }
        match result {
            Ok(_) => progress.upnp_added.push(protocol),
            Err(failure) => {
                upnp_delete(config, port, upnp, &progress.upnp_added).await;
                progress.upnp_added.clear();
                return Err(failure.reason());
            }
        }
    }
    let external = soap_call(
        config,
        &upnp.control,
        &upnp.service,
        "GetExternalIPAddress",
        &[],
    )
    .await
    .ok()
    .and_then(|body| soap_element(&body, "NewExternalIPAddress"))
    .and_then(|text| text.parse::<Ipv4Addr>().ok());
    Ok(Mapping {
        method: Method::Upnp,
        port,
        external,
        lifetime: (lease != 0).then_some(lease),
        upnp: Some(upnp.clone()),
    })
}

async fn upnp_delete(config: &Config, port: u16, upnp: &Upnp, protocols: &[Protocol]) {
    let quick = Config {
        http_timeout: config.http_timeout.min(Duration::from_secs(2)),
        ..config.clone()
    };
    for protocol in protocols {
        let _ = soap_call(
            &quick,
            &upnp.control,
            &upnp.service,
            "DeletePortMapping",
            &[
                ("NewRemoteHost", String::new()),
                ("NewExternalPort", port.to_string()),
                ("NewProtocol", protocol.upnp_name().to_string()),
            ],
        )
        .await;
    }
}

async fn establish(
    config: &Config,
    route: Route,
    port: u16,
    progress: &mut Progress,
) -> Result<Mapping, String> {
    let mut reasons = Vec::new();
    if config.natpmp {
        match natpmp_map(config, route, port, port, progress).await {
            Ok(mapping) => return Ok(mapping),
            Err(reason) => reasons.push(format!("NAT-PMP: {reason}")),
        }
    }
    if config.upnp {
        match upnp_map(config, route, port, progress).await {
            Ok(mapping) => return Ok(mapping),
            Err(reason) => reasons.push(format!("UPnP: {reason}")),
        }
    }
    if reasons.is_empty() {
        reasons.push("no mapping method is enabled".into());
    }
    Err(reasons.join("; "))
}

async fn renew(
    config: &Config,
    route: Route,
    port: u16,
    mapping: &Mapping,
) -> Result<Mapping, String> {
    let mut progress = Progress::default();
    match &mapping.upnp {
        None => natpmp_map(config, route, port, mapping.port, &mut progress)
            .await
            .map_err(|reason| format!("NAT-PMP renewal: {reason}")),
        Some(upnp) => upnp_add(config, route, port, upnp, &mut progress)
            .await
            .map_err(|reason| format!("UPnP renewal: {reason}")),
    }
}

async fn remove(config: &Config, route: Route, port: u16, mapping: &Mapping) {
    match &mapping.upnp {
        None => natpmp_delete(config, route, port, &[Protocol::Tcp, Protocol::Udp]).await,
        Some(upnp) => upnp_delete(config, port, upnp, &[Protocol::Tcp, Protocol::Udp]).await,
    }
}

async fn remove_progress(config: &Config, route: Route, port: u16, progress: &Progress) {
    natpmp_delete(config, route, port, &progress.natpmp).await;
    if let Some(upnp) = &progress.upnp {
        upnp_delete(config, port, upnp, &progress.upnp_added).await;
    }
}

// ---------------------------------------------------------------------------
// Background maintenance

/// A task keeping the listen port mapped. Dropping it aborts the task
/// without removing the mapping; [`PortMapper::stop`] removes it.
pub struct PortMapper {
    status: watch::Receiver<Status>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl PortMapper {
    /// Start mapping `listen`'s port. Must run inside a Tokio runtime.
    pub fn start(config: Config, listen: SocketAddr) -> Self {
        let (sender, status) = watch::channel(Status::Probing);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(maintain(config, listen, sender, stopped));
        Self {
            status,
            stop: Some(stop),
            task,
        }
    }

    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    /// Remove the mapping and end the task, waiting at most `bound`.
    pub async fn stop(mut self, bound: Duration) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if timeout(bound, &mut self.task).await.is_err() {
            self.task.abort();
        }
    }
}

impl Drop for PortMapper {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn maintain(
    config: Config,
    listen: SocketAddr,
    status: watch::Sender<Status>,
    mut stop: oneshot::Receiver<()>,
) {
    let port = listen.port();
    loop {
        // Resolved per attempt: a cable plugged in or a new DHCP lease
        // changes the gateway. Resolving sends nothing.
        let route = match resolve_route(&config, Some(listen)) {
            Ok(route) => route,
            Err(reason) => {
                status.send_replace(Status::Failed(reason.into()));
                if stopped_during(config.retry_after, &mut stop).await {
                    status.send_replace(Status::Off);
                    return;
                }
                continue;
            }
        };
        status.send_replace(Status::Probing);
        let mut progress = Progress::default();
        let attempt = {
            let attempt = establish(&config, route, port, &mut progress);
            tokio::pin!(attempt);
            tokio::select! {
                result = &mut attempt => Some(result),
                _ = &mut stop => None,
            }
        };
        let mut mapping = match attempt {
            None => {
                remove_progress(&config, route, port, &progress).await;
                status.send_replace(Status::Off);
                return;
            }
            Some(Err(reason)) => {
                status.send_replace(Status::Failed(reason));
                if stopped_during(config.retry_after, &mut stop).await {
                    status.send_replace(Status::Off);
                    return;
                }
                continue;
            }
            Some(Ok(mapping)) => mapping,
        };
        status.send_replace(mapping.status());
        loop {
            tokio::select! {
                _ = tokio::time::sleep(mapping.renew_after(&config)) => {}
                _ = &mut stop => {
                    remove(&config, route, port, &mapping).await;
                    status.send_replace(Status::Off);
                    return;
                }
            }
            match renew(&config, route, port, &mapping).await {
                Ok(renewed) => {
                    mapping = renewed;
                    status.send_replace(mapping.status());
                }
                Err(reason) => {
                    status.send_replace(Status::Failed(reason));
                    break;
                }
            }
        }
    }
}

/// Sleep for `wait`; true when the stop signal came first.
async fn stopped_during(wait: Duration, stop: &mut oneshot::Receiver<()>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(wait) => false,
        _ = stop => true,
    }
}

// ---------------------------------------------------------------------------
// Read-only probe

/// What a read-only probe saw. No mapping is requested.
#[derive(Debug)]
pub struct ProbeReport {
    pub route: Result<Route, &'static str>,
    /// The NAT-PMP external address (opcode 0), or why there was none.
    pub natpmp: Result<Ipv4Addr, &'static str>,
    pub ssdp: Result<SsdpOutcome, &'static str>,
    /// The admitted LOCATION's WAN services, when SSDP found one.
    pub services: Option<Result<Vec<IgdService>, &'static str>>,
}

/// NAT-PMP external-address request, SSDP search, and a GET of the
/// description the gateway advertised. Creates nothing on the router.
pub async fn probe(config: &Config) -> ProbeReport {
    let route = resolve_route(config, None);
    let Ok(found) = route else {
        return ProbeReport {
            route,
            natpmp: Err("no route"),
            ssdp: Err("no route"),
            services: None,
        };
    };
    let natpmp = async {
        let socket = UdpSocket::bind(SocketAddrV4::new(found.local, 0))
            .await
            .map_err(|_| "no UDP socket for NAT-PMP")?;
        let gateway = SocketAddrV4::new(found.gateway, config.natpmp_port);
        match natpmp_exchange(
            &socket,
            gateway,
            &natpmp_external_request(),
            config.natpmp_initial,
            config.natpmp_tries,
        )
        .await?
        {
            NatPmpReply::External { address, .. } => Ok(address),
            NatPmpReply::Mapping { .. } => Err("NAT-PMP reply has the wrong opcode"),
        }
    }
    .await;
    let ssdp = ssdp_search(config, found).await;
    let services = match &ssdp {
        Ok(SsdpOutcome {
            accepted: Some((location, _)),
            ..
        }) => Some(fetch_services(config, location).await),
        _ => None,
    };
    ProbeReport {
        route,
        natpmp,
        ssdp,
        services,
    }
}

#[cfg(test)]
mod tests;
