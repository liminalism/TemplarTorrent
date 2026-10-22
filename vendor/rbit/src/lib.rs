//! # rbit
//!
//! A comprehensive BitTorrent library implementing core BEP (BitTorrent Enhancement
//! Proposals) specifications in pure Rust.
//!
//! ## Overview
//!
//! `rbit` provides building blocks for creating BitTorrent applications, including:
//!
//! - **Torrent parsing** - Read `.torrent` files and magnet links
//! - **Peer communication** - Connect to and exchange data with peers
//! - **Tracker protocols** - Discover peers via HTTP and UDP trackers
//! - **DHT** - Trackerless peer discovery using a distributed hash table
//! - **Storage management** - Efficient disk I/O with piece verification
//! - **Caching** - Memory-efficient caching using the ARC algorithm
//!
//! ## Quick Start
//!
//! ### Parsing a torrent file
//!
//! ```no_run
//! use rbit::Metainfo;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let torrent_data = std::fs::read("example.torrent")?;
//! let metainfo = Metainfo::from_bytes(&torrent_data)?;
//!
//! println!("Name: {}", metainfo.info.name);
//! println!("Info hash: {}", metainfo.info_hash);
//! println!("Total size: {} bytes", metainfo.info.total_length);
//! println!("Piece count: {}", metainfo.info.piece_count());
//!
//! for tracker in metainfo.trackers() {
//!     println!("Tracker: {}", tracker);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ### Parsing a magnet link
//!
//! ```
//! use rbit::MagnetLink;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let magnet = MagnetLink::parse(
//!     "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=Example"
//! )?;
//!
//! println!("Info hash: {}", magnet.info_hash);
//! println!("Display name: {:?}", magnet.display_name);
//! # Ok(())
//! # }
//! ```
//!
//! ### Connecting to a peer
//!
//! ```no_run
//! use rbit::{PeerConnection, PeerId, Message};
//! use std::net::SocketAddr;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let peer_addr: SocketAddr = "192.168.1.100:6881".parse()?;
//! let info_hash = [0u8; 20]; // Your torrent's info hash
//! let our_peer_id = PeerId::generate();
//!
//! let mut conn = PeerConnection::connect(
//!     peer_addr,
//!     info_hash,
//!     *our_peer_id.as_bytes()
//! ).await?;
//!
//! // Express interest in downloading
//! conn.send(Message::Interested).await?;
//!
//! // Wait for unchoke before requesting pieces
//! loop {
//!     match conn.receive().await? {
//!         Message::Unchoke => break,
//!         Message::Bitfield(bits) => println!("Peer has {} bytes of bitfield", bits.len()),
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ### Announcing to an HTTP tracker
//!
//! ```no_run
//! use rbit::{HttpTracker, TrackerEvent};
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
//!     1000,                 // left
//!     TrackerEvent::Started,
//! ).await?;
//!
//! println!("Found {} peers", response.peers.len());
//! println!("Re-announce in {} seconds", response.interval);
//! # Ok(())
//! # }
//! ```
//!
//! ### Using the DHT for trackerless peer discovery
//!
//! ```no_run
//! use rbit::DhtServer;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let dht = DhtServer::bind(6881).await?;
//!
//! // Bootstrap from well-known nodes
//! dht.bootstrap().await?;
//!
//! // Find peers for a specific info hash
//! let info_hash = [0u8; 20];
//! let peers = dht.get_peers(info_hash).await?;
//!
//! for peer in peers {
//!     println!("Found peer: {}", peer);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Supported BEPs
//!
//! | BEP | Description | Module |
//! |-----|-------------|--------|
//! | [BEP-3](http://bittorrent.org/beps/bep_0003.html) | BitTorrent Protocol | [`peer`], [`metainfo`], [`tracker`] |
//! | [BEP-5](http://bittorrent.org/beps/bep_0005.html) | DHT Protocol | [`dht`] |
//! | [BEP-6](http://bittorrent.org/beps/bep_0006.html) | Fast Extension | [`peer`] |
//! | [BEP-9](http://bittorrent.org/beps/bep_0009.html) | Magnet Links | [`metainfo`] |
//! | [BEP-10](http://bittorrent.org/beps/bep_0010.html) | Extension Protocol | [`peer`] |
//! | [BEP-11](http://bittorrent.org/beps/bep_0011.html) | Peer Exchange (PEX) | [`pex`] |
//! | [BEP-14](http://bittorrent.org/beps/bep_0014.html) | Local Service Discovery | [`lsd`] |
//! | [BEP-15](http://bittorrent.org/beps/bep_0015.html) | UDP Tracker Protocol | [`tracker`] |
//! | [BEP-23](http://bittorrent.org/beps/bep_0023.html) | Compact Peer Lists | [`tracker`] |
//! | [BEP-52](http://bittorrent.org/beps/bep_0052.html) | BitTorrent v2 | [`metainfo`], [`storage`] |
//!
//! ## Module Overview
//!
//! - [`bencode`] - Bencode serialization format used throughout BitTorrent
//! - [`metainfo`] - Torrent file parsing, magnet links, and info hashes
//! - [`peer`] - Peer wire protocol for data exchange between clients
//! - [`tracker`] - HTTP and UDP tracker clients for peer discovery
//! - [`dht`] - Kademlia-based distributed hash table for trackerless operation
//! - [`pex`] - Peer Exchange for sharing peer lists between connected peers
//! - [`lsd`] - Local Service Discovery via multicast for LAN peers
//! - [`storage`] - Disk I/O management with piece verification
//! - [`cache`] - Memory caching for pieces and blocks using ARC
//!
//! ## Feature Highlights
//!
//! - **Async/await** - Built on tokio for efficient async I/O
//! - **Zero-copy** - Uses `bytes::Bytes` for efficient buffer handling
//! - **Memory-safe** - Pure Rust with no unsafe code in the public API
//! - **Concurrent** - Thread-safe primitives from `parking_lot` and `dashmap`
//!
//! ## Architecture Notes
//!
//! This library provides low-level building blocks rather than a complete
//! BitTorrent client. You are responsible for:
//!
//! - Coordinating peer connections and piece selection
//! - Managing download/upload state across peers
//! - Implementing rate limiting and choking algorithms
//! - Handling torrent lifecycle (start, pause, resume, remove)
//!
//! For a complete client implementation, you would combine these modules
//! with your own orchestration logic.

pub mod bandwidth;
pub mod bencode;
pub mod cache;
pub mod constants;
pub mod dht;
pub mod lsd;
pub mod metainfo;
pub mod peer;
pub mod pex;
pub mod storage;
pub mod tracker;
#[cfg(feature = "http-client")]
pub mod upnp;
#[cfg(feature = "http-client")]
pub mod webseed;

// Bandwidth limiting
pub use bandwidth::{BandwidthLimiter, RateLimiter};
// Bencode
pub use bencode::{decode, encode, BencodeError, Value};
// Caching
pub use cache::{BlockCache, BufferPool, MemoryBudget, PieceCache};
// DHT
pub use dht::{
    decode_compact_ip_port, encode_compact_ip_port, generate_secure_node_id, is_local_network,
    validate_node_id, Bep42Config, DhtError, DhtMessage, DhtServer, Node, NodeId, RoutingTable,
    BEP42_IPV4_MASK, BEP42_IPV6_MASK, BEP42_REQUIRED_VOTES,
};
// LSD
pub use lsd::{LsdAnnounce, LsdError, LsdService};
// Metainfo
pub use metainfo::{
    compute_piece_root, compute_root, extract_layer_hashes, generate_proof_hashes, hash_block,
    hash_data_blocks, verify_piece, verify_piece_layer, File, FileTree, FlattenedFile, Info,
    InfoHash, InfoHashV1, InfoHashV2, MagnetLink, MerkleTree, Metainfo, MetainfoError, PieceHashes,
    PieceLayers, TorrentBuilder, TorrentVersion, DEFAULT_PIECE_LENGTH, MERKLE_BLOCK_SIZE,
    MIN_V2_PIECE_LENGTH,
};
// Peer
pub use peer::{
    generate_allowed_fast_set, metadata_piece_count, metadata_piece_size, validate_hash_request,
    Bitfield, Block, BlockRequest, ChokingAlgorithm, ChokingDecision, ExtensionHandshake,
    FastExtensionState, Handshake, HolepunchError, HolepunchErrorCode, HolepunchMessage,
    HolepunchMessageType, HolepunchState, Message, MessageId, MetadataMessage, MetadataMessageType,
    PeerConnection, PeerError, PeerId, PeerState, PieceManager, ProtocolMode,
    HOLEPUNCH_EXTENSION_NAME, METADATA_PIECE_SIZE,
};
// PEX
pub use pex::{PexFlags, PexMessage, PexPeer};
// Storage
pub use storage::{
    coalesce_blocks, AllocationMode, CachingDiskManager, DiskManager, FileEntry, FlushRequest,
    FlushResult, IoQueue, IoWorker, MemoryStats, PieceFileSpan, PieceInfo, StorageError,
    TorrentStorage, V2PieceMap, WriteCoalescer, WriteOp, WritePriority, WriteRegion, WriteResult,
};
// Tracker
#[cfg(feature = "http-client")]
pub use tracker::HttpTracker;
pub use tracker::{
    AnnounceParams, AnnounceResponse, CompactPeer, Peer, ScrapeResponse, TrackerClient,
    TrackerError, TrackerEvent, UdpTracker,
};
// UPnP
#[cfg(feature = "http-client")]
pub use upnp::{PortMapper, PortMapping, Protocol, UpnpError};
// WebSeed (BEP-19)
#[cfg(feature = "http-client")]
pub use webseed::{
    FileInfo as WebSeedFileInfo, GapAwarePieceSelector, TorrentInfo as WebSeedTorrentInfo,
    WebSeedClient, WebSeedError, WEBSEED_CONNECT_TIMEOUT, WEBSEED_GAP_FILL_THRESHOLD,
    WEBSEED_MAX_RETRIES, WEBSEED_READ_TIMEOUT,
};
