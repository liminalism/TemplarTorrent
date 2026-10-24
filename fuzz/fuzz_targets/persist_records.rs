//! Session files read back at startup. A crash or another process can leave
//! any bytes there; decoding must refuse, never panic, and accepted records
//! must survive their own encoder.
#![no_main]
use libfuzzer_sys::fuzz_target;
use templartorrent::persist::{
    decode_record, decode_resume, decode_settings, decode_ui, encode_record, encode_resume,
    encode_settings, encode_ui,
};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(record) = decode_record(text, [7; 20], Vec::new()) {
        let again = decode_record(&encode_record(&record), [7; 20], Vec::new())
            .expect("encoded record decodes");
        assert!(again == record, "record changed across encode");
    }
    if let Ok(resume) = decode_resume(text) {
        assert_eq!(decode_resume(&encode_resume(&resume)), Ok(resume));
    }
    if let Ok(settings) = decode_settings(text) {
        let first = encode_settings(&settings);
        let again = decode_settings(&first).expect("encoded settings decode");
        assert_eq!(encode_settings(&again), first);
    }
    if let Ok(ui) = decode_ui(text) {
        let first = encode_ui(&ui);
        let again = decode_ui(&first).expect("encoded ui state decodes");
        assert_eq!(encode_ui(&again), first);
    }
});
