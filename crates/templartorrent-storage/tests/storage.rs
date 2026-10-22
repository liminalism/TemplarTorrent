//! File storage tests: real filesystem, owned temp roots only. Each test
//! creates its own directory (process id + counter makes parallel runs unique)
//! and removes it on scope exit; declare `TempRoot` before any store so file
//! handles close first, which Windows requires for removal.
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use templartorrent_core::model::TorrentId;
use templartorrent_engine::worker::{PieceReader, PieceSink};
use templartorrent_storage::{FileStore, StorageError};

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "templartorrent-storage-{tag}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const ID: TorrentId = TorrentId(41);

fn two_files() -> Vec<(Vec<String>, u64)> {
    vec![
        (vec!["a.bin".to_string()], 3000),
        (vec!["sub".to_string(), "b.bin".to_string()], 1000),
    ]
}

fn pattern(piece: u32, length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| (piece as u8).wrapping_mul(31).wrapping_add(index as u8))
        .collect()
}

#[test]
fn write_then_read_roundtrip_across_boundary() {
    let root = TempRoot::new("roundtrip");
    let mut store = FileStore::new();
    // 4000 bytes / 1024: pieces of 1024, 1024, 1024, 928. Piece 2 spans a/b.
    store.add_job(ID, root.path(), &two_files(), 1024).unwrap();
    for piece in 0..4u32 {
        let length = if piece < 3 { 1024 } else { 928 };
        store
            .write_piece(ID, piece, &pattern(piece, length))
            .unwrap();
    }
    for piece in 0..4u32 {
        let length = if piece < 3 { 1024 } else { 928 };
        assert_eq!(
            store.read_piece(ID, piece, length).unwrap(),
            Some(pattern(piece, length)),
            "piece {piece} mismatch"
        );
    }
    // On-disk layout: exact sizes, boundary bytes in the right files.
    let a = std::fs::read(root.path().join("a.bin")).unwrap();
    let b = std::fs::read(root.path().join("sub").join("b.bin")).unwrap();
    assert_eq!((a.len(), b.len()), (3000, 1000));
    let mut joined = a;
    joined.extend_from_slice(&b);
    let mut expected = Vec::new();
    for piece in 0..4u32 {
        expected.extend_from_slice(&pattern(piece, if piece < 3 { 1024 } else { 928 }));
    }
    assert_eq!(joined, expected);
}

#[test]
fn durable_identity_tracks_synced_bytes_and_refuses_a_replaced_file() {
    let root = TempRoot::new("identity");
    let mut store = FileStore::new();
    store.add_job(ID, root.path(), &two_files(), 1024).unwrap();
    let created = store.durable_identity(ID).unwrap();
    assert_eq!(created.len(), 2);
    assert_eq!(created[0].size, 0);
    assert_eq!(store.durable_identity(ID).unwrap(), created);
    store.write_piece(ID, 0, &pattern(0, 1024)).unwrap();
    let written = store.durable_identity(ID).unwrap();
    assert_eq!(written[0].size, 1024);
    assert_eq!(written[0].dev, created[0].dev);
    assert_eq!(written[0].ino, created[0].ino);
    assert_eq!(written[1], created[1], "the untouched file keeps its stamp");
    let path = root.path().join("a.bin");
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"replaced").unwrap();
    assert!(matches!(
        store.durable_identity(ID),
        Err(StorageError::IdentityChanged { .. })
    ));
}

#[test]
fn pre_existing_complete_files_read_back() {
    let root = TempRoot::new("preexisting");
    let mut full = Vec::new();
    for piece in 0..4u32 {
        full.extend_from_slice(&pattern(piece, if piece < 3 { 1024 } else { 928 }));
    }
    std::fs::create_dir_all(root.path().join("sub")).unwrap();
    std::fs::write(root.path().join("a.bin"), &full[..3000]).unwrap();
    std::fs::write(root.path().join("sub").join("b.bin"), &full[3000..]).unwrap();
    let store = FileStore::new();
    let mut store = store;
    store.add_job(ID, root.path(), &two_files(), 1024).unwrap();
    for piece in 0..4u32 {
        let length = if piece < 3 { 1024 } else { 928 };
        assert_eq!(
            store.read_piece(ID, piece, length).unwrap(),
            Some(pattern(piece, length)),
            "pre-existing piece {piece} unreadable"
        );
    }
}

#[test]
fn partial_files_read_as_missing_not_error() {
    let root = TempRoot::new("partial");
    std::fs::create_dir_all(root.path().join("sub")).unwrap();
    std::fs::write(root.path().join("a.bin"), vec![9u8; 3000]).unwrap();
    std::fs::write(root.path().join("sub").join("b.bin"), vec![9u8; 500]).unwrap();
    let mut store = FileStore::new();
    store.add_job(ID, root.path(), &two_files(), 1024).unwrap();
    // Pieces 0-2 sit within present bytes (piece 2 needs only b[0..72));
    // piece 3 needs b[72..1000] but the file ends at 500.
    for piece in 0..3u32 {
        assert!(
            store.read_piece(ID, piece, 1024).unwrap().is_some(),
            "piece {piece} should read"
        );
    }
    assert_eq!(store.read_piece(ID, 3, 928).unwrap(), None);
}

#[test]
fn longer_file_is_rejected_and_left_untouched() {
    let root = TempRoot::new("longer");
    std::fs::create_dir_all(root.path().join("sub")).unwrap();
    std::fs::write(root.path().join("a.bin"), vec![1u8; 3000]).unwrap();
    std::fs::write(root.path().join("sub").join("b.bin"), vec![1u8; 1001]).unwrap();
    let mut store = FileStore::new();
    let error = store
        .add_job(ID, root.path(), &two_files(), 1024)
        .unwrap_err();
    assert!(
        matches!(error, StorageError::LengthMismatch { found: 1001, .. }),
        "wrong error: {error}"
    );
    assert_eq!(
        std::fs::metadata(root.path().join("sub").join("b.bin"))
            .unwrap()
            .len(),
        1001,
        "overlong file must never be truncated"
    );
}

#[test]
fn hostile_paths_are_rejected() {
    let root = TempRoot::new("hostile");
    for hostile in [
        vec!["..".to_string()],
        vec!["".to_string()],
        vec!["CON".to_string()],
        vec!["a/b".to_string()],
        vec!["x.".to_string()],
    ] {
        let mut store = FileStore::new();
        assert!(
            matches!(
                store.add_job(ID, root.path(), &[(hostile, 10)], 1024),
                Err(StorageError::Path(_))
            ),
            "hostile path accepted"
        );
    }
}

#[test]
fn oversize_and_zero_pieces_are_gated() {
    let root = TempRoot::new("gated");
    for piece_length in [0, templartorrent_storage::MAX_STORED_PIECE_LENGTH + 1] {
        let mut store = FileStore::new();
        assert_eq!(
            store
                .add_job(ID, root.path(), &two_files(), piece_length)
                .unwrap_err(),
            StorageError::PieceTooLarge(piece_length)
        );
    }
}

#[test]
fn unknown_job_piece_and_duplicate_are_explicit() {
    let root = TempRoot::new("explicit");
    let mut store = FileStore::new();
    store.add_job(ID, root.path(), &two_files(), 1024).unwrap();
    assert_eq!(
        store
            .add_job(ID, root.path(), &two_files(), 1024)
            .unwrap_err(),
        StorageError::DuplicateJob(ID)
    );
    assert!(store.read_piece(TorrentId(7), 0, 1024).is_err());
    assert!(store.write_piece(TorrentId(7), 0, &[0u8; 1024]).is_err());
    assert!(store.read_piece(ID, 99, 1024).is_err());
    assert!(store.write_piece(ID, 99, &[0u8; 10]).is_err());
    // A caller/engine length disagreement reads as missing, never verified.
    assert_eq!(store.read_piece(ID, 0, 1023).unwrap(), None);
}

#[test]
fn closing_job_releases_storage_without_deleting_files() {
    let root = TempRoot::new("close");
    let mut store = FileStore::new();
    store.add_job(ID, root.path(), &two_files(), 1024).unwrap();
    store.write_piece(ID, 0, &[3; 1024]).unwrap();
    assert!(store.close_job(ID));
    assert!(!store.close_job(ID));
    assert_eq!(
        store.read_piece(ID, 0, 1024),
        Err(templartorrent_engine::worker::SinkError::UnknownJob)
    );
    assert_eq!(std::fs::read(root.path().join("a.bin")).unwrap(), [3; 1024]);
    std::fs::rename(root.path().join("a.bin"), root.path().join("retained.bin")).unwrap();
}

#[test]
fn delete_removes_only_job_files_and_keeps_the_root() {
    let root = TempRoot::new("delete");
    let mut store = FileStore::new();
    store.add_job(ID, root.path(), &two_files(), 1024).unwrap();
    store.write_piece(ID, 0, &[7; 1024]).unwrap();
    std::fs::write(root.path().join("keep.txt"), b"keep").unwrap();
    store.delete_job_data(ID).unwrap();
    assert!(!root.path().join("a.bin").exists());
    assert!(!root.path().join("sub").exists());
    assert_eq!(
        std::fs::read(root.path().join("keep.txt")).unwrap(),
        b"keep"
    );
    assert!(root.path().is_dir());
    assert!(matches!(
        store.preflight_delete(ID),
        Err(StorageError::UnknownJob(_))
    ));
}

#[cfg(unix)]
#[test]
fn delete_refuses_hardlink_symlink_and_swapped_inode_without_removing_the_job() {
    let root = TempRoot::new("delete-refuse");
    let mut store = FileStore::new();
    store.add_job(ID, root.path(), &two_files(), 1024).unwrap();
    let episode = root.path().join("a.bin");
    std::fs::hard_link(&episode, root.path().join("a-link.bin")).unwrap();
    assert!(matches!(
        store.preflight_delete(ID),
        Err(StorageError::Hardlink { .. })
    ));
    assert!(episode.exists());
    assert!(store.read_piece(ID, 0, 1024).is_ok());
    std::fs::remove_file(root.path().join("a-link.bin")).unwrap();

    std::fs::remove_file(root.path().join("sub").join("b.bin")).unwrap();
    std::os::unix::fs::symlink(&episode, root.path().join("sub").join("b.bin")).unwrap();
    assert!(matches!(
        store.preflight_delete(ID),
        Err(StorageError::Symlink { .. })
    ));
    std::fs::remove_file(root.path().join("sub").join("b.bin")).unwrap();
    std::fs::write(root.path().join("sub").join("b.bin"), vec![1u8; 1000]).unwrap();
    assert!(matches!(
        store.preflight_delete(ID),
        Err(StorageError::IdentityChanged { .. })
    ));
    assert_eq!(
        std::fs::metadata(root.path().join("sub").join("b.bin"))
            .unwrap()
            .len(),
        1000
    );
    assert!(store.close_job(ID));
}

#[cfg(unix)]
#[test]
fn symlinks_are_refused() {
    let root = TempRoot::new("symlink");
    let target = root.path().join("real.bin");
    std::fs::write(&target, vec![0u8; 3000]).unwrap();
    std::os::unix::fs::symlink(&target, root.path().join("a.bin")).unwrap();
    let mut store = FileStore::new();
    assert!(
        matches!(
            store.add_job(ID, root.path(), &[(vec!["a.bin".to_string()], 3000)], 1024),
            Err(StorageError::Symlink { .. })
        ),
        "symlinked file accepted"
    );
    std::fs::remove_file(root.path().join("a.bin")).unwrap();
    let outside = TempRoot::new("symlink-outside");
    std::os::unix::fs::symlink(outside.path(), root.path().join("sub")).unwrap();
    let mut store = FileStore::new();
    assert!(
        store.add_job(ID, root.path(), &two_files(), 1024).is_err(),
        "symlinked parent accepted"
    );
}
