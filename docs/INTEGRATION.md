# Integration handoff and acceptance sequence

## A. Establish the build before adding behavior

Read README.md, DESIGN.md, DEPENDENCY-AUDIT.md and AGENTS.md. Install Rust 1.99.0.
Run rustfmt, core/SIMD tests and clippy; then build each standalone integration.
Resolve APIs against the pinned revisions rather than moving the pins blindly.
Record compiler errors and fixes. Generate real lockfiles. Do not claim passing
CI because the YAML exists. There was no Rust compiler in the authoring container.

## B. A real local v1 transfer before the polished UI

Create a headless `templartorrent-engine` using the existing core policies and rbit
adapter. It must accept an explicit source/destination and a test-only injected
tracker/peer route. Implement metadata acquisition, raw-infohash verification,
bounded file mapping, peer handshake/extension negotiation, framed message handling,
block leases, cryptographic verification, writes, completion and upload/seed. Use
owned generated data in a temporary test root; compare final bytes to the fixture.

The session must not be a bag of unbounded Tokio tasks. One coordinator serializes
piece lease decisions, one session arbiter controls global jobs/peers/bytes, peer
actors own sockets, and disk/hash workers are bounded. Events identify torrent,
connection and generation, including those received after cancellation.

## C. Actual rbit seams that need careful treatment

* `Metainfo::from_bytes` and `MagnetLink::parse` are real APIs. Enforce allocation,
  nesting/path and capability limits before trusting output. Magnet metadata still
  requires BEP-9 orchestration and hash checking; parsing the URI does not get files.
* `PieceManager::new` returns an `Arc`. Choose a piece externally; its reviewed u8
  priority pickers do not rank positive values. Use `start_piece`, obtain bounded
  block work, and record leases/pending blocks. Avoid cloning the whole bitfield on
  every request. Review `get_block_requests` allocation for large pieces; add a
  bounded/iterator API if necessary instead of hiding an unbounded vector.
* Validate received piece index, offset, exact requested length, live lease,
  requester identity and memory admission before `receive_block`. Hash verification
  and disk success precede the core `mark_verified`. Invalid blocks never make a
  file complete. A piece assembled from multiple peers needs careful blame handling.
* `set_interested` / `set_choking` change local state; send the corresponding wire
  message too. Handle remote Bitfield/Have and extension handshakes in the actor;
  do not assume counters/state in `receive` represent a complete peer state machine.
* Audit framing cancellation safety before `tokio::select!` repeatedly cancels a
  receive future. No claim is made here that this transport is cancellation-safe.
  Use a persistent bounded frame decoder, or patch in split read/write ownership;
  when cancelling an uncertain partially-read transport, discard that connection.
  Full-duplex uploads and prompt control traffic must work while receiving data.
* Extension IDs are directional: send with the remote-advertised ID; dispatch
  incoming IDs according to our advertised mapping. Extension 0 is the handshake.
  Metadata_size, reqq, hash proof sizes and bitfield sizes are hostile input.
* Announce adapters return a real rbit response. The production transport still
  needs bounded response reading, timeout/redirect rules, provenance checks and
  clock/concurrency admission. Do not mistake a post-allocation length check for
  a network memory bound. UDP uses the same policy but its own rbit transport.
* Pure v2 is not v1 concatenation. Implement its piece map/Merkle proof rules with
  explicit fixtures before exposing support. Negotiation of v2 capabilities is not
  proof of correct v2 disk writes or selection.

Sources: rbit 0.2.2 docs and `peer/piece.rs`, `peer/connection.rs`, `peer/message.rs`;
BEP 3, 9, 10, 27 and 52. Exact reviewed references are in DESIGN.md.

## D. Wire the native application

Promote the PixelKit preview into an application without shipping fixture rows in
live mode. Add real typed commands and acknowledgments, add/folder/limits/remove
dialogs, persistent split ratio, stable table IDs, virtual file/peer rows and focus
semantics. Wire one complete path at a time: Add -> metadata -> real stats -> pause
-> resume -> seed -> remove without deletion -> safe explicit data deletion.

The preview intentionally has no active-looking fake Add/Remove buttons. Its file
progress demonstrator is independent synthetic state, not a real piece hasher. Do
not cite preview interactions as proof that any command reached an engine.

Windows clipboard, protocol activation and Linux X11/Wayland checks are required.
Deliver full committed text/IME once; don't call both character insertion paths.
Use local MMDB + licensed flag atlas and asynchronous visible-range queries. None
of those assets or integrations is already present in the preview.

## E. Safety, privacy and persistence before performance tuning

Implement no-follow/reparse-aware storage with controlled ownership and safe resume.
Run adversarial private-tracker fixtures and deletion races before approving public
network smoke tests. Add global rate/queue/peer limits and realistic counter tests.
Malformed metadata, host loss, full disk, path collisions, read-only folders and
shutdown during writes must yield explicit recoverable errors, not panic/silent loss.

## F. Optimize one measured bottleneck at a time

Run PERFORMANCE.md baselines, record raw JSONL and environment, then optimize.
First targets: UI startup gating, bounded buffers, duplicate metadata/bitmaps,
request pipeline stalls, file-handle churn and O(P) picker scans. Compare AVX2 against
scalar, then optionally wide. Never trade away hash verification or boundary tests
for a favorable throughput number. Keep core/engine runnable headlessly.
