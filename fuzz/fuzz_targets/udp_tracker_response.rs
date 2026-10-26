//! Datagrams a UDP tracker (BEP 15) sends back, read as IPv4 and as IPv6
//! replies: connect, announce, error and anything else.
#![no_main]
use libfuzzer_sys::fuzz_target;
use templartorrent_rbit_adapter::{
    announce::MAX_PEERS,
    udp_announce::{MAX_ERROR_CHARS, Reply, parse_reply, sanitize_message},
};

fuzz_target!(|data: &[u8]| {
    for ipv6 in [false, true] {
        match parse_reply(data, ipv6) {
            Ok(Reply::Announce { response, .. }) => {
                assert!(response.peers.len() <= MAX_PEERS && response.peers6.len() <= MAX_PEERS);
                if ipv6 {
                    assert!(response.peers.is_empty());
                    assert!(response.peers6.iter().all(|peer| peer.is_ipv6()));
                } else {
                    assert!(response.peers6.is_empty());
                    assert!(response.peers.iter().all(|peer| peer.is_ipv4()));
                }
                assert!(response.interval <= i32::MAX as u32);
            }
            Ok(Reply::Error { message, .. }) => {
                assert!(message.chars().count() <= MAX_ERROR_CHARS);
                assert!(!message.chars().any(char::is_control));
            }
            Ok(Reply::Connect { .. }) => assert!(data.len() >= 16),
            Err(_) => {}
        }
    }
    let message = sanitize_message(data);
    assert!(!message.is_empty() && message.chars().count() <= MAX_ERROR_CHARS);
});
