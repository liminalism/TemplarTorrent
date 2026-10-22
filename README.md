# TemplarTorrent

Windows/Linux BitTorrent client design and Rust reference workspace, prepared 2026-10-09.

**Status: implementation handoff, not a finished torrent client.** The archive contains
an application specification, a substantial dependency-free policy core, an isolated
AVX2 kernel, deterministic microbenchmarks, a PixelKit layout preview, and an rbit
0.2.2 adapter boundary. The preview uses labeled fixtures and does not download.
There is no completed session engine, live UI wiring, destructive deletion executor,
GeoIP/flag asset bundle, or packaged executable in this delivery.

Start with **DESIGN.md**. The implementation order is in **AGENTS.md** and
**docs/INTEGRATION.md**. **docs/PERFORMANCE.md** defines repeatable measurement and
acceptance criteria. **docs/DEPENDENCY-AUDIT.md** records the strict Rust/TLS blocker.

## Toolchain and dependencies

Rust 1.99.0, edition 2024. `cargo build` from this directory builds the
PixelKit client `templartorrent` and the policy crates. The policy crates
themselves have no third-party dependencies. The fixture preview stays a
separate workspace and is not part of that default build.

`cargo build --release` produces the GUI at `target/release/templartorrent`.
It opens a window with no arguments: the font is auto-detected from common
system paths with a fontconfig fallback (`--font` overrides it).

The GUI preview pins all PixelKit crates to:
`614c9c6e84ce3cd0bc6c81988f636b493b30a497`.
The rbit adapter pins `rbit = "=0.2.2"`, resolved through `[patch.crates-io]`
to the in-tree fork at `vendor/rbit`: identical source with `reqwest` behind an
off-by-default `http-client` feature, so the default graph carries no
rustls/ring. HTTP tracker announces use the application's own bounded
transport (`integration/rbit-adapter/src/announce.rs`); `https://` trackers
are refused with a visible reason until a reviewed pure-Rust TLS stack exists.
See `vendor/rbit/TEMPLARTORRENT-FORK.md` and `docs/DEPENDENCY-AUDIT.md`.

Dependency resolution, Cargo.lock generation and compilation were **not performed**
in the authoring environment: `rustc` and `cargo` were unavailable, and container
network resolution was unavailable. Public API documentation and repository source
were inspected; build manifests and local archive structure were checked. Do not
interpret the presence of tests or CI as evidence that they passed.

## Commands for the implementation machine

```sh
rustup toolchain install 1.99.0 --profile minimal --component rustfmt --component clippy
cargo fmt --all
cargo build
cargo test
cargo clippy --workspace --all-targets
cargo run -p templartorrent
cargo run -p templartorrent -- --geoip-db /path/to/GeoLite2-Country.mmdb
cargo run --release -p templartorrent-bench -- --pieces 100000 --iterations 500
cargo test --workspace -p templartorrent-rbit-adapter
cargo check --manifest-path integration/pixelkit-preview/Cargo.toml
cargo run --release --manifest-path integration/pixelkit-preview/Cargo.toml -- --font /path/to/static-TrueType.ttf
```

## Peer country lookup

The Peers tab resolves peer countries locally from a user-supplied MaxMind
DB (GeoLite2 Country or City), read with the pure-Rust `maxminddb` crate.
No database is bundled. Resolution order is `--geoip-db FILE`, then
`$TEMPLARTORRENT_GEOIP_DB`, then
`~/.local/share/templartorrent/GeoLite2-Country.mmdb` (or the City file, or
`/usr/share/GeoIP/`, `/var/lib/GeoIP/`). Without a database, peers show
`--` and downloads are unaffected.

```sh
mkdir -p ~/.local/share/templartorrent
ln -s /path/to/GeoLite2-Country.mmdb ~/.local/share/templartorrent/
```

GeoLite2 data is a MaxMind product under its own license and attribution
terms; check them before redistributing any database file.

On Windows, supply a supported installed `.ttf`, for example the path to a static
TrueType system UI face. The preview's font loader is size-bounded and retains one
font for the process lifetime. No fonts are included. It does not silently download
fonts or embed PixelKit's optional test fonts.

The preview supports dragging the horizontal splitter, switching inspector tabs,
selecting a fixture with the Torrent 1/2 selector or numeric keys, scrolling files,
`P` to pause/resume the second fixture and exercise the title, `C` to mark the next
synthetic piece complete and exercise file promotion, and `Q` to exit. The Files
tab is an independent ordering fixture, not the byte totals of either summary row. This is a
layout/policy demonstrator, not a connectivity test or production UI.

## Included versus specified

| Area | Included code | Still to implement/validate |
|---|---|---|
| File order | Natural comparator, u32 ranks, piece overlap, promotion/invalidation | Sequence-group detection, v2 map, real lease orchestration |
| Memory/rates | Atomic byte permits, decimal token bucket | Charge actual buffers, socket admission, actual I/O enforcement |
| Trackers | Privacy gate, interval/backoff state | HTTP/UDP actors, discovery arbitration, feed validation/cache |
| Engine | Real rbit type wrappers and bitfield conversion | Magnets-to-payload transfer loop, metadata exchange, hashing/storage |
| UI | Native PixelKit split panes and fixture inspector | Add/limits/remove dialogs, real commands/acks, file browser |
| Peers | Typed state, local Prefer/Avoid/Block applied to upload slots, pipelines, retention and reconnection, MMDB country lookup | Automatic scoring, optimistic rotation, peer-ID client fallback, flag atlas |
| Removal | Default-safe intent, Linux handle-relative no-follow deletion with inode identity and hardlink refusal, forced-kill restart recheck | Windows reparse-aware deletion, crash journal |
| Persistence | Per-job records, settings, window size and split ratio under `$XDG_DATA_HOME/templartorrent/session`, atomic writes, recheck on launch | Resume bitmap/journal |
| Performance | Scalar/AVX2 kernel, bucketed `(rank, availability)` piece index 65x to 170x below the O(P) oracle pick, microbench JSONL, localhost magnet-startup samples | Large piece-count end-to-end scenario, UI first-frame/RSS evidence, AVX2 use in the picker (no measured win) |

All executable application/reference source supplied here is Rust. This is **not** a
claim that stock rbit's transitive TLS dependency graph contains no C or assembly;
see the dependency audit. Neither integration directory should be represented as
release-ready until its explicit gates have passed.
