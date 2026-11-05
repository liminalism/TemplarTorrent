//! One URL job as a Tokio task: probe, split, transfer, verify, finish.
//!
//! The task never touches the manager's record. It reports what it learned
//! ([`Event::Described`]) and how it ended ([`Event::Finished`]), and bumps
//! the shared [`Progress`] counters after every `write_at`. Cancellation is
//! `JoinHandle::abort`: it lands on an await point, so a write in flight
//! completes and the counters the manager reads afterwards are written.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use rustls::ClientConfig;
use sha2::{Digest, Sha256};
use templartorrent_core::{bandwidth::SharedBucket, budget::ByteBudget, model::TorrentId};
use tokio::{sync::mpsc, task::JoinSet};

use super::{
    Event, MAX_ATTEMPTS, PartRecord, Progress, READ_CHUNK, RETRY_BASE, RETRY_CAP, UrlRecord,
    Validator, derive_name, hex, lock, plan_parts,
    transport::{self, Connection, Request, parse_url},
};

/// Positional read: `pread` on Unix, `ReadFile` with an offset on Windows.
/// Neither depends on (Windows: nor serialises through) a shared cursor that
/// any other reader or writer relies on.
fn read_at(file: &std::fs::File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buffer, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
    }
}

/// Positional write of the whole buffer.
#[cfg(unix)]
fn write_all_at(file: &std::fs::File, buffer: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buffer, offset)
}

/// Positional write of the whole buffer.
#[cfg(windows)]
fn write_all_at(file: &std::fs::File, mut buffer: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !buffer.is_empty() {
        match std::os::windows::fs::FileExt::seek_write(file, buffer, offset) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buffer = &buffer[n..];
                offset += n as u64;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) struct Context {
    pub id: TorrentId,
    pub record: UrlRecord,
    pub progress: Arc<Progress>,
    pub events: mpsc::Sender<Event>,
    pub budget: ByteBudget,
    pub rate: SharedBucket,
    pub origin: Instant,
    pub allow_local: bool,
    pub tls: Option<Arc<ClientConfig>>,
    pub idle: Duration,
}

/// Everything a part needs that the job shares.
struct Shared {
    budget: ByteBudget,
    rate: SharedBucket,
    origin: Instant,
    allow_local: bool,
    tls: Option<Arc<ClientConfig>>,
    idle: Duration,
    url: transport::ParsedUrl,
    file: Arc<std::fs::File>,
    progress: Arc<Progress>,
    validator: Option<Validator>,
    size: Option<u64>,
    ranged: bool,
}

pub(super) async fn run(context: Context) {
    let id = context.id;
    let events = context.events.clone();
    let progress = Arc::clone(&context.progress);
    let result = run_inner(context).await;
    // A stopped job's offsets may be folded into its record only once the
    // bytes behind them are durable; success synced inside `run_inner`.
    let synced = match &result {
        Ok(_) => true,
        Err(_) => {
            let file = lock(&progress.file).clone();
            match file {
                Some(file) => matches!(
                    tokio::task::spawn_blocking(move || file.sync_data()).await,
                    Ok(Ok(()))
                ),
                None => false,
            }
        }
    };
    let _ = events.send(Event::Finished { id, result, synced }).await;
}

/// `Ok(size)` once the final file is in place.
async fn run_inner(context: Context) -> Result<u64, String> {
    let Context {
        id,
        mut record,
        progress,
        events,
        budget,
        rate,
        origin,
        allow_local,
        tls,
        idle,
    } = context;
    let url = parse_url(&record.url).map_err(str::to_string)?;
    let mut probe = None;
    let fresh = record.name.is_none();
    let mut split = false;
    if fresh {
        let connection = transport::open(
            &url,
            &Request {
                range: Some((0, None)),
                if_range: None,
            },
            allow_local,
            tls.clone(),
            idle,
        )
        .await
        .map_err(|reason| reason.to_string())?;
        let head = &connection.head;
        let identity = head.identity_encoded();
        let (size, ranged) = match head.status {
            206 => {
                let (first, _, total) = head
                    .content_range()
                    .ok_or("the server sent 206 without a usable Content-Range")?;
                if first != 0 {
                    return Err("the server's 206 did not start at byte 0".into());
                }
                let total = total.ok_or("the server's 206 has no total length")?;
                if !identity {
                    return Err(
                        "the server applied a Content-Encoding to a ranged body; offsets cannot be trusted"
                            .into(),
                    );
                }
                split = true;
                (Some(total), true)
            }
            200 => {
                let size = if identity {
                    head.content_length()
                } else {
                    None
                };
                (size, identity && head.accepts_ranges())
            }
            other => return Err(format!("the server answered {other}")),
        };
        let validator = if identity {
            Validator::from_head(head)
        } else {
            None
        };
        let name = derive_name(head, &connection.url);
        record.name = Some(name.clone());
        record.size = size;
        record.ranged = ranged;
        record.validator = validator.clone();
        record.parts = plan_parts(size, split, record.connections);
        let final_path = record.final_path().ok_or("no file name")?;
        let part_path = record.part_path().ok_or("no file name")?;
        if exists_nofollow(&final_path)? {
            return Err(format!(
                "a file named {name} already exists in the destination; choose another folder"
            ));
        }
        if exists_nofollow(&part_path)? {
            return Err(format!(
                "{name}.part already exists in the destination and no job owns it"
            ));
        }
        *lock(&progress.parts) = record.parts.iter().map(|part| part.done).collect();
        events
            .send(Event::Described {
                id,
                name,
                size,
                ranged,
                validator,
                parts: record.parts.clone(),
            })
            .await
            .map_err(|_| "the session is shutting down".to_string())?;
        probe = Some(connection);
    }
    let part_path = record.part_path().ok_or("no file name")?;
    let final_path = record.final_path().ok_or("no file name")?;
    if !fresh && exists_nofollow(&final_path)? {
        return Err(format!(
            "a file named {} already exists in the destination",
            record.name.as_deref().unwrap_or("download")
        ));
    }
    let file = open_part(&part_path)?;
    let on_disk = file
        .metadata()
        .map_err(|error| format!("{}: {error}", part_path.display()))?
        .len();
    // Resume rules: offsets need a validator; bytes beyond the file are gone.
    let resuming = record.parts.iter().any(|part| part.done > 0);
    if resuming && record.validator.is_none() {
        for part in &mut record.parts {
            part.done = 0;
        }
        let _ = events
            .send(Event::Note {
                id,
                text: "restarted from byte 0: the server gave no ETag or Last-Modified to resume against"
                    .into(),
            })
            .await;
    }
    for part in &mut record.parts {
        let written_end = part.start.saturating_add(part.done);
        if written_end > on_disk {
            part.done = on_disk.saturating_sub(part.start);
        }
    }
    *lock(&progress.parts) = record.parts.iter().map(|part| part.done).collect();
    let file = Arc::new(file);
    *lock(&progress.file) = Some(Arc::clone(&file));
    let shared = Arc::new(Shared {
        budget,
        rate,
        origin,
        allow_local,
        tls,
        idle,
        url: url.clone(),
        file: Arc::clone(&file),
        progress: Arc::clone(&progress),
        validator: record.validator.clone(),
        size: record.size,
        ranged: record.ranged,
    });
    let mut set = JoinSet::new();
    for (index, part) in record.parts.iter().enumerate() {
        if part.is_complete() {
            continue;
        }
        let initial = if index == 0 && part.done == 0 {
            probe.take()
        } else {
            None
        };
        set.spawn(part_task(Arc::clone(&shared), index, *part, initial));
    }
    drop(probe);
    let mut received_total = 0u64;
    while let Some(joined) = set.join_next().await {
        let outcome = joined.map_err(|_| "a transfer task stopped unexpectedly".to_string())?;
        received_total = received_total.saturating_add(outcome?);
    }
    let _ = received_total;
    let size = match record.size {
        Some(size) => size,
        None => progress.offsets().iter().sum(),
    };
    // Finish: digest, sync, rename, directory sync.
    progress.phase.store(1, Ordering::Release);
    let expected = record.sha256;
    let finishing = Arc::clone(&file);
    let actual = tokio::task::spawn_blocking(move || -> Result<Option<[u8; 32]>, String> {
        let digest = match expected {
            Some(_) => Some(hash_file(&finishing, size)?),
            None => None,
        };
        finishing
            .sync_data()
            .map_err(|error| format!("sync failed: {error}"))?;
        Ok(digest)
    })
    .await
    .map_err(|_| "the finishing task stopped unexpectedly".to_string())??;
    if let (Some(expected), Some(actual)) = (expected, actual)
        && expected != actual
    {
        return Err(format!(
            "SHA-256 mismatch: expected {}, got {}; the file was kept as {}.part",
            hex(&expected),
            hex(&actual),
            record.name.as_deref().unwrap_or("download")
        ));
    }
    progress.phase.store(2, Ordering::Release);
    if exists_nofollow(&final_path)? {
        return Err(format!(
            "a file named {} appeared in the destination before the download finished",
            record.name.as_deref().unwrap_or("download")
        ));
    }
    // hard_link + unlink never replaces an existing file, unlike rename.
    std::fs::hard_link(&part_path, &final_path)
        .map_err(|error| format!("could not place {}: {error}", final_path.display()))?;
    std::fs::remove_file(&part_path)
        .map_err(|error| format!("could not remove {}: {error}", part_path.display()))?;
    if let Ok(dir) = std::fs::File::open(&record.destination) {
        let _ = dir.sync_all();
    }
    lock(&progress.file).take();
    Ok(size)
}

fn exists_nofollow(path: &std::path::Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "{} is a symbolic link; refusing to write through it",
            path.display()
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Open or create `<name>.part` for reading and writing without truncating.
/// A symlink at that name is refused before the open.
fn open_part(path: &std::path::Path) -> Result<std::fs::File, String> {
    if exists_nofollow(path)? {
        let meta = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        if !meta.is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))
}

fn hash_file(file: &std::fs::File, size: u64) -> Result<[u8; 32], String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut offset = 0u64;
    while offset < size {
        let want = buffer
            .len()
            .min(usize::try_from(size - offset).unwrap_or(usize::MAX));
        let n = read_at(file, &mut buffer[..want], offset)
            .map_err(|error| format!("digest read failed: {error}"))?;
        if n == 0 {
            return Err("the file is shorter than the download".into());
        }
        hasher.update(&buffer[..n]);
        offset += n as u64;
    }
    Ok(hasher.finalize().into())
}

enum Failure {
    /// Retry after backoff.
    Transient(String),
    /// Stop the job; the reason is shown.
    Permanent(String),
}

/// Download one part, reconnecting with bounded backoff. `initial` is the
/// probe connection when this is part 0 of a fresh job. Returns the bytes
/// received over every connection.
async fn part_task(
    shared: Arc<Shared>,
    index: usize,
    part: PartRecord,
    mut initial: Option<Connection>,
) -> Result<u64, String> {
    let done = Arc::new(AtomicU64::new(part.done));
    let mut attempts = 0u32;
    let mut last_error = String::from("no connection was attempted");
    let mut received_total = 0u64;
    loop {
        let offset = part.start + done.load(Ordering::Acquire);
        if part.end != u64::MAX && offset >= part.end {
            return Ok(received_total);
        }
        let (connection, expect_206) = match initial.take() {
            Some(connection) => (connection, false),
            None => {
                attempts += 1;
                if attempts > MAX_ATTEMPTS {
                    return Err(format!(
                        "gave up after {MAX_ATTEMPTS} attempts without progress: {last_error}"
                    ));
                }
                if attempts > 1 {
                    let backoff = RETRY_BASE
                        .saturating_mul(1 << (attempts - 2).min(4))
                        .min(RETRY_CAP);
                    tokio::time::sleep(backoff).await;
                }
                // A server that never answered 206 gets a plain request; a
                // 200 is then the whole body again, from byte 0.
                let ranged_request = shared.ranged;
                if offset > 0 && !shared.ranged {
                    return Err(
                        "the connection dropped and the server does not support resuming; use Retry to start again"
                            .into(),
                    );
                }
                let range = if ranged_request {
                    Some((offset, (part.end != u64::MAX).then(|| part.end - 1)))
                } else {
                    None
                };
                let request = Request {
                    range,
                    if_range: shared
                        .validator
                        .as_ref()
                        .filter(|_| ranged_request)
                        .map(|validator| validator.value().to_string()),
                };
                match transport::open(
                    &shared.url,
                    &request,
                    shared.allow_local,
                    shared.tls.clone(),
                    shared.idle,
                )
                .await
                {
                    Ok(connection) => (connection, ranged_request),
                    Err(reason) => {
                        last_error = reason.to_string();
                        continue;
                    }
                }
            }
        };
        check_response(&shared, &connection, offset, expect_206)?;
        shared.progress.active.fetch_add(1, Ordering::AcqRel);
        let outcome = read_part(&shared, index, &part, &done, connection).await;
        shared.progress.active.fetch_sub(1, Ordering::AcqRel);
        match outcome {
            Ok(received) => {
                received_total = received_total.saturating_add(received);
                return Ok(received_total);
            }
            Err((received, Failure::Transient(reason))) => {
                received_total = received_total.saturating_add(received);
                if received > 0 {
                    // Progress keeps the remaining attempts.
                    attempts = 0;
                }
                last_error = reason;
            }
            Err((_, Failure::Permanent(reason))) => return Err(reason),
        }
    }
}

/// A ranged request must come back as a 206 at the requested offset with
/// the known total and an unchanged validator; anything else means the
/// server's file changed or ranges stopped working, and the job stops.
fn check_response(
    shared: &Shared,
    connection: &Connection,
    offset: u64,
    expect_206: bool,
) -> Result<(), String> {
    let head = &connection.head;
    if let Some(validator) = &shared.validator
        && let Some(echoed) = head.header(validator.header_name())
        && echoed.trim() != validator.value()
    {
        return Err(format!(
            "the server's file changed ({} differs); the partial file was kept",
            validator.kind()
        ));
    }
    if !expect_206 {
        return Ok(());
    }
    match head.status {
        206 => {
            let (first, _, total) = head
                .content_range()
                .ok_or("the server sent 206 without a usable Content-Range")?;
            if first != offset {
                return Err(format!(
                    "the server answered the wrong range (asked from {offset}, got {first}); the partial file was kept"
                ));
            }
            if let (Some(known), Some(total)) = (shared.size, total)
                && known != total
            {
                return Err(
                    "the server's file changed (its length differs); the partial file was kept"
                        .into(),
                );
            }
            if !head.identity_encoded() {
                return Err("the server applied a Content-Encoding to a ranged body".into());
            }
            Ok(())
        }
        200 => Err(
            "the server's file changed or it no longer serves ranges (200 to a Range request); the partial file was kept"
                .into(),
        ),
        other => Err(format!("the server answered {other} to a Range request")),
    }
}

/// Read one connection into the file. `Ok(received)` when the part is
/// complete (or the body ended for an unknown size).
async fn read_part(
    shared: &Shared,
    index: usize,
    part: &PartRecord,
    done: &AtomicU64,
    mut connection: Connection,
) -> Result<u64, (u64, Failure)> {
    let mut buffer = vec![0u8; READ_CHUNK];
    let mut received = 0u64;
    loop {
        let written = done.load(Ordering::Acquire);
        let left = (part.end != u64::MAX).then(|| part.end - part.start - written);
        if left == Some(0) {
            return Ok(received);
        }
        let mut want = READ_CHUNK;
        if let Some(left) = left {
            want = want.min(usize::try_from(left).unwrap_or(usize::MAX));
        }
        if shared.rate.rate() != 0 {
            want = want.min(usize::try_from(shared.rate.capacity()).unwrap_or(usize::MAX));
        }
        let want = want.max(1);
        // Memory admission before the read; the permit lives until the bytes
        // are on their way to the file.
        let permit = loop {
            if let Some(permit) = shared.budget.try_acquire(want) {
                break permit;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        // Rate: the whole chunk is reserved up front; a short read forfeits
        // the rest rather than minting credit.
        loop {
            let now = now_ns(shared.origin);
            match shared.rate.wait_ns(want as u64, now) {
                Some(0) => {
                    if shared.rate.reserve(want as u64, now) {
                        break;
                    }
                }
                Some(wait) => tokio::time::sleep(Duration::from_nanos(wait)).await,
                None => {
                    return Err((
                        received,
                        Failure::Permanent("read chunk exceeds the rate bucket capacity".into()),
                    ));
                }
            }
        }
        let n = match connection.read(&mut buffer[..want]).await {
            Ok(n) => n,
            Err(error) => {
                return Err((received, Failure::Transient(error.to_string())));
            }
        };
        if n == 0 {
            drop(permit);
            if left.is_some() {
                return Err((
                    received,
                    Failure::Transient("the server closed the connection early".into()),
                ));
            }
            return Ok(received);
        }
        let offset = part.start + written;
        if let Err(error) = write_all_at(&shared.file, &buffer[..n], offset) {
            return Err((
                received,
                Failure::Permanent(format!("write failed at byte {offset}: {error}")),
            ));
        }
        drop(permit);
        let now_done = written + n as u64;
        done.store(now_done, Ordering::Release);
        if let Some(slot) = lock(&shared.progress.parts).get_mut(index) {
            *slot = now_done;
        }
        shared
            .progress
            .received
            .fetch_add(n as u64, Ordering::AcqRel);
        received += n as u64;
    }
}

fn now_ns(origin: Instant) -> u64 {
    u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
}
