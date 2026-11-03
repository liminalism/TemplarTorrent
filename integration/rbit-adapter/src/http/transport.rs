//! One HTTP/1.1 GET over the application's transport, streamed.
//!
//! The tracker announce path reads a whole response into memory; a file
//! download cannot. [`open`] admits the URL (scheme, userinfo, length, every
//! resolved address classified before any connection), follows at most
//! [`MAX_REDIRECTS`] redirects re-admitting each hop, speaks HTTP/1.1 with
//! `Connection: close` over TCP or rustls, parses the status line and
//! headers (bounded by [`MAX_HEAD_BYTES`]), and returns a [`Connection`]
//! whose [`Connection::read`] yields decoded body bytes: a `Content-Length`
//! body stops at its length, a chunked body is de-chunked, anything else is
//! read to the server's close. Nothing here writes files or decides ranges.

use std::{borrow::Cow, net::SocketAddr, pin::Pin, sync::Arc, time::Duration};

use rustls::{ClientConfig, pki_types::ServerName};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::TlsConnector;

use crate::discovery::{classify_addresses, resolve_host, split_host_port};

/// Longest accepted URL, before and after each redirect.
pub const MAX_URL_LEN: usize = 2048;
/// Redirects followed for one request; the next is refused.
pub const MAX_REDIRECTS: usize = 5;
/// Status line plus headers may not exceed this.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;
/// TCP connect and TLS handshake deadline per hop.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// A connection is dropped after this long without a byte. There is no
/// deadline on the whole transfer.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// A parsed, syntactically admitted `http` or `https` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    /// Normalised URL without any fragment.
    pub url: String,
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// Request target: path and query, starting with `/`.
    pub target: String,
}

impl ParsedUrl {
    /// The URL with its query replaced, for labels and logs.
    pub fn redacted(&self) -> String {
        crate::discovery::redact_tracker_url(&self.url)
    }

    /// Last path segment, percent-decoded, for a file name. Empty when the
    /// path ends in `/` or has no segment.
    pub fn last_segment(&self) -> String {
        let path = self.target.split('?').next().unwrap_or("");
        let segment = path.rsplit('/').next().unwrap_or("");
        percent_decode(segment)
    }
}

/// Syntax only; nothing is resolved. Userinfo, whitespace, fragments
/// beyond the first `#`, other schemes and over-long URLs are refused.
pub fn parse_url(raw: &str) -> Result<ParsedUrl, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_URL_LEN {
        return Err("URL length is invalid");
    }
    if raw.contains([' ', '\n', '\r', '\t', '\0']) {
        return Err("URL contains whitespace");
    }
    let (https, rest) = if let Some(rest) = raw.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = raw.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err("URL scheme must be http or https");
    };
    let (hostport, path) = match rest.find(['/', '?', '#']) {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if hostport.contains('@') {
        return Err("URL userinfo is rejected");
    }
    if hostport.is_empty() {
        return Err("URL host is missing");
    }
    let (host, port) = split_host_port(hostport, if https { 443 } else { 80 })
        .map_err(|_| "URL host or port is invalid")?;
    if host.is_empty() {
        return Err("URL host is missing");
    }
    let path = path.split('#').next().unwrap_or("");
    let target = if path.is_empty() || path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    if !target.bytes().all(|byte| (0x21..0x7f).contains(&byte)) {
        return Err("URL path contains characters that must be percent-encoded");
    }
    let url = format!(
        "{}://{hostport}{}",
        if https { "https" } else { "http" },
        if target == "/" && path.is_empty() {
            String::new()
        } else {
            target.clone()
        }
    );
    Ok(ParsedUrl {
        url,
        https,
        host,
        port,
        target,
    })
}

/// Resolve and classify every address of `url`. Local, private, link-local
/// and metadata addresses need `allow_local`. Returns the addresses to
/// connect to, at most one per family, IPv4 first.
pub async fn admit_url(
    url: &ParsedUrl,
    allow_local: bool,
) -> Result<Vec<SocketAddr>, &'static str> {
    let addresses = resolve_host(&url.host, url.port)
        .await
        .map_err(|_| "URL host did not resolve")?;
    classify_addresses(&addresses, allow_local).map_err(|reason| match reason {
        "tracker resolves to a local or private address" => {
            "URL resolves to a local or private address; pass --allow-local-urls to allow it"
        }
        "tracker resolves to both public and private addresses" => {
            "URL resolves to both public and private addresses"
        }
        _ => "URL host did not resolve",
    })?;
    let canonical = || addresses.iter().map(|ip| ip.to_canonical());
    let v4 = canonical().find(std::net::IpAddr::is_ipv4);
    let v6 = canonical().find(std::net::IpAddr::is_ipv6);
    Ok(v4
        .into_iter()
        .chain(v6)
        .map(|ip| SocketAddr::new(ip, url.port))
        .collect())
}

/// What one GET asks for.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// `Range: bytes=start-end` with `end` inclusive, or open when `None`.
    pub range: Option<(u64, Option<u64>)>,
    /// `If-Range` validator, sent verbatim.
    pub if_range: Option<String>,
}

/// Status line and headers of one response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    /// First header named `name`, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length")
            .and_then(|value| value.trim().parse().ok())
    }

    pub fn chunked(&self) -> bool {
        self.header("transfer-encoding")
            .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    }

    /// `Content-Range: bytes first-last/total` as `(first, last, total)`;
    /// an unknown total (`*`) is `None`.
    pub fn content_range(&self) -> Option<(u64, u64, Option<u64>)> {
        let value = self.header("content-range")?.trim();
        let rest = value.strip_prefix("bytes ")?;
        let (range, total) = rest.split_once('/')?;
        let (first, last) = range.split_once('-')?;
        let first = first.trim().parse().ok()?;
        let last = last.trim().parse().ok()?;
        let total = match total.trim() {
            "*" => None,
            text => Some(text.parse().ok()?),
        };
        (first <= last && total.is_none_or(|total| last < total)).then_some((first, last, total))
    }

    /// `Accept-Ranges: bytes`.
    pub fn accepts_ranges(&self) -> bool {
        self.header("accept-ranges")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("bytes"))
    }

    /// A `Content-Encoding` other than identity means the byte stream is not
    /// the file's bytes, so offsets cannot be trusted.
    pub fn identity_encoded(&self) -> bool {
        self.header("content-encoding")
            .is_none_or(|value| value.trim().eq_ignore_ascii_case("identity"))
    }
}

type Stream = Pin<Box<dyn AsyncRead + Send>>;

enum Body {
    /// Exactly this many bytes remain.
    Length(u64),
    /// Chunked: bytes left in the current chunk, or `None` between chunks.
    Chunked { remaining: Option<u64>, done: bool },
    /// Until the server closes.
    UntilClose,
}

/// An open response whose body is read through [`Connection::read`].
pub struct Connection {
    stream: Stream,
    buffer: Vec<u8>,
    body: Body,
    pub head: ResponseHead,
    /// The URL that answered, after redirects.
    pub url: ParsedUrl,
    pub address: SocketAddr,
    idle: Duration,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("status", &self.head.status)
            .field("url", &self.url.redacted())
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Decoded body bytes into `buf`; `Ok(0)` at the end of the body. Waits at
    /// most the idle timeout for each read and reports `TimedOut` after it.
    pub async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            enum Step {
                Done,
                Fill(Option<u64>),
                ChunkEnd,
                ChunkSize,
            }
            let step = match &self.body {
                Body::Length(0) => Step::Done,
                Body::Length(remaining) => Step::Fill(Some(*remaining)),
                Body::UntilClose => Step::Fill(None),
                Body::Chunked { done: true, .. } => Step::Done,
                Body::Chunked {
                    remaining: Some(0), ..
                } => Step::ChunkEnd,
                Body::Chunked {
                    remaining: Some(remaining),
                    ..
                } => Step::Fill(Some(*remaining)),
                Body::Chunked {
                    remaining: None, ..
                } => Step::ChunkSize,
            };
            match step {
                Step::Done => return Ok(0),
                Step::Fill(None) => return self.fill(buf).await,
                Step::Fill(Some(remaining)) => {
                    let want = buf
                        .len()
                        .min(usize::try_from(remaining).unwrap_or(usize::MAX));
                    let n = self.fill(&mut buf[..want]).await?;
                    if n == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "the server closed before the announced length",
                        ));
                    }
                    match &mut self.body {
                        Body::Length(left) => *left -= n as u64,
                        Body::Chunked {
                            remaining: Some(left),
                            ..
                        } => *left -= n as u64,
                        _ => {}
                    }
                    return Ok(n);
                }
                Step::ChunkEnd => {
                    let line = self.read_line().await?;
                    if !line.is_empty() {
                        return Err(invalid("chunk is not terminated by CRLF"));
                    }
                    self.body = Body::Chunked {
                        remaining: None,
                        done: false,
                    };
                }
                Step::ChunkSize => {
                    let line = self.read_line().await?;
                    let size_text = line.split(';').next().unwrap_or("").trim();
                    let size = u64::from_str_radix(size_text, 16)
                        .map_err(|_| invalid("chunk size is invalid"))?;
                    if size == 0 {
                        loop {
                            let trailer = self.read_line().await?;
                            if trailer.is_empty() {
                                break;
                            }
                        }
                        self.body = Body::Chunked {
                            remaining: None,
                            done: true,
                        };
                        return Ok(0);
                    }
                    self.body = Body::Chunked {
                        remaining: Some(size),
                        done: false,
                    };
                }
            }
        }
    }

    /// Bytes from the buffered head remainder first, then the socket.
    async fn fill(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.buffer.is_empty() {
            let n = buf.len().min(self.buffer.len());
            buf[..n].copy_from_slice(&self.buffer[..n]);
            self.buffer.drain(..n);
            return Ok(n);
        }
        match timeout(self.idle, self.stream.read(buf)).await {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "no data for 30 s",
            )),
        }
    }

    async fn read_line(&mut self) -> std::io::Result<String> {
        let mut line = Vec::new();
        loop {
            if let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
                line.extend_from_slice(&self.buffer[..index]);
                self.buffer.drain(..=index);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return String::from_utf8(line).map_err(|_| invalid("chunk line is not ASCII"));
            }
            if self.buffer.len() + line.len() > 1024 {
                return Err(invalid("chunk line is too long"));
            }
            let mut chunk = [0u8; 1024];
            let n = match timeout(self.idle, self.stream.read(&mut chunk)).await {
                Ok(result) => result?,
                Err(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "no data for 30 s",
                    ));
                }
            };
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the server closed inside the chunk framing",
                ));
            }
            self.buffer.extend_from_slice(&chunk[..n]);
        }
    }
}

fn invalid(reason: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, reason)
}

/// Reasons [`open`] can fail. Fixed text for most; the server's status for
/// the rest.
pub type OpenError = Cow<'static, str>;

/// Admit `url`, connect, send one GET, follow redirects, and return the open
/// response. `tls` replaces the public roots (tests trust a local CA).
/// `idle` is the per-read deadline, the production value being
/// [`IDLE_TIMEOUT`].
pub async fn open(
    url: &ParsedUrl,
    request: &Request,
    allow_local: bool,
    tls: Option<Arc<ClientConfig>>,
    idle: Duration,
) -> Result<Connection, OpenError> {
    let mut current = url.clone();
    for hop in 0..=MAX_REDIRECTS {
        let addresses = admit_url(&current, allow_local).await?;
        let mut failures: Vec<String> = Vec::new();
        let mut answered = None;
        for address in addresses {
            match open_once(&current, address, request, tls.clone(), idle).await {
                Ok(connection) => {
                    answered = Some(connection);
                    break;
                }
                Err(reason) => {
                    let family = if address.is_ipv4() { "IPv4" } else { "IPv6" };
                    failures.push(format!("{family}: {reason}"));
                }
            }
        }
        let Some(connection) = answered else {
            return Err(match failures.len() {
                0 => Cow::Borrowed("URL host did not resolve"),
                1 => Cow::Owned(failures.remove(0).split_once(": ").map_or_else(
                    || "connection failed".to_string(),
                    |(_, reason)| reason.to_string(),
                )),
                _ => Cow::Owned(failures.join("; ")),
            });
        };
        let status = connection.head.status;
        if !matches!(status, 301 | 302 | 303 | 307 | 308) {
            return Ok(connection);
        }
        if hop == MAX_REDIRECTS {
            return Err("too many redirects (the limit is 5)".into());
        }
        let location = connection
            .head
            .header("location")
            .ok_or("redirect without a Location header")?
            .trim()
            .to_string();
        drop(connection);
        let next = resolve_location(&current, &location)?;
        if current.https && !next.https {
            return Err("redirect from https to http refused".into());
        }
        current = next;
    }
    Err("too many redirects (the limit is 5)".into())
}

/// `Location` may be absolute, scheme-relative, absolute-path or relative.
pub fn resolve_location(base: &ParsedUrl, location: &str) -> Result<ParsedUrl, OpenError> {
    if location.is_empty() {
        return Err("redirect Location is empty".into());
    }
    let joined = if location.contains("://") {
        location.to_string()
    } else if let Some(rest) = location.strip_prefix("//") {
        format!("{}://{rest}", if base.https { "https" } else { "http" })
    } else {
        let authority = base.url.split('/').nth(2).unwrap_or(&base.host).to_string();
        let scheme = if base.https { "https" } else { "http" };
        if location.starts_with('/') {
            format!("{scheme}://{authority}{location}")
        } else if location.starts_with('?') {
            let path = base.target.split('?').next().unwrap_or("/");
            format!("{scheme}://{authority}{path}{location}")
        } else {
            let path = base.target.split('?').next().unwrap_or("/");
            let directory = &path[..=path.rfind('/').unwrap_or(0)];
            format!("{scheme}://{authority}{directory}{location}")
        }
    };
    parse_url(&joined).map_err(|reason| Cow::Owned(format!("redirect refused: {reason}")))
}

async fn open_once(
    url: &ParsedUrl,
    address: SocketAddr,
    request: &Request,
    tls: Option<Arc<ClientConfig>>,
    idle: Duration,
) -> Result<Connection, OpenError> {
    let bytes = build_request(url, request);
    let connect = async {
        let mut tcp = TcpStream::connect(address)
            .await
            .map_err(|_| Cow::Borrowed("connection failed"))?;
        let _ = tcp.set_nodelay(true);
        let stream: Stream = if url.https {
            let name = ServerName::try_from(url.host.clone())
                .map_err(|_| Cow::Borrowed("host is not a valid TLS server name"))?;
            let config = tls.unwrap_or_else(crate::announce::public_tls);
            let connector = TlsConnector::from(config);
            let mut stream = connector
                .connect(name, tcp)
                .await
                .map_err(|_| Cow::Borrowed("TLS handshake failed (certificate or protocol)"))?;
            send_request(&mut stream, &bytes).await?;
            Box::pin(stream)
        } else {
            send_request(&mut tcp, &bytes).await?;
            Box::pin(tcp)
        };
        Ok::<Stream, OpenError>(stream)
    };
    let mut stream = timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| Cow::Borrowed("connection deadline exceeded"))??;
    let (head, buffer) = read_head(&mut stream, idle).await?;
    let body = if head.chunked() {
        Body::Chunked {
            remaining: None,
            done: false,
        }
    } else if let Some(length) = head.content_length() {
        Body::Length(length)
    } else if matches!(head.status, 204 | 304) {
        Body::Length(0)
    } else {
        Body::UntilClose
    };
    Ok(Connection {
        stream,
        buffer,
        body,
        head,
        url: url.clone(),
        address,
        idle,
    })
}

/// Status line and headers, plus any body bytes read past the terminator.
async fn read_head<S>(stream: &mut S, idle: Duration) -> Result<(ResponseHead, Vec<u8>), OpenError>
where
    S: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let end = loop {
        if let Some(index) = find(&bytes, b"\r\n\r\n") {
            break index;
        }
        if bytes.len() > MAX_HEAD_BYTES {
            return Err("response headers exceed 16 KiB".into());
        }
        let n = timeout(idle, stream.read(&mut chunk))
            .await
            .map_err(|_| Cow::Borrowed("no response headers within the idle timeout"))?
            .map_err(|_| Cow::Borrowed("response read failed"))?;
        if n == 0 {
            return Err("the server closed before sending headers".into());
        }
        bytes.extend_from_slice(&chunk[..n]);
    };
    let head = parse_head(&bytes[..end])?;
    let rest = bytes[end + 4..].to_vec();
    Ok((head, rest))
}

/// Parse `HTTP/1.x <code> ...` and `Name: value` lines.
pub fn parse_head(bytes: &[u8]) -> Result<ResponseHead, OpenError> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| Cow::Borrowed("response headers are not ASCII"))?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().ok_or("response has no status line")?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err("response is not HTTP/1.x".into());
    }
    let status: u16 = parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or("status code is invalid")?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err("malformed header line".into());
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
        if headers.len() > 128 {
            return Err("too many response headers".into());
        }
    }
    Ok(ResponseHead { status, headers })
}

/// The exact request bytes: GET, Host, identity encoding, optional Range
/// and If-Range, `Connection: close`.
pub fn build_request(url: &ParsedUrl, request: &Request) -> Vec<u8> {
    let host = if url.host.contains(':') {
        format!("[{}]", url.host)
    } else {
        url.host.clone()
    };
    let default_port = if url.https { 443 } else { 80 };
    let host_header = if url.port == default_port {
        host
    } else {
        format!("{host}:{}", url.port)
    };
    let mut text = format!(
        "GET {} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: TemplarTorrent/0.1\r\nAccept: */*\r\nAccept-Encoding: identity\r\n",
        url.target
    );
    if let Some((start, end)) = request.range {
        match end {
            Some(end) => text.push_str(&format!("Range: bytes={start}-{end}\r\n")),
            None => text.push_str(&format!("Range: bytes={start}-\r\n")),
        }
    }
    if let Some(validator) = &request.if_range {
        text.push_str(&format!("If-Range: {validator}\r\n"));
    }
    text.push_str("Connection: close\r\n\r\n");
    text.into_bytes()
}

pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(value) = u8::from_str_radix(&text[index + 1..index + 3], 16)
        {
            out.push(value);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn send_request<S>(stream: &mut S, bytes: &[u8]) -> Result<(), OpenError>
where
    S: AsyncWrite + Unpin,
{
    stream
        .write_all(bytes)
        .await
        .map_err(|_| Cow::Borrowed("request write failed"))
}
