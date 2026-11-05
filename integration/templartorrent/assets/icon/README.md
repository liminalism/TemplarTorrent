# Program icon (Windows)

`templartorrent.ico` is rendered from `icons/vector-detail.svg` at 16, 20, 24,
32, 40, 48, 64, 96, 128 and 256 px (PNG-compressed entries). Each size is drawn
from the vector at that size, centred on a transparent square.

- `build.rs` turns it into a `.res` file and links it into `templartorrent.exe`
  (MSVC targets), so Explorer, desktop shortcuts and the Start menu show it.
- `src/window_icon.rs` loads the sizes Windows asks for at the window's DPI and
  sets them on the window: the small one in the title bar, the large one on the
  taskbar and in Alt-Tab. It reloads them when the window moves to a monitor
  with a different scale.

To regenerate after editing the SVG:

    cargo run --release --manifest-path tools/app-icon/Cargo.toml -- icons/vector-detail.svg integration/templartorrent/assets/icon/templartorrent.ico

Linux packaging does not use this icon yet.
