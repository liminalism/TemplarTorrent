# rbit

A comprehensive BitTorrent library implementing core BEP (BitTorrent Enhancement Proposals) specifications in pure Rust.

[![Crates.io](https://img.shields.io/crates/v/rbit.svg)](https://crates.io/crates/rbit)
[![Documentation](https://docs.rs/rbit/badge.svg)](https://docs.rs/rbit)
[![License](https://img.shields.io/crates/l/rbit.svg)](LICENSE)

## Features

- **Torrent parsing** - Read `.torrent` files and magnet links
- **Peer communication** - Connect to and exchange data with peers using the wire protocol
- **Tracker protocols** - Discover peers via HTTP and UDP trackers
- **DHT** - Trackerless peer discovery using a Kademlia-based distributed hash table
- **Storage management** - Efficient disk I/O with piece verification
- **Caching** - Memory-efficient piece and block caching

### Highlights

- **Async/await** - Built on tokio for efficient async I/O
- **Zero-copy** - Uses `bytes::Bytes` for efficient buffer handling
- **Memory-safe** - Pure Rust with no unsafe code in the public API
- **Concurrent** - Thread-safe primitives from `parking_lot` and `dashmap`

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
rbit = "0.1"
```

## Quick Start

### Parsing a torrent file

```rust
use rbit::Metainfo;

let torrent_data = std::fs::read("example.torrent")?;
let metainfo = Metainfo::from_bytes(&torrent_data)?;

println!("Name: {}", metainfo.info.name);
println!("Info hash: {}", metainfo.info_hash);
println!("Total size: {} bytes", metainfo.info.total_length);
println!("Piece count: {}", metainfo.info.pieces.len());

for tracker in metainfo.trackers() {
    println!("Tracker: {}", tracker);
}
```

### Parsing a magnet link

```rust
use rbit::MagnetLink;

let magnet = MagnetLink::parse(
    "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=Example"
)?;

println!("Info hash: {}", magnet.info_hash);
println!("Display name: {:?}", magnet.display_name);
```

### Connecting to a peer

```rust
use rbit::{PeerConnection, PeerId, Message};
use std::net::SocketAddr;

let peer_addr: SocketAddr = "192.168.1.100:6881".parse()?;
let info_hash = [0u8; 20]; // Your torrent's info hash
let our_peer_id = PeerId::generate();

let mut conn = PeerConnection::connect(
    peer_addr,
    info_hash,
    *our_peer_id.as_bytes()
).await?;

// Express interest in downloading
conn.send(Message::Interested).await?;

// Wait for unchoke before requesting pieces
loop {
    match conn.receive().await? {
        Message::Unchoke => break,
        Message::Bitfield(bits) => println!("Peer has {} bytes of bitfield", bits.len()),
        _ => {}
    }
}
```

### Announcing to an HTTP tracker

```rust
use rbit::{HttpTracker, TrackerEvent};

let tracker = HttpTracker::new("http://tracker.example.com/announce")?;

let response = tracker.announce(
    &[0u8; 20],           // info_hash
    &[0u8; 20],           // peer_id
    6881,                 // port
    0,                    // uploaded
    0,                    // downloaded
    1000,                 // left
    TrackerEvent::Started,
).await?;

println!("Found {} peers", response.peers.len());
println!("Re-announce in {} seconds", response.interval);
```

### Using the DHT

```rust
use rbit::DhtServer;

let dht = DhtServer::bind(6881).await?;

// Bootstrap from well-known nodes
dht.bootstrap().await?;

// Find peers for a specific info hash
let info_hash = [0u8; 20];
let peers = dht.get_peers(info_hash).await?;

for peer in peers {
    println!("Found peer: {}", peer);
}
```

## Supported BEPs

| BEP | Description | Module |
|-----|-------------|--------|
| [BEP-3](http://bittorrent.org/beps/bep_0003.html) | BitTorrent Protocol | `peer`, `metainfo`, `tracker` |
| [BEP-5](http://bittorrent.org/beps/bep_0005.html) | DHT Protocol | `dht` |
| [BEP-6](http://bittorrent.org/beps/bep_0006.html) | Fast Extension | `peer` |
| [BEP-9](http://bittorrent.org/beps/bep_0009.html) | Magnet Links | `metainfo` |
| [BEP-10](http://bittorrent.org/beps/bep_0010.html) | Extension Protocol | `peer` |
| [BEP-11](http://bittorrent.org/beps/bep_0011.html) | Peer Exchange (PEX) | `pex` |
| [BEP-14](http://bittorrent.org/beps/bep_0014.html) | Local Service Discovery | `lsd` |
| [BEP-15](http://bittorrent.org/beps/bep_0015.html) | UDP Tracker Protocol | `tracker` |
| [BEP-23](http://bittorrent.org/beps/bep_0023.html) | Compact Peer Lists | `tracker` |
| [BEP-52](http://bittorrent.org/beps/bep_0052.html) | BitTorrent v2 (partial) | `metainfo` |

## Modules

- **bencode** - Bencode serialization format used throughout BitTorrent
- **metainfo** - Torrent file parsing, magnet links, and info hashes
- **peer** - Peer wire protocol for data exchange between clients
- **tracker** - HTTP and UDP tracker clients for peer discovery
- **dht** - Kademlia-based distributed hash table for trackerless operation
- **pex** - Peer Exchange for sharing peer lists between connected peers
- **lsd** - Local Service Discovery via multicast for LAN peers
- **storage** - Disk I/O management with piece verification
- **cache** - Memory caching for pieces and blocks

## Architecture Notes

This library provides low-level building blocks rather than a complete BitTorrent client. You are responsible for:

- Coordinating peer connections and piece selection
- Managing download/upload state across peers
- Implementing rate limiting and choking algorithms
- Handling torrent lifecycle (start, pause, resume, remove)

For a complete client implementation, combine these modules with your own orchestration logic.

## Minimum Supported Rust Version

This crate requires Rust 1.85 or later.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
