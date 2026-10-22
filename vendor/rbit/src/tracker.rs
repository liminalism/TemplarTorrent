//! Tracker protocols ([BEP-3], [BEP-15], [BEP-23]).
//!
//! Trackers are servers that help peers find each other. This module implements
//! both HTTP and UDP tracker protocols.
//!
//! # Overview
//!
//! When downloading a torrent, clients "announce" to trackers to:
//! 1. Register themselves in the swarm
//! 2. Get a list of other peers to connect to
//! 3. Report download/upload statistics
//!
//! # HTTP Trackers
//!
//! HTTP trackers use simple GET requests with query parameters. The response
//! is a bencoded dictionary containing peer information.
//!
//! ```no_run
//! use rbit::tracker::{HttpTracker, TrackerEvent};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let tracker = HttpTracker::new("http://tracker.example.com/announce")?;
//!
//! let response = tracker.announce(
//!     &[0u8; 20],           // info_hash
//!     &[0u8; 20],           // peer_id
//!     6881,                 // port
//!     0,                    // uploaded
//!     0,                    // downloaded
//!     1000000,              // left (bytes remaining)
//!     TrackerEvent::Started,
//! ).await?;
//!
//! println!("Interval: {} seconds", response.interval);
//! println!("Seeders: {:?}", response.complete);
//! println!("Leechers: {:?}", response.incomplete);
//!
//! for peer in response.peers {
//!     println!("Peer: {}", peer);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # UDP Trackers
//!
//! UDP trackers ([BEP-15]) are more efficient than HTTP, using a connection-based
//! protocol with binary messages.
//!
//! ```no_run
//! use rbit::tracker::{UdpTracker, TrackerEvent};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut tracker = UdpTracker::connect("udp://tracker.example.com:6969").await?;
//!
//! let response = tracker.announce(
//!     &[0u8; 20],           // info_hash
//!     &[0u8; 20],           // peer_id
//!     0,                    // downloaded
//!     1000000,              // left
//!     0,                    // uploaded
//!     TrackerEvent::Started,
//!     6881,                 // port
//! ).await?;
//!
//! for peer in response.peers {
//!     println!("Peer: {}", peer);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Tracker Events
//!
//! Clients send different events during the torrent lifecycle:
//!
//! - **Started** - First announce when beginning the download
//! - **Completed** - Sent when download finishes (becomes a seeder)
//! - **Stopped** - Sent when removing the torrent
//! - **None** - Regular periodic announcement
//!
//! # Compact Peer Format
//!
//! [BEP-23] defines a compact format for peer lists that's more efficient than
//! the dictionary format. IPv4 peers are 6 bytes (4 IP + 2 port), IPv6 peers
//! are 18 bytes (16 IP + 2 port).
//!
//! [BEP-3]: http://bittorrent.org/beps/bep_0003.html
//! [BEP-15]: http://bittorrent.org/beps/bep_0015.html
//! [BEP-23]: http://bittorrent.org/beps/bep_0023.html

mod error;
#[cfg(feature = "http-client")]
mod http;
mod response;
mod udp;

pub use error::TrackerError;
#[cfg(feature = "http-client")]
pub use http::HttpTracker;
pub use response::{AnnounceResponse, CompactPeer, Peer, ScrapeResponse, TrackerEvent};
pub use udp::UdpTracker;

use crate::metainfo::InfoHash;

/// Parameters for a tracker announce request.
///
/// This struct groups all the parameters needed to announce to a tracker,
/// making it easier to pass them to [`TrackerClient::announce`].
///
/// # Example
///
/// ```no_run
/// use rbit::tracker::{AnnounceParams, TrackerClient, TrackerEvent};
/// use rbit::metainfo::InfoHash;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let info_hash = InfoHash::from_hex("c12fe1c06bba254a9dc9f519b335aa7c1367a88a")?;
/// let peer_id = [0u8; 20];
///
/// let params = AnnounceParams {
///     url: "http://tracker.example.com/announce",
///     info_hash: &info_hash,
///     peer_id: &peer_id,
///     port: 6881,
///     uploaded: 0,
///     downloaded: 0,
///     left: 1000000,
///     event: TrackerEvent::Started,
/// };
///
/// let client = TrackerClient::new();
/// let response = client.announce(params).await?;
/// # Ok(())
/// # }
/// ```
pub struct AnnounceParams<'a> {
    /// The tracker URL (http://, https://, or udp://)
    pub url: &'a str,
    /// The torrent's info hash
    pub info_hash: &'a InfoHash,
    /// Our peer ID (20 bytes)
    pub peer_id: &'a [u8; 20],
    /// The port we're listening on
    pub port: u16,
    /// Total bytes uploaded
    pub uploaded: u64,
    /// Total bytes downloaded
    pub downloaded: u64,
    /// Bytes left to download
    pub left: u64,
    /// Event type (started, completed, stopped, or none)
    pub event: TrackerEvent,
}

/// A unified tracker client supporting both HTTP and UDP trackers.
///
/// This client automatically selects the appropriate protocol based on the
/// tracker URL scheme.
///
/// # Example
///
/// ```no_run
/// use rbit::tracker::{AnnounceParams, TrackerClient, TrackerEvent};
/// use rbit::metainfo::InfoHash;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let client = TrackerClient::new();
/// let info_hash = InfoHash::from_hex("c12fe1c06bba254a9dc9f519b335aa7c1367a88a")?;
/// let peer_id = [0u8; 20];
///
/// // Works with HTTP trackers
/// let params = AnnounceParams {
///     url: "http://tracker.example.com/announce",
///     info_hash: &info_hash,
///     peer_id: &peer_id,
///     port: 6881,
///     uploaded: 0,
///     downloaded: 0,
///     left: 1000000,
///     event: TrackerEvent::Started,
/// };
/// let response = client.announce(params).await?;
///
/// // Also works with UDP trackers
/// let params = AnnounceParams {
///     url: "udp://tracker.example.com:6969",
///     info_hash: &info_hash,
///     peer_id: &peer_id,
///     port: 6881,
///     uploaded: 0,
///     downloaded: 0,
///     left: 1000000,
///     event: TrackerEvent::Started,
/// };
/// let response = client.announce(params).await?;
/// # Ok(())
/// # }
/// ```
pub struct TrackerClient {
    _private: (),
}

impl TrackerClient {
    /// Creates a new tracker client.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Announces to a tracker and returns the list of peers.
    ///
    /// Automatically selects HTTP or UDP protocol based on the URL scheme.
    ///
    /// # Errors
    ///
    /// Returns [`TrackerError::UnsupportedProtocol`] if the URL doesn't start
    /// with `http://`, `https://`, or `udp://`.
    pub async fn announce(
        &self,
        params: AnnounceParams<'_>,
    ) -> Result<AnnounceResponse, TrackerError> {
        // Get the v1 info hash bytes (trackers only support v1)
        let info_hash_bytes: [u8; 20] = match params.info_hash {
            InfoHash::V1(bytes) => *bytes,
            InfoHash::Hybrid { v1, .. } => *v1.as_bytes(),
            InfoHash::V2(_) => {
                return Err(TrackerError::InvalidResponse(
                    "trackers only support v1 info hashes".into(),
                ));
            }
        };

        if params.url.starts_with("http://") || params.url.starts_with("https://") {
            #[cfg(feature = "http-client")]
            {
                let tracker = HttpTracker::new(params.url)?;
                tracker
                    .announce(
                        &info_hash_bytes,
                        params.peer_id,
                        params.port,
                        params.uploaded,
                        params.downloaded,
                        params.left,
                        params.event,
                    )
                    .await
            }
            #[cfg(not(feature = "http-client"))]
            {
                Err(TrackerError::UnsupportedProtocol(params.url.to_string()))
            }
        } else if params.url.starts_with("udp://") {
            let mut tracker = UdpTracker::connect(params.url).await?;
            tracker
                .announce(
                    &info_hash_bytes,
                    params.peer_id,
                    params.downloaded,
                    params.left,
                    params.uploaded,
                    params.event,
                    params.port,
                )
                .await
        } else {
            Err(TrackerError::UnsupportedProtocol(params.url.to_string()))
        }
    }
}

impl Default for TrackerClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
