use bytes::Bytes;

use super::*;

#[test]
fn test_peer_id_generate() {
    let id1 = PeerId::generate();
    let id2 = PeerId::generate();
    assert_ne!(id1.0, id2.0);
    assert!(id1.client_id().is_some());
}

#[test]
fn test_bitfield() {
    let mut bf = Bitfield::new(100);
    assert!(!bf.has(0));

    bf.set(0);
    assert!(bf.has(0));

    bf.set(99);
    assert!(bf.has(99));

    bf.clear(0);
    assert!(!bf.has(0));

    assert_eq!(bf.count_ones(), 1);
}

#[test]
fn test_bitfield_from_bytes() {
    let bytes = Bytes::from_static(&[0x80, 0x00]);
    let bf = Bitfield::from_bytes(bytes, 16);

    assert!(bf.has(0));
    assert!(!bf.has(1));
}

#[test]
fn test_handshake_encode_decode() {
    let info_hash = [1u8; 20];
    let peer_id = [2u8; 20];

    let handshake = Handshake::new(info_hash, peer_id);
    let encoded = handshake.encode();

    let decoded = Handshake::decode(&encoded).unwrap();
    assert_eq!(decoded.info_hash, info_hash);
    assert_eq!(decoded.peer_id, peer_id);
    assert!(decoded.supports_extension_protocol());
    assert!(decoded.supports_fast_extension());
}

#[test]
fn test_message_encode_decode() {
    let messages = vec![
        Message::KeepAlive,
        Message::Choke,
        Message::Unchoke,
        Message::Interested,
        Message::NotInterested,
        Message::Have { piece: 42 },
        Message::Request {
            index: 1,
            begin: 0,
            length: 16384,
        },
        Message::Cancel {
            index: 1,
            begin: 0,
            length: 16384,
        },
        Message::Port(6881),
        Message::HaveAll,
        Message::HaveNone,
    ];

    for msg in messages {
        let encoded = msg.encode();
        let decoded = Message::decode(encoded).unwrap();

        match (&msg, &decoded) {
            (Message::KeepAlive, Message::KeepAlive) => {}
            (Message::Choke, Message::Choke) => {}
            (Message::Unchoke, Message::Unchoke) => {}
            (Message::Interested, Message::Interested) => {}
            (Message::NotInterested, Message::NotInterested) => {}
            (Message::Have { piece: p1 }, Message::Have { piece: p2 }) => {
                assert_eq!(p1, p2);
            }
            (
                Message::Request {
                    index: i1,
                    begin: b1,
                    length: l1,
                },
                Message::Request {
                    index: i2,
                    begin: b2,
                    length: l2,
                },
            ) => {
                assert_eq!((i1, b1, l1), (i2, b2, l2));
            }
            (
                Message::Cancel {
                    index: i1,
                    begin: b1,
                    length: l1,
                },
                Message::Cancel {
                    index: i2,
                    begin: b2,
                    length: l2,
                },
            ) => {
                assert_eq!((i1, b1, l1), (i2, b2, l2));
            }
            (Message::Port(p1), Message::Port(p2)) => {
                assert_eq!(p1, p2);
            }
            (Message::HaveAll, Message::HaveAll) => {}
            (Message::HaveNone, Message::HaveNone) => {}
            _ => panic!("message mismatch"),
        }
    }
}

#[test]
fn test_piece_message() {
    let data = Bytes::from_static(b"hello world");
    let msg = Message::Piece {
        index: 0,
        begin: 0,
        data: data.clone(),
    };

    let encoded = msg.encode();
    let decoded = Message::decode(encoded).unwrap();

    if let Message::Piece {
        index,
        begin,
        data: decoded_data,
    } = decoded
    {
        assert_eq!(index, 0);
        assert_eq!(begin, 0);
        assert_eq!(decoded_data, data);
    } else {
        panic!("expected piece message");
    }
}

#[test]
fn test_extension_handshake() {
    let mut hs = ExtensionHandshake::new();
    hs.extensions.insert("ut_pex".to_string(), 1);
    hs.extensions.insert("ut_metadata".to_string(), 2);
    hs.client = Some("rbit/0.1".to_string());

    let encoded = hs.encode().unwrap();
    let decoded = ExtensionHandshake::decode(&encoded).unwrap();

    assert_eq!(decoded.get_extension_id("ut_pex"), Some(1));
    assert_eq!(decoded.get_extension_id("ut_metadata"), Some(2));
    assert_eq!(decoded.client, Some("rbit/0.1".to_string()));
}

#[test]
fn test_block_request() {
    let req = BlockRequest::new(0, 0, 16384);
    assert_eq!(req.piece_index, 0);
    assert_eq!(req.offset, 0);
    assert_eq!(req.length, 16384);
}

#[test]
fn test_choking_state_default() {
    let state = ChokingState::default();
    assert!(state.am_choking);
    assert!(!state.am_interested);
    assert!(state.peer_choking);
    assert!(!state.peer_interested);
}

#[test]
fn test_handshake_v2_support() {
    let info_hash = [1u8; 20];
    let peer_id = [2u8; 20];

    // Test v1 handshake doesn't have v2 support
    let handshake_v1 = Handshake::new(info_hash, peer_id);
    assert!(!handshake_v1.supports_v2());

    // Test v2 handshake has v2 support
    let handshake_v2 = Handshake::new_v2(info_hash, peer_id);
    assert!(handshake_v2.supports_v2());
    assert!(handshake_v2.supports_extension_protocol());
    assert!(handshake_v2.supports_fast_extension());

    // Test round-trip
    let encoded = handshake_v2.encode();
    let decoded = Handshake::decode(&encoded).unwrap();
    assert!(decoded.supports_v2());
}

#[test]
fn test_v2_hash_request_encode_decode() {
    let pieces_root = [0xABu8; 32];
    let msg = Message::HashRequest {
        pieces_root,
        base_layer: 0,
        index: 0,
        length: 512,
        proof_layers: 3,
    };

    let encoded = msg.encode();
    let decoded = Message::decode(encoded).unwrap();

    if let Message::HashRequest {
        pieces_root: root,
        base_layer,
        index,
        length,
        proof_layers,
    } = decoded
    {
        assert_eq!(root, pieces_root);
        assert_eq!(base_layer, 0);
        assert_eq!(index, 0);
        assert_eq!(length, 512);
        assert_eq!(proof_layers, 3);
    } else {
        panic!("expected HashRequest message");
    }
}

#[test]
fn test_v2_hashes_encode_decode() {
    let pieces_root = [0xCDu8; 32];
    // Create some fake hashes (3 hashes of 32 bytes each)
    let mut hash_data = vec![0u8; 96];
    for (i, chunk) in hash_data.chunks_mut(32).enumerate() {
        chunk.fill((i + 1) as u8);
    }
    let hashes = Bytes::from(hash_data.clone());

    let msg = Message::Hashes {
        pieces_root,
        base_layer: 1,
        index: 64,
        length: 2,
        proof_layers: 1,
        hashes: hashes.clone(),
    };

    let encoded = msg.encode();
    let decoded = Message::decode(encoded).unwrap();

    if let Message::Hashes {
        pieces_root: root,
        base_layer,
        index,
        length,
        proof_layers,
        hashes: decoded_hashes,
    } = decoded
    {
        assert_eq!(root, pieces_root);
        assert_eq!(base_layer, 1);
        assert_eq!(index, 64);
        assert_eq!(length, 2);
        assert_eq!(proof_layers, 1);
        assert_eq!(decoded_hashes, hashes);
    } else {
        panic!("expected Hashes message");
    }
}

#[test]
fn test_v2_hash_reject_encode_decode() {
    let pieces_root = [0xEFu8; 32];
    let msg = Message::HashReject {
        pieces_root,
        base_layer: 2,
        index: 128,
        length: 64,
        proof_layers: 0,
    };

    let encoded = msg.encode();
    let decoded = Message::decode(encoded).unwrap();

    if let Message::HashReject {
        pieces_root: root,
        base_layer,
        index,
        length,
        proof_layers,
    } = decoded
    {
        assert_eq!(root, pieces_root);
        assert_eq!(base_layer, 2);
        assert_eq!(index, 128);
        assert_eq!(length, 64);
        assert_eq!(proof_layers, 0);
    } else {
        panic!("expected HashReject message");
    }
}

#[test]
fn test_validate_hash_request() {
    use super::message::validate_hash_request;

    // Valid requests
    assert!(validate_hash_request(2, 0).is_none());
    assert!(validate_hash_request(4, 0).is_none());
    assert!(validate_hash_request(4, 4).is_none());
    assert!(validate_hash_request(512, 512).is_none());

    // Length must be >= 2
    assert!(validate_hash_request(1, 0).is_some());

    // Length must be power of 2
    assert!(validate_hash_request(3, 0).is_some());
    assert!(validate_hash_request(5, 0).is_some());

    // Length must not exceed 512
    assert!(validate_hash_request(1024, 0).is_some());

    // Index must be multiple of length
    assert!(validate_hash_request(4, 1).is_some());
    assert!(validate_hash_request(4, 2).is_some());
    assert!(validate_hash_request(4, 3).is_some());
}

// =========================================================================
// Hash Request Manager Tests (BEP-52)
// =========================================================================

#[test]
fn test_hash_request_manager_basic() {
    use super::hash_request::{HashRequestManager, PendingHashRequest};

    let manager = HashRequestManager::new();
    let root = [0xABu8; 32];

    // Add a request
    let req = PendingHashRequest::new(root, 0, 0, 4, 2);
    assert!(manager.add_request(req));
    assert_eq!(manager.pending_count(), 1);

    // Remove the request
    let removed = manager.remove_request(&root, 0, 0, 4);
    assert!(removed.is_some());
    assert_eq!(manager.pending_count(), 0);

    // Try to remove again (should fail)
    let removed_again = manager.remove_request(&root, 0, 0, 4);
    assert!(removed_again.is_none());
}

#[test]
fn test_hash_request_manager_max_pending() {
    use super::hash_request::{HashRequestManager, PendingHashRequest, MAX_PENDING_HASH_REQUESTS};

    let manager = HashRequestManager::new();

    // Fill up to max
    for i in 0..MAX_PENDING_HASH_REQUESTS {
        let mut root = [0u8; 32];
        root[0] = i as u8;
        let req = PendingHashRequest::new(root, 0, 0, 4, 2);
        assert!(manager.add_request(req), "Failed to add request {}", i);
    }

    assert_eq!(manager.pending_count(), MAX_PENDING_HASH_REQUESTS);

    // Try to add one more (should fail)
    let extra = PendingHashRequest::new([0xFFu8; 32], 0, 0, 4, 2);
    assert!(!manager.add_request(extra));
}

#[test]
fn test_hash_request_manager_store_hashes() {
    use super::hash_request::{HashRequestManager, HashResponse};

    let manager = HashRequestManager::new();
    let root = [0xCDu8; 32];

    // Store some hashes
    let response = HashResponse {
        pieces_root: root,
        base_layer: 0,
        index: 0,
        length: 2,
        proof_layers: 0,
        layer_hashes: vec![[1u8; 32], [2u8; 32]],
        proof_hashes: vec![],
    };

    manager.store_hashes(&response);

    // Retrieve them
    assert!(manager.has_hashes(&root, 0, 0));
    let retrieved = manager.get_hashes(&root, 0, 0);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().len(), 2);

    // Check for non-existent hashes
    assert!(!manager.has_hashes(&root, 0, 4));
    assert!(!manager.has_hashes(&[0xEFu8; 32], 0, 0));
}

#[test]
fn test_hash_response_from_raw() {
    use bytes::Bytes;

    use super::hash_request::HashResponse;

    let root = [0x12u8; 32];

    // Create raw hash data: 2 layer hashes + 1 proof hash = 96 bytes
    let mut hash_data = vec![0u8; 96];
    hash_data[0..32].copy_from_slice(&[0xAAu8; 32]);
    hash_data[32..64].copy_from_slice(&[0xBBu8; 32]);
    hash_data[64..96].copy_from_slice(&[0xCCu8; 32]);

    let hashes = Bytes::from(hash_data);
    let response = HashResponse::from_raw(root, 0, 0, 2, 1, &hashes);

    assert!(response.is_some());
    let response = response.unwrap();
    assert_eq!(response.layer_hashes.len(), 2);
    assert_eq!(response.proof_hashes.len(), 1);
    assert_eq!(response.layer_hashes[0], [0xAAu8; 32]);
    assert_eq!(response.layer_hashes[1], [0xBBu8; 32]);
    assert_eq!(response.proof_hashes[0], [0xCCu8; 32]);
}

#[test]
fn test_hash_response_from_raw_invalid_length() {
    use bytes::Bytes;

    use super::hash_request::HashResponse;

    let root = [0x12u8; 32];

    // Wrong length: expecting 96 bytes but providing 64
    let hash_data = vec![0u8; 64];
    let hashes = Bytes::from(hash_data);
    let response = HashResponse::from_raw(root, 0, 0, 2, 1, &hashes);

    assert!(response.is_none());
}

#[test]
fn test_hash_server_basic() {
    use super::hash_request::HashServer;
    use crate::metainfo::MerkleTree;

    let server = HashServer::new();

    // Create a simple merkle tree with 4 leaves
    let leaves: Vec<[u8; 32]> = (0..4u8)
        .map(|i| {
            let mut h = [0u8; 32];
            h[0] = i;
            h
        })
        .collect();

    let tree = MerkleTree::from_piece_hashes(leaves);
    let root = tree.root().unwrap();

    // Register the tree
    server.register_tree(root, tree);
    assert!(server.has_tree(&root));

    // Generate a response
    let response = server.generate_response(root, 0, 0, 2, 1);
    assert!(response.is_some());

    // Unregister
    server.unregister_tree(&root);
    assert!(!server.has_tree(&root));
}

#[test]
fn test_hash_server_generate_response_unknown_root() {
    use super::hash_request::HashServer;

    let server = HashServer::new();
    let unknown_root = [0xFFu8; 32];

    // Should return None for unknown root
    let response = server.generate_response(unknown_root, 0, 0, 2, 0);
    assert!(response.is_none());
}

#[test]
fn test_pending_hash_request_key() {
    use super::hash_request::PendingHashRequest;

    let root = [0x42u8; 32];
    let req1 = PendingHashRequest::new(root, 0, 0, 4, 2);
    let req2 = PendingHashRequest::new(root, 0, 0, 4, 3); // different proof_layers

    // Keys should be equal (proof_layers not part of key)
    assert_eq!(req1.key().pieces_root, req2.key().pieces_root);
    assert_eq!(req1.key().base_layer, req2.key().base_layer);
    assert_eq!(req1.key().index, req2.key().index);
    assert_eq!(req1.key().length, req2.key().length);
}
