//! Local Service Discovery ([BEP-14]).
//!
//! LSD allows finding peers on the local network via UDP multicast,
//! without requiring trackers or DHT.
//!
//! # Overview
//!
//! LSD works by broadcasting announce messages to a multicast group.
//! Other clients on the same network receive these messages and can
//! connect directly.
//!
//! # Protocol
//!
//! LSD uses UDP multicast on port 6771:
//! - IPv4: `239.192.152.143:6771`
//! - IPv6: `[ff15::efc0:988f]:6771`
//!
//! # Examples
//!
//! ```no_run
//! use rbit::lsd::LsdService;
//! use std::sync::Arc;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create LSD service on port 6881
//! let lsd = Arc::new(LsdService::new(6881).await?);
//!
//! // Subscribe to receive announcements
//! let mut rx = lsd.subscribe();
//!
//! // Start announcing our torrents
//! let info_hashes = vec![[0u8; 20]];
//! lsd.clone().start(info_hashes);
//!
//! // Listen for announcements
//! while let Ok(announce) = rx.recv().await {
//!     println!("Found local peer at {}:{}", announce.source.ip(), announce.port);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! [BEP-14]: http://bittorrent.org/beps/bep_0014.html

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::Arc;

use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tokio::time::{interval, Duration};

const LSD_PORT: u16 = 6771;
const LSD_MULTICAST_V4: Ipv4Addr = Ipv4Addr::new(239, 192, 152, 143);
const LSD_MULTICAST_V6: Ipv6Addr = Ipv6Addr::new(0xff15, 0, 0, 0, 0, 0, 0, 0x0050);
const LSD_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const LSD_COOKIE_SIZE: usize = 8;
const LSD_CHANNEL_CAPACITY: usize = 64;

/// Errors that can occur during LSD operations.
#[derive(Debug, Error)]
pub enum LsdError {
    /// Network I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// LSD protocol error.
    #[error("lsd error: {0}")]
    Lsd(String),

    /// Invalid announce message format.
    #[error("invalid response: {0}")]
    InvalidResponse(String),
}

/// An LSD announce message from a local peer.
#[derive(Debug, Clone)]
pub struct LsdAnnounce {
    /// The info hash being announced.
    pub info_hash: [u8; 20],
    /// The port the peer is listening on.
    pub port: u16,
    /// The source address of the announcement.
    pub source: SocketAddr,
}

/// Local Service Discovery service.
///
/// Handles sending and receiving LSD multicast messages.
pub struct LsdService {
    socket_v4: Option<Arc<UdpSocket>>,
    socket_v6: Option<Arc<UdpSocket>>,
    port: u16,
    cookie: String,
    announce_tx: broadcast::Sender<LsdAnnounce>,
}

impl LsdService {
    pub async fn new(port: u16) -> Result<Self, LsdError> {
        let mut cookie_bytes = [0u8; LSD_COOKIE_SIZE];
        rand::Rng::fill(&mut rand::rng(), &mut cookie_bytes);
        let cookie = hex_encode(&cookie_bytes);

        let socket_v4 = Self::bind_v4().await.ok();
        let socket_v6 = Self::bind_v6().await.ok();

        if socket_v4.is_none() && socket_v6.is_none() {
            return Err(LsdError::Lsd("failed to bind any socket".into()));
        }

        let (announce_tx, _) = broadcast::channel(LSD_CHANNEL_CAPACITY);

        Ok(Self {
            socket_v4,
            socket_v6,
            port,
            cookie,
            announce_tx,
        })
    }

    async fn bind_v4() -> Result<Arc<UdpSocket>, LsdError> {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, LSD_PORT)).await?;
        socket.set_multicast_loop_v4(false)?;
        socket.join_multicast_v4(LSD_MULTICAST_V4, Ipv4Addr::UNSPECIFIED)?;
        Ok(Arc::new(socket))
    }

    async fn bind_v6() -> Result<Arc<UdpSocket>, LsdError> {
        let socket =
            UdpSocket::bind(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, LSD_PORT, 0, 0)).await?;
        socket.set_multicast_loop_v6(false)?;
        socket.join_multicast_v6(&LSD_MULTICAST_V6, 0)?;
        Ok(Arc::new(socket))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LsdAnnounce> {
        self.announce_tx.subscribe()
    }

    pub fn start(self: Arc<Self>, info_hashes: Vec<[u8; 20]>) {
        let service = self.clone();
        tokio::spawn(async move {
            service.run(info_hashes).await;
        });
    }

    async fn run(&self, info_hashes: Vec<[u8; 20]>) {
        let mut announce_interval = interval(LSD_ANNOUNCE_INTERVAL);

        loop {
            tokio::select! {
                _ = announce_interval.tick() => {
                    for hash in &info_hashes {
                        let _ = self.announce(hash).await;
                    }
                }
                result = self.receive() => {
                    if let Ok(announce) = result {
                        let _ = self.announce_tx.send(announce);
                    }
                }
            }
        }
    }

    pub async fn announce(&self, info_hash: &[u8; 20]) -> Result<(), LsdError> {
        let message = self.format_announce(info_hash);

        if let Some(ref socket) = self.socket_v4 {
            let dest = SocketAddrV4::new(LSD_MULTICAST_V4, LSD_PORT);
            let _ = socket.send_to(message.as_bytes(), dest).await;
        }

        if let Some(ref socket) = self.socket_v6 {
            let dest = SocketAddrV6::new(LSD_MULTICAST_V6, LSD_PORT, 0, 0);
            let _ = socket.send_to(message.as_bytes(), dest).await;
        }

        Ok(())
    }

    fn format_announce(&self, info_hash: &[u8; 20]) -> String {
        let hash_hex = hex_encode(info_hash);
        format!(
            "BT-SEARCH * HTTP/1.1\r\n\
             Host: {}:{}\r\n\
             Port: {}\r\n\
             Infohash: {}\r\n\
             cookie: {}\r\n\
             \r\n",
            LSD_MULTICAST_V4, LSD_PORT, self.port, hash_hex, self.cookie
        )
    }

    async fn receive(&self) -> Result<LsdAnnounce, LsdError> {
        let mut buf_v4 = vec![0u8; 1024];
        let mut buf_v6 = vec![0u8; 1024];

        match (&self.socket_v4, &self.socket_v6) {
            (Some(v4), Some(v6)) => {
                tokio::select! {
                    result = v4.recv_from(&mut buf_v4) => {
                        let (n, source) = result?;
                        self.parse_announce(&buf_v4[..n], source)
                    }
                    result = v6.recv_from(&mut buf_v6) => {
                        let (n, source) = result?;
                        self.parse_announce(&buf_v6[..n], source)
                    }
                }
            }
            (Some(v4), None) => {
                let (n, source) = v4.recv_from(&mut buf_v4).await?;
                self.parse_announce(&buf_v4[..n], source)
            }
            (None, Some(v6)) => {
                let (n, source) = v6.recv_from(&mut buf_v6).await?;
                self.parse_announce(&buf_v6[..n], source)
            }
            (None, None) => Err(LsdError::Lsd("no socket available".into())),
        }
    }

    fn parse_announce(&self, data: &[u8], source: SocketAddr) -> Result<LsdAnnounce, LsdError> {
        let text = std::str::from_utf8(data)
            .map_err(|_| LsdError::InvalidResponse("invalid utf8".into()))?;

        if !text.starts_with("BT-SEARCH") {
            return Err(LsdError::InvalidResponse("not a BT-SEARCH message".into()));
        }

        let mut port = None;
        let mut info_hash = None;
        let mut cookie = None;

        for line in text.lines() {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("Port:") {
                port = value.trim().parse().ok();
            } else if let Some(value) = line.strip_prefix("Infohash:") {
                let hash_hex = value.trim();
                if hash_hex.len() == 40 {
                    if let Some(bytes) = hex_decode(hash_hex) {
                        if bytes.len() == 20 {
                            let mut hash = [0u8; 20];
                            hash.copy_from_slice(&bytes);
                            info_hash = Some(hash);
                        }
                    }
                }
            } else if let Some(value) = line.strip_prefix("cookie:") {
                cookie = Some(value.trim().to_string());
            }
        }

        if cookie.as_deref() == Some(&self.cookie) {
            return Err(LsdError::InvalidResponse("own announce".into()));
        }

        let port = port.ok_or_else(|| LsdError::InvalidResponse("missing port".into()))?;
        let info_hash =
            info_hash.ok_or_else(|| LsdError::InvalidResponse("missing info hash".into()))?;

        Ok(LsdAnnounce {
            info_hash,
            port,
            source,
        })
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{:02x}", b);
            s
        })
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }

    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
