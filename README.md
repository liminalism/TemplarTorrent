# TemplarTorrent

A small Linux desktop client for BitTorrent and for ordinary `http://` and
`https://` file downloads, written in Rust with a liminalkit (formerly
PixelKit) window and the in-tree rbit 0.2.2 fork for protocol primitives. One process, one window;
closing the window stops every transfer. Windows is optional per the
2026-10-15 scope change and is not a completion gate.

**Status: a working client with open gates.** The window adds magnets,
`.torrent` files and URLs, downloads and seeds torrents with hashing and file
storage, downloads files over HTTP(S) with parallel ranges and resume, enforces
global limits, persists and restores the session, and removes jobs with a safe
default. A public Ubuntu ISO has been downloaded and verified from the Internet
swarm. The remaining torrent gates are listed below; the file-download path
has been proven on localhost fixtures only. Internet transfers are opt-in and
use content the user names.

Start with **DESIGN.md** (section 11 covers file downloads). **AGENTS.md** is
the coding-agent contract, **docs/INTEGRATION.md** the acceptance sequence,
**docs/PERFORMANCE.md** the measurement protocol, **docs/DEPENDENCY-AUDIT.md**
the native-source audit, and **docs/BROWSER-INTEGRATION.md** a review of what
catching browser downloads would take. Project knowledge is an AKR ledger in
`.akr/`; `docs/generated/` is its build output and is not hand-edited.

## Toolchain and dependencies

Rust 1.99.0, edition 2024, pinned by `rust-toolchain.toml`. `cargo build`
builds the client `templartorrent` and the policy crates; the policy crates
(`crates/`) have no third-party dependencies. The fixture preview
(`integration/pixelkit-preview`) and the fuzz targets (`fuzz/`) are separate
workspaces.

The GUI takes the liminalkit crates from
`https://github.com/liminalism/liminalkit` (main branch); the exact commit
is held by `Cargo.lock`, and a bump is a reviewed lockfile change. The rbit
adapter pins
`rbit = "=0.2.2"`, resolved through `[patch.crates-io]` to the fork at
`vendor/rbit`: identical source with `reqwest` behind an off-by-default
`http-client` feature, so the default graph carries no reqwest or ring. Tracker
announces and file downloads use the application's own bounded transport
(`integration/rbit-adapter/src/announce.rs`, `integration/rbit-adapter/src/http/`)
over rustls 0.23.46 with its aws-lc-rs provider and the webpki roots. aws-lc is
C: the strict all-Rust claim does not cover TLS. See
`vendor/rbit/TEMPLARTORRENT-FORK.md` and `docs/DEPENDENCY-AUDIT.md`.

## Commands

```sh
cargo build --release                 # target/release/templartorrent
cargo test --workspace --all-targets  # policy, engine, storage, adapter, client
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p templartorrent
cargo run -p templartorrent -- --geoip-db /path/to/GeoLite2-Country.mmdb
cargo run -p templartorrent -- --allow-local-urls http://192.168.1.10/file.iso
cargo test -p templartorrent-rbit-adapter --test http_download   # loopback HTTP fixtures
cargo run --release -p templartorrent-bench -- --pieces 100000 --iterations 500
cargo check --manifest-path integration/pixelkit-preview/Cargo.toml
```

`templartorrent --help` lists the flags. A second launch forwards each magnet,
`.torrent` path or URL to the running instance, which opens the Add dialog.
The font is auto-detected from common system paths with a fontconfig fallback;
`--font` overrides it. No fonts are shipped.

Two tests need things a container may not have: the IPv6 loopback transfer
needs `::1`, and the clipboard paste test needs an X11 or Wayland display.
Both are environmental, and the rest of the suite runs without them.

## Using it

**Torrents.** Add a magnet or a `.torrent`, choose a folder (the folder
browser is built in), confirm. Jobs show progress, rates, peers and state;
the inspector has Details, Files (order and selection), Peers (country, client,
Prefer/Normal/Avoid/Block) and Trackers (reannounce with cooldown). Limits
cover job slots, upload peer slots, connections, decimal kB/s in each direction
and the payload memory budget. Settings cover local tracker addresses, the
public tracker list, the listen port, MSE/PE encryption and opt-in router
port mapping.

**File downloads.** Paste one `http(s)://` URL, or up to 16 separated by
spaces, into the same field; an optional SHA-256 is checked before the job
completes. A server that answers a Range request with `206` is downloaded in
up to 4 parallel parts (hard cap 8), each spending from the same payload budget
and download limit as peers; otherwise one connection. Resume, after a pause,
a cancel, a restart or a dropped connection, happens only while the server's
ETag or Last-Modified still names the same file; otherwise the job stops with
the reason and keeps `<name>.part`. A connection is dropped after 30 s without
data and retried; there is no total timeout. Unknown sizes show as `—`. Bytes
go to a temporary sibling, are synced, then renamed; nothing existing is
overwritten. Local, private and link-local addresses are refused unless
`--allow-local-urls` or the Settings checkbox is on.

**Removal.** The remove dialog's data checkbox is unchecked by default. With it
checked, only the job's own files are deleted, through a handle-relative,
no-follow path that refuses symlinks, hardlinks and swapped inodes, and the
destination folder stays.

**Persistence.** Jobs, limits, settings, window size and split ratio live
under `$XDG_DATA_HOME/templartorrent/session` (default
`~/.local/share/templartorrent/session`), written atomically. Torrents are
rechecked on launch unless a trusted resume bitmap says otherwise; URL jobs
resume from offsets persisted only after the file was synced.

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

Known countries also get a small flag beside the code, from an embedded
atlas of the MIT-licensed lipis/flag-icons artwork (see
`integration/templartorrent/assets/flags/`).

## Workspace

| Crate | Role |
|---|---|
| `crates/templartorrent-core` | Dependency-free policy: model, file-order sequence, picker, token bucket, payload budget, tracker clock |
| `crates/templartorrent-engine` | Headless session engine: leases, arbiter, peer actors, bounded workers |
| `crates/templartorrent-storage` | Validated file storage, resume identity, Linux handle-relative deletion |
| `crates/templartorrent-simd` | Runtime-dispatched AVX2 eligibility kernel with a scalar oracle |
| `crates/templartorrent-bench` | Deterministic microbenchmarks (JSONL) |
| `integration/rbit-adapter` | The rbit driver (peers, trackers, DHT, PEX, MSE, port mapping) and the HTTP download engine |
| `integration/templartorrent` | The liminalkit client: window, dialogs, worker thread, session store, activation |
| `integration/pixelkit-preview` | Labeled layout fixture on liminalkit, a separate workspace (directory name kept) |
| `fuzz/` | libFuzzer targets for every parser remote or on-disk bytes reach |
| `vendor/rbit` | The rbit 0.2.2 fork |

## Done versus open

| Area | Working | Still open |
|---|---|---|
| Torrent formats | v1, hybrid through the v1 view with BEP 47 pad files never written, up to 4096 files, sequence-group detection (episodes, discs and tracks, parts, archive volumes) with strict ranks for a clear group and rarest-first for everything else, sidecars after their member | Pure v2 (refused with a reason) |
| Discovery | HTTP(S) and UDP announces per address family, DHT get_peers and announce_peer, PEX, MSE/PE with plaintext fallback, dual-stack listen port, opt-in NAT-PMP/UPnP on the default gateway | Port mapping on a real router (loopback fixtures only), uTP, BEP 19 webseeds |
| Engine | Magnet and .torrent transfers, BEP-9 metadata from up to four peers, hashing, seeding, global budget and rate authority, a fault-injected power-loss test of the batched sync | Rate limits charging real socket buffers and I/O |
| File downloads | Ranged parallel parts on the shared budget and rate, redirect cap, address admission, resume identity, idle cut, SHA-256, atomic placement, persistence and restore, proven on loopback fixtures | Request headers for browser-captured downloads; an Internet transfer of user-named content |
| UI | Native liminalkit panes, Add/Limits/Remove/Settings dialogs, folder browser, clipboard paste, Details/Files/Peers/Trackers, Retry/Cancel for URL jobs, peer names from the extension handshake or the BEP 20 peer id | Flag atlas |
| Storage and removal | Validated paths opened handle-relative with O_NOFOLLOW at every level, no silent overwrite, Linux no-follow deletion with inode identity and hardlink refusal, forced-kill restart recheck, fault-injected power-loss resume | Windows reparse-aware deletion (optional) |
| Persistence | Per-job records, resume bitmaps trusted only with the durability barrier and matching file identity, URL records with synced offsets, settings, window geometry | Pieces written after the last checkpoint are rechecked |
| Performance | Scalar/AVX2 kernel, bucketed piece index, loopback write-path before/after, Internet ISO run | Large piece-count end-to-end scenario, UI first-frame and RSS evidence |

All executable application source here is Rust. This is **not** a claim that
the TLS dependency graph contains no C or assembly: rustls uses aws-lc. See
the dependency audit. No release claim until the open gates above have
evidence in the AKR ledger.
