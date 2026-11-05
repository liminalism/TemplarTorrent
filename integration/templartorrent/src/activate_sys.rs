//! Activation transport per platform.
//!
//! Unix: a mode-0600 Unix socket in a mode-0700 runtime directory.
//!
//! Windows: std has no Unix sockets, so the primary listens on loopback TCP
//! and writes `<port> <token>` to `instance.port` in the per-user
//! `%LOCALAPPDATA%` runtime directory (private to the user by its inherited
//! ACL). A client sends the 16-byte random token before its payload; a
//! loopback connection without it is dropped unread.

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod unix {
    use std::fs::Permissions;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};

    pub type Listener = UnixListener;
    pub type Stream = UnixStream;
    pub const ENDPOINT: &str = "instance.sock";

    pub fn default_runtime_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(dir).join("templartorrent");
        }
        std::env::temp_dir().join(format!("templartorrent-{}", uid()))
    }

    fn uid() -> u32 {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    }

    pub fn prepare_dir(dir: &Path) -> io::Result<()> {
        std::fs::set_permissions(dir, Permissions::from_mode(0o700))
    }

    pub fn bind(path: &Path) -> io::Result<Listener> {
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    pub fn connect(path: &Path) -> io::Result<Stream> {
        UnixStream::connect(path)
    }

    pub fn authenticate(_listener: &Listener, _stream: &mut Stream) -> bool {
        true
    }
}

#[cfg(windows)]
mod windows {
    use std::io::{self, Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    const TOKEN_LEN: usize = 16;

    #[derive(Debug)]
    pub struct Listener {
        tcp: TcpListener,
        token: [u8; TOKEN_LEN],
    }

    impl Listener {
        pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
            self.tcp.set_nonblocking(nonblocking)
        }

        pub fn accept(&self) -> io::Result<(Stream, SocketAddr)> {
            self.tcp.accept()
        }
    }

    pub type Stream = TcpStream;
    pub const ENDPOINT: &str = "instance.port";

    pub fn default_runtime_dir() -> PathBuf {
        crate::workflow::data_home()
            .unwrap_or_else(std::env::temp_dir)
            .join("templartorrent")
            .join("run")
    }

    pub fn prepare_dir(_dir: &Path) -> io::Result<()> {
        Ok(())
    }

    fn random_token() -> io::Result<[u8; TOKEN_LEN]> {
        use windows_sys::Win32::Security::Cryptography::{
            BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
        };
        let mut token = [0u8; TOKEN_LEN];
        // SAFETY: the buffer and its length match; a null algorithm handle
        // is valid with BCRYPT_USE_SYSTEM_PREFERRED_RNG.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                token.as_mut_ptr(),
                TOKEN_LEN as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            return Err(io::Error::other(crate::text::t("activate.random_failed")));
        }
        Ok(token)
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn parse_endpoint(text: &str) -> Option<(u16, [u8; TOKEN_LEN])> {
        let mut fields = text.split_whitespace();
        let port = fields.next()?.parse().ok()?;
        let token_hex = fields.next()?;
        if token_hex.len() != TOKEN_LEN * 2 || fields.next().is_some() {
            return None;
        }
        let mut token = [0u8; TOKEN_LEN];
        for (index, byte) in token.iter_mut().enumerate() {
            *byte = u8::from_str_radix(token_hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
        }
        Some((port, token))
    }

    /// Bind loopback and publish the endpoint. `create_new` makes two
    /// simultaneous first launches race on the file, not on the port: the
    /// loser sees `AlreadyExists` and forwards instead.
    pub fn bind(path: &Path) -> io::Result<Listener> {
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = tcp.local_addr()?.port();
        let token = random_token()?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let written = file
            .write_all(format!("{port} {}\n", hex(&token)).as_bytes())
            .and_then(|()| file.sync_all());
        if let Err(error) = written {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        Ok(Listener { tcp, token })
    }

    pub fn connect(path: &Path) -> io::Result<Stream> {
        allow_primary_foreground();
        let mut text = String::new();
        std::fs::File::open(path)?
            .take(256)
            .read_to_string(&mut text)?;
        let (port, token) = parse_endpoint(&text).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                crate::text::t("activate.endpoint_malformed"),
            )
        })?;
        let mut stream = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            Duration::from_secs(2),
        )?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(&token)?;
        Ok(stream)
    }

    /// This launch usually holds the foreground right (a browser or Explorer
    /// started it for a click); pass it on so the running instance may raise
    /// its window for the forwarded payload.
    fn allow_primary_foreground() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
        // SAFETY: no pointers; failure only means the window stays behind.
        unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    }

    pub fn authenticate(listener: &Listener, stream: &mut Stream) -> bool {
        let mut token = [0u8; TOKEN_LEN];
        if stream.read_exact(&mut token).is_err() {
            return false;
        }
        token
            .iter()
            .zip(listener.token.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn endpoint_round_trips_and_rejects_garbage() {
            let token = [7u8; super::TOKEN_LEN];
            let text = format!("4242 {}\n", super::hex(&token));
            assert_eq!(super::parse_endpoint(&text), Some((4242, token)));
            assert_eq!(super::parse_endpoint("4242"), None);
            assert_eq!(super::parse_endpoint("4242 zz"), None);
            assert_eq!(super::parse_endpoint(&format!("{text} extra")), None);
        }
    }
}
