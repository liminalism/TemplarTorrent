//! Distributed Hash Table ([BEP-5]).
//!
//! This module implements the Kademlia-based DHT used by BitTorrent for
//! trackerless peer discovery. The DHT allows finding peers without relying
//! on centralized trackers.
//!
//! # Overview
//!
//! The BitTorrent DHT is a distributed database of peers for each torrent.
//! Nodes are identified by 160-bit node IDs, and torrents by their 20-byte
//! info hash. The DHT uses the Kademlia protocol with XOR distance metric.
//!
//! # Getting Started
//!
//! ```no_run
//! use rbit::dht::DhtServer;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Bind to a UDP port
//! let dht = DhtServer::bind(6881).await?;
//!
//! // Bootstrap from known nodes
//! dht.bootstrap().await?;
//!
//! // Find peers for a torrent
//! let info_hash = [0u8; 20]; // Your torrent's info hash
//! let peers = dht.get_peers(info_hash).await?;
//!
//! for peer in peers {
//!     println!("Found peer: {}", peer);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # DHT Operations
//!
//! ## Queries
//!
//! - **ping** - Check if a node is alive
//! - **find_node** - Find nodes close to a target ID
//! - **get_peers** - Find peers for an info hash
//! - **announce_peer** - Announce that we have a torrent
//!
//! ## Bootstrap Nodes
//!
//! The DHT starts empty and needs to bootstrap from known nodes.
//! Default bootstrap nodes are provided:
//!
//! - `router.bittorrent.com:6881`
//! - `dht.transmissionbt.com:6881`
//! - `router.utorrent.com:6881`
//!
//! # Architecture
//!
//! The DHT uses several components:
//!
//! - [`DhtServer`] - Main server handling UDP communication
//! - [`RoutingTable`] - K-bucket storage for known nodes
//! - [`NodeId`] - 160-bit node identifier
//! - [`Node`] - A known DHT node (ID + address)
//!
//! # Routing Table
//!
//! The routing table uses 160 k-buckets (one per bit of distance from our ID).
//! Each bucket holds up to 8 nodes. Nodes are categorized as:
//!
//! - **Good** - Recently seen, no failures
//! - **Questionable** - Not seen recently but no failures
//! - **Bad** - Multiple consecutive failures
//!
//! [BEP-5]: http://bittorrent.org/beps/bep_0005.html

mod error;
mod message;
mod node;
mod node_id_security;
mod routing;
mod server;

pub use error::DhtError;
pub use message::{DhtMessage, DhtQuery, DhtResponse, TransactionId};
pub use node::{Node, NodeId};
pub use node_id_security::{
    decode_compact_ip_port, encode_compact_ip_port, generate_secure_node_id, is_local_network,
    validate_node_id, BEP42_IPV4_MASK, BEP42_IPV6_MASK, BEP42_REQUIRED_VOTES,
};
pub use routing::RoutingTable;
pub use server::{Bep42Config, DhtServer};

#[cfg(test)]
mod tests;
