use std::net::SocketAddr;
use std::time::Duration;

use rand::Rng as _;
use tokio::net::UdpSocket;
use tokio::time::timeout;

use super::error::TrackerError;
use super::response::{parse_compact_peers, AnnounceResponse, TrackerEvent};

const PROTOCOL_ID: u64 = 0x41727101980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const UDP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RETRIES: u32 = 3;

pub struct UdpTracker {
    socket: UdpSocket,
    addr: SocketAddr,
    connection_id: Option<u64>,
}

impl UdpTracker {
    pub async fn connect(url: &str) -> Result<Self, TrackerError> {
        let addr = parse_udp_url(url)?;

        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(addr).await?;

        let mut tracker = Self {
            socket,
            addr,
            connection_id: None,
        };

        tracker.do_connect().await?;

        Ok(tracker)
    }

    async fn do_connect(&mut self) -> Result<(), TrackerError> {
        let transaction_id: u32 = rand::rng().random();

        let mut request = Vec::with_capacity(16);
        request.extend_from_slice(&PROTOCOL_ID.to_be_bytes());
        request.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
        request.extend_from_slice(&transaction_id.to_be_bytes());

        let response = self.send_and_receive(&request, 16).await?;

        let action = u32::from_be_bytes([response[0], response[1], response[2], response[3]]);
        let resp_tid = u32::from_be_bytes([response[4], response[5], response[6], response[7]]);

        if action != ACTION_CONNECT || resp_tid != transaction_id {
            return Err(TrackerError::InvalidResponse(
                "connect response mismatch".into(),
            ));
        }

        self.connection_id = Some(u64::from_be_bytes([
            response[8],
            response[9],
            response[10],
            response[11],
            response[12],
            response[13],
            response[14],
            response[15],
        ]));

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn announce(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
        event: TrackerEvent,
        port: u16,
    ) -> Result<AnnounceResponse, TrackerError> {
        let connection_id = self
            .connection_id
            .ok_or_else(|| TrackerError::InvalidResponse("not connected".into()))?;

        let transaction_id: u32 = rand::rng().random();
        let key: u32 = rand::rng().random();

        let mut request = Vec::with_capacity(98);
        request.extend_from_slice(&connection_id.to_be_bytes());
        request.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
        request.extend_from_slice(&transaction_id.to_be_bytes());
        request.extend_from_slice(info_hash);
        request.extend_from_slice(peer_id);
        request.extend_from_slice(&downloaded.to_be_bytes());
        request.extend_from_slice(&left.to_be_bytes());
        request.extend_from_slice(&uploaded.to_be_bytes());
        request.extend_from_slice(&event.as_udp_id().to_be_bytes());
        request.extend_from_slice(&0u32.to_be_bytes()); // IP address (0 = default)
        request.extend_from_slice(&key.to_be_bytes());
        request.extend_from_slice(&(-1i32).to_be_bytes()); // num_want (-1 = default)
        request.extend_from_slice(&port.to_be_bytes());

        let response = self.send_and_receive(&request, 20).await?;

        let action = u32::from_be_bytes([response[0], response[1], response[2], response[3]]);
        let resp_tid = u32::from_be_bytes([response[4], response[5], response[6], response[7]]);

        if action == 3 {
            let message = String::from_utf8_lossy(&response[8..]).to_string();
            return Err(TrackerError::TrackerError(message));
        }

        if action != ACTION_ANNOUNCE || resp_tid != transaction_id {
            return Err(TrackerError::InvalidResponse(
                "announce response mismatch".into(),
            ));
        }

        let interval = u32::from_be_bytes([response[8], response[9], response[10], response[11]]);
        let leechers = u32::from_be_bytes([response[12], response[13], response[14], response[15]]);
        let seeders = u32::from_be_bytes([response[16], response[17], response[18], response[19]]);

        let peers = if response.len() > 20 {
            parse_compact_peers(&response[20..])
        } else {
            Vec::new()
        };

        let mut result = AnnounceResponse::new(interval);
        result.complete = Some(seeders);
        result.incomplete = Some(leechers);
        result.peers = peers;

        Ok(result)
    }

    async fn send_and_receive(
        &self,
        request: &[u8],
        min_response_size: usize,
    ) -> Result<Vec<u8>, TrackerError> {
        let mut buf = vec![0u8; 2048];

        for attempt in 0..MAX_RETRIES {
            self.socket.send(request).await?;

            let timeout_duration = UDP_TIMEOUT * (1 << attempt);

            match timeout(timeout_duration, self.socket.recv(&mut buf)).await {
                Ok(Ok(n)) if n >= min_response_size => {
                    return Ok(buf[..n].to_vec());
                }
                Ok(Ok(_)) => {
                    return Err(TrackerError::InvalidResponse("response too short".into()));
                }
                Ok(Err(e)) => return Err(TrackerError::Io(e)),
                Err(_) => continue,
            }
        }

        Err(TrackerError::Timeout)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

fn parse_udp_url(url: &str) -> Result<SocketAddr, TrackerError> {
    let url = url
        .strip_prefix("udp://")
        .ok_or_else(|| TrackerError::InvalidUrl(url.to_string()))?;

    let url = url.split('/').next().unwrap_or(url);

    url.parse()
        .map_err(|_| TrackerError::InvalidUrl(url.to_string()))
}
