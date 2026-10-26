//! BEP 11 peer exchange messages: the bencoded dictionary around rbit's
//! compact peer lists. Only public jobs use this; a private torrent must not
//! advertise or accept PEX (BEP 27).
use bytes::Bytes;
use rbit::{PexMessage, PexPeer, Value};
use std::{collections::BTreeMap, net::SocketAddr};

/// Our `m` id for ut_pex. ut_metadata is 3.
pub const OUR_PEX_ID: u8 = 2;
/// BEP 11 asks senders for at most 50 added and 50 dropped per message.
pub const MAX_SENT: usize = 50;
/// Accepted from one message; extra entries are ignored, not an error.
pub const MAX_ACCEPTED: usize = 100;
/// Larger payloads are refused before bencode decoding.
pub const MAX_PAYLOAD: usize = 8 * 1024;
/// Seconds between messages to one peer.
pub const INTERVAL_S: u64 = 60;

/// Added peers (IPv4 then IPv6) from one ut_pex payload, at most
/// [`MAX_ACCEPTED`]. Dropped entries are ignored: a candidate that left
/// simply fails its next dial.
pub fn parse_added(payload: &[u8]) -> Result<Vec<SocketAddr>, &'static str> {
    if payload.len() > MAX_PAYLOAD {
        return Err("peer exchange message too large");
    }
    let value = rbit::decode(payload).map_err(|_| "peer exchange message is not bencode")?;
    let dict = value
        .as_dict()
        .ok_or("peer exchange message is not a dictionary")?;
    let bytes = |key: &[u8]| {
        dict.get(key)
            .and_then(Value::as_bytes)
            .map(|b| b.as_ref())
            .unwrap_or(&[])
    };
    let mut peers: Vec<SocketAddr> = PexMessage::decode_added(bytes(b"added"), bytes(b"added.f"))
        .into_iter()
        .chain(PexMessage::decode_added6(
            bytes(b"added6"),
            bytes(b"added6.f"),
        ))
        .map(|peer| peer.addr)
        .filter(|addr| addr.port() != 0 && !addr.ip().is_unspecified())
        .take(MAX_ACCEPTED)
        .collect();
    peers.dedup();
    Ok(peers)
}

/// One ut_pex payload announcing `added` and `dropped`, each capped at
/// [`MAX_SENT`].
pub fn encode(added: &[SocketAddr], dropped: &[SocketAddr]) -> Vec<u8> {
    let mut message = PexMessage::new();
    for addr in added.iter().take(MAX_SENT) {
        message.add_peer(PexPeer::new(*addr));
    }
    for addr in dropped.iter().take(MAX_SENT) {
        message.drop_peer(*addr);
    }
    let mut dict = BTreeMap::new();
    let mut put = |key: &'static [u8], value: Bytes| {
        dict.insert(Bytes::from_static(key), Value::Bytes(value));
    };
    put(b"added", message.encode_added());
    put(b"added.f", message.encode_added_flags());
    put(b"added6", message.encode_added6());
    put(b"added6.f", message.encode_added6_flags());
    put(b"dropped", message.encode_dropped());
    put(b"dropped6", message.encode_dropped6());
    rbit::encode(&Value::Dict(dict)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_both_families_and_caps() {
        let v4: Vec<SocketAddr> = (1..=60u16)
            .map(|i| {
                format!("10.0.0.{}:{}", i % 250 + 1, 6000 + i)
                    .parse()
                    .unwrap()
            })
            .collect();
        let v6: SocketAddr = "[2001:db8::7]:51413".parse().unwrap();
        let mut added = vec![v6];
        added.extend(&v4);
        let payload = encode(&added, &v4[..3]);
        let parsed = parse_added(&payload).unwrap();
        // 50 sent: the IPv6 entry and 49 IPv4 ones, IPv4 listed first.
        assert_eq!(parsed.len(), MAX_SENT);
        assert!(parsed.contains(&v6));
        assert_eq!(parsed[0], v4[0]);
    }

    #[test]
    fn hostile_payloads_are_refused_or_trimmed() {
        assert!(parse_added(b"not bencode").is_err());
        assert!(parse_added(b"li1ee").is_err());
        assert!(parse_added(&vec![b'0'; MAX_PAYLOAD + 1]).is_err());
        // One valid peer, one unspecified address and a ragged trailing byte.
        let payload = b"d5:added13:\x0a\x00\x00\x01\x1a\xe1\x00\x00\x00\x00\x1a\xe1\x09e";
        let expected: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        assert_eq!(parse_added(payload).unwrap(), vec![expected]);
        let payload = b"d5:added6:\x0a\x00\x00\x01\x00\x00e";
        assert!(parse_added(payload).unwrap().is_empty());
        assert!(parse_added(b"de").unwrap().is_empty());
    }
}
