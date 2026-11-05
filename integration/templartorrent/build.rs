//! Build-time assets:
//! - inflate the bundled DB-IP country database into OUT_DIR so `geo.rs` can
//!   embed it uncompressed (see assets/geo/README.md);
//! - on Windows (MSVC), link the program icon into `templartorrent.exe` as a
//!   resource (see assets/icon/README.md).

use std::io::Read;
use std::path::{Path, PathBuf};

const GEO_BUNDLED_FILE: &str = "assets/geo/dbip-country-lite-2026-11.mmdb.gz";
const ICON_FILE: &str = "assets/icon/templartorrent.ico";

/// Resource id of the icon group. Explorer shows the first group; the
/// window loads this id at runtime (`foreground.rs`).
const ICON_GROUP_ID: u16 = 1;

fn main() {
    println!("cargo:rerun-if-changed={GEO_BUNDLED_FILE}");
    println!("cargo:rerun-if-changed={ICON_FILE}");
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    inflate_geo(&out);
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if windows && msvc {
        let res = out.join("templartorrent-icon.res");
        let ico = std::fs::read(ICON_FILE).expect("read the program icon");
        std::fs::write(&res, icon_res(&ico)).expect("write the icon resource");
        // The MSVC linker takes a compiled .res file as an input directly,
        // so no resource compiler is needed. Binaries only: the library and
        // test executables carry no icon.
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else if windows {
        println!("cargo:warning=program icon skipped: only the MSVC linker is wired up");
    }
}

fn inflate_geo(out: &Path) {
    let compressed = std::fs::read(GEO_BUNDLED_FILE).expect("read the bundled geo database");
    let mut mmdb = Vec::with_capacity(16 * 1024 * 1024);
    flate2::read::GzDecoder::new(compressed.as_slice())
        .take(64 * 1024 * 1024)
        .read_to_end(&mut mmdb)
        .expect("inflate the bundled geo database");
    std::fs::write(out.join("dbip-country-lite.mmdb"), mmdb).expect("write the geo database");
}

/// Convert a `.ico` file into a 32-bit `.res` file: one RT_ICON resource
/// per image (ids 1..=n) and one RT_GROUP_ICON ([`ICON_GROUP_ID`]) listing
/// them, which is what `rc.exe` produces for an `ICON` statement.
fn icon_res(ico: &[u8]) -> Vec<u8> {
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    let u16_at = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes([ico[at], ico[at + 1], ico[at + 2], ico[at + 3]]);
    assert!(
        ico.len() >= 6 && u16_at(0) == 0 && u16_at(2) == 1,
        "not an .ico file"
    );
    let count = usize::from(u16_at(4));
    assert!(
        count > 0 && ico.len() >= 6 + 16 * count,
        "truncated .ico directory"
    );

    let mut res = Vec::new();
    // A .res file starts with an empty resource entry.
    entry(&mut res, 0, 0, 0, &[]);
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for index in 0..count {
        let at = 6 + 16 * index;
        let size = u32_at(at + 8) as usize;
        let offset = u32_at(at + 12) as usize;
        let image = ico
            .get(offset..offset + size)
            .expect("an .ico image lies outside the file");
        let id = index as u16 + 1;
        entry(&mut res, RT_ICON, id, 0x1010, image);
        // GRPICONDIRENTRY: the ICONDIRENTRY's first 12 bytes, then the id.
        group.extend_from_slice(&ico[at..at + 12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut res, RT_GROUP_ICON, ICON_GROUP_ID, 0x1030, &group);
    res
}

/// Append one resource with an ordinal type and name, English (US), padded
/// to a 4-byte boundary as the format requires.
fn entry(res: &mut Vec<u8>, kind: u16, name: u16, flags: u16, data: &[u8]) {
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes());
    res.extend_from_slice(&0xFFFFu16.to_le_bytes());
    res.extend_from_slice(&kind.to_le_bytes());
    res.extend_from_slice(&0xFFFFu16.to_le_bytes());
    res.extend_from_slice(&name.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes());
    res.extend_from_slice(&flags.to_le_bytes());
    let language: u16 = if kind == 0 { 0 } else { 0x0409 };
    res.extend_from_slice(&language.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes());
    res.extend_from_slice(data);
    while !res.len().is_multiple_of(4) {
        res.push(0);
    }
}
