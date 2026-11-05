//! Render the lipis/flag-icons 4x3 SVGs into one PNG atlas plus an index.
//!
//! Usage: `flag-atlas <flag-icons checkout> <output dir>`
//!
//! Writes `flags-4x3.png` (cells of CELL_W x CELL_H, COLUMNS per row, in
//! index order) and `flags-4x3.txt` (one upper-case ISO 3166-1 alpha-2 code
//! per line, sorted). Only two-letter file names are taken; regional and
//! organisational flags (`gb-eng`, `arab`, ...) and the `xx` placeholder are
//! skipped because GeoLite2 never reports them. Each flag is rendered at
//! SUPERSAMPLE times the cell size and box-filtered down in premultiplied
//! space, which keeps thin emblems legible at 32x24.
//!
//! The constants must match `integration/templartorrent/src/flags.rs`.

use resvg::{tiny_skia, usvg};
use std::path::PathBuf;

const CELL_W: u32 = 32;
const CELL_H: u32 = 24;
const COLUMNS: u32 = 16;
const SUPERSAMPLE: u32 = 4;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let (Some(source), Some(output)) = (args.next(), args.next()) else {
        eprintln!("usage: flag-atlas <flag-icons checkout> <output dir>");
        std::process::exit(2);
    };
    let flags = PathBuf::from(source).join("flags").join("4x3");
    let output = PathBuf::from(output);

    let mut codes: Vec<String> = std::fs::read_dir(&flags)
        .unwrap_or_else(|e| panic!("read {}: {e}", flags.display()))
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".svg")?;
            (stem.len() == 2 && stem.bytes().all(|b| b.is_ascii_lowercase()) && stem != "xx")
                .then(|| stem.to_owned())
        })
        .collect();
    codes.sort();
    assert!(!codes.is_empty(), "no flags found in {}", flags.display());

    let rows = (codes.len() as u32).div_ceil(COLUMNS);
    let mut atlas = tiny_skia::Pixmap::new(COLUMNS * CELL_W, rows * CELL_H).expect("atlas");
    let options = usvg::Options::default();
    let (big_w, big_h) = (CELL_W * SUPERSAMPLE, CELL_H * SUPERSAMPLE);

    for (index, code) in codes.iter().enumerate() {
        let path = flags.join(format!("{code}.svg"));
        let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let tree = usvg::Tree::from_data(&data, &options)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let size = tree.size();
        let mut big = tiny_skia::Pixmap::new(big_w, big_h).expect("cell");
        let transform = tiny_skia::Transform::from_scale(
            big_w as f32 / size.width(),
            big_h as f32 / size.height(),
        );
        resvg::render(&tree, transform, &mut big.as_mut());

        let cx = (index as u32 % COLUMNS) * CELL_W;
        let cy = (index as u32 / COLUMNS) * CELL_H;
        let atlas_w = atlas.width();
        let src = big.pixels();
        let dst = atlas.pixels_mut();
        let n = SUPERSAMPLE * SUPERSAMPLE;
        for y in 0..CELL_H {
            for x in 0..CELL_W {
                let mut sum = [0u32; 4];
                for sy in 0..SUPERSAMPLE {
                    for sx in 0..SUPERSAMPLE {
                        let p =
                            src[((y * SUPERSAMPLE + sy) * big_w + x * SUPERSAMPLE + sx) as usize];
                        sum[0] += u32::from(p.red());
                        sum[1] += u32::from(p.green());
                        sum[2] += u32::from(p.blue());
                        sum[3] += u32::from(p.alpha());
                    }
                }
                let avg = sum.map(|s| ((s + n / 2) / n) as u8);
                let alpha = avg[3];
                let pixel = tiny_skia::PremultipliedColorU8::from_rgba(
                    avg[0].min(alpha),
                    avg[1].min(alpha),
                    avg[2].min(alpha),
                    alpha,
                )
                .expect("premultiplied");
                dst[((cy + y) * atlas_w + cx + x) as usize] = pixel;
            }
        }
    }

    std::fs::create_dir_all(&output).expect("output dir");
    let png = atlas.encode_png().expect("encode atlas");
    std::fs::write(output.join("flags-4x3.png"), &png).expect("write atlas");
    let mut index = String::new();
    for code in &codes {
        index.push_str(&code.to_ascii_uppercase());
        index.push('\n');
    }
    std::fs::write(output.join("flags-4x3.txt"), index).expect("write index");
    println!(
        "{} flags, atlas {}x{}, {} bytes",
        codes.len(),
        atlas.width(),
        atlas.height(),
        png.len()
    );
}
