# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- BEP-19: WebSeed - HTTP/FTP seeding support for downloading from web servers
- BEP-42: DHT Security Extension - node ID generation and validation based on IP address
- BEP-55: Holepunch Extension - NAT traversal via UDP hole punching

## [0.2.2] - 2025-12-02

### Added
- BEP-52: Complete storage integration for v2 torrents (merkle tree verification)
- BEP-52: `V2PieceMap.build_piece_info()` helper for creating piece info from v2 metainfo
- BEP-52: `TorrentBuilder` for creating v1, v2, and hybrid torrent files

### Fixed
- `CachingDiskManager.write_block()` now correctly uses v2 merkle verification

## [0.1.2] - 2024-12-01

### Added
- Additional functionality from oxidebt micro crates

### Changed
- Switched edition configuration for docs.rs compatibility

## [0.1.1] - 2024-11-30

### Changed
- Documentation improvements

## [0.1.0] - 2024-11-30

### Added
- Initial release
- BEP-3: BitTorrent protocol (bencode, metainfo, tracker)
- BEP-5: DHT Protocol (Kademlia-based distributed hash table)
- BEP-6: Fast Extension
- BEP-9: Extension for Peers to Send Metadata Files (magnet links)
- BEP-10: Extension Protocol
- BEP-11: Peer Exchange (PEX)
- BEP-14: Local Service Discovery
- BEP-15: UDP Tracker Protocol
- BEP-23: Tracker Returns Compact Peer Lists
- BEP-52: BitTorrent v2 support
- Async storage layer with piece/block mapping
- Memory caching with budget management
- UPnP port forwarding support

[Unreleased]: https://github.com/jspwrd/rbit/compare/v0.2.2...HEAD
[0.2.2]: https://github.com/jspwrd/rbit/compare/v0.1.2...v0.2.2
[0.1.2]: https://github.com/jasper/rbit/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/jasper/rbit/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/jasper/rbit/releases/tag/v0.1.0
