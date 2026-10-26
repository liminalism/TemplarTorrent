//! Application-owned HTTP tracker announce (BEP-3 over HTTP/1.1).
//!
//! The fork of rbit compiles without reqwest, so announces go through this
//! bounded transport: one TCP connection to an address the discovery module
//! already admitted (no second DNS lookup between classification and use),
//! one GET with `Connection: close`, a response capped at
//! [`MAX_RESPONSE_BYTES`], no redirects followed, and the bencoded body parsed
//! into rbit's [`AnnounceResponse`]. `https://` trackers use rustls 0.23.46
//! with its aws-lc-rs provider. The certificate is checked against the webpki
//! public roots and the name in the tracker URL. Ring is not linked.
//!
//! [`announce_tracker`] is the entry point every driver announce uses: it
//! admits the URL, announces once per address family the host resolved to
//! (one IPv4 and one IPv6 address at most, concurrently), over HTTP(S) or
//! the UDP client in [`crate::udp_announce`], and merges the peers.

use std::{
    borrow::Cow,
    future::Future,
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use rbit::{AnnounceResponse, CompactPeer, TrackerEvent, Value};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::TlsConnector;

use crate::discovery::{ParsedTracker, TrackerScheme};

/// Whole-announce deadline: connect, send, and read to EOF.
pub const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest accepted HTTP response (headers plus body). A compact peer list
/// for 200 peers is about 1.2 KiB; this leaves room for verbose trackers.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// Most peers kept per address-family list of one response.
pub const MAX_PEERS: usize = 512;
/// How long the second address family may keep running once the first has
/// succeeded. A blackholed family must not hold up a healthy announce for
/// its whole transport deadline (the driver awaits announces inline).
pub const FAMILY_GRACE: Duration = Duration::from_secs(3);

pub struct AnnounceRequest<'a> {
    pub info_hash: &'a [u8; 20],
    pub peer_id: &'a [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: TrackerEvent,
}

/// The exact request bytes for `tracker`. The original query string (and
/// any passkey in it) is preserved; announce parameters are appended.
pub fn build_request(
    tracker: &ParsedTracker,
    request: &AnnounceRequest<'_>,
) -> Result<Vec<u8>, &'static str> {
    let rest = tracker
        .url
        .strip_prefix("https://")
        .or_else(|| tracker.url.strip_prefix("http://"))
        .ok_or("tracker scheme must be http or https")?;
    let target = match rest.find(['/', '?']) {
        Some(index) => &rest[index..],
        None => "",
    };
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    let path = if path.is_empty() { "/" } else { path };
    let mut line = String::with_capacity(512);
    line.push_str("GET ");
    line.push_str(path);
    line.push('?');
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        line.push_str(query);
        line.push('&');
    }
    line.push_str("info_hash=");
    line.push_str(&percent_encode_bytes(request.info_hash));
    line.push_str("&peer_id=");
    line.push_str(&percent_encode_bytes(request.peer_id));
    line.push_str(&format!(
        "&port={}&uploaded={}&downloaded={}&left={}&compact=1&numwant=50",
        request.port, request.uploaded, request.downloaded, request.left
    ));
    let event = request.event.as_str();
    if !event.is_empty() {
        line.push_str("&event=");
        line.push_str(event);
    }
    let host = if tracker.host.contains(':') {
        format!("[{}]", tracker.host)
    } else {
        tracker.host.clone()
    };
    let default_port = if tracker.url.starts_with("https://") {
        443
    } else {
        80
    };
    let host_header = if tracker.port == default_port {
        host
    } else {
        format!("{host}:{}", tracker.port)
    };
    line.push_str(" HTTP/1.1\r\nHost: ");
    line.push_str(&host_header);
    line.push_str("\r\nUser-Agent: TemplarTorrent/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n");
    if line.len() > 4096 {
        return Err("announce request is too long");
    }
    Ok(line.into_bytes())
}

/// One announce to `address`, which must be an address the caller resolved
/// and classified for `tracker`.
pub async fn announce(
    tracker: &ParsedTracker,
    address: SocketAddr,
    request: &AnnounceRequest<'_>,
) -> Result<AnnounceResponse, &'static str> {
    announce_with(tracker, address, request, None).await
}

/// Same as [`announce`], with `tls` replacing the public root bundle. Tests
/// use this to trust a localhost certificate. `None` verifies against the
/// webpki roots.
async fn announce_with(
    tracker: &ParsedTracker,
    address: SocketAddr,
    request: &AnnounceRequest<'_>,
    tls: Option<Arc<ClientConfig>>,
) -> Result<AnnounceResponse, &'static str> {
    let bytes = build_request(tracker, request)?;
    let https = tracker.url.starts_with("https://");
    let exchange = async {
        let tcp = TcpStream::connect(address)
            .await
            .map_err(|_| "tracker connection failed")?;
        if https {
            let name = ServerName::try_from(tracker.host.clone())
                .map_err(|_| "tracker host is not a valid TLS server name")?;
            let config = tls.unwrap_or_else(public_tls);
            let connector = TlsConnector::from(config);
            let mut stream = connector
                .connect(name, tcp)
                .await
                .map_err(|_| "tracker TLS handshake failed")?;
            exchange_http(&mut stream, &bytes).await
        } else {
            let mut stream = tcp;
            exchange_http(&mut stream, &bytes).await
        }
    };
    let response = timeout(ANNOUNCE_TIMEOUT, exchange)
        .await
        .map_err(|_| "tracker announce deadline exceeded")??;
    let body = parse_http_response(&response)?;
    parse_announce_body(&body)
}

fn public_tls() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    Arc::clone(CONFIG.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        // No ALPN. Many trackers terminate TLS and then speak HTTP/1.1
        // without negotiating a protocol, and offering one makes rustls
        // reject that handshake. This client always writes HTTP/1.1.
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }))
}

async fn exchange_http<S>(stream: &mut S, bytes: &[u8]) -> Result<Vec<u8>, &'static str>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream
        .write_all(bytes)
        .await
        .map_err(|_| "tracker request failed")?;
    let mut response = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|_| "tracker response read failed")?;
        if n == 0 {
            break;
        }
        if response.len() + n > MAX_RESPONSE_BYTES {
            return Err("tracker response exceeds 64 KiB");
        }
        response.extend_from_slice(&chunk[..n]);
    }
    Ok(response)
}

/// Status line, headers and body of one HTTP/1.x response. Only 200 is
/// accepted; redirects are refused rather than followed. Supports
/// `Content-Length` and chunked transfer encoding, both bounded by the
/// caller's read cap.
pub fn parse_http_response(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let header_end = find(bytes, b"\r\n\r\n").ok_or("tracker response has no header terminator")?;
    let head =
        std::str::from_utf8(&bytes[..header_end]).map_err(|_| "tracker headers are not ASCII")?;
    let mut lines = head.split("\r\n");
    let status = lines.next().ok_or("tracker response has no status line")?;
    let mut parts = status.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err("tracker response is not HTTP/1.x");
    }
    let code: u16 = parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or("tracker status code is invalid")?;
    match code {
        200 => {}
        301 | 302 | 303 | 307 | 308 => return Err("tracker redirect refused"),
        _ => return Err("tracker returned a non-200 status"),
    }
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| "tracker content length is invalid")?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.eq_ignore_ascii_case("chunked")
        {
            chunked = true;
        }
    }
    let body = &bytes[header_end + 4..];
    if chunked {
        return decode_chunked(body);
    }
    match content_length {
        Some(length) if length > body.len() => Err("tracker response body is truncated"),
        Some(length) => Ok(body[..length].to_vec()),
        None => Ok(body.to_vec()),
    }
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::new();
    loop {
        let line_end = find(body, b"\r\n").ok_or("tracker chunk size is truncated")?;
        let size_text =
            std::str::from_utf8(&body[..line_end]).map_err(|_| "tracker chunk size is invalid")?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size =
            usize::from_str_radix(size_text, 16).map_err(|_| "tracker chunk size is invalid")?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        // Checked before `size + 2`, which a 16-digit size would overflow.
        if size > MAX_RESPONSE_BYTES {
            return Err("tracker response exceeds 64 KiB");
        }
        if body.len() < size + 2 {
            return Err("tracker chunk is truncated");
        }
        out.extend_from_slice(&body[..size]);
        if out.len() > MAX_RESPONSE_BYTES {
            return Err("tracker response exceeds 64 KiB");
        }
        body = &body[size + 2..];
    }
}

/// Bencoded announce body to rbit's response type, same fields rbit's own
/// client filled: failure reason, intervals, counts and compact or
/// dictionary peers, capped at [`MAX_PEERS`] per family.
pub fn parse_announce_body(body: &[u8]) -> Result<AnnounceResponse, &'static str> {
    let value = rbit::decode(body).map_err(|_| "tracker response is not bencode")?;
    let dict = value
        .as_dict()
        .ok_or("tracker response is not a dictionary")?;
    if dict.get(b"failure reason".as_slice()).is_some() {
        return Err("tracker reported a failure");
    }
    let integer = |key: &[u8]| dict.get(key).and_then(Value::as_integer);
    let interval = integer(b"interval").ok_or("tracker response lacks an interval")?;
    let interval = u32::try_from(interval).map_err(|_| "tracker interval is out of range")?;
    let mut response = AnnounceResponse::new(interval);
    let small = |value: i64| u32::try_from(value).ok();
    response.min_interval = integer(b"min interval").and_then(small);
    response.complete = integer(b"complete").and_then(small);
    response.incomplete = integer(b"incomplete").and_then(small);
    if let Some(peers) = dict.get(b"peers".as_slice()) {
        if let Some(bytes) = peers.as_bytes() {
            response.peers = bytes
                .as_chunks::<6>()
                .0
                .iter()
                .filter_map(|chunk| CompactPeer::from_v4_bytes(chunk))
                .map(|peer| peer.to_socket_addr())
                .take(MAX_PEERS)
                .collect();
        } else if let Some(list) = peers.as_list() {
            for peer in list.iter().take(MAX_PEERS) {
                let Some(entry) = peer.as_dict() else {
                    continue;
                };
                let ip = entry
                    .get(b"ip".as_slice())
                    .and_then(Value::as_str)
                    .and_then(|text| text.parse().ok());
                let port = entry
                    .get(b"port".as_slice())
                    .and_then(Value::as_integer)
                    .and_then(|port| u16::try_from(port).ok());
                if let (Some(ip), Some(port)) = (ip, port) {
                    response.peers.push(SocketAddr::new(ip, port));
                }
            }
        }
    }
    if let Some(bytes) = dict.get(b"peers6".as_slice()).and_then(Value::as_bytes) {
        response.peers6 = bytes
            .as_chunks::<18>()
            .0
            .iter()
            .filter_map(|chunk| CompactPeer::from_v6_bytes(chunk))
            .map(|peer| peer.to_socket_addr())
            .take(MAX_PEERS)
            .collect();
    }
    Ok(response)
}

/// Why an announce failed. `summary` is a fixed phrase for error values;
/// `detail` is what the Trackers tab shows: one reason per address family
/// tried, including a UDP tracker's own error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceFailure {
    pub summary: &'static str,
    pub detail: String,
}

impl From<&'static str> for AnnounceFailure {
    fn from(reason: &'static str) -> Self {
        Self {
            summary: reason,
            detail: reason.to_string(),
        }
    }
}

/// A transport's reason for one family: fixed text, or a tracker message.
pub type FamilyError = Cow<'static, str>;

/// Admit `url` (resolve and classify every address, as at add time), then
/// announce once per address family over the URL's scheme. The tracker is
/// healthy when either family answered; the peers of both are merged.
pub async fn announce_tracker(
    url: &str,
    allow_local: bool,
    request: &AnnounceRequest<'_>,
) -> Result<AnnounceResponse, AnnounceFailure> {
    let admitted = crate::discovery::admit_tracker(url, allow_local).await?;
    let addresses = admitted.connect_addresses();
    let tracker = &admitted.tracker;
    announce_per_family(&addresses, FAMILY_GRACE, |address| async move {
        match tracker.scheme {
            TrackerScheme::Udp => crate::udp_announce::announce(tracker, address, request).await,
            TrackerScheme::Http | TrackerScheme::Https => announce(tracker, address, request)
                .await
                .map_err(Cow::Borrowed),
        }
    })
    .await
}

/// Run `announce_one` for each of `addresses` (at most one per family is
/// expected; only the first two are used) concurrently. After one succeeds
/// the other gets at most `grace` more. Any success is merged into one
/// response; otherwise every family's reason is reported.
pub async fn announce_per_family<F, Fut>(
    addresses: &[SocketAddr],
    grace: Duration,
    announce_one: F,
) -> Result<AnnounceResponse, AnnounceFailure>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = Result<AnnounceResponse, FamilyError>>,
{
    let outcomes: Vec<(SocketAddr, Result<AnnounceResponse, FamilyError>)> = match addresses {
        [] => return Err("tracker host did not resolve".into()),
        [only] => vec![(*only, announce_one(*only).await)],
        [first, second, ..] => {
            let a = announce_one(*first);
            let b = announce_one(*second);
            tokio::pin!(a);
            tokio::pin!(b);
            let (first_done, early) = tokio::select! {
                outcome = &mut a => (true, outcome),
                outcome = &mut b => (false, outcome),
            };
            let rest = if first_done { b.as_mut() } else { a.as_mut() };
            let late = if early.is_ok() {
                tokio::time::timeout(grace, rest)
                    .await
                    .unwrap_or(Err(Cow::Borrowed(
                        "cut short after the other address family answered",
                    )))
            } else {
                rest.await
            };
            if first_done {
                vec![(*first, early), (*second, late)]
            } else {
                vec![(*first, late), (*second, early)]
            }
        }
    };
    let family = |address: &SocketAddr| if address.is_ipv4() { "IPv4" } else { "IPv6" };
    let mut answered = Vec::new();
    let mut failures = Vec::new();
    for (address, outcome) in outcomes {
        match outcome {
            Ok(response) => answered.push(response),
            Err(reason) => failures.push((family(&address), reason)),
        }
    }
    if !answered.is_empty() {
        return Ok(merge_responses(answered));
    }
    if failures.len() == 1
        && let Some((_, reason)) = failures.pop()
    {
        let detail = reason.to_string();
        let summary = match reason {
            Cow::Borrowed(text) => text,
            Cow::Owned(_) => "tracker reported an error",
        };
        return Err(AnnounceFailure { summary, detail });
    }
    let detail = failures
        .iter()
        .map(|(family, reason)| format!("{family}: {reason}"))
        .collect::<Vec<_>>()
        .join("; ");
    Err(AnnounceFailure {
        summary: "tracker announce failed on every address family",
        detail,
    })
}

/// One response from the answers of several address families. IPv4 peers go
/// to `peers` and IPv6 peers to `peers6` whichever list carried them, each
/// deduplicated and capped at [`MAX_PEERS`]. The shortest interval wins but
/// never undercuts the longest minimum interval; counts take the maximum,
/// since each family reports the same swarm.
pub fn merge_responses(responses: Vec<AnnounceResponse>) -> AnnounceResponse {
    let interval = responses
        .iter()
        .map(|response| response.interval)
        .min()
        .unwrap_or(0);
    let min_interval = responses
        .iter()
        .filter_map(|response| response.min_interval)
        .max();
    let mut merged = AnnounceResponse::new(min_interval.map_or(interval, |m| interval.max(m)));
    merged.min_interval = min_interval;
    merged.complete = responses
        .iter()
        .filter_map(|response| response.complete)
        .max();
    merged.incomplete = responses
        .iter()
        .filter_map(|response| response.incomplete)
        .max();
    for response in responses {
        merged.warning_message = merged.warning_message.or(response.warning_message);
        merged.tracker_id = merged.tracker_id.or(response.tracker_id);
        for address in response.peers.into_iter().chain(response.peers6) {
            let list = if address.is_ipv4() {
                &mut merged.peers
            } else {
                &mut merged.peers6
            };
            if list.len() < MAX_PEERS && !list.contains(&address) {
                list.push(address);
            }
        }
    }
    merged
}

fn percent_encode_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * 3);
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0xf) as usize] as char);
        }
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::parse_tracker_url;

    fn request<'a>(hash: &'a [u8; 20], id: &'a [u8; 20]) -> AnnounceRequest<'a> {
        AnnounceRequest {
            info_hash: hash,
            peer_id: id,
            port: 6881,
            uploaded: 1,
            downloaded: 2,
            left: 3,
            event: TrackerEvent::Started,
        }
    }

    #[test]
    fn request_preserves_passkey_query_and_percent_encodes_binary() {
        let tracker =
            parse_tracker_url("http://tracker.example:8080/announce?passkey=abc").unwrap();
        let hash = [
            0x00, 0xff, b'a', b' ', 0x7e, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let id = [b'-'; 20];
        let bytes = build_request(&tracker, &request(&hash, &id)).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.starts_with("GET /announce?passkey=abc&info_hash=%00%FFa%20~%00"),
            "{text}"
        );
        assert!(text.contains("&peer_id=--------------------&port=6881&uploaded=1&downloaded=2&left=3&compact=1&numwant=50&event=started HTTP/1.1\r\n"));
        assert!(text.contains("\r\nHost: tracker.example:8080\r\n"));
        assert!(text.ends_with("Connection: close\r\n\r\n"));
        let bare = parse_tracker_url("http://tracker.example").unwrap();
        let text = String::from_utf8(build_request(&bare, &request(&hash, &id)).unwrap()).unwrap();
        assert!(text.starts_with("GET /?info_hash="));
        assert!(text.contains("\r\nHost: tracker.example\r\n"));
        let https = parse_tracker_url("https://tracker.example/announce").unwrap();
        let text = String::from_utf8(build_request(&https, &request(&hash, &id)).unwrap()).unwrap();
        assert!(text.starts_with("GET /announce?info_hash="), "{text}");
        assert!(text.contains("\r\nHost: tracker.example\r\n"), "{text}");
        let custom = parse_tracker_url("https://tracker.example:8443/announce").unwrap();
        let text =
            String::from_utf8(build_request(&custom, &request(&hash, &id)).unwrap()).unwrap();
        assert!(
            text.contains("\r\nHost: tracker.example:8443\r\n"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn https_announce_checks_the_certificate_name() {
        let ca = test_ca();
        let trusted = issued_cert("localhost", &ca);
        let trusted_config = tls_trusting(&ca);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(trusted.server));
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(tcp).await.unwrap();
            let mut got = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&chunk[..n]);
                if got.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let body = b"d8:intervali60e5:peers0:e";
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            stream.shutdown().await.unwrap();
            String::from_utf8(got).unwrap()
        });

        let tracker = parse_tracker_url("https://localhost/announce").unwrap();
        let hash = [1u8; 20];
        let id = [2u8; 20];
        let response = announce_with(
            &tracker,
            address,
            &request(&hash, &id),
            Some(trusted_config),
        )
        .await
        .unwrap();
        assert_eq!(response.interval, 60);
        let request_text = server.await.unwrap();
        assert!(request_text.starts_with("GET /announce?"), "{request_text}");
        assert!(
            request_text.contains("\r\nHost: localhost\r\n"),
            "{request_text}"
        );

        let mismatched = issued_cert("other.example", &ca);
        let mismatch_config = tls_trusting(&ca);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(mismatched.server));
        tokio::spawn(async move {
            if let Ok((tcp, _)) = listener.accept().await {
                let _ = acceptor.accept(tcp).await;
            }
        });
        let error = announce_with(
            &tracker,
            address,
            &request(&hash, &id),
            Some(mismatch_config),
        )
        .await
        .unwrap_err();
        assert_eq!(error, "tracker TLS handshake failed");
    }

    struct TestCa {
        cert: rcgen::Certificate,
        key: rcgen::KeyPair,
        der: rustls::pki_types::CertificateDer<'static>,
    }

    struct Issued {
        server: rustls::ServerConfig,
    }

    fn test_ca() -> TestCa {
        use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};

        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params
            .distinguished_name
            .push(DnType::CommonName, "Templar Test CA");
        let cert = params.self_signed(&key).unwrap();
        let der = cert.der().clone();
        TestCa { cert, key, der }
    }

    fn issued_cert(name: &str, ca: &TestCa) -> Issued {
        use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
        use rustls::pki_types::PrivateKeyDer;

        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![name.to_string()]).unwrap();
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let cert = params.signed_by(&key, &ca.cert, &ca.key).unwrap();
        let key_der = PrivateKeyDer::Pkcs8(key.serialize_der().into());
        let server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key_der)
            .unwrap();
        Issued { server }
    }

    fn tls_trusting(ca: &TestCa) -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        roots.add(ca.der.clone()).unwrap();
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }

    #[test]
    fn http_parsing_accepts_200_with_length_or_chunks_and_refuses_the_rest() {
        let body = b"d8:intervali60ee";
        let plain = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let mut bytes = plain.into_bytes();
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(b"trailing junk");
        assert_eq!(parse_http_response(&bytes).unwrap(), body);
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nd8:i\r\nC;ext\r\nntervali60ee\r\n0\r\n\r\n";
        assert_eq!(parse_http_response(chunked).unwrap(), body);
        for (response, reason) in [
            (
                &b"HTTP/1.1 302 Found\r\nLocation: http://x/\r\n\r\n"[..],
                "redirect",
            ),
            (b"HTTP/1.1 404 Not Found\r\n\r\n", "non-200"),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\nshort",
                "truncated",
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
                "chunk size",
            ),
            (b"HTTP/1.1 200 OK\r\nContent-Length: 2", "terminator"),
            (b"ICY 200 OK\r\n\r\n", "not HTTP"),
        ] {
            let error = parse_http_response(response).unwrap_err();
            assert!(error.contains(reason), "{reason}: {error}");
        }
    }

    #[test]
    fn announce_body_parses_compact_and_dict_peers_and_refuses_failures() {
        let mut body =
            b"d8:completei3e10:incompletei4e8:intervali60e12:min intervali30e5:peers12:".to_vec();
        body.extend_from_slice(&[127, 0, 0, 1, 0x1a, 0xe1, 10, 0, 0, 9, 0, 80]);
        body.extend_from_slice(b"6:peers618:");
        body.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0x1a, 0xe1]);
        body.push(b'e');
        let response = parse_announce_body(&body).unwrap();
        assert_eq!(response.interval, 60);
        assert_eq!(response.min_interval, Some(30));
        assert_eq!((response.complete, response.incomplete), (Some(3), Some(4)));
        assert_eq!(
            response.peers,
            vec![
                "127.0.0.1:6881".parse().unwrap(),
                "10.0.0.9:80".parse().unwrap()
            ]
        );
        assert_eq!(response.peers6, vec!["[::1]:6881".parse().unwrap()]);
        let dict = b"d8:intervali60e5:peersld2:ip9:127.0.0.24:porti7000eeee";
        assert_eq!(
            parse_announce_body(dict).unwrap().peers,
            vec!["127.0.0.2:7000".parse().unwrap()]
        );
        assert_eq!(
            parse_announce_body(b"d14:failure reason7:bad keye").unwrap_err(),
            "tracker reported a failure"
        );
        assert!(
            parse_announce_body(b"d5:peers0:e")
                .unwrap_err()
                .contains("interval")
        );
        assert!(
            parse_announce_body(b"not bencode")
                .unwrap_err()
                .contains("bencode")
        );
        assert!(
            parse_announce_body(b"li1ee")
                .unwrap_err()
                .contains("dictionary")
        );
    }

    fn answer(interval: u32, peers: &[&str]) -> AnnounceResponse {
        let mut response = AnnounceResponse::new(interval);
        response.peers = peers.iter().map(|peer| peer.parse().unwrap()).collect();
        response
    }

    const V4: &str = "1.1.1.1:6969";
    const V6: &str = "[2001:db8::1]:6969";

    fn both() -> [SocketAddr; 2] {
        [V4.parse().unwrap(), V6.parse().unwrap()]
    }

    #[test]
    fn merged_responses_sort_peers_by_family_dedupe_and_respect_intervals() {
        let mut v4 = answer(1800, &["10.0.0.1:1", "10.0.0.2:2"]);
        v4.min_interval = Some(900);
        v4.complete = Some(4);
        // An HTTP tracker over IPv6 can list both families in `peers`.
        let mut v6 = answer(600, &["10.0.0.2:2", "[2001:db8::9]:3"]);
        v6.peers6 = vec!["[2001:db8::9]:3".parse().unwrap()];
        v6.incomplete = Some(25);
        v6.complete = Some(2);
        let merged = merge_responses(vec![v4, v6]);
        assert_eq!(merged.interval, 900, "shortest interval, floored at min");
        assert_eq!(merged.min_interval, Some(900));
        assert_eq!((merged.complete, merged.incomplete), (Some(4), Some(25)));
        assert_eq!(
            merged.peers,
            vec![
                "10.0.0.1:1".parse::<SocketAddr>().unwrap(),
                "10.0.0.2:2".parse().unwrap()
            ]
        );
        assert_eq!(merged.peers6, vec!["[2001:db8::9]:3".parse().unwrap()]);
        let many: Vec<String> = (0..MAX_PEERS + 5)
            .map(|port| format!("10.1.0.1:{}", port + 1))
            .collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let merged = merge_responses(vec![answer(60, &refs), answer(60, &refs)]);
        assert_eq!(merged.peers.len(), MAX_PEERS);
    }

    #[tokio::test(start_paused = true)]
    async fn one_family_failing_leaves_the_tracker_healthy() {
        let response = announce_per_family(&both(), FAMILY_GRACE, |address| async move {
            if address.is_ipv4() {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Err(Cow::Borrowed("tracker connection failed"))
            } else {
                Ok(answer(60, &["[2001:db8::7]:1"]))
            }
        })
        .await
        .unwrap();
        assert_eq!(response.peers6.len(), 1);
        assert!(response.peers.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn every_family_failing_reports_each_reason() {
        let failure = announce_per_family(&both(), FAMILY_GRACE, |address| async move {
            if address.is_ipv4() {
                Err(Cow::Borrowed("tracker connection failed"))
            } else {
                Err(Cow::Owned("tracker error: not registered".to_string()))
            }
        })
        .await
        .unwrap_err();
        assert_eq!(
            failure.summary,
            "tracker announce failed on every address family"
        );
        assert_eq!(
            failure.detail,
            "IPv4: tracker connection failed; IPv6: tracker error: not registered"
        );
        let only = [V6.parse().unwrap()];
        let failure = announce_per_family(&only, FAMILY_GRACE, |_| async {
            Err(Cow::Owned("tracker error: banned".to_string()))
        })
        .await
        .unwrap_err();
        assert_eq!(failure.summary, "tracker reported an error");
        assert_eq!(failure.detail, "tracker error: banned");
        let failure = announce_per_family(&[], FAMILY_GRACE, |_| async { Ok(answer(60, &[])) })
            .await
            .unwrap_err();
        assert_eq!(failure.summary, "tracker host did not resolve");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_family_is_cut_after_the_grace_once_the_other_answers() {
        let started = tokio::time::Instant::now();
        let response = announce_per_family(&both(), FAMILY_GRACE, |address| async move {
            if address.is_ipv4() {
                Ok(answer(60, &["10.0.0.1:1"]))
            } else {
                std::future::pending().await
            }
        })
        .await
        .unwrap();
        assert_eq!(started.elapsed(), FAMILY_GRACE);
        assert_eq!(response.peers.len(), 1);
        // A family that answers inside the grace is merged in.
        let response = announce_per_family(&both(), FAMILY_GRACE, |address| async move {
            if address.is_ipv4() {
                Ok(answer(60, &["10.0.0.1:1"]))
            } else {
                tokio::time::sleep(FAMILY_GRACE / 2).await;
                Ok(answer(60, &["[2001:db8::7]:1"]))
            }
        })
        .await
        .unwrap();
        assert_eq!((response.peers.len(), response.peers6.len()), (1, 1));
    }

    /// The same port on 127.0.0.1 and ::1, so `localhost` reaches both.
    async fn dual_stack_udp(
        fault: crate::udp_announce::fixture::Fault,
    ) -> Option<[crate::udp_announce::fixture::Tracker; 2]> {
        use crate::udp_announce::fixture::start;
        for _ in 0..20 {
            let v4 = start("127.0.0.1:0".parse().unwrap(), fault).await?;
            let v6_bind = SocketAddr::new("::1".parse().unwrap(), v4.address.port());
            if let Some(v6) = start(v6_bind, fault).await {
                return Some([v4, v6]);
            }
        }
        None
    }

    #[tokio::test]
    async fn udp_localhost_announces_over_both_families_and_merges_peers() {
        use crate::udp_announce::fixture::{Fault, peer_for};
        let Some([v4, v6]) = dual_stack_udp(Fault::None).await else {
            eprintln!("skipping: no ::1 on this host");
            return;
        };
        let url = format!("udp://localhost:{}/announce", v4.address.port());
        let hash = [3u8; 20];
        let id = [4u8; 20];
        let refused = announce_tracker(&url, false, &request(&hash, &id))
            .await
            .unwrap_err();
        assert_eq!(
            refused.summary,
            "tracker resolves to a local or private address"
        );
        let response = announce_tracker(&url, true, &request(&hash, &id))
            .await
            .unwrap();
        assert_eq!(response.peers, vec![peer_for(v4.address, 0)]);
        assert_eq!(response.peers6, vec![peer_for(v6.address, 0)]);
        assert_eq!((v4.announces(), v6.announces()), (1, 1));
        assert_eq!((response.complete, response.incomplete), (Some(3), Some(4)));
    }

    #[tokio::test]
    async fn udp_errors_on_both_families_are_reported_per_family() {
        use crate::udp_announce::fixture::Fault;
        let Some([v4, _v6]) = dual_stack_udp(Fault::Error).await else {
            eprintln!("skipping: no ::1 on this host");
            return;
        };
        let url = format!("udp://localhost:{}", v4.address.port());
        let hash = [3u8; 20];
        let id = [4u8; 20];
        let failure = announce_tracker(&url, true, &request(&hash, &id))
            .await
            .unwrap_err();
        assert_eq!(
            failure.detail,
            "IPv4: tracker error: torrent not registered; IPv6: tracker error: torrent not registered"
        );
    }

    #[tokio::test]
    async fn http_localhost_announces_over_both_families() {
        async fn serve(listener: tokio::net::TcpListener, body: &'static [u8]) {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut got = Vec::new();
                let mut chunk = [0u8; 1024];
                while !got.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => got.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body).await;
            }
        }
        let mut pair = None;
        for _ in 0..20 {
            let v4 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = v4.local_addr().unwrap().port();
            match tokio::net::TcpListener::bind(("::1", port)).await {
                Ok(v6) => {
                    pair = Some((v4, v6));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(_) => break,
            }
        }
        let Some((v4, v6)) = pair else {
            eprintln!("skipping: no ::1 on this host");
            return;
        };
        let port = v4.local_addr().unwrap().port();
        // Dictionary peers, the way the Ubuntu tracker answers.
        let a = tokio::spawn(serve(
            v4,
            b"d8:intervali1800e5:peersld2:ip8:10.0.0.14:porti1eeee",
        ));
        let b = tokio::spawn(serve(
            v6,
            b"d8:intervali1800e5:peersld2:ip8:10.0.0.14:porti1eed2:ip13:2001:db8::1:24:porti2eeee",
        ));
        let hash = [3u8; 20];
        let id = [4u8; 20];
        let url = format!("http://localhost:{port}/announce");
        let response = announce_tracker(&url, true, &request(&hash, &id))
            .await
            .unwrap();
        assert_eq!(response.peers, vec!["10.0.0.1:1".parse().unwrap()]);
        assert_eq!(response.peers6, vec!["[2001:db8::1:2]:2".parse().unwrap()]);
        a.abort();
        b.abort();
    }
}
