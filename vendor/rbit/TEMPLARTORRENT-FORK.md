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
governs). Nothing else is modified. TemplarTorrent announces to HTTP trackers through its
own bounded transport in `integration/rbit-adapter/src/announce.rs`. Upgrading
rbit means re-applying exactly these hunks.
