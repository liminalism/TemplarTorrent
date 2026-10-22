use super::response::parse_compact_peers;
use super::*;

#[test]
fn test_tracker_event() {
    assert_eq!(TrackerEvent::Started.as_str(), "started");
    assert_eq!(TrackerEvent::Stopped.as_str(), "stopped");
    assert_eq!(TrackerEvent::Completed.as_str(), "completed");
    assert_eq!(TrackerEvent::None.as_str(), "");

    assert_eq!(TrackerEvent::None.as_udp_id(), 0);
    assert_eq!(TrackerEvent::Completed.as_udp_id(), 1);
    assert_eq!(TrackerEvent::Started.as_udp_id(), 2);
    assert_eq!(TrackerEvent::Stopped.as_udp_id(), 3);
}

#[test]
fn test_compact_peer_v4() {
    let bytes = [192, 168, 1, 1, 0x1A, 0xE1]; // 192.168.1.1:6881
    let peer = CompactPeer::from_v4_bytes(&bytes).unwrap();

    assert_eq!(peer.port, 6881);
    match peer.ip {
        std::net::IpAddr::V4(ip) => {
            assert_eq!(ip.octets(), [192, 168, 1, 1]);
        }
        _ => panic!("expected v4"),
    }
}

#[test]
fn test_parse_compact_peers() {
    let data = [
        192, 168, 1, 1, 0x1A, 0xE1, // 192.168.1.1:6881
        10, 0, 0, 1, 0x1A, 0xE1, // 10.0.0.1:6881
    ];

    let peers = parse_compact_peers(&data);
    assert_eq!(peers.len(), 2);
}

#[test]
fn test_announce_response() {
    let mut response = AnnounceResponse::new(1800);
    response.complete = Some(10);
    response.incomplete = Some(5);

    assert_eq!(response.interval, 1800);
    assert_eq!(response.complete, Some(10));
    assert_eq!(response.incomplete, Some(5));
}

#[test]
fn test_http_tracker_invalid_url() {
    let result = HttpTracker::new("ftp://tracker.example.com");
    assert!(result.is_err());
}
