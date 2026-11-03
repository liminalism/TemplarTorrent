//! All transfers use the public driver, real rbit peers, generated v1 fixtures
//! and exclusively created temporary roots. No internet discovery is enabled.
use bytes::Bytes;
use rbit::{
    ExtensionHandshake, InfoHash, METADATA_PIECE_SIZE, Message, MetadataMessage, Metainfo,
    PeerConnection, TorrentBuilder, TorrentVersion,
    peer::{EncryptionPolicy, mse},
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use templartorrent_core::model::{JobState, Limits, PeerPreference, TorrentRow};
use templartorrent_rbit_adapter::{
    driver::{
        Driver, ExistingData, JobRecord, PeerEncryption, PeerOrigin, PeerSource, ResumeRecord,
        ResumeScan,
    },
    metadata::OUR_MD_ID,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

const PIECE: usize = 65536;
const DEADLINE: Duration = Duration::from_secs(15);
fn process_rss() -> Option<usize> {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|line| line.starts_with("VmRSS:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|v| v.parse::<usize>().ok())
        })
        .map(|kb| kb * 1024)
}
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "templartorrent-driver-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn job(&self) -> PathBuf {
        self.0.join("job")
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
    fn new(salt: u8, private: bool) -> Arc<Self> {
        Self::with_version(salt, private, TorrentVersion::V1)
    }
    fn torrent_bytes(salt: u8, private: bool, version: TorrentVersion) -> (Vec<u8>, Vec<u8>) {
        Self::torrent_bytes_sized(salt, private, version, PIECE, 3 * PIECE + 73)
    }
    fn torrent_bytes_sized(
        salt: u8,
        private: bool,
        version: TorrentVersion,
        piece: usize,
        size: usize,
    ) -> (Vec<u8>, Vec<u8>) {
        let data: Vec<_> = (0..size)
            .map(|i| ((i / 16384) as u8).wrapping_mul(23) ^ (i as u8).wrapping_add(salt))
            .collect();
        let torrent = TorrentBuilder::new("owned-series")
            .version(version)
            .private(private)
            .piece_length(piece as u64)
            .add_file("Episode02.bin", data[..piece].to_vec())
            .add_file("Episode01.bin", data[piece..].to_vec())
            .build()
            .unwrap();
        (data, torrent)
    }
    /// Many small pieces: the scheduler, not the wire, is what this stresses.
    fn large(salt: u8, pieces: usize) -> Arc<Self> {
        let (data, torrent) =
            Self::torrent_bytes_sized(salt, false, TorrentVersion::V1, 16384, pieces * 16384);
        let meta = Metainfo::from_bytes(&torrent).unwrap();
        let InfoHash::V1(hash) = meta.info_hash else {
            panic!("fixture must be v1");
        };
        Arc::new(Self { data, meta, hash })
    }
    /// A hybrid fixture keeps its v1 info hash so a magnet resolves to it; the
    /// driver must then reject the metadata rather than treating it as v1.
    fn with_version(salt: u8, private: bool, version: TorrentVersion) -> Arc<Self> {
        let (data, torrent) = Self::torrent_bytes(salt, private, version);
        let meta = Metainfo::from_bytes(&torrent).unwrap();
        let hash = match meta.info_hash {
            InfoHash::V1(hash) => hash,
            InfoHash::Hybrid { v1, .. } => *v1.as_bytes(),
            InfoHash::V2(_) => panic!("fixture needs a v1 info hash"),
        };
        Arc::new(Self { data, meta, hash })
    }
    fn magnet(&self) -> String {
        let hex = self
            .hash
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        format!("magnet:?xt=urn:btih:{hex}")
    }
    fn assert_files(&self, root: &Path) {
        for file in &self.meta.info.files {
            assert_eq!(
                std::fs::read(root.join(&file.path)).unwrap(),
                self.data[file.offset as usize..(file.offset + file.length) as usize]
            );
        }
    }
}
#[derive(Default)]
struct Stats {
    requests: Mutex<Vec<(u32, u32)>>,
    metadata: AtomicUsize,
    active: AtomicUsize,
    /// TCP connections accepted, including ones whose handshake failed.
    accepted: AtomicUsize,
    corrupted: AtomicBool,
}
#[derive(Clone, Copy, Default)]
struct Fault {
    corrupt: bool,
    hold: bool,
    /// Record every request and keep reading, but never send a block.
    silent: bool,
    /// Sleep before answering each request so a transfer stays observable.
    delay_ms: u64,
    control_flood: bool,
    hold_metadata: bool,
    disconnect_after: Option<usize>,
}
struct Server {
    source: PeerSource,
    stats: Arc<Stats>,
    task: JoinHandle<()>,
}
impl Server {
    async fn start(fixture: Arc<Fixture>, fault: Fault) -> Self {
        Self::start_at("127.0.0.1", fixture, fault).await
    }
    /// Loopback accepts the whole 127/8 range, so fixtures can present
    /// distinct peer IP addresses without leaving the host.
    async fn start_at(ip: &str, fixture: Arc<Fixture>, fault: Fault) -> Self {
        let listener = TcpListener::bind((ip, 0)).await.unwrap();
        let source = PeerSource {
            address: listener.local_addr().unwrap(),
            origin: PeerOrigin::OriginalTracker(1),
        };
        let stats = Arc::new(Stats::default());
        let observed = stats.clone();
        let task = tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    incoming = listener.accept() => {
                        let (stream, addr) = incoming.unwrap(); assert!(addr.ip().is_loopback());
                        observed.accepted.fetch_add(1, Ordering::SeqCst);
                        let fixture = fixture.clone(); let stats = observed.clone();
                        clients.spawn(async move {
                            let Ok(mut conn) = PeerConnection::accept(stream, fixture.hash, FIXTURE_PEER_ID).await else { return; };
                            stats.active.fetch_add(1, Ordering::SeqCst);
                            let result = serve(&mut conn, &fixture, &stats, fault).await;
                            stats.active.fetch_sub(1, Ordering::SeqCst);
                            result
                        });
                    }
                    result = clients.join_next(), if !clients.is_empty() => {
                        result.unwrap().unwrap();
                    }
                }
            }
        });
        Self {
            source,
            stats,
            task,
        }
    }
    async fn close(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// An interested downloader that only records the choke state it receives.
struct Leech {
    source: PeerSource,
    seen: Arc<Mutex<Vec<&'static str>>>,
    task: JoinHandle<()>,
}
impl Leech {
    async fn start(ip: &str, hash: [u8; 20]) -> Self {
        let listener = TcpListener::bind((ip, 0)).await.unwrap();
        let source = PeerSource {
            address: listener.local_addr().unwrap(),
            origin: PeerOrigin::OriginalTracker(1),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let task = tokio::spawn(async move {
            // A plaintext-only peer: the driver's default first attempt is
            // MSE, which fails here, and then it redials in plaintext.
            let mut conn = loop {
                let (stream, addr) = listener.accept().await.unwrap();
                assert!(addr.ip().is_loopback());
                if let Ok(conn) = PeerConnection::accept(stream, hash, [71; 20]).await {
                    break conn;
                }
            };
            conn.send(Message::Interested).await.unwrap();
            while let Ok(message) = conn.receive().await {
                match message {
                    Message::Unchoke => observed.lock().unwrap().push("unchoke"),
                    Message::Choke => observed.lock().unwrap().push("choke"),
                    _ => {}
                }
            }
        });
        Self { source, seen, task }
    }
    fn seen(&self) -> Vec<&'static str> {
        self.seen.lock().unwrap().clone()
    }
    async fn close(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// Fixture peers present an Azureus-style id and send no extension `v`
/// name, so the Peers tab label has to come from BEP 20.
const FIXTURE_PEER_ID: [u8; 20] = *b"-TR4040-fixture-peer";

async fn serve(conn: &mut PeerConnection, fixture: &Fixture, stats: &Stats, fault: Fault) {
    let mut served = 0;
    let handshake = {
        let mut h = ExtensionHandshake::with_extensions(&[("ut_metadata", 7)]);
        h.metadata_size = Some(fixture.meta.raw_info().len() as i64);
        h.encode().unwrap()
    };
    let count = fixture.meta.info.piece_count();
    let mut bits = vec![0; count.div_ceil(8)];
    for piece in 0..count {
        bits[piece / 8] |= 0x80 >> (piece % 8);
    }
    for message in [Message::Bitfield(Bytes::from(bits)), Message::Unchoke] {
        if conn.send(message).await.is_err() {
            return;
        }
    }
    if fault.control_flood {
        for _ in 0..65 {
            if conn.send(Message::KeepAlive).await.is_err() {
                return;
            }
        }
    }
    if conn
        .send(Message::Extended {
            id: 0,
            payload: handshake,
        })
        .await
        .is_err()
    {
        return;
    }
    while let Ok(message) = conn.receive().await {
        match message {
            Message::Extended { id: 0, payload } => {
                let ours = ExtensionHandshake::decode(&payload).unwrap();
                assert_eq!(ours.get_extension_id("ut_metadata"), Some(OUR_MD_ID));
            }
            Message::Extended { id: 7, payload } => {
                let request = MetadataMessage::decode(&payload).unwrap();
                let start = request.piece as usize * METADATA_PIECE_SIZE;
                assert!(start < fixture.meta.raw_info().len());
                let end = (start + METADATA_PIECE_SIZE).min(fixture.meta.raw_info().len());
                let message = MetadataMessage::data(
                    request.piece,
                    fixture.meta.raw_info().len() as u32,
                    Bytes::copy_from_slice(&fixture.meta.raw_info()[start..end]),
                );
                stats.metadata.fetch_add(1, Ordering::SeqCst);
                if fault.hold_metadata {
                    std::future::pending::<()>().await;
                }
                if conn
                    .send(Message::Extended {
                        id: OUR_MD_ID,
                        payload: message.encode().unwrap(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Message::Request {
                index,
                begin,
                length,
            } => {
                if fault.disconnect_after.is_some_and(|limit| served >= limit) {
                    return;
                }
                served += 1;
                stats.requests.lock().unwrap().push((index, begin));
                let piece_len = fixture.meta.info.piece_length as usize;
                assert!((index as usize) < count && length > 0 && length <= 16384);
                let start = index as usize * piece_len + begin as usize;
                let end = start + length as usize;
                assert!(end <= fixture.data.len() && begin as usize + length as usize <= piece_len);
                if fault.silent {
                    continue;
                }
                if fault.delay_ms != 0 {
                    tokio::time::sleep(Duration::from_millis(fault.delay_ms)).await;
                }
                if fault.hold {
                    std::future::pending::<()>().await;
                }
                let mut data = fixture.data[start..end].to_vec();
                if fault.corrupt && !stats.corrupted.swap(true, Ordering::SeqCst) {
                    data[0] ^= 0xff;
                }
                if conn
                    .send(Message::Piece {
                        index,
                        begin,
                        data: Bytes::from(data),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Message::Interested
            | Message::NotInterested
            | Message::Cancel { .. }
            | Message::KeepAlive => {}
            Message::Bitfield(_) | Message::Have { .. } | Message::Choke => {}
            _ => panic!("unexpected owned seeder message"),
        }
    }
}

fn driver() -> Driver {
    Driver::with_peer_id(
        Limits {
            payload_budget_bytes: 6 * PIECE,
            ..Limits::default()
        },
        [23; 20],
    )
    .unwrap()
}
async fn complete(driver: &mut Driver) -> usize {
    let start = Instant::now();
    let mut failed = 0;
    loop {
        assert!(
            start.elapsed() < DEADLINE,
            "driver transfer deadline exceeded"
        );
        let report = driver.step(Duration::from_millis(10)).await.unwrap();
        failed += report.hash_failures;
        assert!(report.peer_errors.is_empty(), "{:?}", report.peer_errors);
        if driver
            .snapshot()
            .jobs
            .iter()
            .all(|row| row.state == JobState::Complete)
        {
            return failed;
        }
    }
}

async fn drive_client<T>(seed: &mut Driver, future: impl std::future::Future<Output = T>) -> T {
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            result = seed.step(Duration::from_millis(2)) => { result.unwrap(); }
        }
    }
}

async fn upload_peer(addr: std::net::SocketAddr, hash: [u8; 20]) -> PeerConnection {
    let mut conn = PeerConnection::connect(addr, hash, [61; 20]).await.unwrap();
    conn.send(Message::Interested).await.unwrap();
    loop {
        if matches!(conn.receive().await.unwrap(), Message::Unchoke) {
            return conn;
        }
    }
}

async fn seed_fixture(
    root: &Root,
    fixture: &Arc<Fixture>,
) -> (Driver, templartorrent_core::model::TorrentId) {
    let server = Server::start(fixture.clone(), Fault::default()).await;
    let mut seed = driver();
    let id = seed
        .add_magnet(
            &fixture.magnet(),
            &root.job(),
            ExistingData::Refuse,
            &[server.source],
            false,
        )
        .await
        .unwrap();
    complete(&mut seed).await;
    server.close().await;
    seed.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    (seed, id)
}

#[tokio::test]
async fn driver_seed_rejects_invalid_requests_and_invalidated_disk_without_uploading() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(79, false);
        let (mut seed, id) = seed_fixture(&root, &fixture).await;
        let addr = seed.listening_address().unwrap();
        for (index, begin, length) in [(u32::MAX, 0, 1), (0, 0, 0), (0, 0, 16385), (3, 73, 1)] {
            drive_client(&mut seed, async {
                let mut conn = upload_peer(addr, fixture.hash).await;
                conn.send(Message::Request {
                    index,
                    begin,
                    length,
                })
                .await
                .unwrap();
                while let Ok(message) = conn.receive().await {
                    assert!(
                        !matches!(message, Message::Piece { .. }),
                        "invalid requests must not upload"
                    );
                }
            })
            .await;
            assert_eq!(seed.payload_stats().reserved_bytes, 0);
        }
        assert_eq!(seed.snapshot().jobs[0].total_payload_uploaded, 0);
        // Multiple peers must all be retired if one discovers changed storage.
        let idle = drive_client(&mut seed, upload_peer(addr, fixture.hash)).await;
        // Owned temporary payload only. Piece zero covers Episode02.
        let path = root.job().join(&fixture.meta.info.files[0].path);
        let mut changed = std::fs::read(&path).unwrap();
        changed[0] ^= 1;
        std::fs::write(path, changed).unwrap();
        drive_client(&mut seed, async {
            let mut conn = upload_peer(addr, fixture.hash).await;
            conn.send(Message::Request {
                index: 0,
                begin: 0,
                length: 16384,
            })
            .await
            .unwrap();
            while let Ok(message) = conn.receive().await {
                assert!(!matches!(message, Message::Piece { .. }));
            }
        })
        .await;
        let snapshot = seed.snapshot();
        assert_eq!(snapshot.jobs[0].state, JobState::Error);
        assert_eq!(
            snapshot.jobs[0].verified_bytes,
            fixture.data.len() as u64 - PIECE as u64
        );
        assert_eq!(snapshot.jobs[0].total_payload_uploaded, 0);
        assert!(seed.last_error(id).unwrap().contains("recheck"));
        assert_eq!(seed.payload_stats().reserved_bytes, 0);
        assert_eq!(seed.connection_count(), 0);
        drop(idle);
        seed.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_seed_cancel_queue_flood_and_slot_reduction_quiesce_writers() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(79, false);
        let (mut seed, id) = seed_fixture(&root, &fixture).await;
        let addr = seed.listening_address().unwrap();
        let mut limits = Limits {
            upload_kb_s: 1,
            ..Limits::default()
        };
        seed.set_limits(limits.clone()).await.unwrap();
        let mut conn = drive_client(&mut seed, upload_peer(addr, fixture.hash)).await;
        drive_client(&mut seed, async {
            conn.send(Message::Request {
                index: 0,
                begin: 0,
                length: 16384,
            })
            .await
            .unwrap();
            loop {
                if matches!(conn.receive().await.unwrap(), Message::Piece { .. }) {
                    break;
                }
            }
            conn.send(Message::Request {
                index: 0,
                begin: 16384,
                length: 16384,
            })
            .await
            .unwrap();
            conn.send(Message::Cancel {
                index: 0,
                begin: 16384,
                length: 16384,
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(60)).await;
        })
        .await;
        for _ in 0..5 {
            seed.step(Duration::from_millis(2)).await.unwrap();
        }
        assert_eq!(seed.snapshot().jobs[0].total_payload_uploaded, 16384);
        assert_eq!(seed.payload_stats().reserved_bytes, 0);
        limits.upload_peer_slots = 0;
        seed.set_limits(limits.clone()).await.unwrap();
        assert_eq!(
            seed.connection_count(),
            0,
            "limit ack must join excess writers"
        );
        assert_eq!(seed.payload_stats().reserved_bytes, 0);
        drop(conn);
        limits.upload_peer_slots = 1;
        seed.set_limits(limits.clone()).await.unwrap();
        // A ninth queued request while the first is rate-stalled is rejected.
        drive_client(&mut seed, async {
            let mut conn = upload_peer(addr, fixture.hash).await;
            for begin in 0..9 {
                if conn
                    .send(Message::Request {
                        index: 0,
                        begin,
                        length: 16384,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            while let Ok(message) = conn.receive().await {
                assert!(!matches!(message, Message::Piece { .. }));
            }
        })
        .await;
        assert_eq!(seed.connection_count(), 0);
        limits.seed_jobs = 0;
        seed.set_limits(limits).await.unwrap();
        assert_eq!(seed.snapshot().jobs[0].state, JobState::Complete);
        seed.pause(id).await.unwrap();
        seed.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_seed_recheck_serves_without_refetch_and_rejects_unknown_or_private_ingress() {
    timeout(DEADLINE, async {
        for private in [false, true] {
            let root = Root::new();
            let fixture = Fixture::new(79, private);
            let server = Server::start(fixture.clone(), Fault::default()).await;
            let mut first = driver();
            first
                .add_magnet(
                    &fixture.magnet(),
                    &root.job(),
                    ExistingData::Refuse,
                    &[server.source],
                    false,
                )
                .await
                .unwrap();
            complete(&mut first).await;
            first.shutdown().await;
            server.stats.requests.lock().unwrap().clear();
            let mut seed = driver();
            seed.add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Recheck,
                &[server.source],
                false,
            )
            .await
            .unwrap();
            assert!(server.stats.requests.lock().unwrap().is_empty());
            let addr = seed.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
            server.close().await;
            drive_client(&mut seed, async {
                assert!(
                    PeerConnection::connect(addr, [0x71; 20], [62; 20])
                        .await
                        .is_err()
                );
            })
            .await;
            if private {
                drive_client(&mut seed, async {
                    assert!(
                        PeerConnection::connect(addr, fixture.hash, [62; 20])
                            .await
                            .is_err()
                    );
                })
                .await;
            } else {
                drive_client(&mut seed, async {
                    let mut conn = upload_peer(addr, fixture.hash).await;
                    conn.send(Message::Request {
                        index: 3,
                        begin: 0,
                        length: 73,
                    })
                    .await
                    .unwrap();
                    loop {
                        if let Message::Piece { data, .. } = conn.receive().await.unwrap() {
                            assert_eq!(&data[..], &fixture.data[3 * PIECE..]);
                            break;
                        }
                    }
                })
                .await;
            }
            seed.shutdown().await;
            assert_eq!(seed.payload_stats().reserved_bytes, 0);
            fixture.assert_files(&root.job());
        }
    })
    .await
    .unwrap();
}

/// This second client uses the same public Driver and resolves its magnet
/// exclusively from our listening Driver, after the source fixture is stopped.
#[tokio::test]
async fn driver_seeds_verified_disk_and_metadata_to_a_second_driver() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let target = Root::new();
        let fixture = Fixture::new(29, false);
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let mut seed = driver();
        let id = seed.add_magnet(&fixture.magnet(), &root.job(), ExistingData::Refuse, &[server.source], false).await.unwrap();
        complete(&mut seed).await;
        server.close().await;
        let addr = seed.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        assert_eq!(seed.snapshot().jobs[0].state, JobState::Seeding);
        let magnet = fixture.magnet();
        let path = target.job();
        let client = tokio::spawn(async move {
            let mut leech = driver();
            let source = PeerSource { address: addr, origin: PeerOrigin::UserProvided };
            leech.add_magnet(&magnet, &path, ExistingData::Refuse, &[source], true).await.unwrap();
            assert_eq!(complete(&mut leech).await, 0);
            leech.shutdown().await;
        });
        let start = Instant::now();
        let mut peak = 0;
        let mut rss = 0;
        while !client.is_finished() {
            seed.step(Duration::from_millis(5)).await.unwrap();
            peak = peak.max(seed.payload_stats().reserved_bytes);
            rss = rss.max(process_rss().unwrap_or(0));
        }
        client.await.unwrap();
        for _ in 0..4 { seed.step(Duration::ZERO).await.unwrap(); }
        fixture.assert_files(&target.job());
        assert_eq!(seed.snapshot().jobs[0].total_payload_uploaded, fixture.data.len() as u64);
        assert_eq!(seed.payload_stats().reserved_bytes, 0);
        assert!(peak <= Limits::default().payload_budget_bytes);
        let (owned, owned_high) = seed.upload_owned_bytes();
        assert_eq!(owned, 0);
        assert!(owned_high > 0 && owned_high <= PIECE + 16384);
        println!("TT_SEED_METRIC {{\"payload_bytes\":{},\"elapsed_ns\":{},\"upload_reserved_peak_sampled_bytes\":{},\"upload_owned_peak_bytes\":{},\"process_rss_peak_sampled_bytes\":{},\"profile\":\"{}\"}}",
            fixture.data.len(), start.elapsed().as_nanos(), peak, owned_high, rss, if cfg!(debug_assertions) { "debug" } else { "release" });
        seed.pause(id).await.unwrap();
        assert_eq!(seed.connection_count(), 0);
        seed.shutdown().await;
        assert!(seed.listening_address().is_none());
    }).await.unwrap();
}

#[tokio::test]
async fn driver_magnet_resolves_metadata_and_routes_two_torrents_to_their_own_hashes() {
    timeout(DEADLINE, async {
        let root_a = Root::new();
        let root_b = Root::new();
        let a = Fixture::new(3, false);
        let b = Fixture::new(11, false);
        let server_a = Server::start(a.clone(), Fault::default()).await;
        let server_b = Server::start(b.clone(), Fault::default()).await;
        let mut driver = driver();
        driver
            .add_magnet(
                &a.magnet(),
                &root_a.job(),
                ExistingData::Refuse,
                &[server_a.source],
                false,
            )
            .await
            .unwrap();
        driver
            .add_magnet(
                &b.magnet(),
                &root_b.job(),
                ExistingData::Refuse,
                &[server_b.source],
                false,
            )
            .await
            .unwrap();
        assert_eq!(complete(&mut driver).await, 0);
        a.assert_files(&root_a.job());
        b.assert_files(&root_b.job());
        assert_eq!(server_a.stats.metadata.load(Ordering::SeqCst), 1);
        assert_eq!(server_b.stats.metadata.load(Ordering::SeqCst), 1);
        // Episode01 (pieces 1..=3) ranks first; the seeded tie-break picks
        // any of its pieces before Episode02's piece 0.
        assert!((1..=3).contains(&server_a.stats.requests.lock().unwrap()[0].0));
        assert!((1..=3).contains(&server_b.stats.requests.lock().unwrap()[0].0));
        assert_eq!(driver.snapshot().jobs.len(), 2);
        assert_eq!(driver.payload_stats().reserved_bytes, 0);
        assert!(!driver.seeding_unavailable_reason().is_empty());
        driver.shutdown().await;
        assert_eq!(driver.metadata_reserved_bytes(), 0);
        server_a.close().await;
        server_b.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_corruption_retries_verified_piece_without_corrupting_files() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(7, false);
        let server = Server::start(
            fixture.clone(),
            Fault {
                corrupt: true,
                ..Fault::default()
            },
        )
        .await;
        let mut driver = driver();
        let id = driver
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                false,
            )
            .await
            .unwrap();
        let start = Instant::now();
        let mut failed = 0;
        let mut attributed = false;
        loop {
            assert!(
                start.elapsed() < DEADLINE,
                "driver transfer deadline exceeded"
            );
            let report = driver.step(Duration::from_millis(10)).await.unwrap();
            failed += report.hash_failures;
            assert!(report.peer_errors.is_empty(), "{:?}", report.peer_errors);
            if driver
                .peer_status(id)
                .iter()
                .any(|peer| peer.hash_failures == 1 && peer.rtt_us.is_some())
            {
                attributed = true;
            }
            if driver
                .snapshot()
                .jobs
                .iter()
                .all(|row| row.state == JobState::Complete)
            {
                break;
            }
        }
        assert_eq!(failed, 1);
        assert!(
            attributed,
            "the blamed peer stayed connected with one hash failure and a measured round trip"
        );
        fixture.assert_files(&root.job());
        // The corrupted block belongs to the first requested piece; every block
        // of that piece is fetched twice (piece 3 is the one-block tail).
        let requests = server.stats.requests.lock().unwrap().clone();
        let first = requests[0].0;
        let expected = if first == 3 { 2 } else { 8 };
        assert_eq!(
            requests.iter().filter(|(piece, _)| *piece == first).count(),
            expected
        );
        driver.shutdown().await;
        server.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_pause_and_remove_join_network_tasks_and_retain_data() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(5, false);
        let server = Server::start(
            fixture.clone(),
            Fault {
                hold: true,
                ..Fault::default()
            },
        )
        .await;
        let mut driver = driver();
        let id = driver
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                false,
            )
            .await
            .unwrap();
        while server.stats.requests.lock().unwrap().is_empty() {
            driver.step(Duration::from_millis(10)).await.unwrap();
        }
        assert!(driver.payload_stats().reserved_bytes > 0);
        driver.pause(id).await.unwrap();
        assert_eq!(driver.connection_count(), 0);
        assert_eq!(driver.payload_stats().reserved_bytes, 0);
        assert_eq!(driver.snapshot().jobs[0].state, JobState::Paused);
        for _ in 0..3 {
            driver.step(Duration::ZERO).await.unwrap();
        }
        assert_eq!(driver.snapshot().jobs[0].verified_bytes, 0);
        let episode = root.job().join("owned-series/Episode01.bin");
        std::fs::hard_link(&episode, root.job().join("owned-series/Episode01-link.bin")).unwrap();
        assert!(
            driver.remove(id, true).await.is_err(),
            "a hardlink must refuse deletion before the job is removed"
        );
        assert_eq!(driver.snapshot().jobs.len(), 1);
        assert!(episode.exists());
        std::fs::remove_file(root.job().join("owned-series/Episode01-link.bin")).unwrap();
        driver.remove(id, false).await.unwrap();
        assert!(driver.snapshot().jobs.is_empty());
        assert_eq!(driver.metadata_reserved_bytes(), 0);
        assert!(root.job().join("owned-series/Episode01.bin").exists());
        // The public remove ack occurs after Store::close_job, so the directory
        // can be moved without retaining any application data handles.
        std::fs::rename(root.job(), root.0.join("retained")).unwrap();
        driver.shutdown().await;
        server.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_restart_rechecks_disk_and_requests_only_missing_pieces() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(13, false);
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let mut first = driver();
        let id = first
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                false,
            )
            .await
            .unwrap();
        while first.snapshot().jobs[0].verified_bytes == 0 {
            first.step(Duration::from_millis(10)).await.unwrap();
        }
        let verified = first.snapshot().jobs[0].verified_bytes;
        assert!(verified >= PIECE as u64 && verified < fixture.data.len() as u64);
        first.remove(id, false).await.unwrap();
        drop(first);
        server.stats.requests.lock().unwrap().clear();
        let mut restarted = driver();
        restarted
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Recheck,
                &[server.source],
                false,
            )
            .await
            .unwrap();
        let adopted = restarted.snapshot().jobs[0].verified_bytes;
        assert!(adopted >= verified);
        complete(&mut restarted).await;
        // 13 blocks in total; adopted bytes are never requested again.
        let after = server.stats.requests.lock().unwrap().len() as u64;
        assert!(
            after + adopted.div_ceil(16384) <= 13,
            "adopted pieces re-requested: {after} after adopting {adopted} bytes"
        );
        fixture.assert_files(&root.job());
        restarted.shutdown().await;
        server.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_bootstrap_privacy_and_private_tracker_scope_are_enforced() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(19, true);
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let public = PeerSource {
            origin: PeerOrigin::UserProvided,
            ..server.source
        };
        let mut driver = driver();
        assert!(
            driver
                .add_magnet(
                    &fixture.magnet(),
                    &root.job(),
                    ExistingData::Refuse,
                    &[public],
                    false
                )
                .await
                .is_err()
        );
        assert_eq!(server.stats.metadata.load(Ordering::SeqCst), 0);
        assert!(
            driver
                .add_magnet(
                    &fixture.magnet(),
                    &root.job(),
                    ExistingData::Refuse,
                    &[public],
                    true
                )
                .await
                .is_err()
        );
        assert!(
            !root.job().exists(),
            "private public-bootstrap metadata must not create files"
        );
        assert_eq!(driver.connection_count(), 0);
        assert_eq!(driver.metadata_reserved_bytes(), 0);
        let id = driver
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                false,
            )
            .await
            .unwrap();
        assert!(driver.connect_peer(id, public).await.is_err());
        assert!(
            driver
                .connect_peer(
                    id,
                    PeerSource {
                        origin: PeerOrigin::OriginalTracker(2),
                        ..server.source
                    }
                )
                .await
                .is_err()
        );
        complete(&mut driver).await;
        fixture.assert_files(&root.job());
        driver.shutdown().await;
        server.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_rejects_duplicate_jobs_conflicting_roots_and_hostile_metadata_control_flood() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(29, false);
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let mut driver = driver();
        driver
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                false,
            )
            .await
            .unwrap();
        let metadata = server.stats.metadata.load(Ordering::SeqCst);
        assert!(
            driver
                .add_magnet(
                    &fixture.magnet(),
                    &root.0.join("duplicate"),
                    ExistingData::Refuse,
                    &[server.source],
                    false
                )
                .await
                .is_err()
        );
        assert_eq!(server.stats.metadata.load(Ordering::SeqCst), metadata);
        assert!(!root.0.join("duplicate").exists());
        let different = Fixture::new(31, false);
        let other = Server::start(different.clone(), Fault::default()).await;
        assert!(
            driver
                .add_magnet(
                    &different.magnet(),
                    &root.job(),
                    ExistingData::Recheck,
                    &[other.source],
                    false
                )
                .await
                .is_err()
        );
        assert_eq!(driver.snapshot().jobs.len(), 1);
        complete(&mut driver).await;
        fixture.assert_files(&root.job());
        let flooded = Server::start(
            different.clone(),
            Fault {
                control_flood: true,
                ..Fault::default()
            },
        )
        .await;
        assert!(
            driver
                .add_magnet(
                    &different.magnet(),
                    &root.0.join("flooded"),
                    ExistingData::Refuse,
                    &[flooded.source],
                    false
                )
                .await
                .is_err()
        );
        assert!(!root.0.join("flooded").exists());
        driver.shutdown().await;
        server.close().await;
        other.close().await;
        flooded.close().await;
    })
    .await
    .unwrap();
}

#[test]
fn driver_can_move_to_a_dedicated_runtime_thread_and_debug_redacts_peer_addresses() {
    fn require_send<T: Send>() {}
    require_send::<Driver>();
    let source = PeerSource {
        address: "127.0.0.1:6881".parse().unwrap(),
        origin: PeerOrigin::UserProvided,
    };
    assert!(!format!("{source:?}").contains("127.0.0.1"));
}

#[tokio::test]
async fn driver_two_peers_share_exclusive_leases_for_one_torrent() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(37, false);
        let a = Server::start(fixture.clone(), Fault::default()).await;
        let b = Server::start(fixture.clone(), Fault::default()).await;
        let mut driver = driver();
        let id = driver
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[a.source, b.source],
                false,
            )
            .await
            .unwrap();
        driver.connect_peer(id, b.source).await.unwrap();
        assert_eq!(driver.connection_count(), 2);
        complete(&mut driver).await;
        let mut requests = a.stats.requests.lock().unwrap().clone();
        requests.extend(b.stats.requests.lock().unwrap().iter().copied());
        assert_eq!(requests.len(), 13);
        assert_eq!(
            requests
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            13
        );
        assert_eq!(
            driver.connection_count(),
            0,
            "download-only completion closes peers"
        );
        fixture.assert_files(&root.job());
        driver.shutdown().await;
        a.close().await;
        b.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_global_connection_caps_and_zero_job_slots_quiesce_actual_network_work() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(43, false);
        let a = Server::start(
            fixture.clone(),
            Fault {
                hold: true,
                ..Fault::default()
            },
        )
        .await;
        let b = Server::start(
            fixture.clone(),
            Fault {
                hold: true,
                ..Fault::default()
            },
        )
        .await;
        let mut driver = driver();
        let id = driver
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[a.source, b.source],
                false,
            )
            .await
            .unwrap();
        driver.connect_peer(id, b.source).await.unwrap();
        assert_eq!(driver.connection_count(), 2);
        driver
            .set_limits(Limits {
                connections_total: 1,
                connections_per_job: 1,
                payload_budget_bytes: 6 * PIECE,
                ..Limits::default()
            })
            .await
            .unwrap();
        assert_eq!(driver.connection_count(), 1);
        assert!(driver.connect_peer(id, b.source).await.is_err());
        driver
            .set_limits(Limits {
                download_jobs: 0,
                payload_budget_bytes: 6 * PIECE,
                ..Limits::default()
            })
            .await
            .unwrap();
        assert_eq!(driver.connection_count(), 0);
        assert_eq!(driver.payload_stats().reserved_bytes, 0);
        assert_eq!(driver.snapshot().jobs[0].state, JobState::Queued);
        driver.resume(id).await.unwrap();
        assert_eq!(driver.snapshot().jobs[0].state, JobState::Queued);
        driver
            .set_limits(Limits {
                download_jobs: 1,
                download_kb_s: 0,
                payload_budget_bytes: 6 * PIECE,
                ..Limits::default()
            })
            .await
            .unwrap();
        // Resuming schedules the known candidates; the dialer connects both
        // within the default per-job cap on the following steps, never
        // inside set_limits.
        assert_eq!(driver.connection_count(), 0);
        while driver.connection_count() < 2 {
            driver.step(Duration::from_millis(10)).await.unwrap();
        }
        assert_eq!(driver.connection_count(), 2);
        assert_eq!(driver.snapshot().jobs[0].state, JobState::Downloading);
        driver.pause(id).await.unwrap();
        driver.set_limits(Limits::default()).await.unwrap();
        assert_eq!(
            driver.snapshot().jobs[0].state,
            JobState::Paused,
            "explicit pause must survive new settings"
        );
        assert_eq!(driver.connection_count(), 0);
        driver.shutdown().await;
        a.close().await;
        b.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_cancelled_metadata_future_drops_connection_and_all_metadata_permits() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(47, false);
        let server = Server::start(
            fixture.clone(),
            Fault {
                hold_metadata: true,
                ..Fault::default()
            },
        )
        .await;
        let mut driver = driver();
        {
            let uri = fixture.magnet();
            let path = root.job();
            let sources = [server.source];
            let add = driver.add_magnet(&uri, &path, ExistingData::Refuse, &sources, false);
            tokio::pin!(add);
            loop {
                tokio::select! {
                    _ = &mut add => panic!("held metadata must not complete"),
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {
                        if server.stats.metadata.load(Ordering::SeqCst) > 0 { break; }
                    }
                }
            }
        }
        assert_eq!(driver.metadata_reserved_bytes(), 0);
        assert_eq!(driver.connection_count(), 0);
        assert!(driver.snapshot().jobs.is_empty());
        assert!(!root.job().exists());
        driver.shutdown().await;
        server.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_peer_drop_then_explicit_fallback_preserves_verified_progress() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(53, false);
        let a = Server::start(
            fixture.clone(),
            Fault {
                disconnect_after: Some(5),
                ..Fault::default()
            },
        )
        .await;
        let b = Server::start(fixture.clone(), Fault::default()).await;
        let mut driver = driver();
        // Only `a` is known, so the dialer has nothing to fall back to and
        // the drop leaves the job idle until `b` is supplied explicitly.
        let id = driver
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[a.source],
                false,
            )
            .await
            .unwrap();
        while driver.connection_count() != 0 {
            driver.step(Duration::from_millis(10)).await.unwrap();
        }
        let kept = driver.snapshot().jobs[0].verified_bytes;
        assert!(
            kept > 0 && kept < fixture.data.len() as u64,
            "partial verified progress survives the drop, got {kept}"
        );
        assert_eq!(driver.payload_stats().reserved_bytes, 0);
        driver.connect_peer(id, b.source).await.unwrap();
        complete(&mut driver).await;
        let from_b = b.stats.requests.lock().unwrap().len() as u64;
        assert!(
            from_b + kept.div_ceil(16384) <= 13,
            "verified pieces must not be fetched again: {from_b} after keeping {kept} bytes"
        );
        assert_eq!(
            b.stats.metadata.load(Ordering::SeqCst),
            0,
            "payload reconnect uses already verified metadata"
        );
        fixture.assert_files(&root.job());
        driver.shutdown().await;
        a.close().await;
        b.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_magnet_to_first_verified_metrics() {
    timeout(DEADLINE, async {
        let root = Root::new(); let fixture = Fixture::new(41, false);
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let mut driver = driver();
        let start = Instant::now();
        driver.add_magnet(&fixture.magnet(), &root.job(), ExistingData::Refuse, &[server.source], false).await.unwrap();
        let metadata_ns = start.elapsed().as_nanos();
        let mut first_ns = None;
        let mut owned = 0;
        let mut rss_peak: Option<usize> = None;
        loop {
            driver.step(Duration::from_millis(10)).await.unwrap();
            owned = owned.max(driver.payload_stats().owned_bytes());
            let rss = std::fs::read_to_string("/proc/self/status").ok().and_then(|s| {
                s.lines().find(|line| line.starts_with("VmRSS:"))
                    .and_then(|line| line.split_whitespace().nth(1)).and_then(|v| v.parse::<usize>().ok())
            }).map(|bytes| bytes * 1024);
            if let Some(rss) = rss { rss_peak = Some(rss_peak.unwrap_or(0).max(rss)); }
            let snapshot = driver.snapshot();
            if first_ns.is_none() && snapshot.jobs[0].verified_bytes > 0 { first_ns = Some(start.elapsed().as_nanos()); }
            if snapshot.jobs[0].state == JobState::Complete { break; }
        }
        let elapsed = start.elapsed().as_nanos();
        fixture.assert_files(&root.job());
        println!("TT_DRIVER_METRIC {{\"payload_bytes\":{},\"metadata_ns\":{},\"magnet_to_first_verified_ns\":{},\"elapsed_ns\":{},\"throughput_including_metadata_bytes_s\":{},\"payload_reserved_peak_bytes\":{},\"engine_owned_peak_sampled_bytes\":{},\"metadata_reserved_bytes\":{},\"process_rss_peak_sampled_bytes\":{},\"profile\":\"{}\"}}",
            fixture.data.len(), metadata_ns, first_ns.unwrap(), elapsed, fixture.data.len() as u128 * 1_000_000_000 / elapsed,
            driver.payload_stats().reserved_high_watermark, owned, driver.metadata_reserved_bytes(), rss_peak.map(|value| value.to_string()).unwrap_or_else(|| "null".into()), if cfg!(debug_assertions) { "debug" } else { "release" });
        driver.shutdown().await; server.close().await;
    }).await.unwrap();
}

async fn http_tracker(listener: TcpListener, peer: std::net::SocketAddr) {
    loop {
        let Ok((mut stream, addr)) = listener.accept().await else {
            return;
        };
        assert!(addr.ip().is_loopback());
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            if request.len() >= 8192 {
                return;
            }
            let mut byte = [0];
            if stream.read_exact(&mut byte).await.is_err() {
                return;
            }
            request.push(byte[0]);
        }
        let mut body = b"d8:intervali60e12:min intervali60e5:peers6:".to_vec();
        let std::net::IpAddr::V4(ip) = peer.ip() else {
            return;
        };
        body.extend_from_slice(&ip.octets());
        body.extend_from_slice(&peer.port().to_be_bytes());
        body.push(b'e');
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if stream.write_all(header.as_bytes()).await.is_err()
            || stream.write_all(&body).await.is_err()
        {
            return;
        }
    }
}

/// A .torrent added from its own bytes: no BEP-9 fetch, the trackers come
/// from the file's announce keys, and the dialer reaches the tracker's peer.
#[tokio::test]
async fn driver_add_torrent_announces_from_file_and_downloads_without_bep9() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(59, false);
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let tracker_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tracker_addr = tracker_listener.local_addr().unwrap();
        let tracker_task = tokio::spawn(http_tracker(tracker_listener, server.source.address));
        let piece = PIECE;
        let torrent = TorrentBuilder::new("owned-series")
            .version(TorrentVersion::V1)
            .piece_length(piece as u64)
            .add_tracker(format!("http://{tracker_addr}/announce"))
            .add_file("Episode02.bin", fixture.data[..piece].to_vec())
            .add_file("Episode01.bin", fixture.data[piece..].to_vec())
            .build()
            .unwrap();
        let mut driver = driver();
        driver.set_allow_local_trackers(true);
        driver.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let id = driver
            .add_torrent(&torrent, &root.job(), ExistingData::Refuse)
            .await
            .unwrap();
        assert_eq!(driver.tracker_status(id).unwrap().len(), 1);
        // A listening driver keeps a finished job seeding.
        while driver.snapshot().jobs[0].state != JobState::Seeding {
            let report = driver.step(Duration::from_millis(10)).await.unwrap();
            assert_eq!(report.hash_failures, 0);
        }
        fixture.assert_files(&root.job());
        assert_eq!(
            server.stats.metadata.load(Ordering::SeqCst),
            0,
            "the info dictionary came from the file, not from a peer"
        );
        assert_eq!(driver.candidate_counts(id).0, 1);
        driver.shutdown().await;
        server.close().await;
        tracker_task.abort();
    })
    .await
    .unwrap();
}

/// D downloads from fixture F and keeps seeding; L knows only D. D's ut_pex
/// message names F, so F lands in L's pool as a PEX candidate. A private
/// torrent offers no ut_pex at all.
#[tokio::test]
async fn driver_pex_shares_dialed_peers_between_public_jobs() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(71, false);
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let mut d = Driver::with_peer_id(Limits::default(), [31; 20]).unwrap();
        d.set_pex_interval_secs(0);
        let d_address = d.listen("127.0.0.2:0".parse().unwrap()).await.unwrap();
        let d_id = d
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                true,
            )
            .await
            .unwrap();
        while d.snapshot().jobs[0].state != JobState::Seeding {
            d.step(Duration::from_millis(10)).await.unwrap();
        }
        let leech_root = Root::new();
        let magnet = fixture.magnet();
        let path = leech_root.job();
        let leech = tokio::spawn(async move {
            let mut l = Driver::with_peer_id(Limits::default(), [37; 20]).unwrap();
            // A listening driver keeps its wires after finishing this small
            // fixture, so D's PEX message still has a peer to reach.
            l.listen("127.0.0.3:0".parse().unwrap()).await.unwrap();
            // F is on loopback; PEX addresses that are not public need the
            // local-address setting.
            l.set_allow_local_trackers(true);
            let source = PeerSource {
                address: d_address,
                origin: PeerOrigin::UserProvided,
            };
            let l_id = l
                .add_magnet(&magnet, &path, ExistingData::Refuse, &[source], true)
                .await
                .unwrap();
            while l.candidate_counts(l_id).2 == 0 {
                l.step(Duration::from_millis(5)).await.unwrap();
            }
            let counts = l.candidate_counts(l_id);
            l.shutdown().await;
            counts
        });
        let start = Instant::now();
        while !leech.is_finished() {
            assert!(start.elapsed() < DEADLINE, "no PEX candidate arrived");
            d.step(Duration::from_millis(5)).await.unwrap();
        }
        let counts = leech.await.unwrap();
        assert_eq!(counts.2, 1, "exactly F from PEX, never D itself");
        assert_eq!(
            d.candidate_counts(d_id).0,
            1,
            "D knew F from its tracker source"
        );
        d.shutdown().await;
        server.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_tracker_discovery_reannounce_and_safe_delete() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(53, false);
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let tracker_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tracker_addr = tracker_listener.local_addr().unwrap();
        let tracker_task = tokio::spawn(http_tracker(tracker_listener, server.source.address));
        let mut driver = driver();
        driver.set_allow_local_trackers(true);
        driver.set_public_trackers(vec![
            "http://203.0.113.9/announce".into(),
            "http://127.0.0.1:9/announce".into(),
        ]);
        driver.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let hex = fixture
            .hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let magnet = format!("magnet:?xt=urn:btih:{hex}&tr=http://{tracker_addr}/announce");
        let id = driver
            .add_magnet_with_discovery(&magnet, &root.job(), ExistingData::Refuse, &[], false)
            .await
            .unwrap();
        let trackers = driver.tracker_status(id).unwrap();
        assert_eq!(
            trackers.len(),
            1,
            "local and documentation trackers stay off a public list"
        );
        assert_eq!(trackers[0].origin, "original");
        assert!(trackers[0].label.contains("127.0.0.1"));
        assert!(!trackers[0].label.contains("203.0.113.9"));
        let error = driver.reannounce(id, 0).await.unwrap_err();
        assert!(
            error.to_string().contains("cooldown"),
            "manual reannounce must honor the tracker minimum interval"
        );
        std::fs::write(root.job().join("keep.txt"), b"keep").unwrap();
        std::fs::write(root.0.join("outside.txt"), b"out").unwrap();
        driver.remove(id, true).await.unwrap();
        assert!(driver.snapshot().jobs.is_empty());
        assert!(!root.job().join("owned-series").exists());
        assert_eq!(std::fs::read(root.job().join("keep.txt")).unwrap(), b"keep");
        assert_eq!(std::fs::read(root.0.join("outside.txt")).unwrap(), b"out");
        assert!(root.job().is_dir());
        driver.shutdown().await;
        server.close().await;
        tracker_task.abort();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_peer_preference_narrows_retains_and_blocks() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(53, false);
        let silent = Fault {
            silent: true,
            ..Fault::default()
        };
        let a = Server::start_at("127.0.0.1", fixture.clone(), silent).await;
        let b = Server::start_at("127.0.0.2", fixture.clone(), silent).await;
        let mut driver = driver();
        assert_eq!(
            driver
                .set_peer_preference(
                    templartorrent_core::model::TorrentId(9),
                    b.source.address,
                    PeerPreference::Avoid
                )
                .await
                .unwrap_err()
                .0,
            "unknown job"
        );
        let id = driver
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[a.source],
                false,
            )
            .await
            .unwrap();
        // Avoid set before the connection exists applies on attach.
        driver
            .set_peer_preference(id, b.source.address, PeerPreference::Avoid)
            .await
            .unwrap();
        driver.connect_peer(id, b.source).await.unwrap();
        assert_eq!(driver.connection_count(), 2);
        assert_eq!(driver.peer_pipeline(id, a.source.address), Some(8));
        assert_eq!(driver.peer_pipeline(id, b.source.address), Some(2));
        let start = Instant::now();
        loop {
            assert!(
                start.elapsed() < DEADLINE,
                "requests never reached the peers"
            );
            driver.step(Duration::from_millis(10)).await.unwrap();
            if a.stats.requests.lock().unwrap().len() >= 4 {
                break;
            }
        }
        assert!(
            b.stats.requests.lock().unwrap().len() <= 2,
            "avoided peer must hold at most two requests in flight"
        );
        let status = driver.peer_status(id);
        assert_eq!(status.len(), 2);
        for row in &status {
            assert_eq!(
                row.client, "Transmission 4.0.4.0",
                "no extension name was sent, so the peer id names the client"
            );
        }
        assert_eq!(
            status
                .iter()
                .find(|row| row.address == b.source.address)
                .unwrap()
                .preference,
            PeerPreference::Avoid
        );
        assert_eq!(
            status
                .iter()
                .find(|row| row.address == a.source.address)
                .unwrap()
                .preference,
            PeerPreference::Normal
        );
        // Prefer is retained when the caps shrink; the Normal peer goes.
        driver
            .set_peer_preference(id, b.source.address, PeerPreference::Prefer)
            .await
            .unwrap();
        assert_eq!(driver.peer_pipeline(id, b.source.address), Some(8));
        driver
            .set_limits(Limits {
                connections_total: 1,
                connections_per_job: 1,
                payload_budget_bytes: 6 * PIECE,
                ..Limits::default()
            })
            .await
            .unwrap();
        let status = driver.peer_status(id);
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].address, b.source.address);
        assert_eq!(status[0].preference, PeerPreference::Prefer);
        driver
            .set_limits(Limits {
                payload_budget_bytes: 6 * PIECE,
                ..Limits::default()
            })
            .await
            .unwrap();
        // Block refuses reconnection of a disconnected peer and closes a live one.
        driver
            .set_peer_preference(id, a.source.address, PeerPreference::Block)
            .await
            .unwrap();
        assert_eq!(
            driver.connect_peer(id, a.source).await.unwrap_err().0,
            "peer address is blocked"
        );
        driver
            .set_peer_preference(id, b.source.address, PeerPreference::Block)
            .await
            .unwrap();
        assert_eq!(driver.connection_count(), 0);
        assert!(driver.peer_status(id).is_empty());
        assert_eq!(
            driver.connect_peer(id, b.source).await.unwrap_err().0,
            "peer address is blocked"
        );
        assert!(
            driver.resume(id).await.is_err(),
            "blocked addresses leave the candidate list"
        );
        // Normal lifts the block.
        driver
            .set_peer_preference(id, b.source.address, PeerPreference::Normal)
            .await
            .unwrap();
        driver.connect_peer(id, b.source).await.unwrap();
        let status = driver.peer_status(id);
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].preference, PeerPreference::Normal);
        assert_eq!(driver.peer_pipeline(id, b.source.address), Some(8));
        driver.shutdown().await;
        a.close().await;
        b.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_peer_preference_orders_upload_slots() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(59, false);
        let (mut seed, id) = seed_fixture(&root, &fixture).await;
        seed.set_limits(Limits {
            upload_peer_slots: 1,
            payload_budget_bytes: 6 * PIECE,
            ..Limits::default()
        })
        .await
        .unwrap();
        let a = Leech::start("127.0.0.1", fixture.hash).await;
        let b = Leech::start("127.0.0.2", fixture.hash).await;
        seed.set_peer_preference(id, b.source.address, PeerPreference::Prefer)
            .await
            .unwrap();
        seed.connect_peer(id, a.source).await.unwrap();
        seed.connect_peer(id, b.source).await.unwrap();
        let start = Instant::now();
        while !b.seen().contains(&"unchoke") {
            assert!(
                start.elapsed() < DEADLINE,
                "preferred peer was never unchoked"
            );
            seed.step(Duration::from_millis(10)).await.unwrap();
        }
        // The Normal peer may have taken the slot before the preferred peer
        // became interested; it must end up choked once both are present.
        let start = Instant::now();
        while a.seen().last() == Some(&"unchoke") {
            assert!(start.elapsed() < DEADLINE, "normal peer kept the only slot");
            seed.step(Duration::from_millis(10)).await.unwrap();
        }
        let status = seed.peer_status(id);
        let row = |address| status.iter().find(|row| row.address == address).unwrap();
        assert!(!row(b.source.address).we_choke);
        assert!(row(a.source.address).we_choke);
        // Reversing the preference moves the slot.
        seed.set_peer_preference(id, a.source.address, PeerPreference::Prefer)
            .await
            .unwrap();
        seed.set_peer_preference(id, b.source.address, PeerPreference::Avoid)
            .await
            .unwrap();
        let start = Instant::now();
        while !a.seen().contains(&"unchoke") || !b.seen().contains(&"choke") {
            assert!(
                start.elapsed() < DEADLINE,
                "slot did not move to the new preference"
            );
            seed.step(Duration::from_millis(10)).await.unwrap();
        }
        assert_eq!(seed.connection_count(), 2, "Avoid never disconnects");
        seed.shutdown().await;
        a.close().await;
        b.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_rejects_pure_v2_before_any_storage_exists() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let (_, bytes) = Fixture::torrent_bytes(61, false, TorrentVersion::V2);
        assert_eq!(
            Driver::magnet_from_torrent_bytes(&bytes).unwrap_err().0,
            "pure v2 torrents are not supported"
        );
        let mut driver = driver();
        assert!(
            driver
                .add_torrent(&bytes, &root.job(), ExistingData::Refuse)
                .await
                .is_err()
        );
        let v2_magnet = format!("magnet:?xt=urn:btmh:1220{}", "ab".repeat(32));
        let unused = PeerSource {
            address: "127.0.0.1:1".parse().unwrap(),
            origin: PeerOrigin::OriginalTracker(1),
        };
        assert_eq!(
            driver
                .add_magnet(
                    &v2_magnet,
                    &root.job(),
                    ExistingData::Refuse,
                    &[unused],
                    false
                )
                .await
                .unwrap_err()
                .0,
            "pure v2 magnets are not supported"
        );
        assert!(
            !root.job().exists(),
            "no destination may exist after rejection"
        );
        assert!(driver.snapshot().jobs.is_empty());
        assert_eq!(driver.metadata_reserved_bytes(), 0);
        driver.shutdown().await;
    })
    .await
    .unwrap();
}

/// The IPv6 peer path without an IPv6 Internet route: an outgoing download
/// from a seeder on ::1, then a dual-stack `[::]` listener seeding to a
/// leech over ::1 and to one over IPv4 loopback through the same socket.
#[tokio::test]
async fn driver_moves_payload_over_ipv6_and_a_dual_stack_listener() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(73, false);
        let server = Server::start_at("::1", fixture.clone(), Fault::default()).await;
        assert!(server.source.address.is_ipv6());
        let mut seed = Driver::with_peer_id(Limits::default(), [41; 20]).unwrap();
        seed.set_allow_local_trackers(true);
        let listen = seed.listen("[::]:0".parse().unwrap()).await.unwrap();
        let id = seed
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                false,
            )
            .await
            .unwrap();
        while seed.snapshot().jobs[0].state != JobState::Seeding {
            seed.step(Duration::from_millis(5)).await.unwrap();
        }
        fixture.assert_files(&root.job());
        for ip in ["::1", "127.0.0.1"] {
            let target = Root::new();
            let address: std::net::SocketAddr = format!(
                "{}:{}",
                if ip.contains(':') { "[::1]" } else { ip },
                listen.port()
            )
            .parse()
            .unwrap();
            let magnet = fixture.magnet();
            let path = target.job();
            let leech = tokio::spawn(async move {
                let mut leech = driver();
                let source = PeerSource {
                    address,
                    origin: PeerOrigin::UserProvided,
                };
                leech
                    .add_magnet(&magnet, &path, ExistingData::Refuse, &[source], true)
                    .await
                    .unwrap();
                assert_eq!(complete(&mut leech).await, 0);
                leech.shutdown().await;
            });
            while !leech.is_finished() {
                seed.step(Duration::from_millis(5)).await.unwrap();
            }
            leech.await.unwrap();
            fixture.assert_files(&target.job());
        }
        // An IPv4 peer reached the [::] listener as a plain IPv4 address.
        assert!(seed.snapshot().jobs[0].total_payload_uploaded >= 2 * fixture.data.len() as u64);
        let _ = id;
        seed.shutdown().await;
        server.close().await;
    })
    .await
    .unwrap();
}

/// More files than the old 128-file gate: 300 small files across piece
/// boundaries, transferred and verified over loopback.
#[tokio::test]
async fn driver_transfers_a_torrent_with_three_hundred_files() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let mut builder = TorrentBuilder::new("owned-series")
            .version(TorrentVersion::V1)
            .piece_length(16384);
        let mut data = Vec::new();
        let mut files = Vec::new();
        for index in 0..300u32 {
            let bytes: Vec<u8> = (0..(700 + index * 13))
                .map(|i| (i.wrapping_mul(31) ^ index) as u8)
                .collect();
            data.extend_from_slice(&bytes);
            let name = format!("Episode{index:03}.bin");
            builder = builder.add_file(&name, bytes.clone());
            files.push((name, bytes));
        }
        let torrent = builder.build().unwrap();
        let meta = Metainfo::from_bytes(&torrent).unwrap();
        let hash = *meta.info_hash.v1_hash().unwrap().as_bytes();
        let fixture = Arc::new(Fixture { data, meta, hash });
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let mut driver = driver();
        driver
            .add_torrent(&torrent, &root.job(), ExistingData::Refuse)
            .await
            .unwrap();
        let id = driver.snapshot().jobs[0].id;
        driver.connect_peer(id, server.source).await.unwrap();
        assert_eq!(complete(&mut driver).await, 0);
        for (name, bytes) in &files {
            assert_eq!(
                &std::fs::read(root.job().join("owned-series").join(name)).unwrap(),
                bytes
            );
        }
        driver.shutdown().await;
        server.close().await;
    })
    .await
    .unwrap();
}

/// A hybrid torrent whose first file is not piece-aligned: the v1 view has a
/// BEP 47 pad file between the two real files. The seeder serves the v1 byte
/// stream, zeros included; the leech must verify every piece, write both
/// real files and never create the pad file.
#[tokio::test]
async fn driver_downloads_hybrid_with_pad_files_through_its_v1_view() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let first: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let second: Vec<u8> = (0..30_000u32).map(|i| (i * 13 % 241) as u8).collect();
        let torrent = TorrentBuilder::new("owned-series")
            .version(TorrentVersion::Hybrid)
            .piece_length(PIECE as u64)
            .add_file("Episode02.bin", first.clone())
            .add_file("Episode01.bin", second.clone())
            .build()
            .unwrap();
        let meta = Metainfo::from_bytes(&torrent).unwrap();
        assert!(meta.is_hybrid());
        let hash = *meta.info_hash.v1_hash().unwrap().as_bytes();
        let pad = PIECE - first.len() % PIECE;
        let mut data = first.clone();
        data.extend(std::iter::repeat_n(0u8, pad));
        data.extend_from_slice(&second);
        let fixture = Arc::new(Fixture { data, meta, hash });
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let mut driver = driver();
        driver
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                false,
            )
            .await
            .unwrap();
        assert_eq!(complete(&mut driver).await, 0);
        let job = root.job().join("owned-series");
        assert_eq!(std::fs::read(job.join("Episode02.bin")).unwrap(), first);
        assert_eq!(std::fs::read(job.join("Episode01.bin")).unwrap(), second);
        assert!(!job.join(".pad").exists(), "pad files are never created");
        let row = &driver.snapshot().jobs[0];
        assert_eq!(row.verified_bytes, row.wanted_bytes);
        assert_eq!(row.wanted_bytes, (first.len() + second.len()) as u64);
        driver.shutdown().await;
        server.close().await;
    })
    .await
    .unwrap();
}

/// Entry point for the forced-crash test's child process. It only acts when
/// the parent sets `TEMPLARTORRENT_CRASH_CHILD`; otherwise it is a no-op.
#[test]
fn crash_child_entry_point() {
    if std::env::var_os("TEMPLARTORRENT_CRASH_CHILD").is_none() {
        return;
    }
    let magnet = std::env::var("TEMPLARTORRENT_CRASH_MAGNET").unwrap();
    let destination = PathBuf::from(std::env::var_os("TEMPLARTORRENT_CRASH_DEST").unwrap());
    let peer: std::net::SocketAddr = std::env::var("TEMPLARTORRENT_CRASH_PEER")
        .unwrap()
        .parse()
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut child = driver();
        child
            .add_magnet(
                &magnet,
                &destination,
                ExistingData::Refuse,
                &[PeerSource {
                    address: peer,
                    origin: PeerOrigin::OriginalTracker(1),
                }],
                false,
            )
            .await
            .unwrap();
        // Run until the parent kills this process; never shut down cleanly.
        loop {
            child.step(Duration::from_millis(10)).await.unwrap();
        }
    });
}

#[tokio::test]
async fn driver_restart_after_forced_process_kill_rechecks_and_completes() {
    timeout(Duration::from_secs(60), async {
        let root = Root::new();
        let fixture = Fixture::new(71, false);
        let server = Server::start(
            fixture.clone(),
            Fault {
                delay_ms: 40,
                ..Fault::default()
            },
        )
        .await;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_child_entry_point", "--test-threads=1"])
            .env("TEMPLARTORRENT_CRASH_CHILD", "1")
            .env("TEMPLARTORRENT_CRASH_MAGNET", fixture.magnet())
            .env("TEMPLARTORRENT_CRASH_DEST", root.job())
            .env(
                "TEMPLARTORRENT_CRASH_PEER",
                server.source.address.to_string(),
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // One lease per peer: the request for a later piece is only issued
        // after the previous piece verified and reached disk with a data sync.
        // Nine served requests therefore mean two durable pieces at least.
        let start = Instant::now();
        loop {
            let served = server.stats.requests.lock().unwrap().len();
            if served >= 9 {
                break;
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("child exited early with {status}");
            }
            assert!(
                start.elapsed() < Duration::from_secs(45),
                "child never transferred"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success(), "SIGKILL must not look like a clean exit");
        let served_before_kill = server.stats.requests.lock().unwrap().len();
        assert!(
            (9..13).contains(&served_before_kill),
            "the child must die mid-transfer, served {served_before_kill} of 13"
        );
        server.stats.requests.lock().unwrap().clear();
        assert!(
            root.job().exists(),
            "the killed process left its destination"
        );
        let mut restarted = driver();
        restarted
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Recheck,
                &[server.source],
                false,
            )
            .await
            .unwrap();
        let adopted = restarted.snapshot().jobs[0].verified_bytes;
        assert!(
            adopted >= PIECE as u64,
            "recheck must adopt at least one durable piece, got {adopted}"
        );
        complete(&mut restarted).await;
        let after = server.stats.requests.lock().unwrap().len();
        assert!(
            after <= 13 - 4,
            "restart must request only missing pieces: {after} after {served_before_kill}"
        );
        assert!(after + served_before_kill >= 13);
        fixture.assert_files(&root.job());
        restarted.shutdown().await;
        server.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_export_and_restore_job_record_rechecks_without_peers() {
    timeout(DEADLINE, async {
        // Part 1: a completed job with a narrowed selection, custom order and
        // local preferences round-trips through a record into a fresh driver.
        let root = Root::new();
        let fixture = Fixture::new(73, false);
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let mut first = driver();
        let id = first
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                true,
            )
            .await
            .unwrap();
        first.set_file_wanted(id, 0, false).unwrap();
        first.set_file_order(id, vec![1, 0]).unwrap();
        first
            .set_peer_preference(id, "127.0.0.9:1".parse().unwrap(), PeerPreference::Block)
            .await
            .unwrap();
        first
            .set_peer_preference(id, "127.0.0.8:1".parse().unwrap(), PeerPreference::Avoid)
            .await
            .unwrap();
        complete(&mut first).await;
        let record = first.export_job(id).unwrap();
        assert_eq!(record.info_hash, fixture.hash);
        assert!(record.torrent.starts_with(b"d4:info"));
        assert_eq!(
            Driver::magnet_from_torrent_bytes(&record.torrent).unwrap(),
            fixture.magnet()
        );
        assert_eq!(record.destination, first.destination(id).unwrap());
        assert!(!record.paused);
        assert!(record.public_bootstrap);
        assert_eq!(record.wanted, vec![false, true]);
        assert_eq!(record.order, vec![1, 0]);
        assert!(record.trackers.is_empty());
        assert_eq!(
            record.preferences,
            vec![
                ("127.0.0.8".parse().unwrap(), PeerPreference::Avoid),
                ("127.0.0.9".parse().unwrap(), PeerPreference::Block),
            ]
        );
        assert!(!format!("{record:?}").contains("127.0.0.9"));
        first.remove(id, false).await.unwrap();
        drop(first);
        server.stats.requests.lock().unwrap().clear();

        let mut second = driver();
        let restored = second.restore_job(&record).await.unwrap();
        let row = second.snapshot().jobs[0].clone();
        assert_eq!(row.id, restored);
        assert_eq!(&*row.name, "owned-series");
        assert_eq!(row.state, JobState::Complete, "adopted from disk");
        assert_eq!(row.verified_bytes, row.wanted_bytes);
        assert_eq!(row.wanted_bytes, (2 * PIECE + 73) as u64);
        assert_eq!(
            second.destination(restored),
            Some(record.destination.clone())
        );
        let files = second.file_progress(restored).unwrap();
        assert_eq!(
            files.iter().map(|f| f.wanted).collect::<Vec<_>>(),
            vec![false, true]
        );
        assert_eq!(second.export_job(restored).unwrap().order, vec![1, 0]);
        assert_eq!(
            second.peer_preference(restored, "127.0.0.9:4".parse().unwrap()),
            PeerPreference::Block
        );
        assert_eq!(
            second.peer_preference(restored, "127.0.0.8:4".parse().unwrap()),
            PeerPreference::Avoid
        );
        assert_eq!(second.connection_count(), 0, "restore contacts no peer");
        assert!(server.stats.requests.lock().unwrap().is_empty());
        assert_eq!(
            second.restore_job(&record).await.unwrap_err().0,
            "torrent already exists"
        );
        let mut tampered = record.clone();
        tampered.torrent[12] ^= 0x01;
        assert!(second.restore_job(&tampered).await.is_err());
        let mut mismatched = record.clone();
        mismatched.info_hash[0] ^= 0xff;
        assert_eq!(
            second.restore_job(&mismatched).await.unwrap_err().0,
            "stored torrent does not match its record"
        );
        assert_eq!(second.snapshot().jobs.len(), 1);
        second.shutdown().await;
        server.close().await;

        // Part 2: a paused partial job restores as paused with its verified
        // bytes, then completes through its saved tracker after resume.
        let root = Root::new();
        let fixture = Fixture::new(79, false);
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let tracker_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tracker_addr = tracker_listener.local_addr().unwrap();
        let tracker_task = tokio::spawn(http_tracker(tracker_listener, server.source.address));
        let mut third = driver();
        let id = third
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                false,
            )
            .await
            .unwrap();
        while third.snapshot().jobs[0].verified_bytes == 0 {
            third.step(Duration::from_millis(10)).await.unwrap();
        }
        third.pause(id).await.unwrap();
        let verified = third.snapshot().jobs[0].verified_bytes;
        assert!(verified >= PIECE as u64 && verified < fixture.data.len() as u64);
        let mut record = third.export_job(id).unwrap();
        assert!(record.paused);
        record
            .trackers
            .push((format!("http://{tracker_addr}/announce"), true));
        third.remove(id, false).await.unwrap();
        drop(third);
        server.stats.requests.lock().unwrap().clear();

        let mut fourth = driver();
        fourth.set_allow_local_trackers(true);
        fourth.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let id = fourth.restore_job(&record).await.unwrap();
        let row = fourth.snapshot().jobs[0].clone();
        assert_eq!(row.state, JobState::Paused);
        assert!(row.verified_bytes >= verified);
        assert_eq!(fourth.tracker_status(id).unwrap().len(), 1);
        assert_eq!(fourth.connection_count(), 0);
        fourth.resume(id).await.unwrap();
        assert_eq!(fourth.snapshot().jobs[0].state, JobState::Downloading);
        let start = Instant::now();
        loop {
            assert!(
                start.elapsed() < DEADLINE,
                "tracker never supplied the peer"
            );
            fourth.step(Duration::from_millis(10)).await.unwrap();
            if fourth
                .snapshot()
                .jobs
                .iter()
                .all(|row| matches!(row.state, JobState::Complete | JobState::Seeding))
            {
                break;
            }
        }
        let after = server.stats.requests.lock().unwrap().len() as u64;
        assert!(
            after + row.verified_bytes.div_ceil(16384) <= 13,
            "adopted pieces are not requested again: {after} after {}",
            row.verified_bytes
        );
        fixture.assert_files(&root.job());
        fourth.shutdown().await;
        server.close().await;
        tracker_task.abort();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_optimistic_slot_rotates_one_slot_between_interested_peers() {
    timeout(Duration::from_secs(30), async {
        let root = Root::new();
        let fixture = Fixture::new(83, false);
        let (mut seed, id) = seed_fixture(&root, &fixture).await;
        seed.set_limits(Limits {
            upload_peer_slots: 1,
            payload_budget_bytes: 6 * PIECE,
            ..Limits::default()
        })
        .await
        .unwrap();
        seed.set_optimistic_interval_secs(1);
        let a = Leech::start("127.0.0.1", fixture.hash).await;
        let b = Leech::start("127.0.0.2", fixture.hash).await;
        seed.connect_peer(id, a.source).await.unwrap();
        seed.connect_peer(id, b.source).await.unwrap();
        let unchoked = |seed: &Driver| {
            seed.peer_status(id)
                .into_iter()
                .filter(|row| !row.we_choke)
                .map(|row| row.address)
                .collect::<Vec<_>>()
        };
        let start = Instant::now();
        let mut holders = Vec::new();
        while holders.len() < 3 {
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "slot never rotated: {holders:?}"
            );
            seed.step(Duration::from_millis(10)).await.unwrap();
            let now = unchoked(&seed);
            assert!(now.len() <= 1, "one slot must never become two: {now:?}");
            if let Some(holder) = now.first()
                && holders.last() != Some(holder)
            {
                holders.push(*holder);
            }
        }
        assert!(holders.contains(&a.source.address) && holders.contains(&b.source.address));
        assert_eq!(
            holders[0], holders[2],
            "rotation returns to the first holder"
        );
        assert!(a.seen().contains(&"unchoke") && a.seen().contains(&"choke"));
        assert!(b.seen().contains(&"unchoke"));
        assert_eq!(seed.optimistic_peer(), Some(holders[2]));
        seed.shutdown().await;
        a.close().await;
        b.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_large_piece_count_scenario_metrics() {
    timeout(Duration::from_secs(120), async {
        let root = Root::new();
        let pieces = 2048;
        let fixture = Fixture::large(89, pieces);
        let a = Server::start_at("127.0.0.1", fixture.clone(), Fault::default()).await;
        let b = Server::start_at("127.0.0.2", fixture.clone(), Fault::default()).await;
        let mut driver = Driver::with_peer_id(Limits::default(), [29; 20]).unwrap();
        let start = Instant::now();
        let id = driver
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[a.source, b.source],
                false,
            )
            .await
            .unwrap();
        driver.connect_peer(id, b.source).await.unwrap();
        let mut peak_reserved = 0usize;
        let mut peak_rss = 0usize;
        let mut peak_down = 0u64;
        loop {
            assert!(
                start.elapsed() < Duration::from_secs(100),
                "large transfer stalled"
            );
            let report = driver.step(Duration::from_millis(10)).await.unwrap();
            assert!(report.peer_errors.is_empty(), "{:?}", report.peer_errors);
            peak_reserved = peak_reserved.max(driver.payload_stats().reserved_bytes);
            if let Some(rss) = process_rss() {
                peak_rss = peak_rss.max(rss);
            }
            for row in driver.peer_status(id) {
                peak_down = peak_down.max(row.down_bps);
            }
            if driver.snapshot().jobs[0].state == JobState::Complete {
                break;
            }
        }
        let elapsed = start.elapsed();
        fixture.assert_files(&root.job());
        let requests =
            a.stats.requests.lock().unwrap().len() + b.stats.requests.lock().unwrap().len();
        assert_eq!(
            requests, pieces,
            "every 16 KiB piece is one block, requested once"
        );
        println!(
            "TT_LARGE_METRIC {{\"pieces\":{pieces},\"payload_bytes\":{},\"elapsed_ns\":{},\"throughput_bytes_s\":{},\"payload_reserved_peak_bytes\":{peak_reserved},\"process_rss_peak_sampled_bytes\":{peak_rss},\"peak_peer_down_bps_observed\":{peak_down},\"profile\":\"{}\"}}",
            fixture.data.len(),
            elapsed.as_nanos(),
            fixture.data.len() as u128 * 1_000_000_000 / elapsed.as_nanos().max(1),
            if cfg!(debug_assertions) { "debug" } else { "release" }
        );
        driver.shutdown().await;
        a.close().await;
        b.close().await;
    })
    .await
    .unwrap();
}

async fn restore_with(record: JobRecord, resume: Option<ResumeRecord>) -> (ResumeScan, TorrentRow) {
    let mut driver = driver();
    let id = driver
        .restore_job_with_resume(&record, resume.as_ref())
        .await
        .unwrap();
    let scan = driver.resume_scan(id).unwrap();
    let row = driver.snapshot().jobs[0].clone();
    driver.remove(id, false).await.unwrap();
    (scan, row)
}

#[tokio::test]
async fn driver_resume_bitmap_trusts_unchanged_files_only() {
    timeout(Duration::from_secs(30), async {
        let root = Root::new();
        let fixture = Fixture::new(91, false);
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let mut first = driver();
        let id = first
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                false,
            )
            .await
            .unwrap();
        complete(&mut first).await;
        let record = first.export_job(id).unwrap();
        let resume = first.export_resume(id).unwrap();
        assert!(resume.barrier);
        assert_eq!(resume.pieces, 4);
        assert_eq!(
            resume
                .bits
                .iter()
                .map(|byte| byte.count_ones())
                .sum::<u32>(),
            4
        );
        assert_eq!(resume.files.len(), 2);
        assert_eq!(resume.files[0].size, PIECE as u64);
        assert_eq!(
            resume.files[1].size,
            fixture.data.len() as u64 - PIECE as u64
        );
        first.remove(id, false).await.unwrap();
        drop(first);
        server.close().await;

        let restore = |resume: Option<&ResumeRecord>| restore_with(record.clone(), resume.cloned());

        let (scan, row) = restore(None).await;
        assert_eq!(scan.trusted, 0);
        assert_eq!(scan.checked, 4);
        assert_eq!(scan.note, None);
        assert_eq!(row.state, JobState::Complete);

        let mut open = resume.clone();
        open.barrier = false;
        let (scan, row) = restore(Some(&open)).await;
        assert_eq!(scan.trusted, 0);
        assert_eq!(scan.checked, 4);
        assert_eq!(scan.note, Some("resume bitmap had no durability barrier"));
        assert_eq!(row.state, JobState::Complete);

        let (scan, row) = restore(Some(&resume)).await;
        assert_eq!(
            (scan.trusted, scan.checked, scan.failed, scan.note),
            (4, 0, 0, None)
        );
        assert_eq!(row.state, JobState::Complete);
        assert_eq!(row.verified_bytes, fixture.data.len() as u64);
        fixture.assert_files(&root.job());

        let episode = root.job().join("owned-series/Episode01.bin");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&episode)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(3600))
            .unwrap();
        let (scan, row) = restore(Some(&resume)).await;
        assert_eq!(
            (scan.trusted, scan.checked, scan.failed, scan.note),
            (1, 3, 0, None),
            "only the untouched file skips the hash"
        );
        assert_eq!(row.state, JobState::Complete);
        fixture.assert_files(&root.job());

        let episode = root.job().join("owned-series/Episode02.bin");
        let mut bytes = std::fs::read(&episode).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&episode, bytes).unwrap();
        let (scan, row) = restore(Some(&resume)).await;
        assert_eq!(scan.trusted, 0, "both files now differ from the bitmap");
        assert_eq!(scan.checked, 4);
        assert_eq!(scan.failed, 1);
        assert_ne!(row.state, JobState::Complete);
        assert_eq!(row.verified_bytes, fixture.data.len() as u64 - PIECE as u64);
    })
    .await
    .unwrap();
}

/// Relays loopback connections to `target` and keeps every byte each
/// direction carried, one buffer per direction per connection.
struct Relay {
    address: std::net::SocketAddr,
    streams: Arc<Mutex<Vec<Vec<u8>>>>,
    task: JoinHandle<()>,
}
impl Relay {
    async fn start(target: std::net::SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let streams = Arc::new(Mutex::new(Vec::new()));
        let record = streams.clone();
        let task = tokio::spawn(async move {
            let mut pipes = JoinSet::new();
            while let Ok((inbound, _)) = listener.accept().await {
                let Ok(outbound) = TcpStream::connect(target).await else {
                    continue;
                };
                let (from_client, to_client) = inbound.into_split();
                let (from_target, to_target) = outbound.into_split();
                pipes.spawn(relay_pipe(from_client, to_target, record.clone()));
                pipes.spawn(relay_pipe(from_target, to_client, record.clone()));
            }
        });
        Self {
            address,
            streams,
            task,
        }
    }
    async fn close(self) -> Vec<Vec<u8>> {
        self.task.abort();
        let _ = self.task.await;
        std::mem::take(&mut *self.streams.lock().unwrap())
    }
}
async fn relay_pipe(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    streams: Arc<Mutex<Vec<Vec<u8>>>>,
) {
    let index = {
        let mut streams = streams.lock().unwrap();
        streams.push(Vec::new());
        streams.len() - 1
    };
    let mut buf = vec![0u8; 16384];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        streams.lock().unwrap()[index].extend_from_slice(&buf[..n]);
        if to.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
}
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Two public Drivers that both require encryption: metadata and every
/// piece cross a recording relay, which never sees the BitTorrent protocol
/// string, the BEP-10 extension name or any run of the file's bytes.
#[tokio::test]
async fn driver_encrypted_transfer_between_two_drivers_hides_protocol_and_payload() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let target = Root::new();
        let fixture = Fixture::new(41, false);
        let (mut seed, _) = seed_fixture(&root, &fixture).await;
        seed.set_peer_encryption(PeerEncryption::RequireEncrypted);
        let addr = seed.listening_address().unwrap();
        drive_client(&mut seed, async {
            assert!(
                PeerConnection::connect(addr, fixture.hash, [62; 20])
                    .await
                    .is_err(),
                "a plaintext peer reached a seed that requires encryption"
            );
        })
        .await;
        let relay = Relay::start(addr).await;
        let magnet = fixture.magnet();
        let path = target.job();
        let via = relay.address;
        let client = tokio::spawn(async move {
            let mut leech = driver();
            leech.set_peer_encryption(PeerEncryption::RequireEncrypted);
            assert_eq!(leech.peer_encryption(), PeerEncryption::RequireEncrypted);
            let source = PeerSource {
                address: via,
                origin: PeerOrigin::UserProvided,
            };
            leech
                .add_magnet(&magnet, &path, ExistingData::Refuse, &[source], true)
                .await
                .unwrap();
            assert_eq!(complete(&mut leech).await, 0);
            leech.shutdown().await;
        });
        while !client.is_finished() {
            seed.step(Duration::from_millis(5)).await.unwrap();
        }
        client.await.unwrap();
        for _ in 0..4 {
            seed.step(Duration::ZERO).await.unwrap();
        }
        fixture.assert_files(&target.job());
        assert_eq!(
            seed.snapshot().jobs[0].total_payload_uploaded,
            fixture.data.len() as u64
        );
        seed.shutdown().await;
        let streams = relay.close().await;
        let carried: usize = streams.iter().map(Vec::len).sum();
        assert!(
            carried > fixture.data.len(),
            "relay carried {carried} bytes"
        );
        let step = fixture.data.len() / 8;
        for stream in &streams {
            assert!(!contains(stream, b"BitTorrent protocol"));
            assert!(!contains(stream, b"ut_metadata"));
            for offset in (0..fixture.data.len() - 32).step_by(step) {
                assert!(!contains(stream, &fixture.data[offset..offset + 32]));
            }
        }
    })
    .await
    .unwrap();
}

/// The default setting dials MSE first. A plaintext-only peer drops that
/// connection, and the Driver redials once in plaintext; PlaintextOnly dials
/// once. Both complete the verified transfer.
#[tokio::test]
async fn driver_prefer_encrypted_falls_back_once_to_a_plaintext_only_peer() {
    timeout(DEADLINE, async {
        assert_eq!(driver().peer_encryption(), PeerEncryption::PreferEncrypted);
        for (encryption, dials) in [
            (PeerEncryption::PreferEncrypted, 2),
            (PeerEncryption::PlaintextOnly, 1),
        ] {
            let root = Root::new();
            let fixture = Fixture::new(43, false);
            let server = Server::start(fixture.clone(), Fault::default()).await;
            let mut driver = driver();
            driver.set_peer_encryption(encryption);
            driver
                .add_magnet(
                    &fixture.magnet(),
                    &root.job(),
                    ExistingData::Refuse,
                    &[server.source],
                    false,
                )
                .await
                .unwrap();
            assert_eq!(complete(&mut driver).await, 0);
            fixture.assert_files(&root.job());
            assert_eq!(server.stats.accepted.load(Ordering::SeqCst), dials);
            assert_eq!(server.stats.metadata.load(Ordering::SeqCst), 1);
            driver.shutdown().await;
            server.close().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn driver_require_encrypted_is_refused_by_a_plaintext_only_peer() {
    timeout(DEADLINE, async {
        let root = Root::new();
        let fixture = Fixture::new(47, false);
        let server = Server::start(fixture.clone(), Fault::default()).await;
        let mut driver = driver();
        driver.set_peer_encryption(PeerEncryption::RequireEncrypted);
        let error = driver
            .add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                false,
            )
            .await
            .unwrap_err();
        assert_eq!(error.0, "no permitted peer supplied valid metadata");
        // One MSE attempt, no plaintext redial, no BitTorrent handshake.
        assert_eq!(server.stats.accepted.load(Ordering::SeqCst), 1);
        assert_eq!(server.stats.active.load(Ordering::SeqCst), 0);
        assert_eq!(server.stats.metadata.load(Ordering::SeqCst), 0);
        assert!(!root.job().exists());
        assert!(driver.snapshot().jobs.is_empty());
        server.close().await;
    })
    .await
    .unwrap();
}

async fn encrypted_upload_peer(addr: std::net::SocketAddr, hash: [u8; 20]) -> PeerConnection {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let stream = mse::initiate(tcp, hash, EncryptionPolicy::RequireRc4)
        .await
        .unwrap();
    let mut conn = PeerConnection::connect_with_stream(stream, addr, hash, [61; 20], false)
        .await
        .unwrap();
    assert!(conn.is_encrypted());
    conn.send(Message::Interested).await.unwrap();
    loop {
        if matches!(conn.receive().await.unwrap(), Message::Unchoke) {
            return conn;
        }
    }
}

/// One listener, two seeding jobs: an MSE initiator is routed by its SKEY to
/// the matching job and served that job's bytes. Silent sockets, noise and
/// an unknown SKEY hold or fail their own probes without blocking others.
#[tokio::test]
async fn driver_routes_encrypted_incoming_to_the_job_its_skey_names() {
    timeout(DEADLINE, async {
        let root_a = Root::new();
        let root_b = Root::new();
        let a = Fixture::new(3, false);
        let b = Fixture::new(11, false);
        let server_a = Server::start(a.clone(), Fault::default()).await;
        let server_b = Server::start(b.clone(), Fault::default()).await;
        let mut seed = driver();
        for (fixture, root, server) in [(&a, &root_a, &server_a), (&b, &root_b, &server_b)] {
            seed.add_magnet(
                &fixture.magnet(),
                &root.job(),
                ExistingData::Refuse,
                &[server.source],
                false,
            )
            .await
            .unwrap();
        }
        complete(&mut seed).await;
        server_a.close().await;
        server_b.close().await;
        let addr = seed.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(TcpStream::connect(addr).await.unwrap());
        }
        let mut noise = TcpStream::connect(addr).await.unwrap();
        noise.write_all(&[0x42; 700]).await.unwrap();
        held.push(noise);
        drive_client(&mut seed, async {
            for fixture in [&b, &a] {
                let mut conn = encrypted_upload_peer(addr, fixture.hash).await;
                conn.send(Message::Request {
                    index: 3,
                    begin: 0,
                    length: 73,
                })
                .await
                .unwrap();
                loop {
                    if let Message::Piece { data, .. } = conn.receive().await.unwrap() {
                        assert_eq!(&data[..], &fixture.data[3 * PIECE..]);
                        break;
                    }
                }
            }
            let tcp = TcpStream::connect(addr).await.unwrap();
            assert!(
                mse::initiate(tcp, [0x71; 20], EncryptionPolicy::RequireRc4)
                    .await
                    .is_err()
            );
        })
        .await;
        drop(held);
        seed.shutdown().await;
        assert_eq!(seed.payload_stats().reserved_bytes, 0);
    })
    .await
    .unwrap();
}
