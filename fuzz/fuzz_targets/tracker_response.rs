//! Bytes a tracker sends back: HTTP framing, then the bencoded announce body.
#![no_main]
use libfuzzer_sys::fuzz_target;
use templartorrent_rbit_adapter::announce::{MAX_PEERS, parse_announce_body, parse_http_response};

fuzz_target!(|data: &[u8]| {
    if let Ok(body) = parse_http_response(data) {
        assert!(body.len() <= data.len());
        let _ = parse_announce_body(&body);
    }
    if let Ok(response) = parse_announce_body(data) {
        assert!(response.peers.len() <= MAX_PEERS && response.peers6.len() <= MAX_PEERS);
    }
});
