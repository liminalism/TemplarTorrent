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

Performance, 2026-10-24: `src/peer/transport.rs` sets `TCP_NODELAY` on
every peer stream. Without it a pipelined Request waited for the delayed ACK
of the previous one (about 40 ms on Linux); the 32 MiB loopback scenario went
from about 5.3 s to under 0.1 s. Evidence in
perf-results/2026-10-24-write-path/.

Nothing else is modified. Upgrading rbit means re-applying exactly these
hunks, or checking that upstream fixed the same inputs.
