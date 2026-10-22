//! Diagnostic use of the reusable driver, not the production native client.
//! Read the magnet from stdin (bounded; never echo it). No internet discovery
//! happens automatically. Only an explicitly supplied public peer is contacted.
//! Optional --listen ADDRESS --seed-seconds N serves verified data for a bounded
//! interval after completion. The listener is opt-in, never opened by default.
use std::{
    io::{self, BufRead, Read},
    path::PathBuf,
    time::{Duration, Instant},
};
use templartorrent_core::model::{JobState, Limits};
use templartorrent_rbit_adapter::driver::{Driver, ExistingData, PeerOrigin, PeerSource};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let destination = PathBuf::from(args.next().ok_or("usage: verified_transfer NEW_DIRECTORY PEER --public-bootstrap [--recheck] [--listen ADDRESS --seed-seconds N]; magnet on stdin")?);
    let address = args
        .next()
        .and_then(|a| a.into_string().ok())
        .ok_or("missing peer")?
        .parse()
        .map_err(|_| "invalid peer address")?;
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--public-bootstrap")) {
        return Err(
            "explicit --public-bootstrap is required for this public-peer diagnostic".into(),
        );
    }
    let mut existing = ExistingData::Refuse;
    let mut listen = None;
    let mut seed_seconds = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--recheck") => existing = ExistingData::Recheck,
            Some("--listen") if listen.is_none() => {
                listen = Some(
                    args.next()
                        .and_then(|s| s.into_string().ok())
                        .ok_or("missing listener address")?
                        .parse::<std::net::SocketAddr>()
                        .map_err(|_| "invalid listener address")?,
                );
            }
            Some("--seed-seconds") if seed_seconds.is_none() => {
                let n = args
                    .next()
                    .and_then(|s| s.into_string().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|n| (1..=86400).contains(n))
                    .ok_or("seed seconds must be 1..=86400")?;
                seed_seconds = Some(n);
            }
            _ => return Err("unexpected argument".into()),
        }
    }
    if listen.is_some() != seed_seconds.is_some() {
        return Err("--listen and --seed-seconds must be supplied together".into());
    }
    let mut uri = Vec::new();
    io::stdin()
        .lock()
        .take(16 * 1024 + 1)
        .read_until(b'\n', &mut uri)?;
    if uri.len() > 16 * 1024 {
        return Err("magnet exceeds 16 KiB limit".into());
    }
    let uri = std::str::from_utf8(&uri)
        .map_err(|_| "magnet is not UTF-8")?
        .trim_end_matches(['\r', '\n']);
    let mut driver = Driver::new(Limits::default())?;
    let result = async {
        if let Some(address) = listen {
            let bound = driver.listen(address).await?;
            println!("Listening on {bound}");
        }
        let id = driver
            .add_magnet(
                uri,
                &destination,
                existing,
                &[PeerSource {
                    address,
                    origin: PeerOrigin::UserProvided,
                }],
                true,
            )
            .await?;
        let mut last = Instant::now();
        let mut seed_started = None;
        loop {
            driver.step(Duration::from_millis(50)).await?;
            let snapshot = driver.snapshot();
            let row = snapshot
                .jobs
                .iter()
                .find(|row| row.id == id)
                .ok_or("job vanished")?;
            if row.state == JobState::Error {
                return Err(driver.last_error(id).unwrap_or("torrent job failed").into());
            }
            if row.state == JobState::Complete {
                println!(
                    "Verified {} bytes. {}.",
                    row.verified_bytes,
                    driver.seeding_unavailable_reason()
                );
                break;
            }
            if row.state == JobState::Seeding {
                let started = seed_started.get_or_insert_with(Instant::now);
                if started.elapsed() >= Duration::from_secs(seed_seconds.unwrap_or(0)) {
                    println!(
                        "Verified {} bytes; uploaded {} bytes.",
                        row.verified_bytes, row.total_payload_uploaded
                    );
                    break;
                }
            }
            if last.elapsed() >= Duration::from_millis(250) {
                println!(
                    "Verified {} / {} bytes; uploaded {} bytes",
                    row.verified_bytes, row.wanted_bytes, row.total_payload_uploaded
                );
                last = Instant::now();
            }
            if row.state == JobState::Downloading && row.connected_peers == 0 {
                return Err("no connected peer; add another permitted source or retry".into());
            }
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    driver.shutdown().await;
    result
}
