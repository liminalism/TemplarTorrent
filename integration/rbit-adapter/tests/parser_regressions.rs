//! Inputs the fuzz targets in fuzz/ found panicking, reduced by hand. Each
//! is a refusal now; a panic here is a remote crash of the client, because
//! trackers, peers and pasted text all reach these parsers.
use bytes::Bytes;
use rbit::{Message, MetadataMessage};
use templartorrent_rbit_adapter::{
    announce::{parse_announce_body, parse_http_response},
    parse_magnet, parse_torrent,
};

#[test]
fn bencode_dict_with_a_non_string_key_at_the_end_is_refused() {
    for input in [&b"dle"[..], b"d1:d1:ai-4e", b"di1e"] {
        assert!(rbit::decode(input).is_err(), "{input:?}");
        assert!(parse_announce_body(input).is_err());
        assert!(parse_torrent(input).is_err());
    }
    let mut torrent = b"d4:infod4:name1:x".to_vec();
    torrent.extend_from_slice(b"le");
    assert!(rbit::Metainfo::from_bytes(&torrent).is_err());
}

#[test]
fn bencode_string_length_near_usize_max_is_refused() {
    let max = usize::MAX.to_string();
    for tail in ["", ":", ":x"] {
        let input = format!("{max}{tail}");
        assert!(rbit::decode(input.as_bytes()).is_err());
    }
    let input = format!("d1:a{}:xe", usize::MAX - 2);
    assert!(rbit::decode(input.as_bytes()).is_err());
}

#[test]
fn metadata_message_string_length_near_usize_max_is_refused() {
    let input = format!("d1:a{}:x", usize::MAX);
    assert!(MetadataMessage::decode(input.as_bytes()).is_err());
}

#[test]
fn magnet_accepts_encoded_xt_dotted_keys_and_any_case() {
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let encoded =
        format!("magnet:?xt=urn%3Abtih%3A{hash}&tr=http%3A%2F%2Ftracker.example%2Fannounce%2520x");
    let magnet = magnet_of(&encoded);
    assert_eq!(magnet.info_hash.to_hex(), hash);
    assert_eq!(
        magnet.trackers,
        vec!["http://tracker.example/announce%20x".to_string()]
    );

    let dotted = format!("magnet:?xt.1=urn:btih:{hash}&tr.1=http://tracker.example/announce");
    let magnet = magnet_of(&dotted);
    assert_eq!(magnet.info_hash.to_hex(), hash);
    assert_eq!(
        magnet.trackers,
        vec!["http://tracker.example/announce".to_string()]
    );

    let mixed = format!("<Magnet:?xt=urn:BTIH:{hash}>");
    let magnet = magnet_of(&mixed);
    assert_eq!(magnet.info_hash.to_hex(), hash);
}

fn magnet_of(uri: &str) -> rbit::MagnetLink {
    match parse_magnet(uri).unwrap_or_else(|error| panic!("{uri}: {error}")) {
        templartorrent_rbit_adapter::Source::Magnet(magnet) => magnet,
        templartorrent_rbit_adapter::Source::Torrent(_) => panic!("torrent {uri}"),
    }
}

#[test]
fn magnet_btih_with_a_multibyte_char_is_refused() {
    // 40 bytes, so it reaches hex decoding, with a two-byte char at an odd
    // offset so a two-byte slice splits it.
    let hash = "0\u{45e}123456789abcdef0123456789abcdef012345";
    assert_eq!(hash.len(), 40);
    let uri = format!("magnet:?xt=urn:btih:{hash}");
    assert!(parse_magnet(&uri).is_err());
    assert!(rbit::MagnetLink::parse(&uri).is_err());
}

#[test]
fn peer_message_ignores_bytes_after_its_frame() {
    // Frame says six bytes of Piece, too short for index and begin; ten more
    // bytes follow. They belong to the next frame, not to this one.
    let mut frame = vec![0, 0, 0, 6, 7, 4, 0, 0, 0, 0];
    frame.extend_from_slice(&[0; 6]);
    assert!(Message::decode(Bytes::from(frame)).is_err());
    let mut have = vec![0, 0, 0, 5, 4, 0, 0, 0, 9];
    have.extend_from_slice(&[0xff; 8]);
    assert!(matches!(
        Message::decode(Bytes::from(have)),
        Ok(Message::Have { piece: 9 })
    ));
    let mut bitfield = vec![0, 0, 0, 2, 5, 0xc0];
    bitfield.extend_from_slice(&[1, 2, 3]);
    match Message::decode(Bytes::from(bitfield)) {
        Ok(Message::Bitfield(bits)) => assert_eq!(&bits[..], &[0xc0]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn chunk_size_that_overflows_is_refused() {
    let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\nab";
    assert_eq!(
        parse_http_response(response),
        Err("tracker response exceeds 64 KiB")
    );
    let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n10001\r\nab";
    assert_eq!(
        parse_http_response(response),
        Err("tracker response exceeds 64 KiB")
    );
}

fn pieces_field(count: usize) -> Vec<u8> {
    let mut out = format!("6:pieces{}:", count * 20).into_bytes();
    out.extend(std::iter::repeat_n(0xab, count * 20));
    out
}

#[test]
fn metainfo_negative_or_overflowing_lengths_are_refused() {
    let torrent = |info: &[u8]| {
        let mut out = b"d4:info".to_vec();
        out.extend_from_slice(info);
        out.push(b'e');
        out
    };
    let mut cases = Vec::new();
    // Negative file length in a multi-file torrent, as fuzz/metainfo found.
    let mut info =
        b"d5:filesld6:lengthi10e4:pathl1:aeed6:lengthi-10e4:pathl1:beee4:name1:x12:piece lengthi16384e"
            .to_vec();
    info.extend(pieces_field(1));
    info.push(b'e');
    cases.push(info);
    // Three lengths whose sum passes u64.
    let mut info = format!(
        "d5:filesld6:lengthi{0}e4:pathl1:aeed6:lengthi{0}e4:pathl1:bee\
         d6:lengthi{0}e4:pathl1:ceee4:name1:x12:piece lengthi16384e",
        i64::MAX
    )
    .into_bytes();
    info.extend(pieces_field(1));
    info.push(b'e');
    cases.push(info);
    // Negative single-file length, negative and zero piece length.
    for (length, piece_length) in [("-1", "16384"), ("10", "-16384"), ("10", "0")] {
        let mut info =
            format!("d6:lengthi{length}e4:name1:x12:piece lengthi{piece_length}e").into_bytes();
        info.extend(pieces_field(1));
        info.push(b'e');
        cases.push(info);
    }
    for info in cases {
        let bytes = torrent(&info);
        assert!(
            rbit::Metainfo::from_bytes(&bytes).is_err(),
            "{}",
            String::from_utf8_lossy(&info)
        );
        assert!(parse_torrent(&bytes).is_err());
    }
}

#[test]
fn metadata_message_integers_out_of_range_are_refused() {
    for dict in [
        "d8:msg_typei257e5:piecei0ee",
        "d8:msg_typei-1e5:piecei0ee",
        "d8:msg_typei0e5:piecei4294967296ee",
        "d8:msg_typei1e5:piecei0e10:total_sizei-1ee",
    ] {
        assert!(MetadataMessage::decode(dict.as_bytes()).is_err(), "{dict}");
    }
    let ok = MetadataMessage::decode(b"d8:msg_typei1e5:piecei2e10:total_sizei5ee").unwrap();
    assert_eq!((ok.piece, ok.total_size), (2, Some(5)));
}

#[test]
fn extension_ids_past_a_byte_are_skipped() {
    let handshake =
        rbit::ExtensionHandshake::decode(b"d1:md11:ut_metadatai256e6:ut_pexi1eee").unwrap();
    assert_eq!(handshake.extensions.get("ut_metadata"), None);
    assert_eq!(handshake.extensions.get("ut_pex"), Some(&1));
}
