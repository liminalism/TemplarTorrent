//! Generation-tagged session events. Every event identifies its torrent, peer and
//! sequence generation, including events that arrive after cancellation or a
//! wanted-set change; the session drops stale ones instead of applying them.
use templartorrent_core::model::TorrentId;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct PeerId(pub u64);

#[derive(Debug, Clone)]
pub struct Event {
    pub torrent: TorrentId,
    pub peer: Option<PeerId>,
    pub generation: u64,
    pub body: EventBody,
}

#[derive(Debug, Clone)]
pub enum EventBody {
    Connected,
    Disconnected,
    Have {
        piece: u32,
    },
    Bitfield(Vec<u64>),
    Block {
        piece: u32,
        begin: u32,
        data: Vec<u8>,
    },
}
