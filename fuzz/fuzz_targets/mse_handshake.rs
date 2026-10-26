//! The incoming Message Stream Encryption handshake, fed over an in-memory
//! duplex pipe. The first byte picks the responder policy and the mode:
//!
//! - raw (high bit clear): the rest of the input is everything the remote
//!   sends. This covers plaintext detection, Ya parsing and the bounded
//!   req1 search.
//! - structured (high bit set): a real initiator sends a valid Ya, a PadA of
//!   fuzzer-chosen length (past the 512-byte bound too), and the correct
//!   req1 and req2 ^ req3 for the responder's Yb; the remaining input is the
//!   plaintext of everything after that (VC, crypto_provide, len(PadC),
//!   PadC, len(IA), IA, payload), RC4-encrypted with keyA. The fuzzer then
//!   reaches every field check behind the Diffie-Hellman exchange.
//!
//! Invariants: no panic, the pipe is never read past what was sent, a
//! plaintext detection replays every byte, and an MSE accept names the only
//! info hash the lookup knows.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rbit::peer::mse::{self, Accepted, DhKeyPair, EncryptionPolicy, KEY_LEN};
use std::sync::OnceLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

const HASH: [u8; 20] = [0x5a; 20];

thread_local! {
    static RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
}

fn initiator_keys() -> &'static DhKeyPair {
    static KEYS: OnceLock<DhKeyPair> = OnceLock::new();
    KEYS.get_or_init(|| DhKeyPair::from_private(&[0x37; 20]))
}

fuzz_target!(|data: &[u8]| {
    RUNTIME.with(|runtime| runtime.block_on(run(data)));
});

async fn run(data: &[u8]) {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let policy = match mode % 3 {
        0 => EncryptionPolicy::PreferRc4,
        1 => EncryptionPolicy::RequireRc4,
        _ => EncryptionPolicy::PlaintextOnly,
    };
    let structured = mode & 0x80 != 0;
    let (mut client, server) = duplex(1 << 17);
    let responder = mse::accept(server, policy, |req2| {
        (mse::req2_hash(&HASH) == *req2).then_some(HASH)
    });
    let initiator = async {
        if structured {
            send_structured(&mut client, rest).await;
        } else {
            let _ = client.write_all(rest).await;
        }
        let _ = client.shutdown().await;
        client
    };
    let (accepted, _client) = tokio::join!(responder, initiator);
    let Ok(accepted) = accepted else {
        return;
    };
    let mut sink = Vec::new();
    match accepted {
        Accepted::Plaintext(mut stream) => {
            assert_ne!(policy, EncryptionPolicy::RequireRc4);
            let _ = stream.read_to_end(&mut sink).await;
            if !structured {
                assert_eq!(sink, rest, "plaintext detection lost bytes");
            }
        }
        Accepted::Encrypted {
            info_hash,
            mut stream,
        } => {
            assert_ne!(policy, EncryptionPolicy::PlaintextOnly);
            assert_eq!(info_hash, HASH);
            if policy == EncryptionPolicy::RequireRc4 {
                assert!(stream.is_encrypted());
            }
            let _ = stream.read_to_end(&mut sink).await;
        }
    }
}

async fn send_structured(client: &mut tokio::io::DuplexStream, rest: &[u8]) {
    let keys = initiator_keys();
    let (pad, tail) = match rest {
        [a, b, tail @ ..] => (usize::from(u16::from_be_bytes([*a, *b])) % 640, tail),
        _ => (0, rest),
    };
    let mut first = keys.public_key().to_vec();
    first.resize(KEY_LEN + pad, 0xa5);
    if client.write_all(&first).await.is_err() {
        return;
    }
    let mut yb = [0u8; KEY_LEN];
    if client.read_exact(&mut yb).await.is_err() {
        return;
    }
    let Ok(secret) = keys.shared_secret(&yb) else {
        return;
    };
    let mut third = mse::req1_hash(&secret).to_vec();
    let req3 = mse::req3_hash(&secret);
    third.extend(
        mse::req2_hash(&HASH)
            .iter()
            .zip(req3.iter())
            .map(|(x, y)| x ^ y),
    );
    let (mut encrypt, _) = mse::session_ciphers(&secret, &HASH);
    let mut sealed = tail.to_vec();
    encrypt.apply_keystream(&mut sealed);
    third.extend(sealed);
    let _ = client.write_all(&third).await;
}
