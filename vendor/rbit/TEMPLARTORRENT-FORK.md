# TemplarTorrent fork of rbit 0.2.2

Source: the `rbit` 0.2.2 crate as published on crates.io (MIT OR Apache-2.0,
copyright the rbit authors; see LICENSE-MIT and LICENSE-APACHE). Upstream:
https://github.com/jspwrd/rbit.

Applied through `[patch.crates-io]` in the workspace root. Changes from the
published crate, all made on 2026-10-22 for the native-source release gate:

- `Cargo.toml`: `reqwest` is `optional = true` behind a new `http-client`
  feature; `default = []`.
- `src/lib.rs`, `src/tracker.rs`, `src/tracker/error.rs`: `HttpTracker`, the
  `upnp` and `webseed` modules, their re-exports and the `TrackerError::Http`
  variant are compiled only with `http-client`; `TrackerClient::announce`
  returns `UnsupportedProtocol` for HTTP URLs without it.
- `src/tracker/response.rs`: `parse_compact_peers6` allows dead code without
  the feature (only the HTTP client called it).

Removed, not modified: upstream's `.github/` CI and dependabot configuration,
`CLAUDE.md` agent notes and the crate-local `Cargo.lock` (the workspace lock
governs). TemplarTorrent announces to HTTP trackers through its
own bounded transport in `integration/rbit-adapter/src/announce.rs`.

Panic fixes, 2026-10-24, each found by a target in `fuzz/` and pinned by
`integration/rbit-adapter/tests/parser_regressions.rs`:

- `src/bencode/decode.rs`: a dictionary key that is not a byte string and
  ends at the end of input indexed one past the buffer (`dle`); a string
  length near `usize::MAX` overflowed `pos + len`. Both now return errors.
- `src/peer/metadata.rs`: `find_dict_end` skips string data with a
  saturating add instead of `i += len`.
- `src/metainfo/info_hash.rs`: `hex_decode` refuses non-ASCII input before
  slicing two bytes at a time, so a magnet btih with a multibyte char is an
  error, not a char-boundary panic.
- `src/metainfo/torrent.rs`, `src/metainfo/file_tree.rs`: negative
  `length` and `piece length` values are refused instead of cast to `u64`,
  v1 refuses `piece length` 0 (later per-file piece counts divide by it), and
  file length and offset sums use checked addition in v1, v2 and hybrid.
- `src/peer/extension.rs`, `src/peer/metadata.rs`: an extension id past 255
  is skipped instead of truncated onto another id, and an out-of-range
  `msg_type`, `piece` or `total_size` is an error instead of a truncation.
- `src/peer/message.rs`: `Message::decode` truncates to the frame length
  before decoding, so bytes after the frame can no longer make `length - 9`
  and its siblings wrap. The transport always passed exact frames; direct
  callers did not have to.

Hybrid torrents, 2026-10-25: `src/metainfo/torrent.rs` counted a hybrid's
expected v1 pieces from the unpadded v2 total, so any hybrid whose first
file was not piece-aligned (including ones rbit's own builder makes) failed
with "hybrid: v1 piece count doesn't match total length". It now sums each
non-empty file's piece count, matching the padded v1 stream.
`src/metainfo/builder.rs` padded the last file too when hashing v1 pieces
of a hybrid, while its v1 file list (correctly) has no trailing pad file,
so the final piece hash covered bytes the torrent does not contain.

DHT, 2026-10-25: `src/dht/server.rs` capped peers per info hash at 1000 but
not the number of info hashes; anyone can obtain a token with get_peers and
announce_peer arbitrary hashes. The store now keeps at most 1024 info
hashes, evicting expired ones first and refusing new ones when all are live.
`get_peers` waited 15 s per query and up to 10 waves, so a lookup could take
150 s, and each wave asked only the nodes the previous wave returned. It now
waits 4 s per query, asks the 8 closest unqueried nodes of a shortlist of
every node seen, and stops at 200 peers.

Performance, 2026-10-24: `src/peer/transport.rs` sets `TCP_NODELAY` on
every peer stream. Without it a pipelined Request waited for the delayed ACK
of the previous one (about 40 ms on Linux); the 32 MiB loopback scenario went
from about 5.3 s to under 0.1 s. Evidence in
perf-results/2026-10-24-write-path/.

Peer-wire encryption, 2026-10-25 (BitTorrent Message Stream Encryption,
the Vuze/libtorrent MSE/PE spec):

- `src/peer/mse.rs` (new, `pub mod mse` in `src/peer.rs`): 768-bit
  Diffie-Hellman with the spec prime and generator 2, RC4 with the first
  1024 keystream bytes dropped, SHA-1 key and req1/req2/req3 derivation,
  `initiate` (outgoing; sends an empty IA, so the BitTorrent handshake
  follows MSE) and `accept` (incoming; detects a plaintext
  `\x13BitTorrent protocol` prefix without consuming it, otherwise routes
  by `HASH('req2', SKEY)` through a caller lookup). Resynchronisation reads
  are bounded by the spec (PadA/PadB at most 512 bytes) and a handshake
  never buffers more than `MAX_HANDSHAKE_BYTES` (1176). `MseStream`
  replays bytes the handshake read ahead, encrypts writes into a bounded
  buffer drained on the next write or `flush`, and decrypts only the IA
  when plaintext was selected. `EncryptionPolicy` is PreferRc4 /
  RequireRc4 / PlaintextOnly. Re-exported from `peer`: `EncryptionPolicy`,
  `MseStream`, `PeerStream`, `BoxedPeerStream`.
- `src/peer/transport.rs`: `PeerTransport<S = TcpStream>` is generic over
  any `AsyncRead + AsyncWrite + Unpin` stream; `PeerTransport::new` (TCP,
  sets `TCP_NODELAY`) and `peer_addr` stay on `PeerTransport<TcpStream>`,
  `from_stream` is new, and `into_inner` returns `S`. Every handshake and
  message write is now followed by `flush` (a no-op on TCP) inside the
  same 30 s write timeout.
- `src/peer/connection.rs`: `PeerConnection` owns a boxed `PeerStream`.
  New `connect_with_stream` / `accept_with_stream` take an `MseStream` and
  the remote address; `connect`, `connect_with_mode`, `accept` and
  `accept_with_mode` behave as before (TCP, `TCP_NODELAY`, plaintext).
  New `is_encrypted()`. The handshake code is shared, not changed.
- `Cargo.toml`: dependency `num-bigint = "0.4"` (pure Rust).

Nothing else is modified. Upgrading rbit means re-applying exactly these
hunks, or checking that upstream fixed the same inputs.
