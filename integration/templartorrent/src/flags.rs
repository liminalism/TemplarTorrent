//! Country flags beside each peer in the Peers tab.
//!
//! The artwork is lipis/flag-icons 7.6.0 (MIT; see
//! `assets/flags/LICENSE-flag-icons`), rasterised offline by
//! `tools/flag-atlas` into one 32x24-per-cell PNG atlas embedded in the
//! binary. Nothing is fetched at runtime and no SVG renderer ships: the atlas
//! is decoded with PixelKit's own PNG decoder the first time a flag is drawn,
//! resampled once to the size the row asks for, and then dropped.
//!
//! A flag is decoration only. The country code stays in the row text, and a
//! code with no flag (or no country at all) draws nothing.

use liminalkit_raster::{Bitmap, Fit, Painter, PreparedImage, Rect, png};
use std::sync::Mutex;

/// The atlas: `COLUMNS` cells per row, in [`INDEX`] order.
const ATLAS: &[u8] = include_bytes!("../assets/flags/flags-4x3.png");
/// Sorted upper-case ISO 3166-1 alpha-2 codes, one per line (`"US\n"`), so
/// entry `i` occupies bytes `3 * i .. 3 * i + 2`.
const INDEX: &str = include_str!("../assets/flags/flags-4x3.txt");

/// Atlas cell size in pixels; must match `tools/flag-atlas`.
pub const CELL_W: u32 = 32;
pub const CELL_H: u32 = 24;
const COLUMNS: u32 = 16;

/// Flags prepared for one drawing size. Replaced when the size changes
/// (a scale change), so memory is one size's worth: 256 flags at 16x12 is
/// about 200 KiB.
struct Prepared {
    width: u32,
    height: u32,
    images: Vec<PreparedImage>,
}

static PREPARED: Mutex<Option<Prepared>> = Mutex::new(None);

fn entries() -> usize {
    INDEX.len() / 3
}

fn entry(index: usize) -> [u8; 2] {
    let bytes = INDEX.as_bytes();
    [bytes[3 * index], bytes[3 * index + 1]]
}

/// The atlas slot for `code`, case-insensitively, if flag-icons has it.
fn slot(code: [u8; 2]) -> Option<usize> {
    let code = code.map(|b| b.to_ascii_uppercase());
    let (mut low, mut high) = (0, entries());
    while low < high {
        let mid = (low + high) / 2;
        match entry(mid).cmp(&code) {
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid,
            std::cmp::Ordering::Equal => return Some(mid),
        }
    }
    None
}

/// Whether a flag exists for `code`.
pub fn has_flag(code: [u8; 2]) -> bool {
    slot(code).is_some()
}

fn crop(atlas: &Bitmap, slot: usize) -> Bitmap {
    let x0 = (slot as u32 % COLUMNS) * CELL_W;
    let y0 = (slot as u32 / COLUMNS) * CELL_H;
    let mut cell = Bitmap::new(CELL_W, CELL_H);
    for y in 0..CELL_H {
        for x in 0..CELL_W {
            cell.pixels[(y * CELL_W + x) as usize] = atlas.pixel(x0 + x, y0 + y);
        }
    }
    cell
}

fn decode_atlas() -> Option<Bitmap> {
    let atlas = png::decode(ATLAS).ok()?;
    let rows = (entries() as u32).div_ceil(COLUMNS);
    (atlas.width == COLUMNS * CELL_W && atlas.height >= rows * CELL_H).then_some(atlas)
}

/// The full-resolution (`CELL_W` x `CELL_H`, straight alpha) flag for
/// `code`. Decodes the atlas on every call; drawing goes through
/// [`draw_flag`], which caches.
pub fn flag_bitmap(code: [u8; 2]) -> Option<Bitmap> {
    let slot = slot(code)?;
    Some(crop(&decode_atlas()?, slot))
}

/// Draw the flag for `country` scaled to fill `rect`. Returns whether a
/// flag was drawn; `None`, an unknown code or a zero-sized rect draw
/// nothing.
pub fn draw_flag(painter: &mut Painter<'_>, country: Option<[u8; 2]>, rect: Rect) -> bool {
    let Some(slot) = country.and_then(slot) else {
        return false;
    };
    if rect.w <= 0 || rect.h <= 0 || !painter.is_visible(rect) {
        return false;
    }
    let (width, height) = (rect.w as u32, rect.h as u32);
    let mut prepared = PREPARED.lock().unwrap_or_else(|poison| poison.into_inner());
    if !matches!(&*prepared, Some(p) if p.width == width && p.height == height) {
        // A failed decode leaves an empty set, so it is not retried per frame.
        let images = decode_atlas()
            .map(|atlas| {
                (0..entries())
                    .map(|slot| {
                        PreparedImage::from_bitmap(
                            &crop(&atlas, slot),
                            width,
                            height,
                            Fit::Stretch,
                            0.0,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        *prepared = Some(Prepared {
            width,
            height,
            images,
        });
    }
    let Some(image) = prepared.as_ref().and_then(|p| p.images.get(slot)) else {
        return false;
    };
    painter.draw_image(image, rect.x, rect.y);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use liminalkit_raster::WindowBuffer;

    #[test]
    fn index_is_sorted_fixed_width_codes() {
        assert_eq!(INDEX.len() % 3, 0);
        assert!(entries() > 200, "{} flags", entries());
        for i in 0..entries() {
            let code = entry(i);
            assert!(code.iter().all(u8::is_ascii_uppercase), "{code:?}");
            assert_eq!(INDEX.as_bytes()[3 * i + 2], b'\n');
            if i > 0 {
                assert!(entry(i - 1) < code, "index not sorted at {i}");
            }
        }
    }

    #[test]
    fn known_codes_have_flags_and_unknown_do_not() {
        for code in [*b"US", *b"DE", *b"GB", *b"us", *b"AD", *b"ZW"] {
            let flag = flag_bitmap(code).unwrap_or_else(|| panic!("no flag for {code:?}"));
            assert_eq!((flag.width, flag.height), (CELL_W, CELL_H));
            assert!(
                flag.pixels.iter().all(|p| p >> 24 > 0),
                "{code:?} flag has transparent pixels"
            );
        }
        for code in [*b"ZZ", *b"XX", *b"--", *b"\0\0"] {
            assert!(!has_flag(code), "{code:?}");
            assert!(flag_bitmap(code).is_none(), "{code:?}");
        }
    }

    #[test]
    fn germany_is_black_red_gold_top_to_bottom() {
        let flag = flag_bitmap(*b"DE").expect("DE");
        let rgb = |y: u32| flag.pixel(CELL_W / 2, y) & 0x00ff_ffff;
        assert_eq!(rgb(2), 0x000001);
        assert_eq!(rgb(CELL_H / 2), 0xff0000);
        assert_eq!(rgb(CELL_H - 3), 0xffcc00);
    }

    #[test]
    fn flag_lands_in_a_peer_row_and_unknown_draws_nothing() {
        const WHITE: u32 = 0xffffff;
        let mut buffer = WindowBuffer::new(120, 44);
        let mut painter = Painter::new(&mut buffer);
        painter.fill_rect(Rect::new(0, 0, 120, 44), WHITE);
        let row = Rect::new(0, 0, 120, 20);
        let flag = Rect::new(row.x + 2, row.y + 4, 16, 12);
        assert!(draw_flag(&mut painter, Some(*b"DE"), flag));
        assert!(!draw_flag(&mut painter, None, Rect::new(40, 4, 16, 12)));
        assert!(!draw_flag(
            &mut painter,
            Some(*b"ZZ"),
            Rect::new(60, 4, 16, 12)
        ));
        // Clipped away: nothing drawn, nothing prepared for it.
        painter.push_clip(Rect::new(0, 20, 120, 24));
        assert!(!draw_flag(
            &mut painter,
            Some(*b"US"),
            Rect::new(80, 4, 16, 12)
        ));
        painter.pop_clip();
        drop(painter);
        let at = |x: u32, y: u32| buffer.pixels[(y * buffer.width + x) as usize] & 0x00ff_ffff;
        // 1x scale: the 32x24 cell is area-averaged to 16x12.
        assert_eq!(at(10, 5), 0x000001, "black band");
        assert_eq!(at(10, 10), 0xff0000, "red band");
        assert_eq!(at(10, 14), 0xffcc00, "gold band");
        for y in 0..44 {
            for x in 0..120 {
                if !flag.contains(x as i32, y as i32) {
                    assert_eq!(at(x, y), WHITE, "pixel ({x},{y}) outside the flag changed");
                }
            }
        }
        // A second size replaces the prepared set (a scale change).
        let mut big = WindowBuffer::new(40, 30);
        let mut painter = Painter::new(&mut big);
        assert!(draw_flag(
            &mut painter,
            Some(*b"GB"),
            Rect::new(4, 3, 32, 24)
        ));
    }
}
