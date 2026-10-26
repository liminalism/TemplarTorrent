//! Opt-in Internet smoke test. Ignored by default and inert without
//! TT_INTERNET_TORRENT, which must name a .torrent for content the operator
//! has explicitly permitted. It drives the same Driver path the client's Add
//! uses for a .torrent: listen, add from the file's info dictionary, step.
//!
//!   TT_INTERNET_TORRENT=/path/x.torrent TT_INTERNET_DEST=/owned/new/dir \
//!   TT_INTERNET_SECONDS=600 cargo test --release -p templartorrent-rbit-adapter \
//!     --test internet_smoke -- --ignored --nocapture
//!
//! Output is counts and rates only: no peer addresses, info hash or tracker
//! URLs. TT_INTERNET_DEST must not exist; the test creates it and leaves the
//! payload for the operator to inspect or delete.
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use templartorrent_core::model::{JobState, Limits};
use templartorrent_rbit_adapter::driver::{Driver, ExistingData};

fn process_rss() -> Option<usize> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|line| line.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse::<usize>()
        .ok()
        .map(|kb| kb * 1024)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "opt-in: contacts the Internet; set TT_INTERNET_TORRENT"]
async fn internet_smoke() {
    let Some(torrent) = std::env::var_os("TT_INTERNET_TORRENT").map(PathBuf::from) else {
        eprintln!("TT_INTERNET_TORRENT unset; nothing contacted");
        return;
    };
    let dest = PathBuf::from(std::env::var_os("TT_INTERNET_DEST").expect("TT_INTERNET_DEST"));
    assert!(!dest.exists(), "TT_INTERNET_DEST must be a new directory");
    let seconds: u64 = std::env::var("TT_INTERNET_SECONDS")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(600);
    let listen = std::env::var("TT_INTERNET_LISTEN").unwrap_or_else(|_| "[::]:0".into());

    let bytes = std::fs::read(&torrent).expect("read torrent");
    let mut driver = Driver::new(Limits::default()).expect("driver");
    let bound = driver.listen(listen.parse().expect("listen address")).await;
    println!("TT_SMOKE listen_ok={} ", bound.is_ok());
    if std::env::var_os("TT_INTERNET_NO_DHT").is_none()
        && let Ok(address) = bound
    {
        println!(
            "TT_SMOKE dht_ok={}",
            driver.enable_dht(address.port()).await.is_ok()
        );
    }
    let start = Instant::now();
    let added = driver
        .add_torrent(&bytes, &dest, ExistingData::Refuse)
        .await;
    let id = match added {
        Ok(id) => id,
        Err(error) => {
            println!(
                "TT_SMOKE add_failed after_ms={} reason={}",
                start.elapsed().as_millis(),
                error.0
            );
            return;
        }
    };
    println!("TT_SMOKE added after_ms={}", start.elapsed().as_millis());
    let mut first_verified_ms = None;
    let mut last_print = Instant::now();
    let mut peak_rss = 0usize;
    let mut peak_peers = 0usize;
    let mut peak_down = 0u64;
    let mut last_verified = 0u64;
    let mut last_verified_at = Instant::now();
    let mut steps = 0u64;
    let mut slow_steps = 0u64;
    let mut hash_failures = 0usize;
    let mut peer_errors: std::collections::BTreeMap<&'static str, usize> = Default::default();
    loop {
        let before = Instant::now();
        let report = driver.step(Duration::from_millis(20)).await;
        if before.elapsed() > Duration::from_millis(500) {
            slow_steps += 1;
        }
        steps += 1;
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                println!("TT_SMOKE step_error={}", error.0);
                break;
            }
        };
        hash_failures += report.hash_failures;
        for error in report.peer_errors {
            *peer_errors.entry(error).or_default() += 1;
        }
        let row = driver
            .snapshot()
            .jobs
            .iter()
            .find(|row| row.id == id)
            .cloned()
            .expect("job row");
        if row.verified_bytes > 0 && first_verified_ms.is_none() {
            first_verified_ms = Some(start.elapsed().as_millis());
        }
        let peers = driver.peer_status(id);
        peak_peers = peak_peers.max(peers.len());
        let down: u64 = peers.iter().map(|peer| peer.down_bps).sum();
        peak_down = peak_down.max(down);
        if let Some(rss) = process_rss() {
            peak_rss = peak_rss.max(rss);
        }
        if last_print.elapsed() >= Duration::from_secs(10) {
            let rate = (row.verified_bytes - last_verified) as f64
                / last_verified_at.elapsed().as_secs_f64();
            last_verified = row.verified_bytes;
            last_verified_at = Instant::now();
            let trackers = driver.tracker_status(id).unwrap_or_default();
            let tracker_ok = trackers.iter().filter(|t| t.last_error.is_none()).count();
            let tracker_errors: Vec<String> = trackers
                .iter()
                .filter_map(|t| t.last_error.clone())
                .collect();
            println!(
                "TT_SMOKE t_s={} state={:?} verified={} wanted={} peers={} unchoked_us={} encrypted={} down_bps={} verified_rate_bps={:.0} connections={} trackers_ok={}/{} tracker_errors={:?} pool_tracker_dht_pex_other={:?} dht_nodes={:?} dht_announced={} leases_inflight_blocked_pickable={:?} payload={:?} hash_failures={} peer_errors={:?} rss={} slow_steps={} steps={}",
                start.elapsed().as_secs(),
                row.state,
                row.verified_bytes,
                row.wanted_bytes,
                peers.len(),
                peers.iter().filter(|peer| !peer.remote_choked).count(),
                peers.iter().filter(|peer| peer.encrypted).count(),
                down,
                rate,
                driver.connection_count(),
                tracker_ok,
                trackers.len(),
                tracker_errors,
                driver.candidate_counts(id),
                driver.dht_nodes(),
                driver.dht_announced(),
                driver.lease_stats(id),
                {
                    let stats = driver.payload_stats();
                    (
                        stats.reserved_bytes,
                        stats.assembly_bytes,
                        stats.inbox_bytes,
                        stats.queued_write_bytes,
                    )
                },
                hash_failures,
                peer_errors,
                process_rss().unwrap_or(0),
                slow_steps,
                steps,
            );
            last_print = Instant::now();
        }
        if row.state == JobState::Complete || row.state == JobState::Seeding {
            println!("TT_SMOKE complete after_s={}", start.elapsed().as_secs());
            break;
        }
        if start.elapsed() >= Duration::from_secs(seconds) {
            break;
        }
    }
    let row = driver
        .snapshot()
        .jobs
        .iter()
        .find(|row| row.id == id)
        .cloned()
        .expect("job row");
    println!(
        "TT_SMOKE_SUMMARY {{\"elapsed_s\":{},\"state\":\"{:?}\",\"verified_bytes\":{},\"wanted_bytes\":{},\"first_verified_ms\":{},\"peak_peers\":{},\"peak_down_bps\":{},\"peak_rss\":{},\"slow_steps\":{},\"steps\":{}}}",
        start.elapsed().as_secs(),
        row.state,
        row.verified_bytes,
        row.wanted_bytes,
        first_verified_ms.map_or("null".into(), |ms| ms.to_string()),
        peak_peers,
        peak_down,
        peak_rss,
        slow_steps,
        steps
    );
    driver.shutdown().await;
}
