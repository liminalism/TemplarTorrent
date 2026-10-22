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

use std::{
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

use crate::discovery::ParsedTracker;

/// Whole-announce deadline: connect, send, and read to EOF.
pub const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest accepted HTTP response (headers plus body). A compact peer list
/// for 200 peers is about 1.2 KiB; this leaves room for verbose trackers.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_PEERS: usize = 512;

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
}
