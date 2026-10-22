//! Bounded BEP-10/BEP-9 resolver reused by the driver and localhost fixtures.
//! No discovery is started here. The caller supplies an already permitted
//! connection and a session-wide metadata budget. Dropping the future requires
//! dropping the connection, because a send may have partially framed a message.
use crate::preflight;
use rbit::{
    ExtensionHandshake, METADATA_PIECE_SIZE, Message, MetadataMessage, MetadataMessageType,
    PeerConnection, metadata_piece_count, metadata_piece_size,
};
use sha1::{Digest, Sha1};
use std::{fmt, time::Duration};
use templartorrent_core::budget::{ByteBudget, Permit};
use tokio::time::timeout;

pub const MAX_METADATA_SIZE: usize = 16 * 1024 * 1024;
pub const METADATA_BUDGET: usize = 32 * 1024 * 1024;
pub const OUR_MD_ID: u8 = 3;
const MAX_BUFFERED_MESSAGES: usize = 64;
const MAX_BUFFERED_BYTES: usize = 256 * 1024;
const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;

pub struct FetchedMetadata {
    pub raw_info: Vec<u8>,
    pub buffered: Vec<Message>,
    pub piece_count: usize,
    pub(crate) permits: Vec<Permit>,
}
impl fmt::Debug for FetchedMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FetchedMetadata")
            .field("bytes", &self.raw_info.len())
            .field("buffered_messages", &self.buffered.len())
            .finish_non_exhaustive()
    }
}

fn buffer(message: Message, buffered: &mut Vec<Message>, bytes: &mut usize) -> Result<(), String> {
    let size = match &message {
        Message::Bitfield(bits) => bits.len(),
        Message::Choke
        | Message::Unchoke
        | Message::Have { .. }
        | Message::HaveAll
        | Message::HaveNone
        | Message::KeepAlive => 16,
        _ => return Err("unexpected message during metadata acquisition".into()),
    };
    if buffered.len() >= MAX_BUFFERED_MESSAGES || size > MAX_BUFFERED_BYTES.saturating_sub(*bytes) {
        return Err("metadata control-message buffer limit exceeded".into());
    }
    *bytes += size;
    buffered.push(message);
    Ok(())
}

async fn receive(conn: &mut PeerConnection) -> Result<Message, String> {
    timeout(Duration::from_secs(5), conn.receive())
        .await
        .map_err(|_| "metadata receive stalled")?
        .map_err(|_| "metadata connection failed".to_string())
}
async fn send(conn: &mut PeerConnection, message: Message) -> Result<(), String> {
    timeout(Duration::from_secs(5), conn.send(message))
        .await
        .map_err(|_| "metadata send stalled")?
        .map_err(|_| "metadata send failed".to_string())
}

/// Convenience resolver for standalone callers. A multi-job driver must use
/// fetch_metadata_budgeted so concurrently retained metadata shares one cap.
pub async fn fetch_metadata(
    conn: &mut PeerConnection,
    hash: [u8; 20],
) -> Result<FetchedMetadata, String> {
    fetch_metadata_budgeted(conn, hash, &ByteBudget::new(METADATA_BUDGET)).await
}

pub async fn fetch_metadata_budgeted(
    conn: &mut PeerConnection,
    hash: [u8; 20],
    budget: &ByteBudget,
) -> Result<FetchedMetadata, String> {
    timeout(Duration::from_secs(30), fetch(conn, hash, budget))
        .await
        .map_err(|_| "metadata acquisition deadline exceeded".to_string())?
}

async fn fetch(
    conn: &mut PeerConnection,
    hash: [u8; 20],
    budget: &ByteBudget,
) -> Result<FetchedMetadata, String> {
    let buffer_permit = budget
        .try_acquire(MAX_BUFFERED_BYTES)
        .ok_or("metadata budget exhausted")?;
    let ours = ExtensionHandshake::with_extensions(&[("ut_metadata", OUR_MD_ID)]);
    send(
        conn,
        Message::Extended {
            id: 0,
            payload: ours
                .encode()
                .map_err(|_| "metadata handshake encoding failed")?,
        },
    )
    .await?;
    let mut buffered = Vec::new();
    let mut buffered_bytes = 0;
    let remote = loop {
        match receive(conn).await? {
            Message::Extended { id: 0, payload } => {
                if payload.len() > MAX_HANDSHAKE_BYTES {
                    return Err("extension handshake exceeds limit".into());
                }
                preflight::validate(&payload)?;
                if let Some(mapping) = preflight::field(&payload, b"m")
                    && let Some(value) = preflight::field(mapping, b"ut_metadata")
                    && !preflight::integer(value).is_some_and(|id| (0..=255).contains(&id))
                {
                    return Err("invalid metadata extension id".into());
                }
                break ExtensionHandshake::decode(&payload)
                    .map_err(|_| "invalid extension handshake")?;
            }
            other => buffer(other, &mut buffered, &mut buffered_bytes)?,
        }
    };
    let remote_id = remote
        .get_extension_id("ut_metadata")
        .filter(|id| *id != 0)
        .ok_or("peer hides ut_metadata")?;
    let size = remote
        .metadata_size
        .filter(|size| *size > 0 && *size <= MAX_METADATA_SIZE as i64)
        .ok_or("invalid or oversized metadata_size")? as usize;
    // Reserve the raw allocation and metadata message/assembly copies before
    // requesting any data. Parsing obtains additional permits based on items.
    let raw_permit = budget
        .try_acquire(size.checked_mul(2).ok_or("metadata size overflow")?)
        .ok_or("metadata budget exhausted")?;
    let mut raw_info = vec![0; size];
    let count = metadata_piece_count(size);
    for piece in (0..count as u32).rev() {
        send(
            conn,
            Message::Extended {
                id: remote_id,
                payload: MetadataMessage::request(piece)
                    .encode()
                    .map_err(|_| "metadata request encoding failed")?,
            },
        )
        .await?;
        loop {
            match receive(conn).await? {
                Message::Extended { id, payload } if id == OUR_MD_ID => {
                    if payload.len() > METADATA_PIECE_SIZE + 1024 {
                        return Err("metadata message exceeds block limit".into());
                    }
                    let (end, _) = preflight::prefix(&payload)?;
                    if end > 1024 {
                        return Err("metadata header exceeds limit".into());
                    }
                    // Check signed values before rbit's decoder casts them to
                    // u8/u32, which would otherwise accept wrapped integers.
                    for (key, expected) in [
                        (b"msg_type".as_slice(), 1),
                        (b"piece".as_slice(), piece as i64),
                        (b"total_size".as_slice(), size as i64),
                    ] {
                        if preflight::field(&payload[..end], key).and_then(preflight::integer)
                            != Some(expected)
                        {
                            return Err(match key {
                                b"total_size" => "metadata total_size mismatch",
                                b"piece" => "metadata piece mismatch",
                                _ => "metadata piece not served",
                            }
                            .into());
                        }
                    }
                    let data = MetadataMessage::decode(&payload)
                        .map_err(|_| "invalid metadata message")?;
                    if data.msg_type != MetadataMessageType::Data {
                        return Err("metadata piece not served".into());
                    }
                    let bytes = data.data.ok_or("metadata data missing")?;
                    let expected = metadata_piece_size(piece, size);
                    if bytes.len() != expected {
                        return Err("metadata piece length mismatch".into());
                    }
                    let start = piece as usize * METADATA_PIECE_SIZE;
                    raw_info[start..start + expected].copy_from_slice(&bytes);
                    break;
                }
                Message::Extended { .. } => {
                    return Err("metadata on unexpected extension id".into());
                }
                other => buffer(other, &mut buffered, &mut buffered_bytes)?,
            }
        }
    }
    if <[u8; 20]>::from(Sha1::digest(&raw_info)) != hash {
        return Err("metadata infohash mismatch: refusing to parse".into());
    }
    // A matching hash is necessary but does not make deeply nested bencode safe.
    preflight::validate(&raw_info)?;
    Ok(FetchedMetadata {
        raw_info,
        buffered,
        piece_count: count,
        permits: vec![raw_permit, buffer_permit],
    })
}
