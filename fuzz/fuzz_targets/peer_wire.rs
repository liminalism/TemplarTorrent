//! Frames a remote peer controls: the handshake, length-prefixed messages,
//! the BEP-10 handshake payload and BEP-9 metadata messages.
#![no_main]
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use rbit::{ExtensionHandshake, Handshake, Message, MetadataMessage, peer::ExtensionMessage};

fuzz_target!(|data: &[u8]| {
    let _ = Handshake::decode(data);
    if let Ok(message) = Message::decode(Bytes::copy_from_slice(data)) {
        // A decoded message must re-encode and decode to itself.
        let again = Message::decode(message.encode()).expect("re-encoded message decodes");
        assert_eq!(format!("{message:?}"), format!("{again:?}"));
    }
    let _ = ExtensionHandshake::decode(data);
    if let Some((&id, payload)) = data.split_first() {
        let _ = ExtensionMessage::decode(id, payload);
    }
    let _ = MetadataMessage::decode(data);
});
