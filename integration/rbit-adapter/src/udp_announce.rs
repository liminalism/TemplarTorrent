//! Application-owned UDP tracker announce (BEP 15, with BEP 41 URL data).
//!
//! rbit's own UDP tracker client is not used: this one is bounded the same
//! way as the HTTP transport in [`crate::announce`]. Each announce opens one
//! socket connected to an address the discovery module already admitted, so
//! the kernel drops datagrams from any other source. Replies are read into a
//! [`MAX_REPLY_BYTES`] buffer, the transaction id is random and checked on
//! every reply, and an error reply (action 3) surfaces its sanitized message.
//! Requests are retransmitted on BEP 15's doubling schedule, scaled down so
//! that a whole announce (connect plus announce) stays inside
//! [`ANNOUNCE_BUDGET`] instead of the spec's hour. Connection ids are cached
//! per tracker address for [`CONNECTION_TTL`] in a table of at most
//! [`MAX_CACHED_CONNECTIONS`] entries.

use std::{
    borrow::Cow,
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use rbit::{AnnounceResponse, CompactPeer, TrackerEvent};
use tokio::net::UdpSocket;

use crate::{
    announce::{AnnounceRequest, MAX_PEERS},
    discovery::{ParsedTracker, TrackerScheme},
};

/// BEP 15 magic constant sent in every connect request.
pub const PROTOCOL_ID: u64 = 0x0417_2710_1980;
pub const ACTION_CONNECT: u32 = 0;
pub const ACTION_ANNOUNCE: u32 = 1;
pub const ACTION_ERROR: u32 = 3;
/// Whole-announce budget: socket setup, connect exchange and announce
/// exchange, retransmissions included.
pub const ANNOUNCE_BUDGET: Duration = Duration::from_secs(15);
/// First retransmission timeout; it doubles after every retransmission
/// (BEP 15 uses 15 s · 2^n). 2 s, 4 s and 8 s fit inside the budget.
pub const FIRST_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a connection id is reused (BEP 15: one minute).
pub const CONNECTION_TTL: Duration = Duration::from_secs(60);
pub const MAX_CACHED_CONNECTIONS: usize = 64;
/// Largest datagram read: an announce header plus [`MAX_PEERS`] IPv6 peers.
/// The kernel discards the rest of a longer datagram.
pub const MAX_REPLY_BYTES: usize = 20 + 18 * MAX_PEERS;
/// Longest tracker error message kept, in characters.
pub const MAX_ERROR_CHARS: usize = 160;
/// Longest BEP 41 URL data (path and query) sent.
const MAX_URL_DATA: usize = 1024;
const NUM_WANT: i32 = 50;

/// Retransmission policy for one announce.
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    pub first_timeout: Duration,
    pub budget: Duration,
}

impl Schedule {
    pub const DEFAULT: Schedule = Schedule {
        first_timeout: FIRST_TIMEOUT,
        budget: ANNOUNCE_BUDGET,
    };
}

/// Connection ids by tracker address, each valid for the table's TTL.
pub struct ConnectionCache {
    ttl: Duration,
    entries: Mutex<Vec<(SocketAddr, u64, Instant)>>,
}

impl ConnectionCache {
    pub const fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(Vec::new()),
        }
    }

    /// The id received from `address` less than one TTL before `now`.
    pub fn get(&self, address: SocketAddr, now: Instant) -> Option<u64> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries
            .iter()
            .find(|(known, _, at)| {
                *known == address && now.saturating_duration_since(*at) < self.ttl
            })
            .map(|(_, id, _)| *id)
    }

    /// Remember `id` for `address` as received at `now`. Expired entries are
    /// dropped first, then the oldest one if the table is still full.
    pub fn put(&self, address: SocketAddr, id: u64, now: Instant) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|(known, _, at)| {
            *known != address && now.saturating_duration_since(*at) < self.ttl
        });
        if entries.len() >= MAX_CACHED_CONNECTIONS
            && let Some(oldest) = entries
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, _, at))| *at)
                .map(|(index, _)| index)
        {
            entries.swap_remove(oldest);
        }
        entries.push((address, id, now));
    }

    pub fn forget(&self, address: SocketAddr) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(known, _, _)| *known != address);
    }

    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

static CONNECTIONS: ConnectionCache = ConnectionCache::new(CONNECTION_TTL);

/// One decoded tracker datagram.
#[derive(Debug)]
pub enum Reply {
    Connect {
        transaction_id: u32,
        connection_id: u64,
    },
    Announce {
        transaction_id: u32,
        response: AnnounceResponse,
    },
    Error {
        transaction_id: u32,
        message: String,
    },
}

impl Reply {
    pub fn action(&self) -> u32 {
        match self {
            Reply::Connect { .. } => ACTION_CONNECT,
            Reply::Announce { .. } => ACTION_ANNOUNCE,
            Reply::Error { .. } => ACTION_ERROR,
        }
    }

    pub fn transaction_id(&self) -> u32 {
        match self {
            Reply::Connect { transaction_id, .. }
            | Reply::Announce { transaction_id, .. }
            | Reply::Error { transaction_id, .. } => *transaction_id,
        }
    }
}

/// Decode one reply datagram. `ipv6` selects the peer format: 18-byte peers
/// on an IPv6 socket, 6-byte peers on an IPv4 one (BEP 15). At most
/// [`MAX_REPLY_BYTES`] are read and at most [`MAX_PEERS`] peers kept.
pub fn parse_reply(bytes: &[u8], ipv6: bool) -> Result<Reply, &'static str> {
    let bytes = &bytes[..bytes.len().min(MAX_REPLY_BYTES)];
    let (Some(action), Some(transaction_id)) = (be_u32(bytes, 0), be_u32(bytes, 4)) else {
        return Err("tracker reply is truncated");
    };
    match action {
        ACTION_CONNECT => {
            let connection_id = be_u64(bytes, 8).ok_or("tracker reply is truncated")?;
            Ok(Reply::Connect {
                transaction_id,
                connection_id,
            })
        }
        ACTION_ANNOUNCE => {
            let (Some(interval), Some(leechers), Some(seeders)) =
                (be_u32(bytes, 8), be_u32(bytes, 12), be_u32(bytes, 16))
            else {
                return Err("tracker reply is truncated");
            };
            // BEP 15 fields are int32; a negative interval is not a schedule.
            let interval =
                u32::try_from(interval as i32).map_err(|_| "tracker interval is out of range")?;
            let count = |value: u32| u32::try_from(value as i32).ok();
            let mut response = AnnounceResponse::new(interval);
            response.incomplete = count(leechers);
            response.complete = count(seeders);
            let peers = &bytes[20..];
            if ipv6 {
                response.peers6 = peers
                    .as_chunks::<18>()
                    .0
                    .iter()
                    .filter_map(|chunk| CompactPeer::from_v6_bytes(chunk))
                    .map(|peer| peer.to_socket_addr())
                    .take(MAX_PEERS)
                    .collect();
            } else {
                response.peers = peers
                    .as_chunks::<6>()
                    .0
                    .iter()
                    .filter_map(|chunk| CompactPeer::from_v4_bytes(chunk))
                    .map(|peer| peer.to_socket_addr())
                    .take(MAX_PEERS)
                    .collect();
            }
            Ok(Reply::Announce {
                transaction_id,
                response,
            })
        }
        ACTION_ERROR => Ok(Reply::Error {
            transaction_id,
            message: sanitize_message(&bytes[8..]),
        }),
        _ => Err("tracker reply has an unknown action"),
    }
}

/// Tracker text for the Trackers tab: lossy UTF-8, control characters as
/// spaces, trimmed, at most [`MAX_ERROR_CHARS`] characters.
pub fn sanitize_message(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_ERROR_CHARS * 4)]);
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let kept: String = cleaned.trim().chars().take(MAX_ERROR_CHARS).collect();
    let kept = kept.trim_end();
    if kept.is_empty() {
        "no message".to_string()
    } else {
        kept.to_string()
    }
}

pub fn connect_request(transaction_id: u32) -> [u8; 16] {
    let mut packet = [0u8; 16];
    packet[..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    packet[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    packet[12..].copy_from_slice(&transaction_id.to_be_bytes());
    packet
}

/// The 98-byte BEP 15 announce, then the path and query as BEP 41 URL data
/// options (type 2, at most 255 bytes each).
pub fn announce_request(
    connection_id: u64,
    transaction_id: u32,
    key: u32,
    request: &AnnounceRequest<'_>,
    url_data: &[u8],
) -> Vec<u8> {
    let event: u32 = match request.event {
        TrackerEvent::None => 0,
        TrackerEvent::Completed => 1,
        TrackerEvent::Started => 2,
        TrackerEvent::Stopped => 3,
    };
    let mut packet = Vec::with_capacity(98 + url_data.len() + url_data.len() / 255 * 2 + 2);
    packet.extend_from_slice(&connection_id.to_be_bytes());
    packet.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    packet.extend_from_slice(&transaction_id.to_be_bytes());
    packet.extend_from_slice(request.info_hash);
    packet.extend_from_slice(request.peer_id);
    packet.extend_from_slice(&request.downloaded.to_be_bytes());
    packet.extend_from_slice(&request.left.to_be_bytes());
    packet.extend_from_slice(&request.uploaded.to_be_bytes());
    packet.extend_from_slice(&event.to_be_bytes());
    // IP address 0: the tracker uses the datagram's source address.
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&key.to_be_bytes());
    packet.extend_from_slice(&NUM_WANT.to_be_bytes());
    packet.extend_from_slice(&request.port.to_be_bytes());
    for chunk in url_data.chunks(255) {
        packet.push(2);
        packet.push(chunk.len() as u8);
        packet.extend_from_slice(chunk);
    }
    packet
}

/// Path and query of a `udp://` URL, the BEP 41 URL data.
pub fn url_data(tracker: &ParsedTracker) -> Result<&str, &'static str> {
    let rest = tracker
        .url
        .strip_prefix("udp://")
        .ok_or("tracker scheme must be udp")?;
    let data = rest.find(['/', '?']).map_or("", |index| &rest[index..]);
    if data.len() > MAX_URL_DATA {
        return Err("UDP tracker URL data exceeds 1 KiB");
    }
    Ok(data)
}

/// One announce to `address`, which must be an address the caller resolved
/// and classified for `tracker`. The peers come back in `peers` on an IPv4
/// address and in `peers6` on an IPv6 one.
pub async fn announce(
    tracker: &ParsedTracker,
    address: SocketAddr,
    request: &AnnounceRequest<'_>,
) -> Result<AnnounceResponse, Cow<'static, str>> {
    announce_with(tracker, address, request, &CONNECTIONS, Schedule::DEFAULT).await
}

/// [`announce`] with an explicit connection-id table and schedule.
pub async fn announce_with(
    tracker: &ParsedTracker,
    address: SocketAddr,
    request: &AnnounceRequest<'_>,
    cache: &ConnectionCache,
    schedule: Schedule,
) -> Result<AnnounceResponse, Cow<'static, str>> {
    if tracker.scheme != TrackerScheme::Udp {
        return Err("tracker scheme must be udp".into());
    }
    let url_data = url_data(tracker)?;
    let deadline = tokio::time::Instant::now() + schedule.budget;
    let ipv6 = address.is_ipv6();
    let bind = if ipv6 {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    };
    let socket = UdpSocket::bind(bind).await.map_err(io_reason)?;
    // A family without a route fails here at once rather than timing out.
    socket.connect(address).await.map_err(io_reason)?;
    let mut exchange = Exchange {
        socket: &socket,
        buffer: vec![0u8; MAX_REPLY_BYTES],
        deadline,
        first_timeout: schedule.first_timeout,
        ipv6,
    };
    let connection_id = match cache.get(address, Instant::now()) {
        Some(id) => id,
        None => {
            let transaction_id = random_u32();
            let reply = exchange
                .request(
                    &connect_request(transaction_id),
                    transaction_id,
                    ACTION_CONNECT,
                )
                .await?;
            let Reply::Connect { connection_id, .. } = reply else {
                return Err("tracker reply has an unexpected action".into());
            };
            cache.put(address, connection_id, Instant::now());
            connection_id
        }
    };
    let transaction_id = random_u32();
    let packet = announce_request(
        connection_id,
        transaction_id,
        key_for(request.peer_id),
        request,
        url_data.as_bytes(),
    );
    match exchange
        .request(&packet, transaction_id, ACTION_ANNOUNCE)
        .await
    {
        Ok(Reply::Announce { response, .. }) => Ok(response),
        Ok(_) => Err("tracker reply has an unexpected action".into()),
        Err(error) => {
            // The id may be what the tracker refused; reconnect next time.
            cache.forget(address);
            Err(error)
        }
    }
}

struct Exchange<'a> {
    socket: &'a UdpSocket,
    buffer: Vec<u8>,
    deadline: tokio::time::Instant,
    first_timeout: Duration,
    ipv6: bool,
}

impl Exchange<'_> {
    /// Send `packet` until a reply carrying `transaction_id` arrives or the
    /// deadline passes. Datagrams with another transaction id, or too short to
    /// carry one, are ignored. A matching error reply ends the exchange with
    /// its message; a matching reply with another action is a protocol error.
    async fn request(
        &mut self,
        packet: &[u8],
        transaction_id: u32,
        expected: u32,
    ) -> Result<Reply, Cow<'static, str>> {
        let mut wait = self.first_timeout;
        loop {
            let now = tokio::time::Instant::now();
            if now >= self.deadline {
                return Err("UDP tracker deadline exceeded".into());
            }
            self.socket.send(packet).await.map_err(io_reason)?;
            let attempt_end = (now + wait).min(self.deadline);
            loop {
                let received =
                    tokio::time::timeout_at(attempt_end, self.socket.recv(&mut self.buffer)).await;
                let n = match received {
                    Err(_) => break,
                    // Windows reports a datagram longer than the buffer as
                    // WSAEMSGSIZE after filling the buffer with its start;
                    // Linux truncates silently. Both keep the first
                    // MAX_REPLY_BYTES, which is all the parser reads.
                    Ok(Err(error)) if is_truncated_datagram(&error) => self.buffer.len(),
                    Ok(Err(error)) => return Err(io_reason(error)),
                    Ok(Ok(n)) => n,
                };
                let datagram = &self.buffer[..n];
                if be_u32(datagram, 4) != Some(transaction_id) {
                    continue;
                }
                return match parse_reply(datagram, self.ipv6) {
                    Ok(Reply::Error { message, .. }) => {
                        Err(Cow::Owned(format!("tracker error: {message}")))
                    }
                    Ok(reply) if reply.action() == expected => Ok(reply),
                    Ok(_) => Err("tracker reply has an unexpected action".into()),
                    Err(reason) => Err(reason.into()),
                };
            }
            wait = wait.saturating_mul(2);
        }
    }
}

/// WSAEMSGSIZE: the datagram was larger than the buffer (Windows only).
fn is_truncated_datagram(error: &std::io::Error) -> bool {
    cfg!(windows) && error.raw_os_error() == Some(10040)
}

fn io_reason(error: std::io::Error) -> Cow<'static, str> {
    use std::io::ErrorKind;
    Cow::Borrowed(match error.kind() {
        ErrorKind::NetworkUnreachable | ErrorKind::HostUnreachable => {
            "tracker address family is unreachable"
        }
        ErrorKind::ConnectionRefused => "UDP tracker port is closed",
        ErrorKind::AddrNotAvailable => "no local address for the tracker family",
        _ => "UDP tracker exchange failed",
    })
}

/// Unpredictable to anyone without this process's SipHash keys, which std
/// draws from the OS. Transaction ids only need to defeat off-path guessing.
fn random_u32() -> u32 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    hasher.write_u128(nanos);
    let hash = hasher.finish();
    (hash ^ (hash >> 32)) as u32
}

/// The BEP 15 `key`: stable for the session's peer id (itself random), so a
/// tracker can match this client across address changes.
fn key_for(peer_id: &[u8; 20]) -> u32 {
    peer_id.iter().fold(0x811c_9dc5u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let chunk = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes(chunk.try_into().ok()?))
}

fn be_u64(bytes: &[u8], at: usize) -> Option<u64> {
    let chunk = bytes.get(at..at.checked_add(8)?)?;
    Some(u64::from_be_bytes(chunk.try_into().ok()?))
}

/// Loopback BEP 15 tracker for tests. It never records or prints request
/// bytes beyond what a test asserts on.
#[cfg(test)]
pub(crate) mod fixture {
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    use tokio::{net::UdpSocket, task::JoinHandle};

    use super::*;

    pub const CONNECTION_ID: u64 = 0x1122_3344_5566_7788;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Fault {
        None,
        /// Drop the first connect request, answer the retransmission.
        DropFirstConnect,
        /// Answer every connect request with an error message.
        Error,
        /// Send a reply with the wrong transaction id before the real one.
        WrongTidFirst,
        /// Answer a connect request with an announce-shaped reply.
        WrongAction,
        /// Answer the announce with only 12 bytes.
        Truncated,
        /// Answer the announce with this many peers.
        Peers(usize),
        /// Send a forged reply from another port before the real one.
        Spoof,
        /// Never answer.
        Silent,
    }

    #[derive(Debug, Default)]
    pub struct Stats {
        pub connects: usize,
        pub announces: usize,
        pub last_announce: Vec<u8>,
    }

    pub struct Tracker {
        pub address: SocketAddr,
        pub stats: Arc<Mutex<Stats>>,
        task: JoinHandle<()>,
    }

    impl Drop for Tracker {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Tracker {
        pub fn connects(&self) -> usize {
            self.stats.lock().unwrap().connects
        }

        pub fn announces(&self) -> usize {
            self.stats.lock().unwrap().announces
        }
    }

    /// The peer this fixture hands out for its own family.
    pub fn peer_for(address: SocketAddr, index: usize) -> SocketAddr {
        let port = 10_000 + index as u16;
        if address.is_ipv4() {
            SocketAddr::from(([127, 0, 0, 9], port))
        } else {
            SocketAddr::from(([0xfd00, 0, 0, 0, 0, 0, 0, 9], port))
        }
    }

    /// `None` when `bind` is unavailable here (no `::1`, or a taken port).
    pub async fn start(bind: SocketAddr, fault: Fault) -> Option<Tracker> {
        let socket = UdpSocket::bind(bind).await.ok()?;
        let address = socket.local_addr().ok()?;
        let stats = Arc::new(Mutex::new(Stats::default()));
        let task = tokio::spawn(serve(socket, fault, Arc::clone(&stats)));
        Some(Tracker {
            address,
            stats,
            task,
        })
    }

    async fn serve(socket: UdpSocket, fault: Fault, stats: Arc<Mutex<Stats>>) {
        let address = socket.local_addr().unwrap();
        let spoofer = UdpSocket::bind(SocketAddr::new(address.ip(), 0))
            .await
            .unwrap();
        let mut buffer = vec![0u8; 4096];
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buffer).await else {
                return;
            };
            let packet = buffer[..n].to_vec();
            let Some(transaction_id) = be_u32(&packet, 12) else {
                continue;
            };
            if n == 16 && be_u64(&packet, 0) == Some(PROTOCOL_ID) {
                let first = {
                    let mut stats = stats.lock().unwrap();
                    stats.connects += 1;
                    stats.connects == 1
                };
                let mut reply = Vec::new();
                match fault {
                    Fault::Silent => continue,
                    Fault::DropFirstConnect if first => continue,
                    Fault::Error => {
                        reply.extend_from_slice(&ACTION_ERROR.to_be_bytes());
                        reply.extend_from_slice(&transaction_id.to_be_bytes());
                        reply.extend_from_slice(b"torrent not registered\n\x07");
                        let _ = socket.send_to(&reply, from).await;
                        continue;
                    }
                    Fault::WrongAction => {
                        reply.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
                        reply.extend_from_slice(&transaction_id.to_be_bytes());
                        reply.extend_from_slice(&[0u8; 12]);
                        let _ = socket.send_to(&reply, from).await;
                        continue;
                    }
                    _ => {}
                }
                if fault == Fault::WrongTidFirst {
                    let mut stray = Vec::new();
                    stray.extend_from_slice(&ACTION_ERROR.to_be_bytes());
                    stray.extend_from_slice(&(transaction_id ^ 1).to_be_bytes());
                    stray.extend_from_slice(b"stray");
                    let _ = socket.send_to(&stray, from).await;
                    let _ = socket.send_to(&[0, 0, 0], from).await;
                }
                reply.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
                reply.extend_from_slice(&transaction_id.to_be_bytes());
                reply.extend_from_slice(&CONNECTION_ID.to_be_bytes());
                let _ = socket.send_to(&reply, from).await;
            } else if n >= 98 && be_u32(&packet, 8) == Some(ACTION_ANNOUNCE) {
                {
                    let mut stats = stats.lock().unwrap();
                    stats.announces += 1;
                    stats.last_announce = packet.clone();
                }
                let mut reply = Vec::new();
                if be_u64(&packet, 0) != Some(CONNECTION_ID) {
                    reply.extend_from_slice(&ACTION_ERROR.to_be_bytes());
                    reply.extend_from_slice(&transaction_id.to_be_bytes());
                    reply.extend_from_slice(b"connection id mismatch");
                    let _ = socket.send_to(&reply, from).await;
                    continue;
                }
                reply.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
                reply.extend_from_slice(&transaction_id.to_be_bytes());
                reply.extend_from_slice(&1800u32.to_be_bytes());
                if fault == Fault::Truncated {
                    let _ = socket.send_to(&reply, from).await;
                    continue;
                }
                reply.extend_from_slice(&4u32.to_be_bytes());
                reply.extend_from_slice(&3u32.to_be_bytes());
                let count = match fault {
                    Fault::Peers(count) => count,
                    _ => 1,
                };
                for index in 0..count {
                    match peer_for(address, index) {
                        SocketAddr::V4(peer) => {
                            reply.extend_from_slice(&peer.ip().octets());
                            reply.extend_from_slice(&peer.port().to_be_bytes());
                        }
                        SocketAddr::V6(peer) => {
                            reply.extend_from_slice(&peer.ip().octets());
                            reply.extend_from_slice(&peer.port().to_be_bytes());
                        }
                    }
                }
                if fault == Fault::Spoof {
                    let mut forged = reply[..20].to_vec();
                    forged.extend_from_slice(&[1, 2, 3, 4, 0, 1]);
                    let _ = spoofer.send_to(&forged, from).await;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let _ = socket.send_to(&reply, from).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fixture::*, *};
    use crate::discovery::parse_tracker_url;

    const FAST: Schedule = Schedule {
        first_timeout: Duration::from_millis(200),
        budget: Duration::from_secs(3),
    };

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

    fn tracker_url(address: SocketAddr) -> ParsedTracker {
        parse_tracker_url(&format!("udp://{address}/announce?passkey=abc")).unwrap()
    }

    async fn run(
        tracker: &Tracker,
        cache: &ConnectionCache,
        schedule: Schedule,
    ) -> Result<AnnounceResponse, Cow<'static, str>> {
        let hash = [7u8; 20];
        let id = [8u8; 20];
        announce_with(
            &tracker_url(tracker.address),
            tracker.address,
            &request(&hash, &id),
            cache,
            schedule,
        )
        .await
    }

    fn loopbacks() -> [SocketAddr; 2] {
        ["127.0.0.1:0".parse().unwrap(), "[::1]:0".parse().unwrap()]
    }

    #[tokio::test]
    async fn announce_succeeds_over_both_families_and_reuses_the_connection_id() {
        for bind in loopbacks() {
            let Some(tracker) = start(bind, Fault::None).await else {
                eprintln!("skipping {bind}: loopback unavailable");
                continue;
            };
            let cache = ConnectionCache::new(CONNECTION_TTL);
            let response = run(&tracker, &cache, FAST).await.unwrap();
            assert_eq!(response.interval, 1800);
            assert_eq!((response.complete, response.incomplete), (Some(3), Some(4)));
            let expected = vec![peer_for(tracker.address, 0)];
            if bind.is_ipv4() {
                assert_eq!((response.peers, response.peers6.len()), (expected, 0));
            } else {
                assert_eq!((response.peers.len(), response.peers6), (0, expected));
            }
            let packet = tracker.stats.lock().unwrap().last_announce.clone();
            assert_eq!(&packet[16..36], &[7u8; 20]);
            assert_eq!(&packet[36..56], &[8u8; 20]);
            assert_eq!(be_u64(&packet, 56), Some(2), "downloaded");
            assert_eq!(be_u64(&packet, 64), Some(3), "left");
            assert_eq!(be_u64(&packet, 72), Some(1), "uploaded");
            assert_eq!(be_u32(&packet, 80), Some(2), "event started");
            assert_eq!(be_u32(&packet, 84), Some(0), "ip");
            assert_eq!(be_u32(&packet, 92), Some(NUM_WANT as u32));
            assert_eq!(&packet[96..98], &6881u16.to_be_bytes());
            let data = b"/announce?passkey=abc";
            assert_eq!(packet[98], 2, "BEP 41 URL data option");
            assert_eq!(packet[99] as usize, data.len());
            assert_eq!(&packet[100..], data);
            run(&tracker, &cache, FAST).await.unwrap();
            assert_eq!(
                (tracker.connects(), tracker.announces()),
                (1, 2),
                "a cached connection id skips the connect exchange"
            );
        }
    }

    #[tokio::test]
    async fn error_action_surfaces_the_sanitized_message_and_drops_the_id() {
        let tracker = start(loopbacks()[0], Fault::Error).await.unwrap();
        let cache = ConnectionCache::new(CONNECTION_TTL);
        let error = run(&tracker, &cache, FAST).await.unwrap_err();
        assert_eq!(error, "tracker error: torrent not registered");
        assert!(cache.is_empty());
    }

    #[tokio::test]
    async fn replies_with_another_transaction_id_or_source_are_ignored() {
        let cache = ConnectionCache::new(CONNECTION_TTL);
        let tracker = start(loopbacks()[0], Fault::WrongTidFirst).await.unwrap();
        let response = run(&tracker, &cache, FAST).await.unwrap();
        assert_eq!(response.peers, vec![peer_for(tracker.address, 0)]);
        assert_eq!(tracker.connects(), 1, "no retransmission was needed");

        let cache = ConnectionCache::new(CONNECTION_TTL);
        let tracker = start(loopbacks()[0], Fault::Spoof).await.unwrap();
        let response = run(&tracker, &cache, FAST).await.unwrap();
        assert_eq!(
            response.peers,
            vec![peer_for(tracker.address, 0)],
            "the connected socket drops the forged reply from another port"
        );
    }

    #[tokio::test]
    async fn wrong_action_and_truncated_replies_fail_without_waiting() {
        for (fault, reason) in [
            (Fault::WrongAction, "tracker reply has an unexpected action"),
            (Fault::Truncated, "tracker reply is truncated"),
        ] {
            let tracker = start(loopbacks()[0], fault).await.unwrap();
            let cache = ConnectionCache::new(CONNECTION_TTL);
            let started = Instant::now();
            let error = run(&tracker, &cache, FAST).await.unwrap_err();
            assert_eq!(error, reason);
            assert!(started.elapsed() < FAST.first_timeout, "{reason}");
        }
    }

    #[tokio::test]
    async fn a_lost_request_is_retransmitted_and_silence_ends_at_the_budget() {
        let tracker = start(loopbacks()[0], Fault::DropFirstConnect)
            .await
            .unwrap();
        let cache = ConnectionCache::new(CONNECTION_TTL);
        let started = Instant::now();
        let response = run(&tracker, &cache, FAST).await.unwrap();
        assert!(started.elapsed() >= FAST.first_timeout);
        assert_eq!(response.peers.len(), 1);
        assert_eq!((tracker.connects(), tracker.announces()), (2, 1));

        let tracker = start(loopbacks()[0], Fault::Silent).await.unwrap();
        let budget = Schedule {
            first_timeout: Duration::from_millis(100),
            budget: Duration::from_millis(800),
        };
        let started = Instant::now();
        let error = run(&tracker, &cache, budget).await.unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(error, "UDP tracker deadline exceeded");
        assert!(
            elapsed >= budget.budget && elapsed < budget.budget * 2,
            "{elapsed:?}"
        );
        // 100 ms, 200 ms, 400 ms, then the 100 ms left of the budget.
        assert_eq!(tracker.connects(), 4);
    }

    #[tokio::test]
    async fn an_expired_connection_id_is_replaced_by_a_new_connect() {
        let tracker = start(loopbacks()[0], Fault::None).await.unwrap();
        let cache = ConnectionCache::new(CONNECTION_TTL);
        let Some(old) = Instant::now().checked_sub(CONNECTION_TTL + Duration::from_secs(1)) else {
            return;
        };
        cache.put(tracker.address, 0xdead, old);
        run(&tracker, &cache, FAST).await.unwrap();
        assert_eq!((tracker.connects(), tracker.announces()), (1, 1));
        assert_eq!(
            cache.get(tracker.address, Instant::now()),
            Some(CONNECTION_ID)
        );
        // A still-valid id the tracker no longer accepts fails once, then the
        // next announce reconnects.
        cache.put(tracker.address, 0xdead, Instant::now());
        let error = run(&tracker, &cache, FAST).await.unwrap_err();
        assert_eq!(error, "tracker error: connection id mismatch");
        run(&tracker, &cache, FAST).await.unwrap();
        assert_eq!((tracker.connects(), tracker.announces()), (2, 3));
    }

    #[tokio::test]
    async fn peers_beyond_the_cap_are_dropped_on_both_families() {
        for bind in loopbacks() {
            let Some(tracker) = start(bind, Fault::Peers(MAX_PEERS + 20)).await else {
                eprintln!("skipping {bind}: loopback unavailable");
                continue;
            };
            let cache = ConnectionCache::new(CONNECTION_TTL);
            let response = run(&tracker, &cache, FAST).await.unwrap();
            let peers = response.peers.len() + response.peers6.len();
            assert_eq!(peers, MAX_PEERS, "{bind}");
        }
    }

    #[test]
    fn connection_cache_expires_and_stays_bounded() {
        let cache = ConnectionCache::new(Duration::from_secs(60));
        let start = Instant::now();
        let address: SocketAddr = "1.1.1.1:6969".parse().unwrap();
        cache.put(address, 5, start);
        assert_eq!(cache.get(address, start + Duration::from_secs(59)), Some(5));
        assert_eq!(cache.get(address, start + Duration::from_secs(60)), None);
        cache.put(address, 6, start);
        assert_eq!(cache.len(), 1, "a new id replaces the old one");
        for port in 0..(MAX_CACHED_CONNECTIONS as u16 + 10) {
            let other = SocketAddr::from(([1, 1, 1, 2], port + 1));
            cache.put(
                other,
                u64::from(port),
                start + Duration::from_millis(u64::from(port)),
            );
        }
        assert_eq!(cache.len(), MAX_CACHED_CONNECTIONS);
        assert_eq!(
            cache.get(address, start),
            None,
            "the oldest entry went first"
        );
        cache.put(address, 7, start + Duration::from_secs(120));
        assert_eq!(cache.len(), 1, "expired entries are dropped on insert");
        cache.forget(address);
        assert!(cache.is_empty());
    }

    #[test]
    fn reply_parser_checks_lengths_actions_and_ranges() {
        let mut connect = vec![0, 0, 0, 0, 0, 0, 0, 9];
        connect.extend_from_slice(&42u64.to_be_bytes());
        let Ok(Reply::Connect {
            transaction_id: 9,
            connection_id: 42,
        }) = parse_reply(&connect, false)
        else {
            panic!("connect reply");
        };
        assert_eq!(
            parse_reply(&connect[..15], false).unwrap_err(),
            "tracker reply is truncated"
        );
        assert_eq!(
            parse_reply(&[0, 0, 0, 1, 0, 0], false).unwrap_err(),
            "tracker reply is truncated"
        );
        assert_eq!(
            parse_reply(&[0, 0, 0, 2, 0, 0, 0, 0], false).unwrap_err(),
            "tracker reply has an unknown action"
        );
        let mut announce = vec![0, 0, 0, 1, 0, 0, 0, 9];
        announce.extend_from_slice(&(-1i32).to_be_bytes());
        announce.extend_from_slice(&[0; 8]);
        assert_eq!(
            parse_reply(&announce, false).unwrap_err(),
            "tracker interval is out of range"
        );
        announce[8..12].copy_from_slice(&60u32.to_be_bytes());
        announce[12..16].copy_from_slice(&(-5i32).to_be_bytes());
        // One whole IPv4 peer and five stray bytes.
        announce.extend_from_slice(&[10, 0, 0, 1, 0x1a, 0xe1, 1, 2, 3, 4, 5]);
        let Ok(Reply::Announce { response, .. }) = parse_reply(&announce, false) else {
            panic!("announce reply");
        };
        assert_eq!(response.interval, 60);
        assert_eq!((response.incomplete, response.complete), (None, Some(0)));
        assert_eq!(response.peers, vec!["10.0.0.1:6881".parse().unwrap()]);
        let Ok(Reply::Announce { response, .. }) = parse_reply(&announce, true) else {
            panic!("announce reply");
        };
        assert!(response.peers.is_empty() && response.peers6.is_empty());
        let mut error = vec![0, 0, 0, 3, 0, 0, 0, 9];
        error.extend_from_slice("\u{1b}[31mbad\tkey ".as_bytes());
        error.extend_from_slice(&[0xff; 400]);
        let Ok(Reply::Error { message, .. }) = parse_reply(&error, false) else {
            panic!("error reply");
        };
        assert!(message.starts_with("[31mbad key \u{fffd}"), "{message}");
        assert!(!message.chars().any(char::is_control));
        assert_eq!(message.chars().count(), MAX_ERROR_CHARS);
        assert_eq!(sanitize_message(b"\n\n"), "no message");
    }

    #[test]
    fn requests_carry_the_bep15_layout_and_chunked_url_data() {
        let connect = connect_request(0xabcd);
        assert_eq!(be_u64(&connect, 0), Some(PROTOCOL_ID));
        assert_eq!(be_u32(&connect, 8), Some(ACTION_CONNECT));
        assert_eq!(be_u32(&connect, 12), Some(0xabcd));
        let hash = [1u8; 20];
        let id = [2u8; 20];
        let mut request = request(&hash, &id);
        request.event = TrackerEvent::None;
        let data = vec![b'a'; 300];
        let packet = announce_request(5, 6, 7, &request, &data);
        assert_eq!(packet.len(), 98 + 2 + 255 + 2 + 45);
        assert_eq!(be_u64(&packet, 0), Some(5));
        assert_eq!(be_u32(&packet, 8), Some(ACTION_ANNOUNCE));
        assert_eq!(be_u32(&packet, 12), Some(6));
        assert_eq!(be_u32(&packet, 80), Some(0), "event none");
        assert_eq!(be_u32(&packet, 88), Some(7), "key");
        assert_eq!((packet[98], packet[99]), (2, 255));
        assert_eq!((packet[355], packet[356]), (2, 45));
        assert_eq!(announce_request(5, 6, 7, &request, b"").len(), 98);
        let tracker = parse_tracker_url("udp://tracker.example:6969").unwrap();
        assert_eq!(url_data(&tracker).unwrap(), "");
        let tracker = parse_tracker_url(&format!(
            "udp://tracker.example:6969/{}",
            "a".repeat(MAX_URL_DATA)
        ))
        .unwrap();
        assert!(url_data(&tracker).unwrap_err().contains("1 KiB"));
        let http = parse_tracker_url("http://tracker.example/announce").unwrap();
        assert!(url_data(&http).is_err());
        assert_ne!(random_u32(), random_u32());
    }
}
