use std::path::PathBuf;

use tempfile::TempDir;

use super::*;

fn create_test_storage(temp: &TempDir, piece_length: u64, file_size: u64) -> TorrentStorage {
    let base_path = temp.path().to_path_buf();
    let piece_count = file_size.div_ceil(piece_length) as usize;

    let files = vec![FileEntry::new(PathBuf::from("test.dat"), file_size, 0)];

    let pieces: Vec<PieceInfo> = (0..piece_count)
        .map(|i| {
            let offset = i as u64 * piece_length;
            let length = if i == piece_count - 1 {
                let rem = file_size % piece_length;
                if rem == 0 {
                    piece_length
                } else {
                    rem
                }
            } else {
                piece_length
            };
            PieceInfo::v1(i as u32, [0u8; 20], offset, length)
        })
        .collect();

    TorrentStorage::new(base_path, files, pieces, file_size, false).expect("test storage creation")
}

#[tokio::test]
async fn test_preallocate() {
    let temp = TempDir::new().unwrap();
    let storage = create_test_storage(&temp, 16384, 65536);

    storage.preallocate().await.unwrap();

    let path = temp.path().join("test.dat");
    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 65536);
}

#[tokio::test]
async fn test_write_and_read_piece() {
    let temp = TempDir::new().unwrap();
    let storage = create_test_storage(&temp, 16384, 32768);

    storage.preallocate().await.unwrap();

    let data: Vec<u8> = (0..16384).map(|i| (i % 256) as u8).collect();
    storage.write_piece(0, &data).await.unwrap();

    let read_data = storage.read_piece(0).await.unwrap();
    assert_eq!(read_data.as_ref(), data.as_slice());
}

#[tokio::test]
async fn test_write_and_read_block() {
    let temp = TempDir::new().unwrap();
    let storage = create_test_storage(&temp, 32768, 65536);

    storage.preallocate().await.unwrap();

    let block_data: Vec<u8> = (0..16384).map(|i| (i % 256) as u8).collect();

    storage.write_block(0, 0, &block_data).await.unwrap();
    storage.write_block(0, 16384, &block_data).await.unwrap();

    let read_block = storage.read_block(0, 0, 16384).await.unwrap();
    assert_eq!(read_block.as_ref(), block_data.as_slice());

    let read_block2 = storage.read_block(0, 16384, 16384).await.unwrap();
    assert_eq!(read_block2.as_ref(), block_data.as_slice());
}

#[tokio::test]
async fn test_multifile_storage() {
    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_path_buf();

    let files = vec![
        FileEntry::new(PathBuf::from("file1.dat"), 10000, 0),
        FileEntry::new(PathBuf::from("file2.dat"), 10000, 10000),
    ];

    let pieces = vec![
        PieceInfo::v1(0, [0u8; 20], 0, 16384),
        PieceInfo::v1(1, [0u8; 20], 16384, 3616),
    ];

    let storage =
        TorrentStorage::new(base_path, files, pieces, 20000, false).expect("test storage creation");
    storage.preallocate().await.unwrap();

    let data: Vec<u8> = (0..16384).map(|i| (i % 256) as u8).collect();
    storage.write_piece(0, &data).await.unwrap();

    let read_data = storage.read_piece(0).await.unwrap();
    assert_eq!(read_data.as_ref(), data.as_slice());
}

#[tokio::test]
async fn test_disk_manager() {
    let temp = TempDir::new().unwrap();
    let storage = create_test_storage(&temp, 16384, 32768);
    storage.preallocate().await.unwrap();

    let manager = DiskManager::new();
    manager.register("test_hash".to_string(), storage);

    let data: Vec<u8> = (0..16384).map(|i| (i % 256) as u8).collect();
    manager.write_piece("test_hash", 0, &data).await.unwrap();

    let read_data = manager.read_piece("test_hash", 0).await.unwrap();
    assert_eq!(read_data.as_ref(), data.as_slice());

    manager.unregister("test_hash");
    assert!(manager.read_piece("test_hash", 0).await.is_err());
}

#[tokio::test]
async fn test_invalid_piece_index() {
    let temp = TempDir::new().unwrap();
    let storage = create_test_storage(&temp, 16384, 32768);
    storage.preallocate().await.unwrap();

    let result = storage.read_piece(999).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_invalid_block_offset() {
    let temp = TempDir::new().unwrap();
    let storage = create_test_storage(&temp, 16384, 32768);
    storage.preallocate().await.unwrap();

    let result = storage.read_block(0, 20000, 1000).await;
    assert!(result.is_err());
}

// ============================================================================
// V2 (BEP-52) Storage Tests
// ============================================================================

use crate::metainfo::compute_piece_root;

/// Creates a v2 test storage with merkle tree verification.
fn create_v2_test_storage(
    temp: &TempDir,
    piece_length: u64,
    file_sizes: &[u64],
    piece_hashes: &[[u8; 32]],
) -> TorrentStorage {
    let base_path = temp.path().to_path_buf();

    // Create file entries for each file (v2 style: piece-aligned, with merkle roots)
    let mut files = Vec::new();
    let mut offset = 0u64;
    let total_length: u64 = file_sizes.iter().sum();

    for (i, &size) in file_sizes.iter().enumerate() {
        // For v2, pieces_root would normally be the file's merkle root
        // For testing, we compute it from the expected piece data
        let pieces_root = if size > 0 {
            // Placeholder - actual merkle root would be computed from file data
            Some([0u8; 32])
        } else {
            None
        };

        files.push(FileEntry::new_v2(
            PathBuf::from(format!("file{}.dat", i)),
            size,
            offset,
            pieces_root,
            false,
        ));

        // In v2, files are piece-aligned
        if size > 0 {
            let pieces_for_file = size.div_ceil(piece_length);
            offset += pieces_for_file * piece_length;
        }
    }

    // Build piece info with v2 hashes
    let v2_map = V2PieceMap::new(&files, piece_length);
    let pieces = v2_map
        .build_piece_info(&files, piece_hashes)
        .expect("piece info creation");

    TorrentStorage::with_piece_length(base_path, files, pieces, total_length, true, piece_length)
        .expect("v2 test storage creation")
}

#[tokio::test]
async fn test_v2_single_piece_verification() {
    let temp = TempDir::new().unwrap();

    // Create a single piece of data (16 KiB = 1 merkle block)
    let piece_data: Vec<u8> = (0..16384).map(|i| (i % 256) as u8).collect();

    // Compute the expected merkle root for this piece
    let expected_root = compute_piece_root(&piece_data, 16384);

    // Create storage with the correct piece hash
    let storage = create_v2_test_storage(&temp, 16384, &[16384], &[expected_root]);
    storage.preallocate().await.unwrap();

    // Write the piece
    storage.write_piece(0, &piece_data).await.unwrap();

    // Verify the piece - should pass
    let valid = storage.verify_piece(0).await.unwrap();
    assert!(valid, "v2 piece verification should pass with correct hash");
}

#[tokio::test]
async fn test_v2_multi_block_piece_verification() {
    let temp = TempDir::new().unwrap();

    // Create a piece with 4 blocks (64 KiB total)
    let piece_length = 65536u64;
    let piece_data: Vec<u8> = (0..piece_length as usize)
        .map(|i| (i % 256) as u8)
        .collect();

    // Compute the expected merkle root
    let expected_root = compute_piece_root(&piece_data, piece_length);

    let storage = create_v2_test_storage(&temp, piece_length, &[piece_length], &[expected_root]);
    storage.preallocate().await.unwrap();

    // Write the piece
    storage.write_piece(0, &piece_data).await.unwrap();

    // Verify
    let valid = storage.verify_piece(0).await.unwrap();
    assert!(valid, "v2 multi-block piece verification should pass");
}

#[tokio::test]
async fn test_v2_partial_piece_verification() {
    let temp = TempDir::new().unwrap();

    // Create a partial piece (last piece of file, smaller than piece_length)
    let piece_length = 65536u64; // 4 blocks
    let actual_size = 40000u64; // Less than 3 full blocks

    let piece_data: Vec<u8> = (0..actual_size as usize).map(|i| (i % 256) as u8).collect();

    // Compute merkle root with proper padding for partial piece
    let expected_root = compute_piece_root(&piece_data, piece_length);

    let storage = create_v2_test_storage(&temp, piece_length, &[actual_size], &[expected_root]);
    storage.preallocate().await.unwrap();

    // Write the piece
    storage.write_piece(0, &piece_data).await.unwrap();

    // Verify - should pad with zero hashes and still match
    let valid = storage.verify_piece(0).await.unwrap();
    assert!(
        valid,
        "v2 partial piece verification should pass with padding"
    );
}

#[tokio::test]
async fn test_v2_verification_fails_wrong_data() {
    let temp = TempDir::new().unwrap();

    let piece_length = 16384u64;
    let piece_data: Vec<u8> = (0..piece_length as usize)
        .map(|i| (i % 256) as u8)
        .collect();

    // Compute correct hash
    let expected_root = compute_piece_root(&piece_data, piece_length);

    let storage = create_v2_test_storage(&temp, piece_length, &[piece_length], &[expected_root]);
    storage.preallocate().await.unwrap();

    // Write WRONG data
    let wrong_data: Vec<u8> = vec![0xFF; piece_length as usize];
    storage.write_piece(0, &wrong_data).await.unwrap();

    // Verify should fail
    let valid = storage.verify_piece(0).await.unwrap();
    assert!(!valid, "v2 verification should fail with wrong data");
}

#[tokio::test]
async fn test_v2_multifile_storage() {
    let temp = TempDir::new().unwrap();

    let piece_length = 16384u64;

    // Two files: first = 1 piece, second = 2 pieces
    let file1_data: Vec<u8> = (0..piece_length as usize).map(|i| i as u8).collect();
    let file2_piece0: Vec<u8> = (0..piece_length as usize)
        .map(|i| (i + 100) as u8)
        .collect();
    let file2_piece1: Vec<u8> = (0..8000usize).map(|i| (i + 200) as u8).collect(); // Partial

    // Compute merkle roots
    let hash0 = compute_piece_root(&file1_data, piece_length);
    let hash1 = compute_piece_root(&file2_piece0, piece_length);
    let hash2 = compute_piece_root(&file2_piece1, piece_length);

    let storage = create_v2_test_storage(
        &temp,
        piece_length,
        &[piece_length, piece_length + 8000],
        &[hash0, hash1, hash2],
    );
    storage.preallocate().await.unwrap();

    // Write all pieces
    storage.write_piece(0, &file1_data).await.unwrap();
    storage.write_piece(1, &file2_piece0).await.unwrap();
    storage.write_piece(2, &file2_piece1).await.unwrap();

    // Verify all pieces
    assert!(
        storage.verify_piece(0).await.unwrap(),
        "file1 piece should verify"
    );
    assert!(
        storage.verify_piece(1).await.unwrap(),
        "file2 piece0 should verify"
    );
    assert!(
        storage.verify_piece(2).await.unwrap(),
        "file2 piece1 (partial) should verify"
    );
}

#[tokio::test]
async fn test_v2_piece_map_mapping() {
    // Test the V2PieceMap correctly maps global indices to files
    let files = vec![
        FileEntry::new_v2(PathBuf::from("a.txt"), 32768, 0, Some([1u8; 32]), false), // 2 pieces
        FileEntry::new_v2(PathBuf::from("b.txt"), 16384, 32768, Some([2u8; 32]), false), // 1 piece
        FileEntry::new_v2(PathBuf::from("c.txt"), 0, 49152, None, false), // 0 pieces (empty)
        FileEntry::new_v2(PathBuf::from("d.txt"), 50000, 49152, Some([3u8; 32]), false), // 4 pieces
    ];

    let piece_length = 16384u64;
    let map = V2PieceMap::new(&files, piece_length);

    // Total should be 2 + 1 + 0 + 4 = 7 pieces
    assert_eq!(map.total_pieces(), 7);

    // Test global_to_file mapping
    assert_eq!(map.global_to_file(0), Some((0, 0))); // file a.txt, piece 0
    assert_eq!(map.global_to_file(1), Some((0, 1))); // file a.txt, piece 1
    assert_eq!(map.global_to_file(2), Some((1, 0))); // file b.txt, piece 0
    assert_eq!(map.global_to_file(3), Some((3, 0))); // file d.txt, piece 0 (skips empty c.txt)
    assert_eq!(map.global_to_file(6), Some((3, 3))); // file d.txt, piece 3
    assert_eq!(map.global_to_file(7), None); // Out of bounds

    // Test file_to_global mapping
    assert_eq!(map.file_to_global(0, 0), Some(0));
    assert_eq!(map.file_to_global(0, 1), Some(1));
    assert_eq!(map.file_to_global(1, 0), Some(2));
    assert_eq!(map.file_to_global(3, 0), Some(3));
    assert_eq!(map.file_to_global(3, 3), Some(6));
}

#[tokio::test]
async fn test_v2_piece_map_build_piece_info() {
    let piece_length = 16384u64;

    let files = vec![
        FileEntry::new_v2(PathBuf::from("a.txt"), 20000, 0, Some([1u8; 32]), false), // 2 pieces
        FileEntry::new_v2(PathBuf::from("b.txt"), 16384, 16384, Some([2u8; 32]), false), // 1 piece
    ];

    let map = V2PieceMap::new(&files, piece_length);

    // Create piece hashes
    let hashes: [[u8; 32]; 3] = [[0xAA; 32], [0xBB; 32], [0xCC; 32]];

    let pieces = map.build_piece_info(&files, &hashes).unwrap();

    assert_eq!(pieces.len(), 3);

    // First piece: full piece length
    assert_eq!(pieces[0].index, 0);
    assert_eq!(pieces[0].length, 16384);
    assert_eq!(pieces[0].hash_v2(), Some([0xAA; 32]));

    // Second piece: partial (20000 - 16384 = 3616 bytes)
    assert_eq!(pieces[1].index, 1);
    assert_eq!(pieces[1].length, 3616);
    assert_eq!(pieces[1].hash_v2(), Some([0xBB; 32]));

    // Third piece: from second file, full length
    assert_eq!(pieces[2].index, 2);
    assert_eq!(pieces[2].length, 16384);
    assert_eq!(pieces[2].hash_v2(), Some([0xCC; 32]));
}

#[tokio::test]
async fn test_v2_verify_piece_merkle() {
    let temp = TempDir::new().unwrap();

    let piece_length = 32768u64; // 2 merkle blocks
    let piece_data: Vec<u8> = (0..piece_length as usize)
        .map(|i| (i % 256) as u8)
        .collect();

    let expected_root = compute_piece_root(&piece_data, piece_length);

    let storage = create_v2_test_storage(&temp, piece_length, &[piece_length], &[expected_root]);
    storage.preallocate().await.unwrap();
    storage.write_piece(0, &piece_data).await.unwrap();

    // Use the explicit merkle verification method
    let valid = storage
        .verify_piece_merkle(0, &expected_root)
        .await
        .unwrap();
    assert!(valid, "verify_piece_merkle should pass");

    // Try with wrong root
    let wrong_root = [0xFF; 32];
    let invalid = storage.verify_piece_merkle(0, &wrong_root).await.unwrap();
    assert!(!invalid, "verify_piece_merkle should fail with wrong root");
}

#[tokio::test]
async fn test_v2_get_piece_hash() {
    let temp = TempDir::new().unwrap();

    let piece_length = 16384u64;
    let expected_hash = [0xDE; 32];

    let storage = create_v2_test_storage(&temp, piece_length, &[piece_length], &[expected_hash]);

    // Should return the v2 hash
    let hash = storage.get_v2_piece_hash(0);
    assert_eq!(hash, Some(expected_hash));

    // Out of bounds should return None
    assert_eq!(storage.get_v2_piece_hash(99), None);
}

#[tokio::test]
async fn test_v2_verify_all() {
    let temp = TempDir::new().unwrap();

    let piece_length = 16384u64;

    // Create 3 pieces of data
    let piece0: Vec<u8> = vec![0x00; piece_length as usize];
    let piece1: Vec<u8> = vec![0x11; piece_length as usize];
    let piece2: Vec<u8> = vec![0x22; piece_length as usize];

    let hash0 = compute_piece_root(&piece0, piece_length);
    let hash1 = compute_piece_root(&piece1, piece_length);
    let hash2 = compute_piece_root(&piece2, piece_length);

    let storage = create_v2_test_storage(
        &temp,
        piece_length,
        &[piece_length * 3],
        &[hash0, hash1, hash2],
    );
    storage.preallocate().await.unwrap();

    // Write all pieces
    storage.write_piece(0, &piece0).await.unwrap();
    storage.write_piece(1, &piece1).await.unwrap();
    storage.write_piece(2, &piece2).await.unwrap();

    // Verify all at once
    let results = storage.verify_all().await.unwrap();
    assert_eq!(results.len(), 3);
    assert!(results[0], "piece 0 should verify");
    assert!(results[1], "piece 1 should verify");
    assert!(results[2], "piece 2 should verify");
}
