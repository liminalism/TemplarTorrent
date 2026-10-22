//! Rbit 0.2.2 protocol boundary and reusable bounded v1 transfer driver.
//! Networking requires explicit calls and permitted sources; this is not yet a
//! release-ready full session client. See driver module for remaining gates.
#![forbid(unsafe_code)]
pub mod announce;
pub mod discovery;
pub mod driver;
pub mod metadata;
mod preflight;
pub mod torrent;
use rbit::{
    AnnounceResponse, Block, BlockRequest, ExtensionHandshake, MagnetLink, Message, Metainfo,
    PeerConnection, PeerError,
};
use templartorrent_core::{
    model::clean_label,
    sequence::{Sequence, SequenceError},
    tracker::TrackerClock,
};
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub enum Source {
    Magnet(MagnetLink),
    Torrent(Metainfo),
}
/// Bounded magnet syntax only; parsing never authorizes discovery or endpoints.
pub fn parse_magnet(uri: &str) -> Result<Source, Error> {
    if uri.len() > 16 * 1024 {
        return Err("magnet exceeds 16 KiB input limit".into());
    }
    Ok(Source::Magnet(MagnetLink::parse(uri)?))
}
/// Bounded v1 parser. Validates raw path components and layout before rbit;
/// currently gates v2/hybrid, file attributes and more than 128 open files.
pub fn parse_torrent(bytes: &[u8]) -> Result<Source, Error> {
    if bytes.len() > 16 * 1024 * 1024 {
        return Err("metainfo exceeds 16 MiB input limit".into());
    }
    preflight::validate(bytes)?;
    let info = preflight::field(bytes, b"info").ok_or("missing info dictionary")?;
    torrent::layout(info)?;
    Ok(Source::Torrent(Metainfo::from_bytes(bytes)?))
}
/// rbit's u8 priorities only filter zero in the reviewed implementation. Rank externally.
pub fn pick_piece(
    sequence: &Sequence,
    peer: &[u64],
    busy: &[u64],
    availability: &[u32],
) -> Result<Option<u32>, SequenceError> {
    sequence.pick(peer, busy, availability)
}
pub fn request_message(request: BlockRequest) -> Message {
    Message::Request {
        index: request.piece_index,
        begin: request.offset,
        length: request.length,
    }
}
pub fn cancel_message(request: BlockRequest) -> Message {
    Message::Cancel {
        index: request.piece_index,
        begin: request.offset,
        length: request.length,
    }
}
/// Validate against an outstanding lease BEFORE passing to PieceManager::receive_block.
/// The caller additionally checks torrent identity, peer ownership, lease generation,
/// reserved memory, pending status and payload admission/rate policy.
pub fn matches_request(block: &Block, request: &BlockRequest, actual_piece_length: u64) -> bool {
    request.length > 0
        && request.length <= 16 * 1024
        && block.piece_index == request.piece_index
        && block.offset == request.offset
        && block.data.len() == request.length as usize
        && u64::from(request.offset)
            .checked_add(u64::from(request.length))
            .is_some_and(|end| end <= actual_piece_length)
}
/// Convert BEP-3 MSB-first wire bytes to the core's LSB-first u64 words.
/// Reject nonzero unused wire bits; validate piece_count against a memory cap first.
pub fn unpack_bitfield(wire: &[u8], piece_count: usize, out: &mut [u64]) -> Result<(), Error> {
    if wire.len() != piece_count.div_ceil(8) || out.len() != piece_count.div_ceil(64) {
        return Err("invalid bitfield dimensions".into());
    }
    if !piece_count.is_multiple_of(8)
        && wire
            .last()
            .is_some_and(|last| last & ((1u8 << (8 - piece_count % 8)) - 1) != 0)
    {
        return Err("nonzero bitfield padding".into());
    }
    out.fill(0);
    for piece in 0..piece_count {
        if wire[piece / 8] & (0x80 >> (piece % 8)) != 0 {
            out[piece / 64] |= 1u64 << (piece % 64);
        }
    }
    Ok(())
}
/// rbit setters change local flags. The corresponding wire message is still necessary.
/// On cancellation or send error, discard this connection; do not reuse partial framing.
pub async fn set_interested(
    connection: &mut PeerConnection,
    interested: bool,
) -> Result<(), PeerError> {
    connection
        .send(if interested {
            Message::Interested
        } else {
            Message::NotInterested
        })
        .await?;
    connection.set_interested(interested);
    Ok(())
}
pub async fn set_choking(connection: &mut PeerConnection, choking: bool) -> Result<(), PeerError> {
    connection
        .send(if choking {
            Message::Choke
        } else {
            Message::Unchoke
        })
        .await?;
    connection.set_choking(choking);
    Ok(())
}
pub fn client_name(handshake: Option<&ExtensionHandshake>) -> String {
    handshake
        .and_then(|h| h.client.as_deref())
        .map(|s| clean_label(s, 80))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown".into())
}
pub fn record_tracker_success(
    clock: &mut TrackerClock,
    now_monotonic_seconds: u64,
    response: &AnnounceResponse,
) {
    clock.success(
        now_monotonic_seconds,
        response.interval,
        response.min_interval,
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_bit_order_is_not_host_bit_order() {
        let mut words = [0];
        unpack_bitfield(&[0b10000001, 0b10000000], 9, &mut words).unwrap();
        assert_eq!(words, [1 | (1 << 7) | (1 << 8)]);
        assert!(unpack_bitfield(&[0b10000001, 0b10000001], 9, &mut words).is_err());
    }
    #[test]
    fn request_variant_uses_actual_rbit_field_names() {
        assert!(matches!(
            request_message(BlockRequest::new(3, 16_384, 16_384)),
            Message::Request {
                index: 3,
                begin: 16_384,
                length: 16_384
            }
        ));
    }
}
