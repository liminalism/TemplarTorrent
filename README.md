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

Rust 1.99.0, edition 2024. The core workspace has no third-party dependencies.
The two integrations are deliberately separate workspaces, so ordinary policy tests
neither resolve GUI/network dependencies nor need a display server.

The GUI preview pins all PixelKit crates to:
`614c9c6e84ce3cd0bc6c81988f636b493b30a497`.
The rbit adapter pins `rbit = "=0.2.2"`.

Dependency resolution, Cargo.lock generation and compilation were **not performed**
in the authoring environment: `rustc` and `cargo` were unavailable, and container
network resolution was unavailable. Public API documentation and repository source
were inspected; build manifests and local archive structure were checked. Do not
interpret the presence of tests or CI as evidence that they passed.

## Commands for the implementation machine

```sh
rustup toolchain install 1.99.0 --profile minimal --component rustfmt --component clippy
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets
cargo run --release -p templartorrent-bench -- --pieces 100000 --iterations 500
cargo test --manifest-path integration/rbit-adapter/Cargo.toml
cargo check --manifest-path integration/pixelkit-preview/Cargo.toml
cargo run --release --manifest-path integration/pixelkit-preview/Cargo.toml -- --font /path/to/static-TrueType.ttf
```

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
| Peers | Typed state and preference enum | Choker/request scoring, client fallback, offline geolocation/flags |
| Removal | Default-safe intent and lexical checks | Handle-relative storage/deletion, quiescence and crash recovery |
| Performance | Scalar/AVX2 kernel, microbench JSONL | End-to-end local swarm, actual RSS/startup/CPU evidence |

All executable application/reference source supplied here is Rust. This is **not** a
claim that stock rbit's transitive TLS dependency graph contains no C or assembly;
see the dependency audit. Neither integration directory should be represented as
release-ready until its explicit gates have passed.
