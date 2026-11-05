//! HTTP file downloads against loopback fixtures only (DESIGN.md section 11).
//! Every root is a fresh temporary directory owned by the test.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use templartorrent_core::{
    bandwidth::{SharedBucket, TokenBucket},
    budget::ByteBudget,
    model::{JobState, Limits, TorrentId},
};
use templartorrent_rbit_adapter::{
    driver::Driver,
    http::{
        READ_CHUNK, UrlConfig, UrlManager, UrlRecord,
        fixture::{self, Behaviour},
        parse_sha256, parse_url,
        transport::{self, admit_url, resolve_location},
    },
};

struct Root(PathBuf);

impl Root {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "templartorrent-http-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn dest(&self) -> PathBuf {
        self.0.join("dest")
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + i / 251) % 251) as u8).collect()
}

fn config(rate_kb_s: u64, idle: Duration) -> UrlConfig {
    let origin = Instant::now();
    UrlConfig {
        budget: ByteBudget::new(48 * 1024 * 1024),
        rate: SharedBucket::new(TokenBucket::new(rate_kb_s, 0).unwrap()),
        origin,
        allow_local: true,
        tls: None,
        idle,
        max_active: 3,
    }
}

fn state(manager: &UrlManager, id: TorrentId) -> JobState {
    manager
        .rows()
        .into_iter()
        .find(|row| row.id == id)
        .map(|row| row.state)
        .expect("row")
}

async fn wait_for(
    manager: &mut UrlManager,
    id: TorrentId,
    limit: Duration,
    done: impl Fn(&UrlManager) -> bool,
) {
    let start = Instant::now();
    loop {
        manager.tick();
        if done(manager) {
            return;
        }
        assert!(
            start.elapsed() < limit,
            "timed out; state {:?} detail {:?}",
            state(manager, id),
            manager.detail(id)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_terminal(manager: &mut UrlManager, id: TorrentId) -> JobState {
    wait_for(manager, id, Duration::from_secs(60), |manager| {
        matches!(state(manager, id), JobState::Complete | JobState::Error)
    })
    .await;
    state(manager, id)
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[tokio::test]
async fn ranged_body_downloads_in_parallel_parts_and_matches() {
    let root = Root::new("ranged");
    let data = body(5 * 1024 * 1024 + 123);
    let server = fixture::start(Behaviour {
        body: data.clone(),
        ..Behaviour::default()
    })
    .await;
    let mut manager = UrlManager::new(config(0, Duration::from_secs(30)));
    let id = manager
        .add(&server.url("/dir/payload.bin"), &root.dest(), None)
        .await
        .unwrap();
    assert!(UrlManager::is_url_id(id));
    assert_eq!(state(&manager, id), JobState::Queued);
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    let detail = manager.detail(id).unwrap();
    assert_eq!(detail.file.as_deref(), Some("payload.bin"));
    assert_eq!(detail.size, Some(data.len() as u64));
    assert_eq!(detail.parts.len(), 4, "{:?}", detail.parts);
    assert!(detail.ranged);
    assert_eq!(detail.validator.as_deref(), Some("ETag \"fixture-v1\""));
    assert_eq!(read(&root.dest().join("payload.bin")), data);
    assert!(!root.dest().join("payload.bin.part").exists());
    assert_eq!(server.request_count(), 4, "one connection per part");
    assert_eq!(server.requests_with("range: bytes=0-"), 1);
    assert!(server.requests_with("accept-encoding: identity") == 4);
    let row = manager.rows().into_iter().find(|row| row.id == id).unwrap();
    assert_eq!(row.verified_bytes, data.len() as u64);
    assert_eq!(row.wanted_bytes, data.len() as u64);
    let records = manager.take_dirty();
    assert_eq!(records.len(), 1);
    assert!(records[0].complete);
    assert_eq!(records[0].done_bytes(), data.len() as u64);
}

#[tokio::test]
async fn plain_body_uses_one_connection_and_a_chunked_size_is_unknown() {
    let root = Root::new("plain");
    let data = body(3 * 1024 * 1024);
    let server = fixture::start(Behaviour {
        body: data.clone(),
        ranges: false,
        etag: None,
        content_disposition: Some("attachment; filename=\"named by server.iso\"".into()),
        ..Behaviour::default()
    })
    .await;
    let mut manager = UrlManager::new(config(0, Duration::from_secs(30)));
    let id = manager
        .add(&server.url("/get?id=7"), &root.dest(), None)
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    assert_eq!(server.request_count(), 1, "no ranges means one connection");
    let detail = manager.detail(id).unwrap();
    assert_eq!(detail.file.as_deref(), Some("named by server.iso"));
    assert!(!detail.ranged);
    assert_eq!(detail.validator, None);
    assert_eq!(read(&root.dest().join("named by server.iso")), data);

    // Chunked: no length, so the size is unknown while it runs. A rate limit
    // keeps it in flight long enough to look.
    let root2 = Root::new("chunked");
    let server = fixture::start(Behaviour {
        body: data.clone(),
        ranges: false,
        chunked: true,
        content_length: false,
        ..Behaviour::default()
    })
    .await;
    let settings = config(400, Duration::from_secs(30));
    let rate = settings.rate.clone();
    let mut manager = UrlManager::new(settings);
    let id = manager
        .add(&server.url("/stream"), &root2.dest(), None)
        .await
        .unwrap();
    wait_for(&mut manager, id, Duration::from_secs(20), |manager| {
        manager
            .rows()
            .iter()
            .any(|row| row.id == id && row.verified_bytes > 0)
    })
    .await;
    let row = manager.rows().into_iter().find(|row| row.id == id).unwrap();
    assert_eq!(row.state, JobState::Downloading);
    assert_eq!(
        row.wanted_bytes, 0,
        "unknown size is 0, rendered as an em dash"
    );
    assert_eq!(manager.detail(id).unwrap().size, None);
    rate.set_limit(0, 0).unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    assert_eq!(manager.detail(id).unwrap().size, Some(data.len() as u64));
    assert_eq!(read(&root2.dest().join("stream")), data);
}

#[tokio::test]
async fn redirects_are_followed_five_times_and_refused_on_the_sixth() {
    let root = Root::new("redirect");
    let data = body(70_000);
    let server = fixture::start(Behaviour {
        body: data.clone(),
        ..Behaviour::default()
    })
    .await;
    let mut manager = UrlManager::new(config(0, Duration::from_secs(30)));
    let id = manager
        .add(&server.url("/redirect/5"), &root.dest(), None)
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    assert_eq!(server.request_count(), 6, "five hops then the file");
    assert_eq!(read(&root.dest().join("file")), data);
    assert!(server.requests_with("get /redirect/1 ") == 1);

    let too_many = manager
        .add(&server.url("/redirect/6"), &root.0.join("six"), None)
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, too_many).await, JobState::Error);
    let reason = manager.detail(too_many).unwrap().error.unwrap();
    assert!(reason.contains("too many redirects"), "{reason}");

    // Admission refuses local addresses without the allowance, at add time
    // and again on every redirect hop.
    let mut strict = UrlManager::new(UrlConfig {
        allow_local: false,
        ..config(0, Duration::from_secs(30))
    });
    let refused = strict
        .add(&server.url("/file"), &root.dest(), None)
        .await
        .unwrap_err();
    assert!(refused.contains("local or private"), "{refused}");
    assert!(
        admit_url(
            &parse_url("http://169.254.169.254/latest/meta-data").unwrap(),
            false
        )
        .await
        .is_err()
    );
    assert!(
        admit_url(&parse_url("http://[fd00:ec2::254]/").unwrap(), false)
            .await
            .is_err()
    );
    assert!(
        admit_url(&parse_url("http://100.100.100.200/").unwrap(), false)
            .await
            .is_err()
    );
    let hop = transport::open(
        &parse_url(&server.url("/redirect-to/http:%2f%2f127.0.0.1:1%2fx")).unwrap(),
        &transport::Request::default(),
        false,
        None,
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert!(hop.contains("local or private"), "{hop}");
    for (raw, reason) in [
        ("ftp://files.example/x", "scheme"),
        ("file:///etc/passwd", "scheme"),
        ("http://user:pw@files.example/x", "userinfo"),
        ("http:///x", "host"),
        ("http://files.example/a b", "whitespace"),
    ] {
        let error = parse_url(raw).unwrap_err();
        assert!(error.contains(reason), "{raw}: {error}");
    }
    let base = parse_url("http://files.example/dir/a?x=1").unwrap();
    assert_eq!(
        resolve_location(&base, "b").unwrap().url,
        "http://files.example/dir/b"
    );
    assert_eq!(
        resolve_location(&base, "/c").unwrap().url,
        "http://files.example/c"
    );
    assert_eq!(
        resolve_location(&base, "//other.example/d").unwrap().url,
        "http://other.example/d"
    );
    assert!(resolve_location(&base, "ftp://other.example/d").is_err());
}

#[tokio::test]
async fn https_checks_the_certificate_name_and_refuses_a_downgrade() {
    let root = Root::new("tls");
    let data = body(50_000);
    let ca = tls::TestCa::new();
    let trusted = ca.issue("localhost");
    let server = fixture::start_tls(
        Behaviour {
            body: data.clone(),
            ..Behaviour::default()
        },
        Arc::new(trusted),
    )
    .await;
    let mut manager = UrlManager::new(UrlConfig {
        tls: Some(ca.client_config()),
        ..config(0, Duration::from_secs(30))
    });
    let id = manager
        .add(
            &server.https_url("localhost", "/secure.bin"),
            &root.dest(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    assert_eq!(read(&root.dest().join("secure.bin")), data);

    let downgrade = manager
        .add(
            &server.https_url(
                "localhost",
                &format!(
                    "/redirect-to/http:%2f%2flocalhost:{}%2ffile",
                    server.address.port()
                ),
            ),
            &root.0.join("down"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        wait_terminal(&mut manager, downgrade).await,
        JobState::Error
    );
    let reason = manager.detail(downgrade).unwrap().error.unwrap();
    assert!(reason.contains("https to http"), "{reason}");

    let wrong_name = fixture::start_tls(
        Behaviour {
            body: data.clone(),
            ..Behaviour::default()
        },
        Arc::new(ca.issue("other.example")),
    )
    .await;
    let mismatch = manager
        .add(
            &wrong_name.https_url("localhost", "/x.bin"),
            &root.0.join("name"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, mismatch).await, JobState::Error);
    let reason = manager.detail(mismatch).unwrap().error.unwrap();
    assert!(reason.contains("TLS handshake failed"), "{reason}");
}

/// Slow the transfer, pause part way, and return the paused manager.
async fn pause_midway(manager: &mut UrlManager, id: TorrentId) -> u64 {
    wait_for(manager, id, Duration::from_secs(20), |manager| {
        manager
            .rows()
            .iter()
            .any(|row| row.id == id && row.verified_bytes > 100_000)
    })
    .await;
    manager.pause(id).await.unwrap();
    assert_eq!(state(manager, id), JobState::Paused);
    let detail = manager.detail(id).unwrap();
    assert!(detail.done > 0 && detail.size.is_some_and(|size| detail.done < size));
    detail.done
}

#[tokio::test]
async fn identity_mismatch_on_resume_stops_with_a_reason_and_keeps_the_partial() {
    let root = Root::new("identity");
    let data = body(2 * 1024 * 1024 + 7);
    let server = fixture::start(Behaviour {
        body: data.clone(),
        ..Behaviour::default()
    })
    .await;
    let settings = config(1500, Duration::from_secs(30));
    let rate = settings.rate.clone();
    let mut manager = UrlManager::new(settings);
    let id = manager
        .add(&server.url("/image.iso"), &root.dest(), None)
        .await
        .unwrap();
    let done = pause_midway(&mut manager, id).await;
    let part = root.dest().join("image.iso.part");
    assert!(part.exists());
    server.behaviour.lock().unwrap().etag = Some("\"fixture-v2\"".into());
    rate.set_limit(0, 0).unwrap();
    manager.resume(id).unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Error);
    let reason = manager.detail(id).unwrap().error.unwrap();
    assert!(
        reason.contains("file changed") && reason.contains("ETag"),
        "{reason}"
    );
    assert!(part.exists(), "the partial file is kept");
    assert!(!root.dest().join("image.iso").exists());
    assert!(std::fs::metadata(&part).unwrap().len() >= done);

    // The same server, identity restored: Retry resumes and completes.
    server.behaviour.lock().unwrap().etag = Some("\"fixture-v1\"".into());
    manager.retry(id).unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    assert_eq!(read(&root.dest().join("image.iso")), data);
    assert!(!part.exists());

    // Last-Modified as the validator, changed under a paused job.
    let root2 = Root::new("modified");
    let server = fixture::start(Behaviour {
        body: data.clone(),
        etag: None,
        dates: Some((
            "Sun, 06 Nov 1994 08:49:37 GMT".into(),
            "Mon, 07 Nov 1994 08:49:37 GMT".into(),
        )),
        ..Behaviour::default()
    })
    .await;
    let settings = config(1500, Duration::from_secs(30));
    let rate = settings.rate.clone();
    let mut manager = UrlManager::new(settings);
    let id = manager
        .add(&server.url("/dated.bin"), &root2.dest(), None)
        .await
        .unwrap();
    pause_midway(&mut manager, id).await;
    assert!(
        manager
            .detail(id)
            .unwrap()
            .validator
            .unwrap()
            .starts_with("Last-Modified")
    );
    server.behaviour.lock().unwrap().dates = Some((
        "Tue, 08 Nov 1994 08:49:37 GMT".into(),
        "Wed, 09 Nov 1994 08:49:37 GMT".into(),
    ));
    rate.set_limit(0, 0).unwrap();
    manager.resume(id).unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Error);
    let reason = manager.detail(id).unwrap().error.unwrap();
    assert!(reason.contains("Last-Modified"), "{reason}");
    assert!(root2.dest().join("dated.bin.part").exists());
}

#[tokio::test]
async fn pause_and_resume_continue_from_the_recorded_offsets() {
    let root = Root::new("pause");
    let data = body(2 * 1024 * 1024 + 1);
    let server = fixture::start(Behaviour {
        body: data.clone(),
        ..Behaviour::default()
    })
    .await;
    let settings = config(1500, Duration::from_secs(30));
    let rate = settings.rate.clone();
    let mut manager = UrlManager::new(settings);
    let id = manager
        .add(&server.url("/paused.bin"), &root.dest(), None)
        .await
        .unwrap();
    let done = pause_midway(&mut manager, id).await;
    let before = server.request_count();
    tokio::time::sleep(Duration::from_millis(200)).await;
    manager.tick();
    assert_eq!(
        manager
            .rows()
            .iter()
            .find(|row| row.id == id)
            .unwrap()
            .connected_peers,
        0,
        "no connection while paused"
    );
    assert_eq!(server.request_count(), before, "a paused job sends nothing");
    let record = manager
        .take_dirty()
        .into_iter()
        .find(|record| record.name.as_deref() == Some("paused.bin"))
        .unwrap();
    assert!(record.paused);
    assert_eq!(record.done_bytes(), done);
    let part_len = std::fs::metadata(root.dest().join("paused.bin.part"))
        .unwrap()
        .len();
    // Parts that wrote nothing yet may start past the end of the sparse file.
    assert!(
        record
            .parts
            .iter()
            .filter(|part| part.done > 0)
            .all(|part| part.start + part.done <= part_len),
        "{:?} within {part_len}",
        record.parts
    );
    rate.set_limit(0, 0).unwrap();
    manager.resume(id).unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    assert_eq!(read(&root.dest().join("paused.bin")), data);
    let resumed: Vec<String> = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .skip(before)
        .cloned()
        .collect();
    assert!(!resumed.is_empty());
    for request in &resumed {
        assert!(request.contains("range: bytes="), "{request}");
        assert!(request.contains("if-range: \"fixture-v1\""), "{request}");
    }
    let continued = record
        .parts
        .iter()
        .filter(|part| part.done > 0 && !part.is_complete())
        .count();
    assert!(
        continued > 0,
        "at least one part resumed mid-way: {:?}",
        record.parts
    );
    for part in record
        .parts
        .iter()
        .filter(|part| part.done > 0 && !part.is_complete())
    {
        let expected = format!("range: bytes={}-{}", part.start + part.done, part.end - 1);
        assert!(
            resumed.iter().any(|request| request.contains(&expected)),
            "{expected} in {resumed:?}"
        );
    }

    // Cancel keeps the partial file and Retry restarts from it.
    let root2 = Root::new("cancel");
    let settings = config(1500, Duration::from_secs(30));
    let rate = settings.rate.clone();
    let mut manager = UrlManager::new(settings);
    let id = manager
        .add(&server.url("/cancel.bin"), &root2.dest(), None)
        .await
        .unwrap();
    wait_for(&mut manager, id, Duration::from_secs(20), |manager| {
        manager
            .rows()
            .iter()
            .any(|row| row.id == id && row.verified_bytes > 50_000)
    })
    .await;
    manager.cancel(id).await.unwrap();
    assert_eq!(state(&manager, id), JobState::Error);
    assert!(
        manager
            .detail(id)
            .unwrap()
            .error
            .unwrap()
            .contains("cancelled")
    );
    assert!(root2.dest().join("cancel.bin.part").exists());
    assert!(
        manager.resume(id).is_err(),
        "Resume does not restart a cancelled job"
    );
    rate.set_limit(0, 0).unwrap();
    manager.retry(id).unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    assert_eq!(read(&root2.dest().join("cancel.bin")), data);
}

#[tokio::test]
async fn shared_speed_limit_and_budget_govern_url_downloads() {
    let root = Root::new("limit");
    let data = body(400_000);
    let server = fixture::start(Behaviour {
        body: data.clone(),
        ..Behaviour::default()
    })
    .await;
    // The driver's own limits: 200 kB/s shared with every peer.
    let driver = Driver::new(Limits {
        download_kb_s: 200,
        ..Limits::default()
    })
    .unwrap();
    let (rate, origin) = driver.download_rate();
    let budget = driver.payload_budget();
    assert!(rate.same_bucket(&driver.download_rate().0));
    let mut manager = UrlManager::new(UrlConfig {
        budget: budget.clone(),
        rate: rate.clone(),
        origin,
        allow_local: true,
        tls: None,
        idle: Duration::from_secs(30),
        max_active: 3,
    });
    let started = Instant::now();
    let id = manager
        .add(&server.url("/limited.bin"), &root.dest(), None)
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    let elapsed = started.elapsed();
    // 400 kB at 200 kB/s with a 50 kB burst: at least 1.75 s.
    assert!(
        elapsed >= Duration::from_millis(1600),
        "took {elapsed:?}, the limit was not applied"
    );
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
    assert_eq!(read(&root.dest().join("limited.bin")), data);
    assert!(
        budget.high_watermark() >= 16 * 1024 && budget.high_watermark() <= 8 * READ_CHUNK,
        "read buffers were reserved from the session budget: {}",
        budget.high_watermark()
    );
    assert_eq!(budget.used(), 0, "every permit was released");

    // Unlimited: the same body is quick, so the delay above was the bucket.
    rate.set_limit(0, 0).unwrap();
    let started = Instant::now();
    let id = manager
        .add(&server.url("/fast.bin"), &root.dest(), None)
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    assert!(started.elapsed() < Duration::from_millis(1600));
    drop(driver);
}

#[tokio::test]
async fn crash_resume_from_a_persisted_record_never_trusts_unsynced_bytes() {
    let root = Root::new("crash");
    let data = body(3 * 1024 * 1024 + 11);
    let server = fixture::start(Behaviour {
        body: data.clone(),
        ..Behaviour::default()
    })
    .await;
    let settings = config(2000, Duration::from_secs(30));
    let budget = settings.budget.clone();
    let rate = settings.rate.clone();
    let origin = settings.origin;
    let mut manager = UrlManager::new(settings);
    let id = manager
        .add(&server.url("/crash.bin"), &root.dest(), None)
        .await
        .unwrap();
    wait_for(&mut manager, id, Duration::from_secs(20), |manager| {
        manager
            .rows()
            .iter()
            .any(|row| row.id == id && row.verified_bytes > 200_000)
    })
    .await;
    let records = manager.checkpoint().await;
    let record = records
        .into_iter()
        .find(|record| record.name.as_deref() == Some("crash.bin"))
        .unwrap();
    let persisted = record.done_bytes();
    assert!(persisted > 0 && persisted < data.len() as u64);
    let on_disk = std::fs::metadata(root.dest().join("crash.bin.part"))
        .unwrap()
        .len();
    assert!(
        record
            .parts
            .iter()
            .filter(|part| part.done > 0)
            .all(|part| part.start + part.done <= on_disk),
        "persisted offsets never exceed the file: {:?} within {on_disk}",
        record.parts
    );
    // Crash: tasks die, nothing is synced or saved after the checkpoint.
    manager.abort_all_for_test();
    drop(manager);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let requests_before = server.request_count();

    let mut restarted = UrlManager::new(UrlConfig {
        budget,
        rate: rate.clone(),
        origin,
        allow_local: true,
        tls: None,
        idle: Duration::from_secs(30),
        max_active: 3,
    });
    rate.set_limit(0, 0).unwrap();
    let id = restarted.restore(record.clone()).unwrap();
    assert_eq!(
        state(&restarted, id),
        JobState::Queued,
        "running intent survives"
    );
    assert_eq!(wait_terminal(&mut restarted, id).await, JobState::Complete);
    assert_eq!(read(&root.dest().join("crash.bin")), data);
    let resumed: Vec<String> = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .skip(requests_before)
        .cloned()
        .collect();
    for part in record.parts.iter().filter(|part| !part.is_complete()) {
        let expected = format!("range: bytes={}-{}", part.start + part.done, part.end - 1);
        assert!(
            resumed.iter().any(|request| request.contains(&expected)),
            "{expected} not in {resumed:?}"
        );
    }
    assert!(resumed.iter().all(|request| request.contains("if-range:")));

    // A record whose server gave no validator restarts from byte 0.
    let root2 = Root::new("novalidator");
    let server = fixture::start(Behaviour {
        body: data.clone(),
        etag: None,
        ..Behaviour::default()
    })
    .await;
    let settings = config(2000, Duration::from_secs(30));
    let rate = settings.rate.clone();
    let mut manager = UrlManager::new(settings);
    let id = manager
        .add(&server.url("/anon.bin"), &root2.dest(), None)
        .await
        .unwrap();
    wait_for(&mut manager, id, Duration::from_secs(20), |manager| {
        manager
            .rows()
            .iter()
            .any(|row| row.id == id && row.verified_bytes > 200_000)
    })
    .await;
    let record = manager
        .checkpoint()
        .await
        .into_iter()
        .find(|r| r.name.as_deref() == Some("anon.bin"))
        .unwrap();
    assert!(record.validator.is_none());
    manager.abort_all_for_test();
    drop(manager);
    let settings = config(0, Duration::from_secs(30));
    let mut restarted = UrlManager::new(settings);
    let before = server.request_count();
    let id = restarted.restore(record).unwrap();
    assert_eq!(wait_terminal(&mut restarted, id).await, JobState::Complete);
    assert_eq!(read(&root2.dest().join("anon.bin")), data);
    let note = restarted.detail(id).unwrap().note.unwrap();
    assert!(note.contains("restarted from byte 0"), "{note}");
    let first = server.requests.lock().unwrap()[before].clone();
    assert!(first.contains("range: bytes=0-"), "{first}");
    let _ = rate;
}

#[tokio::test]
async fn sha256_is_checked_before_completion() {
    let root = Root::new("sha");
    let data = body(90_000);
    let server = fixture::start(Behaviour {
        body: data.clone(),
        ..Behaviour::default()
    })
    .await;
    let digest = {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(&data);
        let out: [u8; 32] = hasher.finalize().into();
        out
    };
    let mut manager = UrlManager::new(config(0, Duration::from_secs(30)));
    let good = manager
        .add(&server.url("/good.bin"), &root.dest(), Some(digest))
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, good).await, JobState::Complete);
    let wrong = parse_sha256(&"ab".repeat(32)).unwrap();
    let bad = manager
        .add(&server.url("/bad.bin"), &root.dest(), Some(wrong))
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, bad).await, JobState::Error);
    let reason = manager.detail(bad).unwrap().error.unwrap();
    assert!(reason.contains("SHA-256 mismatch"), "{reason}");
    assert!(
        root.dest().join("bad.bin.part").exists(),
        "the bytes are kept"
    );
    assert!(!root.dest().join("bad.bin").exists());
}

#[tokio::test]
async fn a_silent_connection_is_cut_and_retried_without_a_total_timeout() {
    let root = Root::new("stall");
    let data = body(600_000);
    let server = fixture::start(Behaviour {
        body: data.clone(),
        // The first connection goes silent after 100 kB.
        stall: Some((100_000, 0)),
        ..Behaviour::default()
    })
    .await;
    let mut manager = UrlManager::new(config(0, Duration::from_millis(300)));
    let started = Instant::now();
    let id = manager
        .add(&server.url("/stall.bin"), &root.dest(), None)
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "the idle cut fired"
    );
    assert_eq!(read(&root.dest().join("stall.bin")), data);
    assert_eq!(
        server.request_count(),
        2,
        "one retry, from the recorded offset"
    );
    let second = server.requests.lock().unwrap()[1].clone();
    assert!(second.contains("range: bytes=100000-"), "{second}");

    // Every attempt silent: the job stops with the reason.
    let root2 = Root::new("stall-all");
    let server = fixture::start(Behaviour {
        body: data.clone(),
        stall: Some((0, usize::MAX)),
        ..Behaviour::default()
    })
    .await;
    // Stall every request: the fixture matches on index, so stall them all
    // by setting the index after each request.
    let shared = Arc::clone(&server.behaviour);
    let watcher = {
        let requests = Arc::clone(&server.requests);
        tokio::spawn(async move {
            loop {
                let count = requests.lock().unwrap().len();
                shared.lock().unwrap().stall = Some((0, count));
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
    };
    let mut manager = UrlManager::new(config(0, Duration::from_millis(200)));
    let id = manager
        .add(&server.url("/never.bin"), &root2.dest(), None)
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Error);
    watcher.abort();
    let reason = manager.detail(id).unwrap().error.unwrap();
    assert!(
        reason.contains("attempts") || reason.contains("idle") || reason.contains("headers"),
        "{reason}"
    );
}

/// Create a file symlink. Windows needs Developer Mode or the symlink
/// privilege; without it the symlink half of a test is skipped, loudly.
fn symlink_file(target: &std::path::Path, link: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }
    #[cfg(windows)]
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("skipping the symlink check: {error}");
            false
        }
    }
}

#[tokio::test]
async fn delete_data_removes_the_file_and_part_but_refuses_links() {
    let root = Root::new("delete");
    let data = body(80_000);
    let server = fixture::start(Behaviour {
        body: data.clone(),
        ..Behaviour::default()
    })
    .await;
    let mut manager = UrlManager::new(config(0, Duration::from_secs(30)));
    let id = manager
        .add(&server.url("/done.bin"), &root.dest(), None)
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    std::fs::write(root.dest().join("bystander.txt"), b"keep me").unwrap();
    let key = manager.remove(id, true).await.unwrap();
    assert_eq!(
        key,
        UrlRecord::new(
            &server.url("/done.bin"),
            templartorrent_core::paths::canonical_dir(&root.dest()).unwrap(),
            None
        )
        .key
    );
    assert!(!root.dest().join("done.bin").exists());
    assert!(root.dest().is_dir(), "the folder stays");
    assert!(root.dest().join("bystander.txt").exists());
    assert!(manager.is_empty());

    // Remove without data keeps everything.
    let id = manager
        .add(&server.url("/kept.bin"), &root.dest(), None)
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Complete);
    manager.remove(id, false).await.unwrap();
    assert_eq!(read(&root.dest().join("kept.bin")), data);

    // A paused job whose partial was swapped for a symlink is refused.
    let big = body(2 * 1024 * 1024);
    let server = fixture::start(Behaviour {
        body: big,
        ..Behaviour::default()
    })
    .await;
    let settings = config(1500, Duration::from_secs(30));
    let mut manager = UrlManager::new(settings);
    let id = manager
        .add(&server.url("/linked.bin"), &root.dest(), None)
        .await
        .unwrap();
    pause_midway(&mut manager, id).await;
    let part = root.dest().join("linked.bin.part");
    let victim = root.0.join("victim.bin");
    std::fs::write(&victim, b"not yours").unwrap();
    std::fs::remove_file(&part).unwrap();
    if symlink_file(&victim, &part) {
        let error = manager.remove(id, true).await.unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert_eq!(read(&victim), b"not yours");
        assert!(part.is_symlink(), "the link itself is untouched");
        std::fs::remove_file(&part).unwrap();
    }
    // A hardlinked partial is refused as well.
    std::fs::hard_link(&victim, &part).unwrap();
    let error = manager.remove(id, true).await.unwrap_err();
    assert!(error.contains("hardlink"), "{error}");
    assert_eq!(read(&victim), b"not yours");
}

#[tokio::test]
async fn existing_files_are_never_overwritten() {
    let root = Root::new("collide");
    let data = body(10_000);
    let server = fixture::start(Behaviour {
        body: data.clone(),
        ..Behaviour::default()
    })
    .await;
    std::fs::create_dir_all(root.dest()).unwrap();
    std::fs::write(root.dest().join("taken.bin"), b"mine").unwrap();
    let mut manager = UrlManager::new(config(0, Duration::from_secs(30)));
    let id = manager
        .add(&server.url("/taken.bin"), &root.dest(), None)
        .await
        .unwrap();
    assert_eq!(wait_terminal(&mut manager, id).await, JobState::Error);
    let reason = manager.detail(id).unwrap().error.unwrap();
    assert!(reason.contains("already exists"), "{reason}");
    assert_eq!(read(&root.dest().join("taken.bin")), b"mine");
    let file = root.0.join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let error = manager
        .add(&server.url("/x"), &file, None)
        .await
        .unwrap_err();
    assert!(error.contains("directory"), "{error}");
    let duplicate = manager
        .add(&server.url("/taken.bin"), &root.dest(), None)
        .await
        .unwrap_err();
    assert!(duplicate.contains("already queued"), "{duplicate}");
}

mod tls {
    use std::sync::Arc;

    pub struct TestCa {
        cert: rcgen::Certificate,
        key: rcgen::KeyPair,
        der: rustls::pki_types::CertificateDer<'static>,
    }

    impl TestCa {
        pub fn new() -> Self {
            use rcgen::{
                BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose,
            };
            let key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
            params
                .distinguished_name
                .push(DnType::CommonName, "Templar Test CA");
            let cert = params.self_signed(&key).unwrap();
            let der = cert.der().clone();
            Self { cert, key, der }
        }

        pub fn issue(&self, name: &str) -> rustls::ServerConfig {
            use rcgen::{
                CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
            };
            use rustls::pki_types::PrivateKeyDer;
            let key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::new(vec![name.to_string()]).unwrap();
            params.is_ca = IsCa::ExplicitNoCa;
            params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            let cert = params.signed_by(&key, &self.cert, &self.key).unwrap();
            let key_der = PrivateKeyDer::Pkcs8(key.serialize_der().into());
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.der().clone()], key_der)
                .unwrap()
        }

        pub fn client_config(&self) -> Arc<rustls::ClientConfig> {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(self.der.clone()).unwrap();
            Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        }
    }
}
