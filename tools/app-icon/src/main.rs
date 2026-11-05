//! Render an SVG into a multi-size Windows `.ico` (PNG-compressed entries,
//! Windows Vista and later).
//!
//! Usage: `app-icon <input.svg> <output.ico>`
//!
//! Each size is rendered from the vector at that size, not scaled from a big
//! bitmap, so small icons keep their edges. A non-square drawing is centred
//! on a transparent square. Windows picks from these sizes: 16/20/24/32 for
//! title bars and small taskbar buttons (100%-200% scaling), 32/40/48/64 for
//! the taskbar and Alt-Tab, and 96/128/256 for desktop and Explorer views.

use resvg::{tiny_skia, usvg};

const SIZES: [u32; 10] = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256];

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("usage: app-icon <input.svg> <output.ico>");
        std::process::exit(2);
    };
    let data = std::fs::read(&input).unwrap_or_else(|e| panic!("{input}: {e}"));
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default())
        .unwrap_or_else(|e| panic!("{input}: {e}"));
    let drawing = tree.size();
    let mut images = Vec::new();
    for size in SIZES {
        let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("pixmap");
        let side = size as f32;
        let scale = (side / drawing.width()).min(side / drawing.height());
        let dx = (side - drawing.width() * scale) / 2.0;
        let dy = (side - drawing.height() * scale) / 2.0;
        let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy);
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        images.push((size, pixmap.encode_png().expect("encode png")));
    }
    // ICONDIR, then one ICONDIRENTRY per image, then the image data.
    let mut ico = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, png) in &images {
        let side = if *size >= 256 { 0 } else { *size as u8 };
        ico.extend_from_slice(&[side, side, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in &images {
        ico.extend_from_slice(png);
    }
    std::fs::write(&output, &ico).unwrap_or_else(|e| panic!("{output}: {e}"));
    println!(
        "{} sizes {:?}, {} bytes -> {output}",
        images.len(),
        SIZES,
        ico.len()
    );
}
