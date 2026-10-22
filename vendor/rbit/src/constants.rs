//! Protocol constants and tuning parameters.
//!
//! This module contains all the constants used throughout the BitTorrent implementation,
//! including timeouts, buffer sizes, protocol values, and tuning parameters.
//!
//! These values are based on defaults from popular clients like qBittorrent, Transmission,
//! and libtorrent, with some adjustments for high-bandwidth connections.

use std::time::Duration;

// ============================================================================
// Client identification
// ============================================================================

/// Client ID prefix for peer ID generation (Azureus-style)
pub const CLIENT_PREFIX: &str = "-RB0001-";

/// User agent string for HTTP requests
pub const USER_AGENT: &str = "rbit/0.1.0";

// ============================================================================
// Ports
// ============================================================================

/// Default BitTorrent listen port
pub const DEFAULT_PORT: u16 = 6881;

/// Local Service Discovery multicast port (BEP-14)
pub const LSD_PORT: u16 = 6771;

/// SSDP port for UPnP discovery
pub const SSDP_PORT: u16 = 1900;

/// NAT-PMP port
pub const NATPMP_PORT: u16 = 5351;

// ============================================================================
// Multicast addresses
// ============================================================================

/// LSD IPv4 multicast address (BEP-14)
pub const LSD_MULTICAST_V4: &str = "239.192.152.143";

/// LSD IPv6 multicast address (BEP-14)
pub const LSD_MULTICAST_V6: &str = "ff15::efc0:988f";

/// SSDP multicast address for UPnP
pub const SSDP_MULTICAST: &str = "239.255.255.250";

// ============================================================================
// Connection limits
// ============================================================================

/// Maximum peers per torrent (qBittorrent default: 100, Transmission: 60)
/// Increased to 200 for high-bandwidth connections
pub const MAX_PEERS_PER_TORRENT: usize = 200;

/// Maximum half-open (connecting) connections per torrent (libtorrent default: 100)
/// This limits connections in progress to prevent resource exhaustion
/// Increased to 200 for faster peer acquisition on high-bandwidth connections
pub const MAX_HALF_OPEN: usize = 200;

/// Global connection limit (qBittorrent: 500, Transmission: 240, libtorrent: 200)
/// We use 500 to match qBittorrent defaults
pub const MAX_GLOBAL_CONNECTIONS: usize = 500;

/// Maximum pending DHT queries
pub const MAX_PENDING_DHT_QUERIES: usize = 1024;

/// Maximum peers we keep unchoked for uploads (qBittorrent: 4, libtorrent: 8)
/// This is upload slots - how many peers can download FROM us simultaneously
pub const MAX_UNCHOKED_PEERS: usize = 4;

/// Upload slots available for seeding (libtorrent default: 8)
pub const DEFAULT_UPLOAD_SLOTS: usize = 8;

/// Maximum retry attempts for failed peer connections
pub const MAX_PEER_RETRY_ATTEMPTS: u32 = 5;

/// Maximum outstanding block requests per peer for request pipelining.
/// qBittorrent/libtorrent default: 500. Higher values improve throughput.
pub const MAX_REQUESTS_PER_PEER: usize = 500;

/// Default number of allowed fast pieces (BEP-6)
pub const DEFAULT_ALLOWED_FAST_COUNT: usize = 10;

// ============================================================================
// Peer thresholds
// ============================================================================

/// Below this threshold, trigger emergency re-announce (very few peers)
pub const PEER_THRESHOLD_CRITICAL: usize = 10;

/// Below this threshold, use aggressive peer discovery
pub const PEER_THRESHOLD_LOW: usize = 30;

/// Target peer count - matches qBittorrent max_connections_per_torrent
pub const PEER_THRESHOLD_MEDIUM: usize = 100;

/// Connection attempts per second (libtorrent default: 30)
/// Increased to 100 for faster peer acquisition on high-bandwidth connections
pub const CONNECTION_SPEED: usize = 100;

// ============================================================================
// Block and piece sizes
// ============================================================================

/// Standard block size (16KB)
pub const BLOCK_SIZE: usize = 16384;

/// Maximum request length per BEP 3 (128KB). Requests larger than this are suspicious.
pub const MAX_REQUEST_LENGTH: u32 = 131072;

/// Maximum concurrent pieces being downloaded
pub const MAX_CONCURRENT_PIECES: usize = 50;

/// Maximum pieces to work on in parallel per peer connection.
/// Higher values improve parallelism but increase memory usage.
pub const MAX_PARALLEL_PIECES: usize = 32;

/// Remaining pieces threshold for endgame mode
pub const ENDGAME_PIECES_THRESHOLD: usize = 10;

/// Metadata piece size (BEP-9)
pub const METADATA_PIECE_SIZE: usize = 16384;

// ============================================================================
// Timeouts - Connection
// ============================================================================

/// TCP connection timeout (libtorrent default: 10s, reduced for faster failure detection)
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(3);

/// Handshake timeout after TCP connect (libtorrent default: 10s)
/// Reduced from 20s to clear failed half-open connections faster
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Peer read timeout
pub const PEER_READ_TIMEOUT: Duration = Duration::from_secs(180);

/// Request timeout for block requests
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Timeout before marking a peer as "snubbed" (not sending data)
pub const SNUB_TIMEOUT: Duration = Duration::from_secs(60);

/// Block request timeout
pub const BLOCK_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ============================================================================
// Timeouts - DHT
// ============================================================================

/// DHT query timeout
pub const DHT_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

// ============================================================================
// Timeouts - Tracker
// ============================================================================

/// HTTP tracker request timeout
pub const HTTP_TRACKER_TIMEOUT: Duration = Duration::from_secs(30);

/// UDP tracker connect timeout
pub const UDP_TRACKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// UDP tracker request timeout
pub const UDP_TRACKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

// ============================================================================
// Timeouts - UPnP
// ============================================================================

/// UPnP discovery timeout
pub const UPNP_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// UPnP request timeout
pub const UPNP_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// UPnP socket read timeout
pub const UPNP_SOCKET_READ_TIMEOUT: Duration = Duration::from_millis(500);

// ============================================================================
// Timeouts - Metadata
// ============================================================================

/// Metadata fetch timeout
pub const METADATA_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Metadata read timeout
pub const METADATA_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Magnet metadata timeout
pub const MAGNET_METADATA_TIMEOUT: Duration = Duration::from_secs(10);

// ============================================================================
// Intervals - Tracker
// ============================================================================

/// Default tracker announce interval
pub const TRACKER_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(1800);

/// Minimum tracker announce interval
pub const TRACKER_MIN_INTERVAL: Duration = Duration::from_secs(60);

/// Aggressive tracker announce interval (when few peers)
pub const TRACKER_AGGRESSIVE_INTERVAL: Duration = Duration::from_secs(300);

/// Moderate tracker announce interval
pub const TRACKER_MODERATE_INTERVAL: Duration = Duration::from_secs(900);

// ============================================================================
// Intervals - DHT
// ============================================================================

/// DHT query interval when peer count is critical
pub const DHT_INTERVAL_CRITICAL: Duration = Duration::from_secs(15);

/// DHT query interval when peer count is low
pub const DHT_INTERVAL_LOW: Duration = Duration::from_secs(30);

/// DHT query interval when peer count is medium
pub const DHT_INTERVAL_MEDIUM: Duration = Duration::from_secs(60);

/// DHT query interval when peer count is high
pub const DHT_INTERVAL_HIGH: Duration = Duration::from_secs(180);

// ============================================================================
// Intervals - Choking
// ============================================================================

/// Choking algorithm run interval
pub const CHOKING_INTERVAL: Duration = Duration::from_secs(10);

/// Optimistic unchoke interval
pub const OPTIMISTIC_UNCHOKE_INTERVAL: Duration = Duration::from_secs(30);

// ============================================================================
// Intervals - Misc
// ============================================================================

/// Base delay for peer retry backoff
pub const PEER_RETRY_BASE_DELAY: Duration = Duration::from_secs(60);

/// Keepalive message interval
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(120);

/// LSD announce interval (BEP-14)
pub const LSD_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(300);

/// PEX messages should be sent no more than once per minute per BEP-11
pub const PEX_SEND_INTERVAL: Duration = Duration::from_secs(60);

/// Delay before sending first PEX - reduced from 120s for faster peer discovery
/// BEP-11 doesn't mandate a delay, but we wait for extension handshake
pub const PEX_INITIAL_DELAY: Duration = Duration::from_secs(30);

/// Rate calculation window for speed measurements
pub const RATE_CALC_WINDOW: Duration = Duration::from_secs(5);

/// Rate update interval
pub const RATE_UPDATE_INTERVAL: Duration = Duration::from_millis(500);

// ============================================================================
// Loop intervals
// ============================================================================

/// Fast loop interval (high activity)
pub const LOOP_INTERVAL_FAST: Duration = Duration::from_millis(200);

/// Normal loop interval
pub const LOOP_INTERVAL_NORMAL: Duration = Duration::from_millis(500);

/// Stable loop interval (low activity)
pub const LOOP_INTERVAL_STABLE: Duration = Duration::from_secs(1);

/// Sleep interval when paused
pub const PAUSED_SLEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Sleep interval during piece checking
pub const CHECKING_SLEEP_INTERVAL: Duration = Duration::from_millis(500);

// ============================================================================
// Buffer sizes
// ============================================================================

/// Socket receive buffer size (2MB for high throughput)
pub const SOCKET_RECV_BUFFER_SIZE: usize = 2097152;

/// Socket send buffer size (2MB for high throughput)
pub const SOCKET_SEND_BUFFER_SIZE: usize = 2097152;

/// Read buffer size for peer connections (512KB)
pub const READ_BUFFER_SIZE: usize = 524288;

/// Maximum message size (16MB)
pub const MAX_MESSAGE_SIZE: usize = 16777216;

/// Maximum metadata size (1MB)
pub const MAX_METADATA_SIZE: usize = 1048576;

// ============================================================================
// LSD constants
// ============================================================================

/// LSD cookie size
pub const LSD_COOKIE_SIZE: usize = 8;

/// LSD channel capacity
pub const LSD_CHANNEL_CAPACITY: usize = 64;

// ============================================================================
// DHT constants
// ============================================================================

/// DHT bucket size (k value in Kademlia)
pub const DHT_BUCKET_SIZE: usize = 8;

/// Number of DHT buckets (160 for SHA-1)
pub const DHT_NUM_BUCKETS: usize = 160;

/// DHT alpha value (parallel queries)
pub const DHT_ALPHA: usize = 8;

/// Maximum DHT lookup iterations
pub const DHT_MAX_ITERATIONS: usize = 15;

/// Number of peers for early return in DHT lookup
pub const DHT_PEERS_EARLY_RETURN: usize = 50;

/// Well-known DHT bootstrap nodes
pub const DHT_BOOTSTRAP_NODES: &[&str] = &[
    "router.bittorrent.com:6881",
    "router.utorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "dht.libtorrent.org:25401",
];

// ============================================================================
// PEX constants
// ============================================================================

/// Maximum peers per PEX message
pub const PEX_MAX_PEERS_PER_MESSAGE: usize = 100;

/// Maximum IPv4 peers in PEX message
pub const PEX_MAX_IPV4_PEERS: usize = 50;

/// Maximum IPv6 peers in PEX message
pub const PEX_MAX_IPV6_PEERS: usize = 50;

/// PEX flag: peer prefers encryption
pub const PEX_FLAG_PREFERS_ENCRYPTION: u8 = 0x01;

/// PEX flag: peer is upload-only (seeder)
pub const PEX_FLAG_UPLOAD_ONLY: u8 = 0x02;

/// PEX flag: peer supports uTP
pub const PEX_FLAG_SUPPORTS_UTP: u8 = 0x04;

/// PEX flag: peer supports hole punching
pub const PEX_FLAG_SUPPORTS_HOLEPUNCH: u8 = 0x08;

/// PEX flag: peer is reachable
pub const PEX_FLAG_REACHABLE: u8 = 0x10;

// ============================================================================
// Protocol constants
// ============================================================================

/// BitTorrent protocol string
pub const PROTOCOL_STRING: &str = "BitTorrent protocol";

/// Reserved bytes in handshake
pub const RESERVED_BYTES: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 0];

/// Extension protocol bit (BEP-10)
pub const EXTENSION_BIT: u8 = 0x10;

/// DHT support bit (BEP-5)
pub const DHT_BIT: u8 = 0x01;

/// Fast extension bit (BEP-6)
pub const FAST_EXTENSION_BIT: u8 = 0x04;

/// Extension handshake message ID
pub const EXTENSION_HANDSHAKE_ID: u8 = 0;

/// ut_metadata extension ID
pub const UT_METADATA_ID: u8 = 1;

/// ut_pex extension ID
pub const UT_PEX_ID: u8 = 2;

// ============================================================================
// UDP tracker protocol constants
// ============================================================================

/// UDP tracker protocol ID (magic number)
pub const UDP_TRACKER_PROTOCOL_ID: i64 = 0x41727101980;

/// UDP tracker connect action
pub const UDP_ACTION_CONNECT: u32 = 0;

/// UDP tracker announce action
pub const UDP_ACTION_ANNOUNCE: u32 = 1;

/// UDP tracker scrape action
pub const UDP_ACTION_SCRAPE: u32 = 2;

/// UDP tracker error action
pub const UDP_ACTION_ERROR: u32 = 3;

// ============================================================================
// UPnP constants
// ============================================================================

/// SSDP MX value for discovery
pub const SSDP_MX_VALUE: u8 = 3;

// ============================================================================
// Bandwidth constants
// ============================================================================

/// Bandwidth burst multiplier
pub const BANDWIDTH_BURST_MULTIPLIER: u64 = 2;

// ============================================================================
// Misc constants
// ============================================================================

/// Progress log interval (pieces)
pub const PROGRESS_LOG_INTERVAL: usize = 100;

/// Backoff exponent cap for retries
pub const BACKOFF_EXPONENT_CAP: u32 = 4;

/// DHT query sleep between iterations
pub const DHT_QUERY_SLEEP: Duration = Duration::from_millis(250);

/// Peer retry sleep
pub const PEER_RETRY_SLEEP: Duration = Duration::from_millis(250);

/// Connection retry sleep
pub const CONNECTION_RETRY_SLEEP: Duration = Duration::from_millis(100);

// ============================================================================
// Cache constants
// ============================================================================

/// Default cache memory (256MB)
pub const DEFAULT_CACHE_MEMORY: usize = 256 * 1024 * 1024;

/// Maximum cache memory (1GB)
pub const MAX_CACHE_MEMORY: usize = 1024 * 1024 * 1024;

/// Ratio of cache memory for block cache
pub const BLOCK_CACHE_RATIO: f32 = 0.6;

/// Ratio of cache memory for piece cache
pub const PIECE_CACHE_RATIO: f32 = 0.3;

/// Write coalesce timeout
pub const WRITE_COALESCE_TIMEOUT: Duration = Duration::from_secs(5);

/// I/O batch size
pub const IO_BATCH_SIZE: usize = 64;

/// I/O batch timeout
pub const IO_BATCH_TIMEOUT: Duration = Duration::from_millis(10);

/// Number of I/O worker threads
pub const IO_WORKERS: usize = 4;

/// Buffer pool block count
pub const BUFFER_POOL_BLOCKS: usize = 1024;

/// Buffer pool piece count
pub const BUFFER_POOL_PIECES: usize = 64;
