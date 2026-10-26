# Parser fuzzing

Ten libFuzzer targets over the parsers that bytes from trackers, peers,
`.torrent` files, pasted text and session files reach:

| Target | Production entry points |
| --- | --- |
| `tracker_response` | `announce::parse_http_response`, `announce::parse_announce_body` |
| `udp_tracker_response` | `udp_announce::parse_reply` (IPv4 and IPv6 peer formats), `udp_announce::sanitize_message` |
| `metainfo` | `parse_torrent`, `rbit::Metainfo::from_bytes`, `Driver::magnet_from_torrent_bytes`, the `d4:info…e` envelope |
| `magnet_and_tracker_url` | `parse_magnet`, `discovery::parse_tracker_url`, `redact_tracker_url`, `percent_encode` |
| `peer_wire` | rbit `Handshake`, `Message` (with re-encode round trip), `ExtensionHandshake`, `ExtensionMessage`, `MetadataMessage` |
| `bencode` | `rbit::decode` with an encode/decode round trip |
| `persist_records` | `persist::decode_record`, `decode_resume`, `decode_settings`, `decode_ui`, each with a round trip |
| `mse_handshake` | rbit `peer::mse::accept` (the incoming MSE/PE handshake) over an in-memory pipe: raw remote bytes, or a real Diffie-Hellman initiator whose post-exchange plaintext is fuzzed |
| `pex_and_dht` | `pex::parse_added` with an encode round trip, rbit `DhtMessage::parse` |

```sh
fuzz/run.sh 60            # replay regressions/, then 60 s per target
fuzz/run.sh 300 bencode   # one target
```

Needs `cargo-fuzz` and a nightly toolchain. Corpora and crash artifacts go
to `.agent/scratch/fuzz` (override with `OUT=`). `seeds/` holds the hand-made
starting inputs and `regressions/` every input that once crashed; both are
tracked. A new crash gets its input copied into `regressions/<target>/`, a
fix, and a stable-toolchain test in
`integration/rbit-adapter/tests/parser_regressions.rs` or next to the parser.
This workspace builds C++ (libFuzzer); see docs/DEPENDENCY-AUDIT.md.
