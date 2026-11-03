use std::{net::SocketAddr, path::PathBuf, sync::Arc};
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct TorrentId(pub u64);
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Metadata,
    Checking,
    Downloading,
    /// Verified selected data, with no upload transport admitted. Keep this
    /// distinct from actively seeding when exposing a download-only driver.
    Complete,
    Seeding,
    Paused,
    Removing,
    Error,
}
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PeerPreference {
    Prefer,
    Normal,
    Avoid,
    Block,
}
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum InspectorTab {
    Details,
    Files,
    Peers,
    Trackers,
}
#[derive(Debug, Clone)]
pub struct Limits {
    pub download_jobs: usize,
    pub seed_jobs: usize,
    /// Total simultaneously unchoked interested peers, INCLUDING optimistic unchokes.
    pub upload_peer_slots: usize,
    pub connections_total: usize,
    pub connections_per_job: usize,
    pub download_kb_s: u64,
    pub upload_kb_s: u64,
    pub payload_budget_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            download_jobs: 3,
            seed_jobs: 3,
            upload_peer_slots: 8,
            connections_total: 128,
            connections_per_job: 48,
            download_kb_s: 0,
            upload_kb_s: 0,
            payload_budget_bytes: 48 * 1024 * 1024,
        }
    }
}
#[derive(Debug, Clone)]
pub struct TorrentRow {
    pub id: TorrentId,
    pub name: Arc<str>,
    pub state: JobState,
    pub wanted_bytes: u64,
    pub verified_bytes: u64,
    pub total_payload_downloaded: u64,
    pub total_payload_uploaded: u64,
    pub down_bps: u64,
    pub up_bps: u64,
    pub connected_seeds: u32,
    pub connected_peers: u32,
    /// Tracker-reported estimates; never add across overlapping trackers.
    pub estimated_seeds: Option<u32>,
    pub estimated_peers: Option<u32>,
}
impl TorrentRow {
    /// None means unknown, not infinite or zero. Production UI uses a smoothed rate.
    pub fn eta_seconds(&self) -> Option<u64> {
        if self.verified_bytes >= self.wanted_bytes {
            return Some(0);
        }
        if self.state != JobState::Downloading || self.down_bps == 0 {
            return None;
        }
        Some((self.wanted_bytes - self.verified_bytes).div_ceil(self.down_bps))
    }
}
#[derive(Debug, Clone)]
pub struct PeerRow {
    pub address: SocketAddr,
    pub country: Option<[u8; 2]>,
    pub client: Arc<str>,
    pub down_bps: u64,
    pub up_bps: u64,
    pub preference: PeerPreference,
    pub peer_choking: bool,
    pub peer_interested: bool,
}
#[derive(Debug, Clone)]
pub enum Command {
    Add {
        source: String,
        destination: PathBuf,
        public_magnet_bootstrap: bool,
    },
    /// One http:// or https:// file into `destination`, optionally checked
    /// against a SHA-256 before completion. One command per URL of a batch.
    AddUrl {
        url: String,
        destination: PathBuf,
        sha256: Option<[u8; 32]>,
    },
    Pause(TorrentId),
    Resume(TorrentId),
    /// Restart a stopped URL job from its partial file.
    Retry(TorrentId),
    /// Stop a URL job and keep its partial file.
    Cancel(TorrentId),
    Remove {
        id: TorrentId,
        delete_data: bool,
    },
    SetLimits(Limits),
    SetFileOrder {
        id: TorrentId,
        file_indices: Vec<usize>,
    },
    /// Back to the detected automatic order (DESIGN.md section 4).
    SetFileOrderAutomatic(TorrentId),
    SetFileWanted {
        id: TorrentId,
        file_index: usize,
        wanted: bool,
    },
    Reannounce {
        id: TorrentId,
        tracker_index: usize,
    },
    SetPeerPreference {
        id: TorrentId,
        address: SocketAddr,
        preference: PeerPreference,
    },
    Inspect {
        id: Option<TorrentId>,
        tab: InspectorTab,
        visible_range: std::ops::Range<usize>,
    },
    Shutdown,
}
/// Latest-value telemetry only. Command acknowledgements use a separate reliable channel.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub sequence: u64,
    pub jobs: Arc<[TorrentRow]>,
    pub visible_peers: Arc<[PeerRow]>,
    pub payload_budget_used: usize,
}
pub fn window_title(jobs: &[TorrentRow]) -> String {
    let mut active = jobs.iter().filter(|j| {
        matches!(
            j.state,
            JobState::Downloading | JobState::Metadata | JobState::Checking
        )
    });
    let Some(first) = active.next() else {
        return "TemplarTorrent".into();
    };
    let remaining = active.count();
    if remaining == 0 {
        format!("{} — TemplarTorrent", clean_label(&first.name, 160))
    } else {
        format!("{} jobs — TemplarTorrent", remaining + 1)
    }
}
/// Do not render remote control characters, bidi overrides, or unbounded client names.
pub fn clean_label(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(max_chars)
        .collect()
}
/// Queue arbitration in stable display order; paused/error/removing jobs never become runnable.
pub fn admitted_jobs(jobs: &[(TorrentId, JobState)], limits: &Limits) -> Vec<TorrentId> {
    let (mut down, mut seed) = (0, 0);
    jobs.iter()
        .filter_map(|&(id, state)| match state {
            JobState::Queued | JobState::Metadata | JobState::Checking | JobState::Downloading
                if down < limits.download_jobs =>
            {
                down += 1;
                Some(id)
            }
            JobState::Seeding if seed < limits.seed_jobs => {
                seed += 1;
                Some(id)
            }
            _ => None,
        })
        .collect()
}
