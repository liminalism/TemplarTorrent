//! Peer exchange payloads from connected peers and DHT datagrams from any
//! host: both reach the client once a public job runs.
#![no_main]
use libfuzzer_sys::fuzz_target;
use templartorrent_rbit_adapter::pex;

fuzz_target!(|data: &[u8]| {
    if let Ok(peers) = pex::parse_added(data) {
        assert!(peers.len() <= pex::MAX_ACCEPTED);
        let again = pex::parse_added(&pex::encode(&peers, &[])).expect("encoded PEX parses");
        assert!(again.len() <= peers.len());
    }
    let _ = rbit::DhtMessage::parse(data);
});
