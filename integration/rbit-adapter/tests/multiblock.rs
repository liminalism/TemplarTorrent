//! Owned v1 fixtures only. Real rbit TCP + HTTP tracker, SHA1, file writes and
//! restart recheck. No internet endpoints, public discovery or generated progress.
use bytes::Bytes;
use rbit::{
    InfoHash, Message, Metainfo, PeerConnection, TorrentBuilder, TorrentVersion, TrackerEvent,
};
use sha1::{Digest, Sha1};
use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use templartorrent_core::{
    model::{JobState, Limits, TorrentId},
    sequence::FileSpec,
    tracker::TrackerClock,
};
use templartorrent_engine::{
    peer::{PeerAction, PeerMessage},
    session::{JobSpec, Session},
    worker::{PieceReader, PieceVerifier},
};
use templartorrent_rbit_adapter::{
    Source,
    announce::{AnnounceRequest, announce},
    discovery::parse_tracker_url,
    parse_magnet, record_tracker_success, unpack_bitfield,
};
use templartorrent_storage::SharedStore;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

const LENGTH: usize = 256 * 1024;
const DEADLINE: Duration = Duration::from_secs(20);
const SEED: [u8; 20] = [17; 20];
const LEECH: [u8; 20] = [19; 20];

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "templartorrent-multiblock-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // Exclusive creation: cleanup must never own a pre-existing directory.
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    data: Vec<u8>,
    meta: Metainfo,
    hash: [u8; 20],
}
impl Fixture {
    fn new(piece_length: usize, size: usize) -> Arc<Self> {
        let data: Vec<_> = (0..size)
            .map(|i| {
                ((i as u64)
                    .wrapping_mul(0x9e37_79b9)
                    .rotate_left((i % 61) as u32)
                    >> 24) as u8
            })
            .collect();
        let split = piece_length.min(size / 2);
        // Natural file priority reverses wire order. A boundary and short tail
        // remain in the concatenated stream, as required by v1 hashing.
        let torrent = TorrentBuilder::new("owned-multiblock")
            .version(TorrentVersion::V1)
            .piece_length(piece_length as u64)
            .add_file("Episode02.bin", data[..split].to_vec())
            .add_file("Episode01.bin", data[split..].to_vec())
            .build()
            .unwrap();
        let meta = Metainfo::from_bytes(&torrent).unwrap();
        let InfoHash::V1(hash) = meta.info_hash else {
            panic!("expected v1");
        };
        for (index, chunk) in data.chunks(piece_length).enumerate() {
            assert_eq!(
                meta.info.get_v1_piece_hash(index),
                Some(Sha1::digest(chunk).into())
            );
        }
        Arc::new(Self { data, meta, hash })
    }
    fn len(&self, piece: u32) -> usize {
        let length = self.meta.info.piece_length as usize;
        (self.data.len() - piece as usize * length).min(length)
    }
    fn session(&self, root: &Path) -> (Session, SharedStore, TorrentId) {
        let store = SharedStore::new();
        let limits = Limits {
            payload_budget_bytes: 3 * self.meta.info.piece_length as usize,
            ..Limits::default()
        };
        let mut session = Session::new(limits, vec![Box::new(store.clone())], 1, 0).unwrap();
        let mut files: Vec<_> = self.meta.info.files.iter().collect();
        files.sort_by_key(|file| file.offset);
        let id = session
            .add(JobSpec {
                name: self.meta.info.name.clone(),
                piece_length: self.meta.info.piece_length,
                max_pieces: 4096,
                files: files
                    .iter()
                    .map(|f| FileSpec {
                        path: f.path.to_str().unwrap().into(),
                        length: f.length,
                        wanted: true,
                    })
                    .collect(),
            })
            .unwrap();
        let paths: Vec<_> = files
            .iter()
            .map(|f| {
                (
                    f.path
                        .components()
                        .map(|c| c.as_os_str().to_str().unwrap().to_string())
                        .collect(),
                    f.length,
                )
            })
            .collect();
        store
            .add_job(id, root, &paths, self.meta.info.piece_length)
            .unwrap();
        (session, store, id)
    }
    fn assert_files(&self, root: &Path) {
        for f in &self.meta.info.files {
            let actual = std::fs::read(root.join(&f.path)).unwrap();
            assert_eq!(
                actual,
                self.data[f.offset as usize..(f.offset + f.length) as usize]
            );
        }
    }
}
impl PieceVerifier for Fixture {
    fn verify(&self, piece: u32, data: &[u8]) -> bool {
        self.meta
            .info
            .get_v1_piece_hash(piece as usize)
            .is_some_and(|expected| expected == <[u8; 20]>::from(Sha1::digest(data)))
    }
}

#[derive(Clone, Copy, Default)]
struct Fault {
    corrupt_once: bool,
    disconnect_after: Option<usize>,
    reverse_batches: bool,
}

async fn listen() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

/// One owned test seeder. If supplied, reads persisted pieces from the completed
/// downloader's store, rather than serving the original fixture allocation.
async fn seed(
    listener: TcpListener,
    fixture: Arc<Fixture>,
    fault: Fault,
    disk: Option<(SharedStore, TorrentId)>,
) {
    let (stream, remote) = listener.accept().await.unwrap();
    assert!(remote.ip().is_loopback());
    let mut conn = PeerConnection::accept(stream, fixture.hash, SEED)
        .await
        .unwrap();
    let count = fixture.meta.info.piece_count();
    let mut bits = vec![0; count.div_ceil(8)];
    for piece in 0..count {
        bits[piece / 8] |= 0x80 >> (piece % 8);
    }
    conn.send(Message::Bitfield(Bytes::from(bits)))
        .await
        .unwrap();
    conn.send(Message::Unchoke).await.unwrap();
    let mut served = 0;
    let mut corrupted = false;
    let mut batch = Vec::new();
    while let Ok(message) = conn.receive().await {
        match message {
            Message::Request {
                index,
                begin,
                length,
            } => {
                assert!((index as usize) < count && length > 0 && length <= 16384);
                assert!((begin as usize) + length as usize <= fixture.len(index));
                if fault.disconnect_after.is_some_and(|limit| served >= limit) {
                    return;
                }
                let mut data = if let Some((store, id)) = &disk {
                    let piece = store
                        .read_piece(*id, index, fixture.len(index))
                        .unwrap()
                        .unwrap();
                    assert!(
                        fixture.verify(index, &piece),
                        "disk seeder must serve verified data"
                    );
                    piece[begin as usize..(begin + length) as usize].to_vec()
                } else {
                    let start =
                        index as usize * fixture.meta.info.piece_length as usize + begin as usize;
                    fixture.data[start..start + length as usize].to_vec()
                };
                if fault.corrupt_once && !corrupted {
                    data[0] ^= 0xff;
                    corrupted = true;
                }
                batch.push(Message::Piece {
                    index,
                    begin,
                    data: Bytes::from(data),
                });
                if !fault.reverse_batches
                    || batch.len() == 8
                    || (begin + length) as usize == fixture.len(index)
                {
                    for message in batch.drain(..).rev() {
                        if conn.send(message).await.is_err() {
                            return;
                        }
                        served += 1;
                    }
                }
            }
            Message::Interested
            | Message::NotInterested
            | Message::Cancel { .. }
            | Message::KeepAlive => {}
            _ => panic!("unexpected control message at owned seeder"),
        }
    }
}

/// Bounded one-shot HTTP tracker. Never records a request URL (it contains the
/// fixture's infohash); only asserts protocol properties before returning peers.
async fn tracker(listener: TcpListener, peer: SocketAddr) {
    let (mut stream, addr) = listener.accept().await.unwrap();
    assert!(addr.ip().is_loopback());
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        assert!(request.len() < 8192);
        let mut byte = [0];
        stream.read_exact(&mut byte).await.unwrap();
        request.push(byte[0]);
    }
    let header = String::from_utf8(request).unwrap();
    assert!(header.starts_with("GET /announce?"));
    assert!(header.contains("compact=1") && header.contains("event=started"));
    let mut body = b"d8:intervali60e12:min intervali30e5:peers6:".to_vec();
    body.extend_from_slice(&[127, 0, 0, 1]);
    body.extend_from_slice(&peer.port().to_be_bytes());
    body.push(b'e');
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.write_all(&body).await.unwrap();
}

#[derive(Default)]
struct Metrics {
    complete: bool,
    requested: Vec<(u32, u32)>,
    blamed: usize,
    reserved_peak: usize,
    rss_peak_sampled: usize,
    owned_peak_sampled: usize,
    first_verified_ns: Option<u128>,
    elapsed_ns: u128,
}
fn rss() -> usize {
    // Named Linux sample; other OSes report unavailable as zero, not a claim.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|line| line.starts_with("VmRSS:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|v| v.parse::<usize>().ok())
        })
        .unwrap_or(0)
        * 1024
}

async fn download(
    addr: SocketAddr,
    fixture: &Fixture,
    session: &mut Session,
    id: TorrentId,
) -> Metrics {
    assert!(addr.ip().is_loopback());
    let start = Instant::now();
    let mut conn = PeerConnection::connect(addr, fixture.hash, LEECH)
        .await
        .unwrap();
    let peer = session.connect(id).unwrap();
    let mut metrics = Metrics::default();
    loop {
        assert!(
            start.elapsed() < DEADLINE,
            "local transfer deadline exceeded"
        );
        let now_ns = start.elapsed().as_nanos() as u64;
        let report = session.update(now_ns, fixture);
        assert!(report.errors.is_empty(), "{report:?}");
        metrics.blamed += report.blamed.len();
        for entry in report.outbox {
            let message = match entry.action {
                PeerAction::Send(PeerMessage::Interested) => Message::Interested,
                PeerAction::Send(PeerMessage::NotInterested) => Message::NotInterested,
                PeerAction::Send(PeerMessage::Request {
                    piece,
                    begin,
                    length,
                }) => {
                    metrics.requested.push((piece, begin));
                    Message::Request {
                        index: piece,
                        begin,
                        length,
                    }
                }
                PeerAction::Send(PeerMessage::Cancel {
                    piece,
                    begin,
                    length,
                }) => Message::Cancel {
                    index: piece,
                    begin,
                    length,
                },
                _ => panic!("download-only driver must not emit upload controls"),
            };
            if conn.send(message).await.is_err() {
                session.disconnect(id, peer).unwrap();
                metrics.elapsed_ns = start.elapsed().as_nanos();
                return metrics;
            }
        }
        let snapshot = session.snapshot();
        let ownership = session.payload_stats();
        metrics.reserved_peak = ownership.reserved_high_watermark;
        metrics.owned_peak_sampled = metrics.owned_peak_sampled.max(ownership.owned_bytes());
        metrics.rss_peak_sampled = metrics.rss_peak_sampled.max(rss());
        if metrics.first_verified_ns.is_none() && snapshot.jobs[0].verified_bytes > 0 {
            metrics.first_verified_ns = Some(start.elapsed().as_nanos());
        }
        if snapshot.jobs[0].state == JobState::Seeding {
            metrics.complete = true;
            break;
        }
        match timeout(Duration::from_millis(2), conn.receive()).await {
            Ok(Ok(Message::Bitfield(bits))) => {
                let mut words = vec![0; fixture.meta.info.piece_count().div_ceil(64)];
                unpack_bitfield(&bits, fixture.meta.info.piece_count(), &mut words).unwrap();
                session.note_bitfield(id, peer, words).unwrap();
            }
            Ok(Ok(Message::Unchoke)) => session.set_peer_choking(id, peer, false).unwrap(),
            Ok(Ok(Message::Piece { index, begin, data })) => session
                .inject_block(id, peer, index, begin, data.to_vec())
                .unwrap(),
            Ok(Ok(Message::KeepAlive)) | Err(_) => {}
            Ok(Err(_)) => {
                session.disconnect(id, peer).unwrap();
                break;
            }
            _ => panic!("unexpected leecher message"),
        }
    }
    metrics.elapsed_ns = start.elapsed().as_nanos();
    metrics
}

#[tokio::test]
async fn tracker_to_multiblock_transfer_then_seed_from_disk() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(LENGTH, 2 * LENGTH + 73);
        let listener = listen().await;
        let addr = listener.local_addr().unwrap();
        let tracker_listener = listen().await;
        let tracker_addr = tracker_listener.local_addr().unwrap();
        let tracker_url = format!("http://{tracker_addr}/announce");
        let fixture_hash = fixture
            .hash
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let Source::Magnet(magnet) = parse_magnet(&format!(
            "magnet:?xt=urn:btih:{fixture_hash}&tr={tracker_url}"
        ))
        .unwrap() else {
            unreachable!()
        };
        let InfoHash::V1(hash) = magnet.info_hash else {
            panic!("expected v1 magnet");
        };
        let tracker_task = tokio::spawn(tracker(tracker_listener, addr));
        let mut clock = TrackerClock::new();
        assert!(clock.begin(0, false));
        let tracker = parse_tracker_url(&magnet.trackers[0]).unwrap();
        let response = announce(
            &tracker,
            tracker_addr,
            &AnnounceRequest {
                info_hash: &hash,
                peer_id: &LEECH,
                port: 6881,
                uploaded: 0,
                downloaded: 0,
                left: fixture.data.len() as u64,
                event: TrackerEvent::Started,
            },
        )
        .await
        .unwrap();
        record_tracker_success(&mut clock, 0, &response);
        assert!(
            !clock.begin(29, true),
            "manual announce must honor minimum interval"
        );
        assert_eq!(response.peers, [addr]);
        tracker_task.await.unwrap();
        let seed_task = tokio::spawn(seed(
            listener,
            fixture.clone(),
            Fault {
                reverse_batches: true,
                ..Fault::default()
            },
            None,
        ));
        let (mut session, store, id) = fixture.session(&root.0);
        let metrics = download(response.peers[0], &fixture, &mut session, id).await;
        assert!(metrics.complete);
        assert_eq!(metrics.requested.len(), 33);
        assert_ne!(
            metrics.requested[0].0, 0,
            "first natural file precedes wire-order file"
        );
        assert_eq!(
            metrics
                .requested
                .iter()
                .copied()
                .collect::<HashSet<_>>()
                .len(),
            33
        );
        assert!(metrics.reserved_peak <= 3 * LENGTH);
        fixture.assert_files(&root.0);
        seed_task.await.unwrap();
        // Same verified files, another wire connection, no original fixture as source.
        let target = Root::new();
        let listener = listen().await;
        let addr = listener.local_addr().unwrap();
        let seed_task = tokio::spawn(seed(
            listener,
            fixture.clone(),
            Fault::default(),
            Some((store.clone(), id)),
        ));
        let (mut third, third_store, third_id) = fixture.session(&target.0);
        assert!(
            download(addr, &fixture, &mut third, third_id)
                .await
                .complete
        );
        fixture.assert_files(&target.0);
        seed_task.await.unwrap();
        drop(third);
        drop(third_store);
    })
    .await
    .expect("tracker/download/seed deadline");
}

#[tokio::test]
async fn multiblock_corruption_retries_entire_piece_and_interrupted_resume_fetches_only_missing() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(LENGTH, 2 * LENGTH + 73);
        let listener = listen().await;
        let addr = listener.local_addr().unwrap();
        let seed_task = tokio::spawn(seed(
            listener,
            fixture.clone(),
            Fault {
                corrupt_once: true,
                ..Fault::default()
            },
            None,
        ));
        let (mut session, store, id) = fixture.session(&root.0);
        let metrics = download(addr, &fixture, &mut session, id).await;
        assert!(metrics.complete);
        assert_eq!(metrics.blamed, 1);
        // The corrupted block is in the first requested piece; all of its
        // blocks are retried (piece 2 is the one-block tail).
        let first = metrics.requested[0].0;
        let expected = if first == 2 { 2 } else { 32 };
        assert_eq!(
            metrics
                .requested
                .iter()
                .filter(|(p, _)| *p == first)
                .count(),
            expected,
            "all blocks of the corrupted piece retried after a bad hash"
        );
        fixture.assert_files(&root.0);
        seed_task.await.unwrap();
        drop(session);
        drop(store);

        let interrupted = Root::new();
        let listener = listen().await;
        let addr = listener.local_addr().unwrap();
        let seed_task = tokio::spawn(seed(
            listener,
            fixture.clone(),
            Fault {
                disconnect_after: Some(18),
                ..Fault::default()
            },
            None,
        ));
        let (mut session, store, id) = fixture.session(&interrupted.0);
        let partial = download(addr, &fixture, &mut session, id).await;
        assert!(!partial.complete);
        assert_eq!(
            session.snapshot().jobs[0].verified_bytes,
            LENGTH as u64 + 73
        );
        assert_eq!(
            session.snapshot().payload_budget_used,
            0,
            "disconnect releases partial assembly"
        );
        seed_task.await.unwrap();
        drop(session);
        drop(store); // Close every original handle before restart.
        let (mut restarted, store, id) = fixture.session(&interrupted.0);
        let recheck = restarted.recheck(id, &store, fixture.as_ref()).unwrap();
        assert_eq!(recheck.verified, 2);
        let listener = listen().await;
        let addr = listener.local_addr().unwrap();
        let seed_task = tokio::spawn(seed(listener, fixture.clone(), Fault::default(), None));
        let metrics = download(addr, &fixture, &mut restarted, id).await;
        assert!(metrics.complete);
        assert_eq!(metrics.requested.len(), 16);
        assert!(
            metrics.requested.iter().all(|(p, _)| *p == 0),
            "resume re-requested verified data"
        );
        fixture.assert_files(&interrupted.0);
        seed_task.await.unwrap();
    })
    .await
    .expect("corruption/restart deadline");
}

#[tokio::test]
async fn local_transfer_metrics() {
    timeout(DEADLINE, async {
        // Same owned bytes and disk path for both piece-size scenarios. These
        // are a current baseline, not a before/after optimization claim.
        for piece_length in [16_384, LENGTH] {
            let root = Root::new();
            let fixture = Fixture::new(piece_length, 2 * 1024 * 1024 + 73);
            let listener = listen().await;
            let addr = listener.local_addr().unwrap();
            let seed_task = tokio::spawn(seed(listener, fixture.clone(), Fault::default(), None));
            let (mut session, store, id) = fixture.session(&root.0);
            let m = download(addr, &fixture, &mut session, id).await;
            assert!(m.complete);
            fixture.assert_files(&root.0);
            assert_eq!(session.snapshot().payload_budget_used, 0);
            println!("TT_METRIC {{\"scenario\":\"localhost_file_backed_v1\",\"piece_length\":{piece_length},\"payload_bytes\":{},\"first_verified_ns\":{},\"elapsed_ns\":{},\"throughput_bytes_s\":{},\"payload_reserved_peak_bytes\":{},\"engine_owned_peak_sampled_bytes\":{},\"process_rss_peak_sampled_bytes\":{},\"profile\":\"{}\"}}",
                fixture.data.len(), m.first_verified_ns.unwrap(), m.elapsed_ns,
                fixture.data.len() as u128 * 1_000_000_000 / m.elapsed_ns,
                m.reserved_peak, m.owned_peak_sampled, m.rss_peak_sampled, if cfg!(debug_assertions) { "debug" } else { "release" });
            seed_task.await.unwrap();
            drop(session); drop(store);
        }
    }).await.expect("metric scenario deadline");
}
