//! Message Stream Encryption in the rbit fork, over in-memory pipes only.
//! Known answers come from an independent implementation: Python's integer
//! `pow` and hashlib for the key schedule and pyca/cryptography's ARC4 for
//! the keystream (RC4 vectors are also RFC 6229 and the published "Key",
//! "Wiki" and "Secret" examples).
use rbit::peer::mse::{
    self, Accepted, CRYPTO_PLAINTEXT, CRYPTO_RC4, DhKeyPair, EncryptionPolicy, KEY_LEN, MAX_PAD,
    MseStream, Rc4,
};
use sha1::{Digest, Sha1};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex},
    time::timeout,
};

const HASH: [u8; 20] = [0x5a; 20];
const BOUND: Duration = Duration::from_secs(5);

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn lookup(req2: &[u8; 20]) -> Option<[u8; 20]> {
    [[0x11; 20], HASH]
        .into_iter()
        .find(|hash| mse::req2_hash(hash) == *req2)
}

/// Deterministic filler so failures reproduce.
fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

#[test]
fn rc4_matches_published_vectors_and_rfc6229_drop1024() {
    for (key, plain, cipher) in [
        (&b"Key"[..], &b"Plaintext"[..], "bbf316e8d940af0ad3"),
        (b"Wiki", b"pedia", "1021bf0420"),
        (b"Secret", b"Attack at dawn", "45a01f645fc35b383552544b9bf5"),
    ] {
        let mut data = plain.to_vec();
        Rc4::new(key).apply_keystream(&mut data);
        assert_eq!(hex(&data), cipher);
        Rc4::new(key).apply_keystream(&mut data);
        assert_eq!(data, plain);
    }
    let mut stream = [0u8; 16];
    Rc4::new_drop1024(&[1, 2, 3, 4, 5]).apply_keystream(&mut stream);
    assert_eq!(hex(&stream), "30abbcc7c20b01609f23ee2d5f6bb7df");
    // Byte-at-a-time and bulk application produce the same keystream.
    let mut bulk = [0u8; 64];
    let mut split = [0u8; 64];
    Rc4::new_drop1024(b"k").apply_keystream(&mut bulk);
    let mut rc4 = Rc4::new_drop1024(b"k");
    for byte in split.chunks_mut(7) {
        rc4.apply_keystream(byte);
    }
    assert_eq!(bulk, split);
}

#[test]
fn key_schedule_matches_independent_reference() {
    let a = DhKeyPair::from_private(&(1..=20).collect::<Vec<u8>>());
    let b = DhKeyPair::from_private(&(0x21..=0x34).collect::<Vec<u8>>());
    let skey: [u8; 20] = std::array::from_fn(|i| 100 + i as u8);
    let sha = |bytes: &[u8]| hex(&Sha1::digest(bytes));
    assert_eq!(
        sha(a.public_key()),
        "9fa4c50e31ec4635ddb9ef30405e9db341313ed2"
    );
    assert_eq!(
        sha(b.public_key()),
        "4108adb946639a11fd2020a4c1276461ca766ef9"
    );
    let secret = a.shared_secret(b.public_key()).unwrap();
    assert_eq!(secret, b.shared_secret(a.public_key()).unwrap());
    assert_eq!(sha(&secret), "104f11881994d7a9a273f2607b78977a7cad194e");
    assert_eq!(
        hex(&mse::req1_hash(&secret)),
        "361e3668027ecec9d1086d5f839d191781e8de17"
    );
    assert_eq!(
        hex(&mse::req2_hash(&skey)),
        "fe8811640d96d350501a13baa0553941fef6faf0"
    );
    assert_eq!(
        hex(&mse::req3_hash(&secret)),
        "3636fc5ac2a9bdd097857f8b02403004817d467d"
    );
    let (key_a, key_b) = mse::rc4_keys(&secret, &skey);
    assert_eq!(hex(&key_a), "7d57856a5929940724066f15b6ae2a7a1dc5f219");
    assert_eq!(hex(&key_b), "f8a93c58524cdf5d9ee5c9b5c6ca36102519d80d");
    let (mut to_b, mut to_a) = mse::session_ciphers(&secret, &skey);
    let mut stream = [0u8; 16];
    to_b.apply_keystream(&mut stream);
    assert_eq!(hex(&stream), "d2a313381da150769e46ed4e6a1871f1");
    let mut stream = [0u8; 16];
    to_a.apply_keystream(&mut stream);
    assert_eq!(hex(&stream), "78120533e9df4b335a451833033ff5ba");
}

#[test]
fn degenerate_public_values_are_refused() {
    let keys = DhKeyPair::generate();
    let mut p_minus_one = mse::DH_PRIME;
    p_minus_one[KEY_LEN - 1] -= 1;
    let mut one = [0u8; KEY_LEN];
    one[KEY_LEN - 1] = 1;
    for remote in [
        [0u8; KEY_LEN],
        one,
        p_minus_one,
        mse::DH_PRIME,
        [0xff; KEY_LEN],
    ] {
        assert!(keys.shared_secret(&remote).is_err());
    }
}

async fn pair(
    initiator: EncryptionPolicy,
    responder: EncryptionPolicy,
) -> (
    Result<MseStream<DuplexStream>, rbit::PeerError>,
    Result<Accepted<DuplexStream>, rbit::PeerError>,
) {
    let (a, b) = duplex(8192);
    let server = tokio::spawn(async move {
        timeout(BOUND, mse::accept(b, responder, lookup))
            .await
            .expect("responder bounded")
    });
    let client = timeout(BOUND, async move {
        let mut stream = mse::initiate(a, HASH, initiator).await?;
        if initiator == EncryptionPolicy::PlaintextOnly {
            // What a plaintext client sends first.
            stream.write_all(b"\x13BitTorrent protocol").await?;
            stream.flush().await?;
        }
        Ok(stream)
    })
    .await
    .expect("initiator bounded");
    (client, server.await.unwrap())
}

async fn echo(client: &mut MseStream<DuplexStream>, server: &mut MseStream<DuplexStream>) {
    client.write_all(b"ping").await.unwrap();
    client.flush().await.unwrap();
    let mut got = [0u8; 4];
    server.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"ping");
    server.write_all(b"pong").await.unwrap();
    server.flush().await.unwrap();
    client.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"pong");
}

#[tokio::test]
async fn policy_matrix_negotiates_or_refuses() {
    use EncryptionPolicy::*;
    for initiator in [PreferRc4, RequireRc4] {
        for responder in [PreferRc4, RequireRc4] {
            let (client, server) = pair(initiator, responder).await;
            let mut client = client.unwrap();
            let Accepted::Encrypted {
                info_hash,
                stream: mut server,
            } = server.unwrap()
            else {
                panic!("expected MSE");
            };
            assert_eq!(info_hash, HASH);
            assert!(client.is_encrypted() && server.is_encrypted());
            echo(&mut client, &mut server).await;
        }
        // An MSE initiator against a plaintext-only responder fails on both
        // sides before any BitTorrent byte, which is the fallback trigger.
        let (client, server) = pair(initiator, PlaintextOnly).await;
        assert!(server.is_err());
        assert!(client.is_err());
    }
    for responder in [PreferRc4, PlaintextOnly] {
        let (client, server) = pair(PlaintextOnly, responder).await;
        assert!(!client.unwrap().is_encrypted());
        let Accepted::Plaintext(mut stream) = server.unwrap() else {
            panic!("expected plaintext");
        };
        let mut got = [0u8; 20];
        stream.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"\x13BitTorrent protocol");
    }
    let (_, server) = pair(PlaintextOnly, RequireRc4).await;
    assert!(server.is_err());
}

#[tokio::test]
async fn unknown_info_hash_is_refused() {
    let (a, b) = duplex(8192);
    let server = tokio::spawn(mse::accept(b, EncryptionPolicy::PreferRc4, |_| None));
    let client = timeout(BOUND, mse::initiate(a, HASH, EncryptionPolicy::PreferRc4))
        .await
        .unwrap();
    assert!(server.await.unwrap().is_err());
    assert!(client.is_err());
}

/// Hand-built initiator: maximum PadA and PadC, plaintext-only provide, a
/// non-empty IA, and payload behind it. The responder must resynchronise at
/// the 628-byte bound, select plaintext, decrypt exactly the IA and pass the
/// rest through untouched.
async fn manual_initiator(pad_a: usize, provide: u32) -> Result<Vec<u8>, rbit::PeerError> {
    let (mut a, b) = duplex(16384);
    let server = tokio::spawn(async move {
        let accepted = mse::accept(b, EncryptionPolicy::PreferRc4, lookup).await?;
        let Accepted::Encrypted { mut stream, .. } = accepted else {
            panic!("expected MSE");
        };
        assert!(!stream.is_encrypted());
        let mut got = vec![0u8; 10];
        stream.read_exact(&mut got).await?;
        Ok::<_, rbit::PeerError>(got)
    });
    let keys = DhKeyPair::generate();
    let mut first = keys.public_key().to_vec();
    first.extend(noise(pad_a, 1));
    a.write_all(&first).await.unwrap();
    let mut yb = [0u8; KEY_LEN];
    a.read_exact(&mut yb).await.unwrap();
    let secret = keys.shared_secret(&yb).unwrap();
    let (mut encrypt, mut decrypt) = mse::session_ciphers(&secret, &HASH);
    let mut third = mse::req1_hash(&secret).to_vec();
    let req3 = mse::req3_hash(&secret);
    third.extend(
        mse::req2_hash(&HASH)
            .iter()
            .zip(req3.iter())
            .map(|(x, y)| x ^ y),
    );
    let mut sealed = vec![0u8; 8];
    sealed.extend_from_slice(&provide.to_be_bytes());
    sealed.extend_from_slice(&(MAX_PAD as u16).to_be_bytes());
    sealed.extend(vec![0u8; MAX_PAD]);
    sealed.extend_from_slice(&5u16.to_be_bytes());
    sealed.extend_from_slice(b"hello");
    encrypt.apply_keystream(&mut sealed);
    third.extend(sealed);
    third.extend_from_slice(b"world");
    if a.write_all(&third).await.is_err() {
        return server.await.unwrap();
    }
    let result = server.await.unwrap();
    if result.is_ok() {
        // Step 4 follows PadB; find ENCRYPT(VC) and check crypto_select.
        let mut rest = vec![0u8; 4096];
        let n = timeout(BOUND, a.read(&mut rest)).await.unwrap().unwrap();
        let mut marker = [0u8; 8];
        decrypt.clone().apply_keystream(&mut marker);
        let at = rest[..n]
            .windows(8)
            .position(|w| w == marker)
            .expect("VC within PadB");
        decrypt.discard(8);
        let mut select = rest[at + 8..at + 12].to_vec();
        decrypt.apply_keystream(&mut select);
        assert_eq!(select, CRYPTO_PLAINTEXT.to_be_bytes());
    }
    result
}

#[tokio::test]
async fn responder_resyncs_at_the_bound_and_streams_ia_then_plaintext() {
    let got = timeout(BOUND, manual_initiator(MAX_PAD, CRYPTO_PLAINTEXT))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&got, b"helloworld");
    // One byte of PadA past the spec's bound is not searched for.
    assert!(
        timeout(BOUND, manual_initiator(MAX_PAD + 1, CRYPTO_PLAINTEXT))
            .await
            .unwrap()
            .is_err()
    );
    // A provide field with no known method is refused.
    assert!(
        timeout(BOUND, manual_initiator(0, 0x10))
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn garbage_and_truncated_input_errors_without_panicking() {
    for (len, seed) in [
        (0, 0),
        (1, 1),
        (19, 2),
        (95, 3),
        (96, 4),
        (700, 5),
        (5000, 6),
    ] {
        let (mut a, b) = duplex(16384);
        let server = tokio::spawn(mse::accept(b, EncryptionPolicy::PreferRc4, lookup));
        a.write_all(&noise(len, seed)).await.unwrap();
        a.shutdown().await.unwrap();
        let result = timeout(BOUND, server).await.expect("bounded").unwrap();
        assert!(result.is_err(), "{len} bytes of noise were accepted");
    }
    // A responder that never answers: the initiator waits, so the caller's
    // timeout is the bound.
    let (a, _b) = duplex(16384);
    assert!(
        timeout(
            Duration::from_millis(100),
            mse::initiate(a, HASH, EncryptionPolicy::PreferRc4)
        )
        .await
        .is_err()
    );
    // A responder that answers with noise: no VC within PadB.
    let (a, mut b) = duplex(16384);
    b.write_all(&noise(KEY_LEN + 700, 9)).await.unwrap();
    let result = timeout(BOUND, mse::initiate(a, HASH, EncryptionPolicy::PreferRc4))
        .await
        .unwrap();
    assert!(result.is_err());
    // A valid initiator cut off mid-step-3 at every length.
    for cut in [0, 1, 20, 39, 40, 47, 55] {
        let (mut a, b) = duplex(16384);
        let server = tokio::spawn(mse::accept(b, EncryptionPolicy::PreferRc4, lookup));
        let keys = DhKeyPair::generate();
        a.write_all(keys.public_key()).await.unwrap();
        let mut yb = [0u8; KEY_LEN];
        a.read_exact(&mut yb).await.unwrap();
        let secret = keys.shared_secret(&yb).unwrap();
        let mut third = mse::req1_hash(&secret).to_vec();
        let req3 = mse::req3_hash(&secret);
        third.extend(
            mse::req2_hash(&HASH)
                .iter()
                .zip(req3.iter())
                .map(|(x, y)| x ^ y),
        );
        let (mut encrypt, _) = mse::session_ciphers(&secret, &HASH);
        let mut sealed = [0u8; 16];
        sealed[11] = CRYPTO_RC4 as u8;
        encrypt.apply_keystream(&mut sealed);
        third.extend_from_slice(&sealed);
        a.write_all(&third[..cut]).await.unwrap();
        a.shutdown().await.unwrap();
        let result = timeout(BOUND, server).await.expect("bounded").unwrap();
        assert!(result.is_err(), "cut at {cut} was accepted");
    }
}

#[tokio::test]
async fn rc4_stream_survives_short_writes_and_short_reads() {
    let (a, b) = duplex(8192);
    let server = tokio::spawn(mse::accept(b, EncryptionPolicy::RequireRc4, lookup));
    let client = mse::initiate(a, HASH, EncryptionPolicy::RequireRc4)
        .await
        .unwrap();
    let Accepted::Encrypted { stream: server, .. } = server.await.unwrap().unwrap() else {
        panic!("expected MSE");
    };
    // 16 KiB frames into an 8 KiB pipe force short writes (the pending
    // buffer); 61-byte reads force short reads.
    let payload = noise(1 << 20, 77);
    let expected = payload.clone();
    let (mut client, mut server) = (client, server);
    let writer = tokio::spawn(async move {
        for chunk in payload.chunks(16397) {
            client.write_all(chunk).await.unwrap();
            client.flush().await.unwrap();
        }
        client
    });
    let mut got = vec![0u8; expected.len()];
    let mut filled = 0;
    while filled < got.len() {
        let end = (filled + 61).min(got.len());
        let n = server.read(&mut got[filled..end]).await.unwrap();
        assert!(n > 0);
        filled += n;
    }
    assert_eq!(got, expected);
    let _client = writer.await.unwrap();
}
