//! Message Stream Encryption (MSE), also called Protocol Encryption (PE).
//!
//! This is the Vuze/libtorrent obfuscation handshake that runs on a TCP
//! connection before the normal BitTorrent handshake:
//!
//! ```text
//! 1 A->B: Ya, PadA
//! 2 B->A: Yb, PadB
//! 3 A->B: HASH('req1', S), HASH('req2', SKEY) xor HASH('req3', S),
//!         ENCRYPT(VC, crypto_provide, len(PadC), PadC, len(IA)), ENCRYPT(IA)
//! 4 B->A: ENCRYPT(VC, crypto_select, len(PadD), PadD), ENCRYPT2(payload)
//! 5 A->B: ENCRYPT2(payload)
//! ```
//!
//! Ya and Yb are 768-bit Diffie-Hellman public values (prime [`DH_PRIME`],
//! generator 2), S is the shared secret, SKEY is the torrent's v1 info hash,
//! HASH is SHA-1 and VC is eight zero bytes. ENCRYPT is RC4 keyed with
//! `HASH('keyA', S, SKEY)` (A to B) or `HASH('keyB', S, SKEY)` (B to A), with
//! the first 1024 keystream bytes discarded. ENCRYPT2 is that RC4 stream when
//! RC4 was selected and the identity when plaintext was selected.
//!
//! This is obfuscation against passive traffic shaping, not authentication:
//! anyone who knows the info hash can complete it. The initiator here sends an
//! empty IA and the BitTorrent handshake afterwards, which every
//! implementation of the spec accepts; the responder accepts any IA.
//!
//! The functions do not apply a deadline of their own. Every read is bounded
//! in size (a peer can make us buffer at most [`MAX_HANDSHAKE_BYTES`]), and
//! callers must bound the time with a timeout.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{ready, Context, Poll};

use num_bigint::BigUint;
use rand::{Rng as _, RngCore as _};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use super::error::PeerError;

/// The MSE Diffie-Hellman prime, big-endian (768 bits).
pub const DH_PRIME: [u8; KEY_LEN] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xc9, 0x0f, 0xda, 0xa2, 0x21, 0x68, 0xc2, 0x34,
    0xc4, 0xc6, 0x62, 0x8b, 0x80, 0xdc, 0x1c, 0xd1, 0x29, 0x02, 0x4e, 0x08, 0x8a, 0x67, 0xcc, 0x74,
    0x02, 0x0b, 0xbe, 0xa6, 0x3b, 0x13, 0x9b, 0x22, 0x51, 0x4a, 0x08, 0x79, 0x8e, 0x34, 0x04, 0xdd,
    0xef, 0x95, 0x19, 0xb3, 0xcd, 0x3a, 0x43, 0x1b, 0x30, 0x2b, 0x0a, 0x6d, 0xf2, 0x5f, 0x14, 0x37,
    0x4f, 0xe1, 0x35, 0x6d, 0x6d, 0x51, 0xc2, 0x45, 0xe4, 0x85, 0xb5, 0x76, 0x62, 0x5e, 0x7e, 0xc6,
    0xf4, 0x4c, 0x42, 0xe9, 0xa6, 0x3a, 0x36, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x05, 0x63,
];
/// Length of a public value and of the shared secret on the wire.
pub const KEY_LEN: usize = 96;
/// Largest PadA..PadD the spec allows.
pub const MAX_PAD: usize = 512;
/// `crypto_provide` / `crypto_select` bit for a plaintext payload stream.
pub const CRYPTO_PLAINTEXT: u32 = 0x01;
/// `crypto_provide` / `crypto_select` bit for an RC4 payload stream.
pub const CRYPTO_RC4: u32 = 0x02;
/// Most bytes either side buffers before the handshake is done: Ya, PadA,
/// both 20-byte hashes, VC, crypto_provide, len(PadC), PadC and len(IA).
/// The initiator's worst case (Yb, PadB, VC, select, len(PadD), PadD) is
/// smaller. IA is not buffered; it streams through [`MseStream`].
pub const MAX_HANDSHAKE_BYTES: usize = KEY_LEN + MAX_PAD + 20 + 20 + 8 + 4 + 2 + MAX_PAD + 2;

const VC: [u8; 8] = [0; 8];
const RC4_DROP: usize = 1024;
const PRIVATE_KEY_LEN: usize = 20;
const PLAINTEXT_PREFIX: &[u8; 20] = b"\x13BitTorrent protocol";
/// Largest plaintext chunk encrypted per `poll_write`.
const WRITE_CHUNK: usize = 64 * 1024;

/// What a side is willing to run on the payload stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncryptionPolicy {
    /// Outgoing: MSE offering RC4 and plaintext; the peer picks (normally
    /// RC4). Incoming: accept plaintext BitTorrent or MSE, selecting RC4
    /// whenever the initiator offers it.
    #[default]
    PreferRc4,
    /// Outgoing: MSE offering RC4 only. Incoming: refuse plaintext
    /// BitTorrent and MSE without RC4.
    RequireRc4,
    /// No MSE at all. Outgoing returns a pass-through stream without sending
    /// anything; incoming refuses MSE.
    PlaintextOnly,
}

/// RC4 stream cipher. Each instance is one direction of one connection.
#[derive(Clone)]
pub struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    /// Key schedule only; no keystream is discarded.
    ///
    /// # Panics
    ///
    /// If `key` is empty or longer than 256 bytes.
    pub fn new(key: &[u8]) -> Self {
        assert!(!key.is_empty() && key.len() <= 256, "RC4 key is 1..=256 bytes");
        let mut s = [0u8; 256];
        for (i, slot) in s.iter_mut().enumerate() {
            *slot = i as u8;
        }
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, j as usize);
        }
        Self { s, i: 0, j: 0 }
    }

    /// The MSE variant: key schedule, then 1024 keystream bytes discarded.
    pub fn new_drop1024(key: &[u8]) -> Self {
        let mut rc4 = Self::new(key);
        rc4.discard(RC4_DROP);
        rc4
    }

    /// Advance the keystream by `count` bytes.
    pub fn discard(&mut self, count: usize) {
        for _ in 0..count {
            self.next_byte();
        }
    }

    /// XOR the next `data.len()` keystream bytes into `data`. Encryption and
    /// decryption are the same operation.
    pub fn apply_keystream(&mut self, data: &mut [u8]) {
        for byte in data {
            *byte ^= self.next_byte();
        }
    }

    #[inline]
    fn next_byte(&mut self) -> u8 {
        self.i = self.i.wrapping_add(1);
        self.j = self.j.wrapping_add(self.s[self.i as usize]);
        self.s.swap(self.i as usize, self.j as usize);
        let index = self.s[self.i as usize].wrapping_add(self.s[self.j as usize]);
        self.s[index as usize]
    }
}

impl fmt::Debug for Rc4 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Rc4 { .. }")
    }
}

fn prime() -> &'static BigUint {
    static PRIME: OnceLock<BigUint> = OnceLock::new();
    PRIME.get_or_init(|| BigUint::from_bytes_be(&DH_PRIME))
}

fn to_key_bytes(value: &BigUint) -> [u8; KEY_LEN] {
    let bytes = value.to_bytes_be();
    let mut out = [0u8; KEY_LEN];
    // Values are reduced mod P, so they always fit.
    out[KEY_LEN - bytes.len()..].copy_from_slice(&bytes);
    out
}

/// One side's Diffie-Hellman key pair.
pub struct DhKeyPair {
    private: BigUint,
    public: [u8; KEY_LEN],
}

impl DhKeyPair {
    /// A fresh 160-bit private exponent from the thread RNG.
    pub fn generate() -> Self {
        let mut private = [0u8; PRIVATE_KEY_LEN];
        rand::rng().fill_bytes(&mut private);
        Self::from_private(&private)
    }

    /// A key pair from a caller-chosen big-endian private exponent. Meant for
    /// known-answer tests; use [`DhKeyPair::generate`] on the wire.
    pub fn from_private(private: &[u8]) -> Self {
        let private = BigUint::from_bytes_be(private);
        let public = to_key_bytes(&BigUint::from(2u32).modpow(&private, prime()));
        Self { private, public }
    }

    /// The 96-byte big-endian public value sent as Ya or Yb.
    pub fn public_key(&self) -> &[u8; KEY_LEN] {
        &self.public
    }

    /// S from the remote public value. Values outside `2..P-1` are refused:
    /// they force a predictable secret and no honest peer sends them.
    pub fn shared_secret(&self, remote: &[u8; KEY_LEN]) -> Result<[u8; KEY_LEN], PeerError> {
        let remote = BigUint::from_bytes_be(remote);
        let p = prime();
        if remote <= BigUint::from(1u32) || remote >= p - 1u32 {
            return Err(PeerError::Protocol("mse: invalid public key".into()));
        }
        Ok(to_key_bytes(&remote.modpow(&self.private, p)))
    }
}

impl fmt::Debug for DhKeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DhKeyPair { .. }")
    }
}

fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// `HASH('req1', S)`: the responder's synchronisation marker.
pub fn req1_hash(secret: &[u8; KEY_LEN]) -> [u8; 20] {
    sha1(&[b"req1", secret])
}

/// `HASH('req2', SKEY)`. A responder computes this for every torrent it
/// serves to find which one an incoming handshake names.
pub fn req2_hash(skey: &[u8; 20]) -> [u8; 20] {
    sha1(&[b"req2", skey])
}

/// `HASH('req3', S)`, which masks req2 on the wire.
pub fn req3_hash(secret: &[u8; KEY_LEN]) -> [u8; 20] {
    sha1(&[b"req3", secret])
}

/// RC4 keys `(HASH('keyA', S, SKEY), HASH('keyB', S, SKEY))`.
pub fn rc4_keys(secret: &[u8; KEY_LEN], skey: &[u8; 20]) -> ([u8; 20], [u8; 20]) {
    (
        sha1(&[b"keyA", secret, skey]),
        sha1(&[b"keyB", secret, skey]),
    )
}

/// `(initiator to responder, responder to initiator)` ciphers, each with the
/// first 1024 keystream bytes already discarded.
pub fn session_ciphers(secret: &[u8; KEY_LEN], skey: &[u8; 20]) -> (Rc4, Rc4) {
    let (a, b) = rc4_keys(secret, skey);
    (Rc4::new_drop1024(&a), Rc4::new_drop1024(&b))
}

fn random_pad() -> Vec<u8> {
    let mut rng = rand::rng();
    let mut pad = vec![0u8; rng.random_range(0..=MAX_PAD)];
    rng.fill_bytes(&mut pad);
    pad
}

fn mse_error(reason: &str) -> PeerError {
    PeerError::Protocol(format!("mse: {reason}"))
}

/// A stream after the MSE handshake: RC4 in both directions, or pass-through.
///
/// Bytes the handshake read past its own end are replayed first, so the
/// BitTorrent handshake that follows sees an untouched stream. Writes are
/// encrypted into a small internal buffer; `flush` (which
/// [`super::PeerTransport`] calls after every frame) drains it.
pub struct MseStream<S> {
    inner: S,
    /// Undecrypted bytes read during the handshake, replayed before `inner`.
    replay: Vec<u8>,
    replay_pos: usize,
    read_cipher: Option<Rc4>,
    /// Bytes `read_cipher` still applies to; `None` means all of them. Set
    /// when plaintext was selected but an encrypted IA is still to come.
    read_limit: Option<usize>,
    write_cipher: Option<Rc4>,
    pending: Vec<u8>,
    pending_pos: usize,
    encrypted: bool,
}

impl<S> MseStream<S> {
    /// A pass-through stream: no handshake, no encryption.
    pub fn plaintext(inner: S) -> Self {
        Self::with_replay(inner, Vec::new())
    }

    fn with_replay(inner: S, replay: Vec<u8>) -> Self {
        Self {
            inner,
            replay,
            replay_pos: 0,
            read_cipher: None,
            read_limit: None,
            write_cipher: None,
            pending: Vec::new(),
            pending_pos: 0,
            encrypted: false,
        }
    }

    /// Whether the payload stream is RC4-encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    /// The underlying transport, for socket options such as `TCP_NODELAY`.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    fn decrypt(&mut self, data: &mut [u8]) {
        let Some(cipher) = self.read_cipher.as_mut() else {
            return;
        };
        let count = self.read_limit.map_or(data.len(), |limit| limit.min(data.len()));
        cipher.apply_keystream(&mut data[..count]);
        if let Some(limit) = self.read_limit.as_mut() {
            *limit -= count;
            if *limit == 0 {
                self.read_cipher = None;
                self.read_limit = None;
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> MseStream<S> {
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.pending_pos < self.pending.len() {
            let written =
                ready!(Pin::new(&mut self.inner).poll_write(cx, &self.pending[self.pending_pos..]))?;
            if written == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.pending_pos += written;
        }
        self.pending.clear();
        self.pending_pos = 0;
        Poll::Ready(Ok(()))
    }
}

impl<S> fmt::Debug for MseStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MseStream")
            .field("encrypted", &self.encrypted)
            .finish_non_exhaustive()
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for MseStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let start = buf.filled().len();
        if this.replay_pos < this.replay.len() {
            let count = buf.remaining().min(this.replay.len() - this.replay_pos);
            buf.put_slice(&this.replay[this.replay_pos..this.replay_pos + count]);
            this.replay_pos += count;
            if this.replay_pos == this.replay.len() {
                this.replay = Vec::new();
                this.replay_pos = 0;
            }
        } else {
            ready!(Pin::new(&mut this.inner).poll_read(cx, buf))?;
        }
        this.decrypt(&mut buf.filled_mut()[start..]);
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for MseStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.write_cipher.is_none() {
            ready!(this.poll_drain(cx))?;
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        ready!(this.poll_drain(cx))?;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let count = buf.len().min(WRITE_CHUNK);
        this.pending.extend_from_slice(&buf[..count]);
        if let Some(cipher) = this.write_cipher.as_mut() {
            cipher.apply_keystream(&mut this.pending);
        }
        // The bytes are accepted (the cipher has advanced over them). What
        // the socket does not take now goes out on the next write or flush.
        if let Poll::Ready(Err(error)) = this.poll_drain(cx) {
            return Poll::Ready(Err(error));
        }
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

/// Bounded handshake reader: everything read is kept, indexed absolutely.
struct HandshakeIo<S> {
    io: S,
    buf: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> HandshakeIo<S> {
    fn new(io: S) -> Self {
        Self {
            io,
            buf: Vec::with_capacity(KEY_LEN + MAX_PAD),
        }
    }

    /// Read until at least `len` bytes have arrived in total.
    async fn fill_to(&mut self, len: usize) -> Result<(), PeerError> {
        if len > MAX_HANDSHAKE_BYTES {
            return Err(mse_error("handshake too long"));
        }
        let mut chunk = [0u8; 512];
        while self.buf.len() < len {
            let want = (MAX_HANDSHAKE_BYTES - self.buf.len()).min(chunk.len());
            let read = self.io.read(&mut chunk[..want]).await?;
            if read == 0 {
                return Err(PeerError::ConnectionClosed);
            }
            self.buf.extend_from_slice(&chunk[..read]);
        }
        Ok(())
    }

    /// Absolute index of `pattern`, which must start at most `max_skip`
    /// bytes after `from`.
    async fn find(
        &mut self,
        from: usize,
        pattern: &[u8],
        max_skip: usize,
    ) -> Result<usize, PeerError> {
        let mut skip = 0;
        loop {
            while skip <= max_skip && from + skip + pattern.len() <= self.buf.len() {
                if &self.buf[from + skip..from + skip + pattern.len()] == pattern {
                    return Ok(from + skip);
                }
                skip += 1;
            }
            if skip > max_skip {
                return Err(mse_error("synchronisation marker not found"));
            }
            self.fill_to(self.buf.len() + 1).await?;
        }
    }

    /// Copy `len` bytes at `at`, reading them first if needed.
    async fn take(&mut self, at: usize, len: usize) -> Result<Vec<u8>, PeerError> {
        self.fill_to(at + len).await?;
        Ok(self.buf[at..at + len].to_vec())
    }

    async fn send(&mut self, data: &[u8]) -> Result<(), PeerError> {
        self.io.write_all(data).await?;
        self.io.flush().await?;
        Ok(())
    }

    fn finish(self, consumed: usize) -> MseStream<S> {
        let replay = self.buf[consumed..].to_vec();
        MseStream::with_replay(self.io, replay)
    }
}

fn key_array(bytes: &[u8]) -> [u8; KEY_LEN] {
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(&bytes[..KEY_LEN]);
    out
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn be_u16(bytes: &[u8]) -> usize {
    u16::from_be_bytes([bytes[0], bytes[1]]) as usize
}

/// Run the initiating side of MSE for the torrent `info_hash` (SKEY).
///
/// Returns once step 4 has been read: nothing of the BitTorrent handshake
/// has been sent and the returned stream is positioned at the start of the
/// responder's payload. With [`EncryptionPolicy::PlaintextOnly`] nothing is
/// exchanged and the stream is pass-through.
pub async fn initiate<S>(
    stream: S,
    info_hash: [u8; 20],
    policy: EncryptionPolicy,
) -> Result<MseStream<S>, PeerError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let provide = match policy {
        EncryptionPolicy::PlaintextOnly => return Ok(MseStream::plaintext(stream)),
        EncryptionPolicy::PreferRc4 => CRYPTO_RC4 | CRYPTO_PLAINTEXT,
        EncryptionPolicy::RequireRc4 => CRYPTO_RC4,
    };
    let keys = DhKeyPair::generate();
    let mut io = HandshakeIo::new(stream);

    // Step 1: Ya, PadA.
    let mut first = keys.public_key().to_vec();
    first.extend_from_slice(&random_pad());
    io.send(&first).await?;

    // Step 2: Yb (PadB is skipped by the VC search below).
    io.fill_to(KEY_LEN).await?;
    let secret = keys.shared_secret(&key_array(&io.buf))?;
    let (mut encrypt, mut decrypt) = session_ciphers(&secret, &info_hash);

    // Step 3: req1, req2 ^ req3, ENCRYPT(VC, provide, len(PadC)=0, len(IA)=0).
    let mut third = Vec::with_capacity(56);
    third.extend_from_slice(&req1_hash(&secret));
    let req2 = req2_hash(&info_hash);
    let req3 = req3_hash(&secret);
    third.extend(req2.iter().zip(req3.iter()).map(|(a, b)| a ^ b));
    let mut sealed = Vec::with_capacity(16);
    sealed.extend_from_slice(&VC);
    sealed.extend_from_slice(&provide.to_be_bytes());
    sealed.extend_from_slice(&0u16.to_be_bytes());
    sealed.extend_from_slice(&0u16.to_be_bytes());
    encrypt.apply_keystream(&mut sealed);
    third.extend_from_slice(&sealed);
    io.send(&third).await?;

    // Step 4: find ENCRYPT(VC) after Yb and at most MAX_PAD bytes of PadB.
    let mut marker = VC;
    decrypt.clone().apply_keystream(&mut marker);
    let at = io.find(KEY_LEN, &marker, MAX_PAD).await?;
    decrypt.discard(VC.len());
    let mut header = io.take(at + VC.len(), 6).await?;
    decrypt.apply_keystream(&mut header);
    let select = be_u32(&header[..4]);
    let pad_len = be_u16(&header[4..]);
    if (select != CRYPTO_RC4 && select != CRYPTO_PLAINTEXT) || select & provide == 0 {
        return Err(mse_error("peer selected an unoffered method"));
    }
    if pad_len > MAX_PAD {
        return Err(mse_error("PadD too long"));
    }
    let pad_at = at + VC.len() + 6;
    io.fill_to(pad_at + pad_len).await?;
    decrypt.discard(pad_len);

    let mut stream = io.finish(pad_at + pad_len);
    if select == CRYPTO_RC4 {
        stream.read_cipher = Some(decrypt);
        stream.write_cipher = Some(encrypt);
        stream.encrypted = true;
    }
    Ok(stream)
}

/// What [`accept`] found on an incoming connection.
pub enum Accepted<S> {
    /// A plaintext BitTorrent handshake. Nothing was consumed: the stream
    /// replays every byte read while detecting it.
    Plaintext(MseStream<S>),
    /// A completed MSE handshake for `info_hash`; the stream is positioned at
    /// the initiator's IA (normally its BitTorrent handshake).
    Encrypted {
        info_hash: [u8; 20],
        stream: MseStream<S>,
    },
}

// Info hashes stay out of Debug output.
impl<S> fmt::Debug for Accepted<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plaintext(_) => f.write_str("Accepted::Plaintext"),
            Self::Encrypted { stream, .. } => f
                .debug_struct("Accepted::Encrypted")
                .field("rc4", &stream.is_encrypted())
                .finish_non_exhaustive(),
        }
    }
}

/// Run the responding side on an incoming connection.
///
/// A plaintext BitTorrent handshake (`\x13BitTorrent protocol`) is detected
/// from at most its first 20 bytes and returned unconsumed. Anything else is
/// treated as MSE: `lookup` maps the received `HASH('req2', SKEY)` (see
/// [`req2_hash`]) to the info hash of a torrent being served, and `None`
/// refuses the connection.
pub async fn accept<S, F>(
    stream: S,
    policy: EncryptionPolicy,
    lookup: F,
) -> Result<Accepted<S>, PeerError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce(&[u8; 20]) -> Option<[u8; 20]>,
{
    let mut io = HandshakeIo::new(stream);
    loop {
        let have = io.buf.len().min(PLAINTEXT_PREFIX.len());
        if io.buf[..have] != PLAINTEXT_PREFIX[..have] {
            break;
        }
        if have == PLAINTEXT_PREFIX.len() {
            if policy == EncryptionPolicy::RequireRc4 {
                return Err(mse_error("plaintext peer refused"));
            }
            return Ok(Accepted::Plaintext(io.finish(0)));
        }
        io.fill_to(io.buf.len() + 1).await?;
    }
    if policy == EncryptionPolicy::PlaintextOnly {
        return Err(mse_error("encrypted peer refused"));
    }

    // Step 1: Ya. Step 2 goes out before PadA can be delimited.
    io.fill_to(KEY_LEN).await?;
    let keys = DhKeyPair::generate();
    let secret = keys.shared_secret(&key_array(&io.buf))?;
    let mut second = keys.public_key().to_vec();
    second.extend_from_slice(&random_pad());
    io.send(&second).await?;

    // Step 3: req1 after at most MAX_PAD bytes of PadA, then req2 ^ req3.
    let at = io.find(KEY_LEN, &req1_hash(&secret), MAX_PAD).await? + 20;
    let masked = io.take(at, 20).await?;
    let req3 = req3_hash(&secret);
    let mut req2 = [0u8; 20];
    for (out, (a, b)) in req2.iter_mut().zip(masked.iter().zip(req3.iter())) {
        *out = a ^ b;
    }
    let info_hash = lookup(&req2).ok_or_else(|| mse_error("unknown torrent"))?;
    let (mut decrypt, mut encrypt) = session_ciphers(&secret, &info_hash);

    let mut header = io.take(at + 20, 14).await?;
    decrypt.apply_keystream(&mut header);
    if header[..8] != VC {
        return Err(mse_error("bad verification constant"));
    }
    let provide = be_u32(&header[8..12]);
    let pad_len = be_u16(&header[12..14]);
    if pad_len > MAX_PAD {
        return Err(mse_error("PadC too long"));
    }
    let pad_at = at + 20 + 14;
    let mut tail = io.take(pad_at, pad_len + 2).await?;
    decrypt.apply_keystream(&mut tail);
    let ia_len = be_u16(&tail[pad_len..]);
    let select = if provide & CRYPTO_RC4 != 0 {
        CRYPTO_RC4
    } else if provide & CRYPTO_PLAINTEXT != 0 && policy == EncryptionPolicy::PreferRc4 {
        CRYPTO_PLAINTEXT
    } else {
        return Err(mse_error("no acceptable crypto method"));
    };

    // Step 4: ENCRYPT(VC, crypto_select, len(PadD)=0).
    let mut fourth = Vec::with_capacity(14);
    fourth.extend_from_slice(&VC);
    fourth.extend_from_slice(&select.to_be_bytes());
    fourth.extend_from_slice(&0u16.to_be_bytes());
    encrypt.apply_keystream(&mut fourth);
    io.send(&fourth).await?;

    let mut stream = io.finish(pad_at + pad_len + 2);
    if select == CRYPTO_RC4 {
        stream.read_cipher = Some(decrypt);
        stream.write_cipher = Some(encrypt);
        stream.encrypted = true;
    } else if ia_len > 0 {
        // IA is always RC4; the plaintext payload starts after it.
        stream.read_cipher = Some(decrypt);
        stream.read_limit = Some(ia_len);
    }
    Ok(Accepted::Encrypted { info_hash, stream })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn rc4_known_answers() {
        for (key, plain, cipher) in [
            (&b"Key"[..], &b"Plaintext"[..], "bbf316e8d940af0ad3"),
            (b"Wiki", b"pedia", "1021bf0420"),
            (b"Secret", b"Attack at dawn", "45a01f645fc35b383552544b9bf5"),
        ] {
            let mut data = plain.to_vec();
            Rc4::new(key).apply_keystream(&mut data);
            assert_eq!(hex(&data), cipher);
        }
        // RFC 6229, key 0x0102030405, keystream at offset 1024.
        let mut stream = [0u8; 16];
        Rc4::new_drop1024(&[1, 2, 3, 4, 5]).apply_keystream(&mut stream);
        assert_eq!(hex(&stream), "30abbcc7c20b01609f23ee2d5f6bb7df");
    }

    #[test]
    fn key_derivation_known_answers() {
        let a = DhKeyPair::from_private(&(1..=20).collect::<Vec<u8>>());
        let b = DhKeyPair::from_private(&(0x21..=0x34).collect::<Vec<u8>>());
        let secret = a.shared_secret(b.public_key()).unwrap();
        assert_eq!(secret, b.shared_secret(a.public_key()).unwrap());
        let skey: [u8; 20] = std::array::from_fn(|i| 100 + i as u8);
        assert_eq!(hex(&sha1(&[a.public_key()])), "9fa4c50e31ec4635ddb9ef30405e9db341313ed2");
        assert_eq!(hex(&sha1(&[&secret])), "104f11881994d7a9a273f2607b78977a7cad194e");
        assert_eq!(hex(&req1_hash(&secret)), "361e3668027ecec9d1086d5f839d191781e8de17");
        let (key_a, key_b) = rc4_keys(&secret, &skey);
        assert_eq!(hex(&key_a), "7d57856a5929940724066f15b6ae2a7a1dc5f219");
        assert_eq!(hex(&key_b), "f8a93c58524cdf5d9ee5c9b5c6ca36102519d80d");
    }

    #[tokio::test]
    async fn round_trip_selects_rc4_and_replays_payload() {
        let hash = [7u8; 20];
        let (a, b) = duplex(4096);
        let server = tokio::spawn(async move {
            let accepted = accept(b, EncryptionPolicy::PreferRc4, |req2| {
                (*req2 == req2_hash(&hash)).then_some(hash)
            })
            .await
            .unwrap();
            let Accepted::Encrypted { info_hash, mut stream } = accepted else {
                panic!("expected MSE");
            };
            assert_eq!(info_hash, hash);
            assert!(stream.is_encrypted());
            let mut got = [0u8; 5];
            stream.read_exact(&mut got).await.unwrap();
            stream.write_all(&got).await.unwrap();
            stream.flush().await.unwrap();
        });
        let mut stream = initiate(a, hash, EncryptionPolicy::RequireRc4).await.unwrap();
        stream.write_all(b"hello").await.unwrap();
        stream.flush().await.unwrap();
        let mut echo = [0u8; 5];
        stream.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"hello");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn plaintext_detection_consumes_nothing() {
        let (mut a, b) = duplex(4096);
        a.write_all(b"\x13BitTorrent protocol rest").await.unwrap();
        let accepted = accept(b, EncryptionPolicy::PreferRc4, |_| None).await.unwrap();
        let Accepted::Plaintext(mut stream) = accepted else {
            panic!("expected plaintext");
        };
        let mut got = [0u8; 25];
        stream.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"\x13BitTorrent protocol rest");
    }
}
