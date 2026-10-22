use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use super::*;

#[test]
fn test_node_id_generate() {
    let id1 = NodeId::generate();
    let id2 = NodeId::generate();
    assert_ne!(id1.0, id2.0);
}

#[test]
fn test_node_id_from_bytes() {
    let bytes = [1u8; 20];
    let id = NodeId::from_bytes(&bytes).unwrap();
    assert_eq!(id.0, bytes);
}

#[test]
fn test_node_id_from_bytes_invalid() {
    let bytes = [1u8; 10];
    assert!(NodeId::from_bytes(&bytes).is_err());
}

#[test]
fn test_node_id_distance() {
    let id1 = NodeId([0u8; 20]);
    let id2 = NodeId([0xFF; 20]);

    let dist = id1.distance(&id2);
    assert_eq!(dist, [0xFF; 20]);

    let dist_self = id1.distance(&id1);
    assert_eq!(dist_self, [0u8; 20]);
}

#[test]
fn test_node_id_bucket_index() {
    let id1 = NodeId([0u8; 20]);
    let mut id2_bytes = [0u8; 20];
    id2_bytes[0] = 0x80;
    let id2 = NodeId(id2_bytes);

    assert_eq!(id1.bucket_index(&id2), 0);
}

#[test]
fn test_node_compact() {
    let id = NodeId([1u8; 20]);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 6881);
    let node = Node::new(id, addr);

    let compact = node.to_compact().unwrap();
    assert_eq!(compact.len(), 26);

    let parsed = Node::from_compact(&compact).unwrap();
    assert_eq!(parsed.id.0, id.0);
    assert_eq!(parsed.addr, addr);
}

#[test]
fn test_node_state() {
    let id = NodeId::generate();
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 6881);
    let mut node = Node::new(id, addr);

    assert!(node.is_good());
    assert!(!node.is_bad());

    node.fail();
    node.fail();
    node.fail();
    assert!(node.is_bad());
}

#[test]
fn test_routing_table_add() {
    let our_id = NodeId::generate();
    let table = RoutingTable::new(our_id);

    for _ in 0..10 {
        let id = NodeId::generate();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 6881);
        table.add_node(Node::new(id, addr));
    }

    assert!(table.node_count() > 0);
}

#[test]
fn test_routing_table_find_closest() {
    let our_id = NodeId::generate();
    let table = RoutingTable::new(our_id);

    for i in 0..20 {
        let mut id_bytes = [0u8; 20];
        id_bytes[0] = i;
        let id = NodeId(id_bytes);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, i)), 6881);
        table.add_node(Node::new(id, addr));
    }

    let target = NodeId([0u8; 20]);
    let closest = table.find_closest(&target, 8);
    assert!(closest.len() <= 8);
}

#[test]
fn test_dht_message_ping() {
    let our_id = NodeId::generate();
    let tid = bytes::Bytes::from_static(b"aa");

    let msg = DhtMessage::ping(tid.clone(), &our_id);
    let encoded = msg.encode().unwrap();

    let parsed = DhtMessage::parse(&encoded).unwrap();
    assert_eq!(parsed.transaction_id, tid);
    assert!(parsed.query.is_some());
}

#[test]
fn test_dht_message_find_node() {
    let our_id = NodeId::generate();
    let target = NodeId::generate();
    let tid = bytes::Bytes::from_static(b"bb");

    let msg = DhtMessage::find_node(tid.clone(), &our_id, target);
    let encoded = msg.encode().unwrap();

    let parsed = DhtMessage::parse(&encoded).unwrap();
    assert_eq!(parsed.transaction_id, tid);

    if let Some((name, query)) = parsed.query {
        assert_eq!(name, "find_node");
        match query {
            DhtQuery::FindNode { target: t } => {
                assert_eq!(t.0, target.0);
            }
            _ => panic!("wrong query type"),
        }
    } else {
        panic!("missing query");
    }
}

#[test]
fn test_dht_message_get_peers() {
    let our_id = NodeId::generate();
    let info_hash = [0xAB; 20];
    let tid = bytes::Bytes::from_static(b"cc");

    let msg = DhtMessage::get_peers(tid.clone(), &our_id, info_hash);
    let encoded = msg.encode().unwrap();

    let parsed = DhtMessage::parse(&encoded).unwrap();
    assert_eq!(parsed.transaction_id, tid);

    if let Some((name, query)) = parsed.query {
        assert_eq!(name, "get_peers");
        match query {
            DhtQuery::GetPeers { info_hash: h } => {
                assert_eq!(h, info_hash);
            }
            _ => panic!("wrong query type"),
        }
    } else {
        panic!("missing query");
    }
}
