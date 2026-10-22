//! Peer wire protocol ([BEP-3], [BEP-6], [BEP-10]).
//!
//! This module implements the BitTorrent peer wire protocol for exchanging
//! data between clients, including support for the Fast Extension and
//! Extension Protocol.
//!
//! # Overview
//!
//! The peer wire protocol operates over TCP connections. After connecting,
//! peers exchange a handshake, then communicate using length-prefixed messages.
//!
//! ## Connection Lifecycle
//!
//! 1. **Handshake** - Exchange protocol identifier, info hash, and peer ID
//! 2. **Bitfield** - (Optional) Announce which pieces the peer has
//! 3. **Interest/Choking** - Negotiate data transfer readiness
//! 4. **Data Transfer** - Request and receive piece data blocks
//!
//! # Examples
//!
//! ## Connecting to a peer
//!
//! ```no_run
//! use rbit::peer::{PeerConnection, PeerId, Message};
//! use std::net::SocketAddr;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let addr: SocketAddr = "192.168.1.100:6881".parse()?;
//! let info_hash = [0u8; 20];
//! let peer_id = PeerId::generate();
//!
//! let mut conn = PeerConnection::connect(addr, info_hash, *peer_id.as_bytes()).await?;
//!
//! // Send interest
//! conn.send(Message::Interested).await?;
//!
//! // Receive messages
//! loop {
//!     match conn.receive().await? {
//!         Message::Unchoke => {
//!             println!("Peer unchoked us, can request pieces");
//!             break;
//!         }
//!         Message::Bitfield(bits) => {
//!             println!("Peer has bitfield of {} bytes", bits.len());
//!         }
//!         Message::Have { piece } => {
//!             println!("Peer has piece {}", piece);
//!         }
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Requesting a block
//!
//! ```no_run
//! use rbit::peer::{PeerConnection, Message};
//! # use std::net::SocketAddr;
//!
//! # async fn example(conn: &mut PeerConnection) -> Result<(), Box<dyn std::error::Error>> {
//! // Request block: piece 0, offset 0, 16KB
//! conn.send(Message::Request {
//!     index: 0,
//!     begin: 0,
//!     length: 16384,
//! }).await?;
//!
//! // Receive the piece data
//! if let Message::Piece { index, begin, data } = conn.receive().await? {
//!     println!("Received {} bytes for piece {} at offset {}", data.len(), index, begin);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Message Types
//!
//! | Message | ID | Description |
//! |---------|----|-------------|
//! | KeepAlive | - | Empty message to maintain connection |
//! | Choke | 0 | Stop sending requests |
//! | Unchoke | 1 | Ready to receive requests |
//! | Interested | 2 | Want to download from peer |
//! | NotInterested | 3 | Don't want to download |
//! | Have | 4 | Announce a new piece |
//! | Bitfield | 5 | Announce all available pieces |
//! | Request | 6 | Request a data block |
//! | Piece | 7 | Send piece data |
//! | Cancel | 8 | Cancel a pending request |
//! | Port | 9 | DHT port announcement |
//!
//! ## Fast Extension ([BEP-6])
//!
//! | Message | ID | Description |
//! |---------|----|-------------|
//! | Suggest | 13 | Suggest a piece to download |
//! | HaveAll | 14 | Peer has all pieces (seeder) |
//! | HaveNone | 15 | Peer has no pieces |
//! | Reject | 16 | Reject a block request |
//! | AllowedFast | 17 | Allow downloading while choked |
//!
//! ## Extension Protocol ([BEP-10])
//!
//! | Message | ID | Description |
//! |---------|----|-------------|
//! | Extended | 20 | Extension message with sub-ID |
//!
//! # Choking Algorithm
//!
//! BitTorrent uses a 4-way handshake for data transfer:
//!
//! - **am_choking** - Are we choking the peer (not sending data)?
//! - **am_interested** - Are we interested in the peer's data?
//! - **peer_choking** - Is the peer choking us?
//! - **peer_interested** - Is the peer interested in our data?
//!
//! Data transfer requires: `!peer_choking && am_interested`
//!
//! [BEP-3]: http://bittorrent.org/beps/bep_0003.html
//! [BEP-6]: http://bittorrent.org/beps/bep_0006.html
//! [BEP-10]: http://bittorrent.org/beps/bep_0010.html

mod bitfield;
mod choking;
mod connection;
mod error;
mod extension;
mod fast;
mod hash_request;
mod holepunch;
mod message;
mod metadata;
mod peer_id;
mod piece;
mod transport;

pub use bitfield::Bitfield;
pub use choking::{ChokingAlgorithm, ChokingDecision, ChokingState, PeerStats};
pub use connection::{PeerConnection, PeerState, ProtocolMode};
pub use error::PeerError;
pub use extension::{ExtensionHandshake, ExtensionMessage};
pub use fast::{generate_allowed_fast_set, FastExtension, FastExtensionState};
pub use hash_request::{
    HashRequestKey, HashRequestManager, HashResponse, HashServer, PendingHashRequest,
    HASH_REQUEST_TIMEOUT, MAX_PENDING_HASH_REQUESTS,
};
pub use holepunch::{
    HolepunchAddrType, HolepunchError, HolepunchErrorCode, HolepunchMessage, HolepunchMessageType,
    HolepunchState, HOLEPUNCH_DEFAULT_EXTENSION_ID, HOLEPUNCH_EXTENSION_NAME,
    HOLEPUNCH_RETRY_DELAY_SECS, HOLEPUNCH_TIMEOUT_SECS,
};
pub use message::{validate_hash_request, Handshake, Message, MessageId};
pub use metadata::{
    metadata_piece_count, metadata_piece_size, MetadataMessage, MetadataMessageType,
    METADATA_PIECE_SIZE,
};
pub use peer_id::PeerId;
pub use piece::{Block, BlockRequest, PieceManager, BLOCK_SIZE, REQUEST_TIMEOUT};
pub use transport::PeerTransport;

#[cfg(test)]
mod tests;
