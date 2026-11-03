//! A loopback HTTP/1.1 file server for tests: one body, optional ranges,
//! chunked or length-delimited, a validator that can change under the
//! client, redirect chains, a stall after N bytes, and TLS when a server
//! configuration is supplied. It serves only what a test puts in it and
//! binds only on 127.0.0.1.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

/// How the fixture answers. Tests mutate it through the shared mutex.
#[derive(Debug, Clone)]
pub struct Behaviour {
    pub body: Vec<u8>,
    /// Answer `Range` requests with 206 and send `Accept-Ranges: bytes`.
    pub ranges: bool,
    /// Send the body chunked with no length.
    pub chunked: bool,
    /// Send `Content-Length` on a 200 (ignored when chunked).
    pub content_length: bool,
    pub etag: Option<String>,
    /// `(Last-Modified, Date)` values.
    pub dates: Option<(String, String)>,
    pub content_disposition: Option<String>,
    /// Stop sending after this many body bytes and hang on the connection
    /// with this request index (0-based, across all requests).
    pub stall: Option<(usize, usize)>,
    /// Answer every request with this status and no body.
    pub status: Option<u16>,
    /// Additional headers on every response.
    pub extra_headers: Vec<(String, String)>,
    /// `Content-Encoding` to claim.
    pub content_encoding: Option<String>,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            body: Vec::new(),
            ranges: true,
            chunked: false,
            content_length: true,
            etag: Some("\"fixture-v1\"".into()),
            dates: None,
            content_disposition: None,
            stall: None,
            status: None,
            extra_headers: Vec::new(),
            content_encoding: None,
        }
    }
}

pub struct Server {
    pub address: SocketAddr,
    /// Every request head, lower-cased, in arrival order.
    pub requests: Arc<Mutex<Vec<String>>>,
    pub behaviour: Arc<Mutex<Behaviour>>,
    task: JoinHandle<()>,
}

impl Server {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    pub fn https_url(&self, host: &str, path: &str) -> String {
        format!("https://{host}:{}{path}", self.address.port())
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// Requests whose head contains `needle`.
    pub fn requests_with(&self, needle: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|head| head.contains(&needle.to_ascii_lowercase()))
            .count()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Plain HTTP on 127.0.0.1.
pub async fn start(behaviour: Behaviour) -> Server {
    start_with(behaviour, None).await
}

/// TLS with `config` on 127.0.0.1.
pub async fn start_tls(behaviour: Behaviour, config: Arc<rustls::ServerConfig>) -> Server {
    start_with(behaviour, Some(config)).await
}

async fn start_with(behaviour: Behaviour, tls: Option<Arc<rustls::ServerConfig>>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback bind");
    let address = listener.local_addr().expect("local address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let behaviour = Arc::new(Mutex::new(behaviour));
    let observed = Arc::clone(&requests);
    let shared = Arc::clone(&behaviour);
    let task = tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                break;
            };
            let observed = Arc::clone(&observed);
            let shared = Arc::clone(&shared);
            let tls = tls.clone();
            tokio::spawn(async move {
                match tls {
                    Some(config) => {
                        let acceptor = tokio_rustls::TlsAcceptor::from(config);
                        if let Ok(mut stream) = acceptor.accept(tcp).await {
                            serve(&mut stream, observed, shared).await;
                            let _ = stream.shutdown().await;
                        }
                    }
                    None => {
                        let mut stream = tcp;
                        serve(&mut stream, observed, shared).await;
                        let _ = stream.shutdown().await;
                    }
                }
            });
        }
    });
    Server {
        address,
        requests,
        behaviour,
        task,
    }
}

async fn serve<S>(stream: &mut S, observed: Arc<Mutex<Vec<String>>>, shared: Arc<Mutex<Behaviour>>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
        if head.len() > 16 * 1024 {
            return;
        }
    }
    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let index = {
        let mut requests = observed.lock().unwrap();
        requests.push(text.clone());
        requests.len() - 1
    };
    let behaviour = shared.lock().unwrap().clone();
    let path = text
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .unwrap_or("/")
        .to_string();
    let mut extra = behaviour.extra_headers.clone();
    if let Some(etag) = &behaviour.etag {
        extra.push(("ETag".into(), etag.clone()));
    }
    if let Some((modified, date)) = &behaviour.dates {
        extra.push(("Last-Modified".into(), modified.clone()));
        extra.push(("Date".into(), date.clone()));
    }
    if let Some(disposition) = &behaviour.content_disposition {
        extra.push(("Content-Disposition".into(), disposition.clone()));
    }
    if let Some(encoding) = &behaviour.content_encoding {
        extra.push(("Content-Encoding".into(), encoding.clone()));
    }
    if behaviour.ranges {
        extra.push(("Accept-Ranges".into(), "bytes".into()));
    }
    // Redirect chains: /redirect/N goes to /redirect/N-1, then /file.
    if let Some(rest) = path.strip_prefix("/redirect/") {
        let hops: usize = rest.split('?').next().unwrap_or("0").parse().unwrap_or(0);
        let location = if hops <= 1 {
            "/file".to_string()
        } else {
            format!("/redirect/{}", hops - 1)
        };
        let _ = stream
            .write_all(
                format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await;
        return;
    }
    if let Some(target) = path.strip_prefix("/redirect-to/") {
        let target = target.replace("%2f", "/");
        let _ = stream
            .write_all(
                format!("HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await;
        return;
    }
    if let Some(status) = behaviour.status {
        let _ = stream
            .write_all(
                format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await;
        return;
    }
    let total = behaviour.body.len();
    let range = text
        .lines()
        .find_map(|line| line.strip_prefix("range: bytes="))
        .and_then(|value| {
            let (first, last) = value.trim().split_once('-')?;
            let first: usize = first.parse().ok()?;
            let last: usize = if last.is_empty() {
                total.checked_sub(1)?
            } else {
                last.parse().ok()?
            };
            (first <= last && last < total).then_some((first, last))
        });
    let (status, first, last) = match range {
        Some((first, last)) if behaviour.ranges => (206, first, last),
        _ => (200, 0, total.saturating_sub(1)),
    };
    let slice: &[u8] = if total == 0 {
        &[]
    } else {
        &behaviour.body[first..=last]
    };
    let mut response = format!("HTTP/1.1 {status} Fixture\r\nConnection: close\r\n");
    for (name, value) in &extra {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    if status == 206 {
        response.push_str(&format!("Content-Range: bytes {first}-{last}/{total}\r\n"));
    }
    let chunked = behaviour.chunked && status == 200;
    if chunked {
        response.push_str("Transfer-Encoding: chunked\r\n");
    } else if behaviour.content_length || status == 206 {
        response.push_str(&format!("Content-Length: {}\r\n", slice.len()));
    }
    response.push_str("\r\n");
    if stream.write_all(response.as_bytes()).await.is_err() {
        return;
    }
    let stall_at = behaviour
        .stall
        .filter(|(_, request)| *request == index)
        .map(|(bytes, _)| bytes);
    let mut sent = 0usize;
    for piece in slice.chunks(1000) {
        let piece = match stall_at {
            Some(limit) if sent + piece.len() > limit => &piece[..limit.saturating_sub(sent)],
            _ => piece,
        };
        if !piece.is_empty() {
            let framed = if chunked {
                let mut framed = format!("{:x}\r\n", piece.len()).into_bytes();
                framed.extend_from_slice(piece);
                framed.extend_from_slice(b"\r\n");
                framed
            } else {
                piece.to_vec()
            };
            if stream.write_all(&framed).await.is_err() {
                return;
            }
            sent += piece.len();
        }
        if stall_at.is_some_and(|limit| sent >= limit) {
            // Hold the connection open and silent until the client gives up.
            std::future::pending::<()>().await;
        }
    }
    if chunked {
        let _ = stream.write_all(b"0\r\n\r\n").await;
    }
    let _ = stream.flush().await;
}
