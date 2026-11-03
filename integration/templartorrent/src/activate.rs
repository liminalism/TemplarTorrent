//! Owner-only single-instance activation. A second process forwards one
//! length-prefixed magnet or `.torrent` path and exits. Nothing is passed to
//! a shell.
#![allow(unsafe_code)]

use std::fs::Permissions;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const MAX_ACTIVATION_BYTES: usize = 16 * 1024;

type WakeHook = Arc<dyn Fn() + Send + Sync>;
type WakeSlot = Arc<Mutex<Option<WakeHook>>>;

#[derive(Debug)]
pub enum Activation {
    Primary {
        listener: UnixListener,
        path: PathBuf,
        payloads: Vec<String>,
    },
    Forwarded,
}

pub struct Activator {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    rx: std::sync::mpsc::Receiver<String>,
    path: PathBuf,
    waker: WakeSlot,
}

pub fn valid_activation(text: &str) -> bool {
    if text.is_empty() || text.len() > MAX_ACTIVATION_BYTES {
        return false;
    }
    if text
        .bytes()
        .any(|byte| byte == b'\n' || byte == b'\r' || byte == 0)
    {
        return false;
    }
    if crate::workflow::normalize_magnet(text).is_ok()
        || text.starts_with("http://")
        || text.starts_with("https://")
    {
        return true;
    }
    Path::new(text)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("torrent"))
}

pub fn default_runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("templartorrent");
    }
    std::env::temp_dir().join(format!("templartorrent-{}", uid()))
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn activate(dir: &Path, payloads: &[String]) -> io::Result<Activation> {
    for payload in payloads {
        if !valid_activation(payload) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "activation must be one magnet or a .torrent path",
            ));
        }
    }
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, Permissions::from_mode(0o700))?;
    let path = dir.join("instance.sock");
    if is_symlink(&path) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "activation socket path is a symlink",
        ));
    }
    if path.exists() {
        match forward_all(&path, payloads) {
            Ok(()) => return Ok(Activation::Forwarded),
            Err(_) => {
                if is_symlink(&path) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "activation socket path is a symlink",
                    ));
                }
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    match UnixListener::bind(&path) {
        Ok(listener) => {
            std::fs::set_permissions(&path, Permissions::from_mode(0o600))?;
            Ok(Activation::Primary {
                listener,
                path,
                payloads: payloads.to_vec(),
            })
        }
        Err(error) => {
            if forward_all(&path, payloads).is_ok() {
                Ok(Activation::Forwarded)
            } else {
                Err(error)
            }
        }
    }
}

pub fn spawn(listener: UnixListener, path: PathBuf) -> Activator {
    let (tx, rx) = std::sync::mpsc::sync_channel(8);
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let waker = Arc::new(Mutex::new(None));
    let wake = Arc::clone(&waker);
    let thread = thread::Builder::new()
        .name("templartorrent-activate".into())
        .spawn(move || serve(listener, tx, flag, wake))
        .ok();
    Activator {
        stop,
        thread,
        rx,
        path,
        waker,
    }
}

impl Activator {
    pub fn try_recv(&self) -> Option<String> {
        self.rx.try_recv().ok()
    }

    pub fn set_waker(&self, wake: WakeHook) {
        *self
            .waker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(wake);
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Activator {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn serve(
    listener: UnixListener,
    tx: SyncSender<String>,
    stop: Arc<AtomicBool>,
    waker: WakeSlot,
) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => accept_one(stream, &tx, &waker),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
}

fn accept_one(mut stream: UnixStream, tx: &SyncSender<String>, waker: &Mutex<Option<WakeHook>>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut len_buf = [0u8; 4];
    if stream.read_exact(&mut len_buf).is_err() {
        return;
    }
    let len = u32::from_le_bytes(len_buf);
    if len == 0 || len as usize > MAX_ACTIVATION_BYTES {
        let _ = reply(&mut stream, 0);
        return;
    }
    let mut buf = vec![0u8; len as usize];
    if stream.read_exact(&mut buf).is_err() {
        return;
    }
    let Ok(text) = String::from_utf8(buf) else {
        let _ = reply(&mut stream, 0);
        return;
    };
    if !valid_activation(&text) || tx.try_send(text).is_err() {
        let _ = reply(&mut stream, 0);
        return;
    }
    let wake = waker
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(wake) = wake {
        wake();
    }
    let _ = reply(&mut stream, 1);
}

fn forward_all(path: &Path, payloads: &[String]) -> io::Result<()> {
    if payloads.is_empty() {
        UnixStream::connect(path).map(|_| ())
    } else {
        for payload in payloads {
            forward_one(path, payload)?;
        }
        Ok(())
    }
}

fn forward_one(path: &Path, payload: &str) -> io::Result<()> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "activation is too large"))?;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(payload.as_bytes())?;
    let mut reply_byte = [0u8; 1];
    stream.read_exact(&mut reply_byte)?;
    if reply_byte[0] == 1 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the running instance rejected the activation",
        ))
    }
}

fn reply(stream: &mut UnixStream, byte: u8) -> io::Result<()> {
    stream.write_all(&[byte])
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn activation_forwards_one_bounded_magnet() {
        let dir = std::env::temp_dir().join(format!("tt-act-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = "magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let activation = activate(&dir, &[first.into()]).unwrap();
        let Activation::Primary {
            listener,
            path,
            payloads,
        } = activation
        else {
            panic!("the first process should own the socket");
        };
        assert_eq!(payloads, vec![first.to_string()]);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "socket must be owner-only, mode {mode:o}");
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&wakes);
        let waker = Arc::new(Mutex::new(Some(Arc::new(move || {
            counted.fetch_add(1, Ordering::Relaxed);
        }) as WakeHook)));
        let thread = thread::spawn(move || serve(listener, tx, flag, waker));
        let shellish = "magnet:?xt=urn:btih:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb;rm -rf /";
        let second = activate(&dir, &[shellish.into()]).unwrap();
        assert!(matches!(second, Activation::Forwarded));
        let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(got, shellish);
        assert!(
            wakes.load(Ordering::Relaxed) >= 1,
            "an accepted activation must wake the window"
        );
        let newline = "magnet:?xt=urn:btih:aa\nrm -rf /";
        assert!(activate(&dir, &[newline.into()]).is_err());
        let huge = format!("magnet:?{}", "a".repeat(20_000));
        assert!(activate(&dir, &[huge]).is_err());
        assert!(rx.try_recv().is_err(), "rejected payloads must not arrive");
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
