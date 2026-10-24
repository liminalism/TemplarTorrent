//! Pasted magnet text and the tracker URLs it or a saved record carries.
#![no_main]
use libfuzzer_sys::fuzz_target;
use templartorrent_rbit_adapter::{
    discovery::{parse_tracker_url, percent_encode, redact_tracker_url},
    parse_magnet,
};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = parse_magnet(text);
    if let Ok(parsed) = parse_tracker_url(text) {
        assert!(!parsed.host.is_empty());
    }
    let _ = redact_tracker_url(text);
    let encoded = percent_encode(text);
    assert!(encoded.is_ascii());
});
