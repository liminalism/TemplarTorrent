//! Client names from peer ids (BEP 20), used when a peer sends no extension
//! handshake `v` string. A guess from a peer id is a convention, not a fact:
//! any client may send any id, so the name is a label for the Peers tab and
//! never a policy input.

use templartorrent_core::model::clean_label;

/// Azureus-style two-character client codes. Kept to clients still seen on
/// public swarms plus the historical ones that explain odd ids.
const AZUREUS: &[(&[u8; 2], &str)] = &[
    (b"AG", "Ares"),
    (b"A~", "Ares"),
    (b"AR", "Arctic"),
    (b"AT", "Artemis"),
    (b"AZ", "Vuze"),
    (b"BB", "BitBuddy"),
    (b"BC", "BitComet"),
    (b"BE", "BitTorrent SDK"),
    (b"BF", "Bitflu"),
    (b"BG", "BTG"),
    (b"BI", "BiglyBT"),
    (b"BL", "BitComet Lite"),
    (b"BP", "BitTorrent Pro"),
    (b"BR", "BitRocket"),
    (b"BS", "BTSlave"),
    (b"BT", "BitTorrent"),
    (b"BW", "BitWombat"),
    (b"BX", "BittorrentX"),
    (b"CD", "Enhanced CTorrent"),
    (b"CT", "CTorrent"),
    (b"DE", "Deluge"),
    (b"DP", "Propagate"),
    (b"EB", "EBit"),
    (b"ES", "Electric Sheep"),
    (b"FC", "FileCroc"),
    (b"FD", "Free Download Manager"),
    (b"FG", "FlashGet"),
    (b"FL", "Folx"),
    (b"FT", "FoxTorrent"),
    (b"FW", "FrostWire"),
    (b"FX", "Freebox"),
    (b"GS", "GSTorrent"),
    (b"HK", "Hekate"),
    (b"HL", "Halite"),
    (b"HM", "hMule"),
    (b"HN", "Hydranode"),
    (b"IL", "iLivid"),
    (b"JS", "Justseed"),
    (b"JT", "JavaTorrent"),
    (b"KG", "KGet"),
    (b"KT", "KTorrent"),
    (b"LC", "LeechCraft"),
    (b"LH", "LH-ABC"),
    (b"LP", "Lphant"),
    (b"LT", "libtorrent"),
    (b"lt", "libTorrent"),
    (b"Lr", "LibreTorrent"),
    (b"LW", "LimeWire"),
    (b"MK", "Meerkat"),
    (b"MO", "MonoTorrent"),
    (b"MP", "MooPolice"),
    (b"MR", "Miro"),
    (b"MT", "MoonlightTorrent"),
    (b"NB", "Net::BitTorrent"),
    (b"NE", "BT Next Evolution"),
    (b"NX", "Net Transport"),
    (b"OS", "OneSwarm"),
    (b"OT", "OmegaTorrent"),
    (b"PB", "Protocol::BitTorrent"),
    (b"PD", "Pando"),
    (b"PI", "PicoTorrent"),
    (b"PT", "PHPTracker"),
    (b"qB", "qBittorrent"),
    (b"QD", "QQDownload"),
    (b"QT", "Qt 4 Torrent"),
    (b"RT", "Retriever"),
    (b"RZ", "RezTorrent"),
    (b"S~", "Shareaza alpha"),
    (b"SB", "Swiftbit"),
    (b"SD", "Thunder"),
    (b"SM", "SoMud"),
    (b"SP", "BitSpirit"),
    (b"SS", "SwarmScope"),
    (b"ST", "SymTorrent"),
    (b"st", "sharktorrent"),
    (b"SZ", "Shareaza"),
    (b"TB", "Torch"),
    (b"TE", "TemplarTorrent"),
    (b"TL", "Tribler"),
    (b"TN", "Torrent.NET"),
    (b"TR", "Transmission"),
    (b"TS", "Torrentstorm"),
    (b"TT", "TuoTu"),
    (b"tT", "tTorrent"),
    (b"TX", "Tixati"),
    (b"UL", "uLeecher!"),
    (b"UM", "uTorrent Mac"),
    (b"UT", "uTorrent"),
    (b"UW", "uTorrent Web"),
    (b"VG", "Vagaa"),
    (b"WD", "WebTorrent Desktop"),
    (b"WT", "BitLet"),
    (b"WW", "WebTorrent"),
    (b"WY", "FireTorrent"),
    (b"XF", "Xfplay"),
    (b"XL", "Xunlei"),
    (b"XT", "XanTorrent"),
    (b"XX", "Xtorrent"),
    (b"ZT", "ZipTorrent"),
    (b"7T", "aTorrent"),
    (b"AB", "AnyEvent::BitTorrent"),
    (b"MG", "MediaGet"),
    (b"RS", "Rufus"),
];

/// Shadow-style single-letter codes: `T58B-----` and the like.
const SHADOW: &[(u8, &str)] = &[
    (b'A', "ABC"),
    (b'O', "Osprey Permaseed"),
    (b'Q', "BTQueue"),
    (b'R', "Tribler"),
    (b'S', "Shadow's client"),
    (b'T', "BitTornado"),
    (b'U', "UPnP NAT Bit Torrent"),
];

/// Fixed prefixes that follow neither convention.
const PREFIXES: &[(&[u8], &str)] = &[
    (b"exbc", "BitComet"),
    (b"FUTB", "BitComet"),
    (b"xUTB", "BitComet"),
    (b"-ML", "MLdonkey"),
    (b"XBT", "XBT Client"),
    (b"OP", "Opera"),
    (b"Mbrst", "Burst!"),
    (b"btpd", "BT Protocol Daemon"),
    (b"Plus", "Plus!"),
    (b"turbobt", "TurboBT"),
    (b"a00---0", "Swarmy"),
    (b"-BOW", "Bits on Wheels"),
    (b"DNA", "BitTorrent DNA"),
    (b"Pando", "Pando"),
    (b"-G3", "G3 Torrent"),
    (b"QVOD", "Qvod"),
    (b"-WS", "HTTP Seed"),
    (b"BLZ", "Blizzard Downloader"),
    (b"-FG", "FlashGet"),
    (b"AZ2500BT", "BitTyrant"),
];

/// A client label from a peer id, or `None` when the id follows no known
/// convention. The result is already a clean, bounded label.
pub fn client_from_peer_id(id: &[u8; 20]) -> Option<String> {
    if let Some(name) = azureus(id).or_else(|| shadow(id)).or_else(|| mainline(id)) {
        return Some(clean_label(&name, 80));
    }
    PREFIXES
        .iter()
        .find(|(prefix, _)| id.starts_with(prefix))
        .map(|(_, name)| (*name).to_string())
}

/// `-XXvvvv-`: the four version characters are rendered dot-separated, the
/// common reading (`-TR4040-` is Transmission 4.0.4.0, `-qB4650-` is
/// qBittorrent 4.6.5.0). Clients that pack a build letter into the fourth
/// slot keep it, so the label never claims more than the id says.
fn azureus(id: &[u8; 20]) -> Option<String> {
    if id[0] != b'-' || id[7] != b'-' {
        return None;
    }
    let code: &[u8; 2] = id[1..3].try_into().ok()?;
    let version = &id[3..7];
    if !version.iter().all(u8::is_ascii_alphanumeric) {
        return None;
    }
    let name = AZUREUS
        .iter()
        .find(|(known, _)| *known == code)
        .map(|(_, name)| *name)?;
    let version: Vec<String> = version.iter().map(|b| (*b as char).to_string()).collect();
    Some(format!("{name} {}", version.join(".")))
}

/// `Xvvv----` with Shadow's base-64-ish version digits, read literally.
fn shadow(id: &[u8; 20]) -> Option<String> {
    let name = SHADOW
        .iter()
        .find(|(code, _)| *code == id[0])
        .map(|(_, name)| *name)?;
    if &id[6..9] != b"---" {
        return None;
    }
    let version: Vec<String> = id[1..6]
        .iter()
        .take_while(|b| **b != b'-')
        .filter(|b| b.is_ascii_alphanumeric())
        .map(|b| (*b as char).to_string())
        .collect();
    if version.is_empty() {
        return Some(name.to_string());
    }
    Some(format!("{name} {}", version.join(".")))
}

/// `M4-3-6--`: Mainline with dash-separated digits.
fn mainline(id: &[u8; 20]) -> Option<String> {
    if id[0] != b'M' || !id[1].is_ascii_digit() || id[2] != b'-' {
        return None;
    }
    let text = std::str::from_utf8(&id[1..8]).ok()?;
    let version: Vec<&str> = text
        .split('-')
        .take_while(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    if version.is_empty() {
        return None;
    }
    Some(format!("Mainline {}", version.join(".")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(prefix: &[u8]) -> [u8; 20] {
        let mut out = [b'x'; 20];
        out[..prefix.len()].copy_from_slice(prefix);
        out
    }

    #[test]
    fn azureus_ids_name_the_client_and_render_the_version() {
        assert_eq!(
            client_from_peer_id(&id(b"-TR4040-")).as_deref(),
            Some("Transmission 4.0.4.0")
        );
        assert_eq!(
            client_from_peer_id(&id(b"-qB4650-")).as_deref(),
            Some("qBittorrent 4.6.5.0")
        );
        assert_eq!(
            client_from_peer_id(&id(b"-TE0100-")).as_deref(),
            Some("TemplarTorrent 0.1.0.0")
        );
        assert_eq!(
            client_from_peer_id(&id(b"-lt0D80-")).as_deref(),
            Some("libTorrent 0.D.8.0")
        );
        assert_eq!(client_from_peer_id(&id(b"-ZZ0000-")), None, "unknown code");
        assert_eq!(client_from_peer_id(&id(b"-TR40 0-")), None, "bad version");
    }

    #[test]
    fn shadow_mainline_and_fixed_prefixes_are_recognised() {
        assert_eq!(
            client_from_peer_id(&id(b"T03I-----")).as_deref(),
            Some("BitTornado 0.3.I")
        );
        assert_eq!(
            client_from_peer_id(&id(b"M4-3-6--")).as_deref(),
            Some("Mainline 4.3.6")
        );
        assert_eq!(
            client_from_peer_id(&id(b"exbc")).as_deref(),
            Some("BitComet")
        );
        assert_eq!(
            client_from_peer_id(&id(b"-ML2.7.2-")).as_deref(),
            Some("MLdonkey")
        );
        assert_eq!(client_from_peer_id(&[0; 20]), None);
        assert_eq!(client_from_peer_id(&[0xff; 20]), None);
        // Control characters can never reach the label.
        let mut hostile = id(b"-TR4040-");
        hostile[8..].copy_from_slice(b"\x1b[31mredred\x07");
        assert_eq!(
            client_from_peer_id(&hostile).as_deref(),
            Some("Transmission 4.0.4.0")
        );
    }
}
