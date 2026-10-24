//! rbit's bencode decoder under every tracker, metainfo and extension path.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(value) = rbit::decode(data) {
        let encoded = rbit::encode(&value).expect("decoded value encodes");
        let again = rbit::decode(&encoded).expect("encoded value decodes");
        assert_eq!(value, again);
    }
});
