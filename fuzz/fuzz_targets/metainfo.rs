//! A .torrent file the user opens, and raw info bytes a peer sends through
//! BEP-9 wrapped the way the driver wraps them.
#![no_main]
use libfuzzer_sys::fuzz_target;
use templartorrent_rbit_adapter::{driver::Driver, parse_torrent};

fuzz_target!(|data: &[u8]| {
    let _ = parse_torrent(data);
    let _ = rbit::Metainfo::from_bytes(data);
    let _ = Driver::magnet_from_torrent_bytes(data);
    let mut envelope = Vec::with_capacity(data.len() + 8);
    envelope.extend_from_slice(b"d4:info");
    envelope.extend_from_slice(data);
    envelope.push(b'e');
    let _ = parse_torrent(&envelope);
});
