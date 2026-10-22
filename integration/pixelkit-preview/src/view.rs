//! Pure view-model formatting shared by the fixture preview and live mode.
//! No window, no I/O: the same functions render [`Snapshot`] rows in both
//! paths, so live mode cannot drift into fixture-looking fabrications.

use templartorrent_core::model::TorrentRow;

pub fn progress_percent(row: &TorrentRow) -> u64 {
    row.verified_bytes
        .saturating_mul(100)
        .checked_div(row.wanted_bytes)
        .unwrap_or(0)
}

/// Single-line row kept for the formatter tests. The window draws a table.
#[allow(dead_code)]
pub fn job_row_text(row: &TorrentRow) -> String {
    let state = match row.state {
        templartorrent_core::model::JobState::Complete => "Complete (seeding unavailable)".into(),
        other => format!("{other:?}"),
    };
    format!(
        "{}     {}%     {} kB/s     {} kB/s     {}",
        row.name,
        progress_percent(row),
        row.down_bps / 1000,
        row.up_bps / 1000,
        state
    )
}

pub fn eta_text(row: &TorrentRow) -> String {
    row.eta_seconds()
        .map(|value| format!("{value} s"))
        .unwrap_or_else(|| "Unknown".into())
}

/// Row-driven details lines; static captions stay with the caller.
pub fn details_lines(row: &TorrentRow) -> [String; 4] {
    [
        format!("{}", row.name),
        format!(
            "Downloaded: {} bytes     Uploaded: {} bytes",
            row.total_payload_downloaded, row.total_payload_uploaded
        ),
        format!(
            "Verified wanted data: {} / {} bytes",
            row.verified_bytes, row.wanted_bytes
        ),
        format!(
            "ETA: {}     Seeds: {}     Peers: {}",
            eta_text(row),
            row.connected_seeds,
            row.connected_peers
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use templartorrent_core::model::{JobState, TorrentId};

    fn row() -> TorrentRow {
        TorrentRow {
            id: TorrentId(1),
            name: Arc::from("Example"),
            state: JobState::Downloading,
            wanted_bytes: 100,
            verified_bytes: 50,
            total_payload_downloaded: 50,
            total_payload_uploaded: 5,
            down_bps: 10_000,
            up_bps: 1_000,
            connected_seeds: 2,
            connected_peers: 3,
            estimated_seeds: None,
            estimated_peers: None,
        }
    }

    #[test]
    fn job_row_shows_real_counters() {
        let text = job_row_text(&row());
        assert!(text.contains("Example"), "{text}");
        assert!(text.contains("50%"), "{text}");
        assert!(text.contains("10 kB/s"), "{text}");
        assert!(text.contains("Downloading"), "{text}");
    }

    #[test]
    fn zero_wanted_is_zero_not_panic() {
        let mut row = row();
        row.wanted_bytes = 0;
        assert_eq!(progress_percent(&row), 0);
        assert!(job_row_text(&row).contains("0%"));
    }

    #[test]
    fn unknown_eta_renders_unknown() {
        let mut row = row();
        row.down_bps = 0;
        assert_eq!(eta_text(&row), "Unknown");
        let lines = details_lines(&row);
        assert!(lines[3].contains("Unknown"), "{}", lines[3]);
        assert!(lines[1].contains("Downloaded: 50"), "{}", lines[1]);
    }

    #[test]
    fn completed_download_explains_why_it_is_not_seeding() {
        let mut row = row();
        row.state = JobState::Complete;
        row.verified_bytes = row.wanted_bytes;
        assert!(job_row_text(&row).contains("Complete (seeding unavailable)"));
    }
}
