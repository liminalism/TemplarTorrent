# Country flag atlas

Embedded by `src/flags.rs` with `include_bytes!`/`include_str!` and drawn
beside each peer in the Peers tab. Nothing here is fetched at runtime.

## Provenance

- Artwork: [lipis/flag-icons](https://github.com/lipis/flag-icons) 7.6.0,
  commit `ac7e0eb3259eb62aa6100c435607e2fe03896a97` (2026-10-16), the
  `flags/4x3/*.svg` set.
- License: MIT, Copyright (c) 2013 Panayiotis Lipiridis. The full notice is
  in `LICENSE-flag-icons` and must ship with any binary distribution.
- Selection: every two-letter file name except the `xx` placeholder (256
  flags, ISO 3166-1 alpha-2 plus `EU` and `UN`). Regional and
  organisational flags (`gb-eng`, `es-ct`, `arab`, ...) are left out: the
  GeoLite2 lookup only ever reports two-letter codes.

## Files

| File | Contents |
| --- | --- |
| `flags-4x3.png` | 512x384 RGBA PNG, 16 columns of 32x24 cells in index order (109,880 bytes) |
| `flags-4x3.txt` | 256 upper-case codes, sorted, one per line, LF only (768 bytes) |

Cell `i` is at `x = (i % 16) * 32`, `y = (i / 16) * 24`. At runtime the
atlas is decoded once with PixelKit's PNG decoder and area-averaged to the
row's flag size (16x12 logical pixels, so 16x12 at 1x and 32x24 at 2x).

## Regenerating

`tools/flag-atlas` is a separate Cargo workspace using `resvg`; it is a
development tool only and never part of the shipped dependency graph.

```sh
git clone --depth 1 https://github.com/lipis/flag-icons /tmp/flag-icons
git -C /tmp/flag-icons log -1 --format=%H   # record it above
cargo run --release --manifest-path tools/flag-atlas/Cargo.toml -- \
    /tmp/flag-icons integration/templartorrent/assets/flags
cargo test -p templartorrent --lib flags
```

Each SVG is rendered at 4x the cell size (128x96) and box-filtered to 32x24
in premultiplied space. If the cell size or column count changes, change
`CELL_W`, `CELL_H` and `COLUMNS` in both `tools/flag-atlas/src/main.rs` and
`src/flags.rs`. Update the commit, version and byte counts above and the
flag graphics entry in `docs/DEPENDENCY-AUDIT.md`.
