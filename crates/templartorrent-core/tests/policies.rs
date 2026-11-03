use std::{cmp::Ordering, sync::Arc};
use templartorrent_core::{
    bandwidth::TokenBucket,
    budget::ByteBudget,
    model::{JobState, Limits, TorrentId, TorrentRow, admitted_jobs, clean_label, window_title},
    paths::{PathError, RelativeFile, RemovalIntent, validate_component},
    sequence::{FileSpec, SKIP, Sequence, SequenceError, natural_cmp},
    tracker::{Discovery, Privacy, TrackerClock, discovery_allowed},
};
fn file(name: &str, length: u64) -> FileSpec {
    FileSpec {
        path: name.into(),
        length,
        wanted: true,
    }
}
#[test]
fn natural_episode_numbers() {
    let mut names = vec!["Episode 10.mkv", "Episode 2.mkv", "Episode 1.mkv"];
    names.sort_by(|a, b| natural_cmp(a, b));
    assert_eq!(names, ["Episode 1.mkv", "Episode 2.mkv", "Episode 10.mkv"]);
}
#[test]
fn natural_huge_numbers_never_parse_an_integer() {
    assert_eq!(
        natural_cmp(
            "track999999999999999999999999",
            "track1000000000000000000000000"
        ),
        Ordering::Less
    );
}
#[test]
fn natural_ties_are_deterministic() {
    assert_ne!(natural_cmp("01", "1"), Ordering::Equal);
    assert_eq!(natural_cmp("same", "same"), Ordering::Equal);
    assert_eq!(natural_cmp("Episode2", "episode10"), Ordering::Less);
}
#[test]
fn natural_order_is_not_wire_order() {
    let seq = Sequence::new(vec![file("10", 4), file("2", 4), file("1", 4)], 4, 100).unwrap();
    assert_eq!(seq.order(), [2, 1, 0]);
    assert_eq!(seq.files()[0].path, "10");
    assert_eq!(seq.piece_ranks(), [2, 1, 0]);
    assert_eq!(seq.pick(&[0b111], &[0], &[1, 1, 1]).unwrap(), Some(2));
}
#[test]
fn completion_promotes_next_file() {
    let mut seq = Sequence::new(vec![file("2", 4), file("1", 4)], 4, 100).unwrap();
    seq.mark_verified(1).unwrap();
    assert_eq!(seq.file_ranks(), [0, SKIP]);
    assert_eq!(seq.pick(&[3], &[0], &[1, 1]).unwrap(), Some(0));
}
#[test]
fn shared_pieces_count_exact_overlap() {
    let mut seq = Sequence::new(vec![file("1", 6), file("2", 6), file("3", 4)], 4, 100).unwrap();
    seq.mark_verified(0).unwrap();
    seq.mark_verified(1).unwrap();
    assert_eq!(seq.file_verified_bytes(), [6, 2, 0]);
    assert_eq!(seq.file_ranks(), [SKIP, 0, 1]);
    seq.mark_verified(2).unwrap();
    assert_eq!(seq.file_verified_bytes(), [6, 6, 0]);
    assert_eq!(seq.file_ranks(), [SKIP, SKIP, 0]);
}
#[test]
fn duplicate_completion_is_idempotent() {
    let mut seq = Sequence::new(vec![file("1", 4)], 4, 100).unwrap();
    assert!(seq.mark_verified(0).unwrap());
    assert!(!seq.mark_verified(0).unwrap());
    assert_eq!(seq.file_verified_bytes(), [4]);
}
#[test]
fn invalidation_restores_rank_and_byte_count() {
    let mut seq = Sequence::new(vec![file("1", 4), file("2", 4)], 4, 100).unwrap();
    seq.mark_verified(0).unwrap();
    assert!(seq.invalidate(0).unwrap());
    assert!(!seq.invalidate(0).unwrap());
    assert_eq!(seq.file_ranks(), [0, 1]);
    assert_eq!(seq.file_verified_bytes(), [0, 0]);
}
#[test]
fn rarity_breaks_ties_only_inside_same_file_rank() {
    let seq = Sequence::new(vec![file("1", 8), file("2", 4)], 4, 100).unwrap();
    assert_eq!(seq.pick(&[7], &[0], &[9, 4, 1]).unwrap(), Some(1));
}
#[test]
fn peer_without_earlier_pieces_does_useful_later_work() {
    let seq = Sequence::new(vec![file("1", 4), file("2", 4)], 4, 100).unwrap();
    assert_eq!(seq.pick(&[2], &[0], &[1, 1]).unwrap(), Some(1));
}
#[test]
fn busy_and_verified_pieces_are_not_selected() {
    let mut seq = Sequence::new(vec![file("1", 12)], 4, 100).unwrap();
    seq.mark_verified(0).unwrap();
    assert_eq!(seq.pick(&[7], &[2], &[1, 1, 1]).unwrap(), Some(2));
}
#[test]
fn bitmap_tail_and_shape_checked() {
    let seq = Sequence::new(vec![file("1", 4)], 4, 100).unwrap();
    assert_eq!(seq.pick(&[u64::MAX], &[0], &[1]).unwrap(), Some(0));
    assert_eq!(seq.pick(&[], &[0], &[1]), Err(SequenceError::ShapeMismatch));
}
#[test]
fn more_than_255_files_have_distinct_ranks() {
    let files = (0..1000).map(|i| file(&format!("track {i}"), 4)).collect();
    let seq = Sequence::new(files, 4, 1000).unwrap();
    assert_eq!(seq.file_ranks()[999], 999);
}
#[test]
fn unwanted_file_still_shares_required_boundary_piece() {
    let mut seq = Sequence::new(vec![file("1", 6), file("2", 6)], 4, 100).unwrap();
    seq.set_wanted(0, false).unwrap();
    assert_eq!(seq.piece_ranks(), [SKIP, 0, 0]);
}
#[test]
fn zero_length_files_are_complete_without_a_piece() {
    let mut seq = Sequence::new(
        vec![file("0", 0), file("1", 4), file("2", 0), file("3", 4)],
        4,
        100,
    )
    .unwrap();
    seq.mark_verified(0).unwrap();
    seq.mark_verified(1).unwrap();
    assert_eq!(seq.file_verified_bytes(), [0, 4, 0, 4]);
}
#[test]
fn sequence_limits_reject_before_piece_allocation() {
    assert!(matches!(
        Sequence::new(vec![file("1", u64::MAX), file("2", 1)], 4, 10),
        Err(SequenceError::SizeOverflow)
    ));
    assert!(matches!(
        Sequence::new(vec![file("1", 1000)], 1, 10),
        Err(SequenceError::TooManyPieces)
    ));
    assert!(matches!(
        Sequence::new(vec![], 0, 10),
        Err(SequenceError::ZeroPieceLength)
    ));
}
#[test]
fn reorder_requires_a_complete_permutation() {
    let mut seq = Sequence::new(vec![file("1", 4), file("2", 4)], 4, 100).unwrap();
    assert_eq!(seq.set_order(vec![0, 0]), Err(SequenceError::InvalidOrder));
    seq.set_order(vec![1, 0]).unwrap();
    assert_eq!(seq.file_ranks(), [1, 0]);
}
#[test]
fn rate_limit_is_decimal_and_low_rates_can_request_blocks() {
    let mut bucket = TokenBucket::new(1, 0).unwrap();
    assert_eq!(bucket.rate(), 1000);
    assert!(bucket.reserve(16_384, 0));
    assert!(!bucket.reserve(1, 999_999));
    assert!(bucket.reserve(1, 1_000_000));
}
#[test]
fn rate_wait_is_exact_and_failed_reservations_do_not_spend() {
    let mut bucket = TokenBucket::new(1, 0).unwrap();
    bucket.reserve(16_384, 0);
    assert_eq!(bucket.wait_ns(1000, 0), Some(1_000_000_000));
    assert!(!bucket.reserve(1001, 1_000_000_000));
    assert!(bucket.reserve(1000, 1_000_000_000));
    assert_eq!(bucket.wait_ns(1_000_000, 0), None);
}
#[test]
fn unlimited_is_not_zero_slots() {
    assert!(TokenBucket::new(0, 0).unwrap().reserve(u64::MAX, 0));
    assert!(TokenBucket::new(u64::MAX, 0).is_none());
    let limits = Limits {
        download_jobs: 0,
        ..Limits::default()
    };
    assert!(admitted_jobs(&[(TorrentId(1), JobState::Queued)], &limits).is_empty());
}
#[test]
fn resetting_same_limit_does_not_refill_burst() {
    let mut bucket = TokenBucket::new(1, 0).unwrap();
    bucket.reserve(16_384, 0);
    bucket.set_limit(1, 0).unwrap();
    assert!(!bucket.reserve(1, 0));
}
#[test]
fn byte_budget_reservations_follow_lifetimes() {
    let budget = ByteBudget::new(10);
    let a = budget.try_acquire(7).unwrap();
    assert!(budget.try_acquire(4).is_none());
    {
        let _b = budget.try_acquire(3).unwrap();
        assert_eq!(budget.used(), 10);
    }
    assert_eq!(budget.used(), 7);
    drop(a);
    assert_eq!(budget.used(), 0);
    assert_eq!(budget.high_watermark(), 10);
}
#[test]
fn private_discovery_is_constrained() {
    assert!(discovery_allowed(
        Privacy::Private,
        Discovery::OriginalTracker,
        false
    ));
    for d in [
        Discovery::AddedTracker,
        Discovery::Dht,
        Discovery::Pex,
        Discovery::Local,
    ] {
        assert!(!discovery_allowed(Privacy::Private, d, true));
    }
    assert!(!discovery_allowed(Privacy::Unknown, Discovery::Dht, false));
    assert!(discovery_allowed(Privacy::Unknown, Discovery::Dht, true));
    assert!(!discovery_allowed(
        Privacy::Unknown,
        Discovery::AddedTracker,
        true
    ));
}
#[test]
fn announces_respect_minimum_and_never_overlap() {
    let mut c = TrackerClock::new();
    assert!(c.begin(0, false));
    assert!(!c.begin(0, true));
    c.success(0, 1800, Some(300));
    assert!(!c.begin(299, true));
    assert!(c.begin(300, true));
    c.success(300, 1800, Some(300));
    assert_eq!(c.next_due(), 2100);
}
#[test]
fn tracker_failures_back_off_with_bounded_jitter() {
    let mut c = TrackerClock::new();
    c.begin(0, false);
    c.failure(0, 9999);
    assert_eq!(c.next_due(), 33);
    assert!(c.begin(33, false));
    c.failure(33, 0);
    assert_eq!(c.next_due(), 93);
}
#[test]
fn unsafe_paths_are_rejected_on_both_platforms() {
    for bad in [
        "..", ".", "a/b", "a\\b", "a:stream", "CON.txt", "LPT9", "COM¹", "x.", "x ", "",
    ] {
        assert!(validate_component(bad).is_err(), "accepted {bad:?}");
    }
    assert!(validate_component("Episode 01.mkv").is_ok());
}
#[test]
fn removal_defaults_to_retaining_data_and_rejects_aliases() {
    assert!(!RemovalIntent::keep_data().delete_data);
    let a = RelativeFile::new(vec!["Album".into(), "01.flac".into()]).unwrap();
    let b = RelativeFile::new(vec!["album".into(), "01.flac".into()]).unwrap();
    assert!(matches!(
        RemovalIntent::confirmed(vec![a, b]),
        Err(PathError::Duplicate)
    ));
}
#[test]
fn queue_has_separate_download_and_seed_limits() {
    let limits = Limits {
        download_jobs: 1,
        seed_jobs: 1,
        ..Limits::default()
    };
    let jobs = [
        (TorrentId(1), JobState::Paused),
        (TorrentId(2), JobState::Queued),
        (TorrentId(3), JobState::Downloading),
        (TorrentId(4), JobState::Seeding),
        (TorrentId(5), JobState::Seeding),
    ];
    assert_eq!(admitted_jobs(&jobs, &limits), [TorrentId(2), TorrentId(4)]);
}
#[test]
fn title_and_remote_labels_are_bounded() {
    let row = TorrentRow {
        id: TorrentId(1),
        name: Arc::from("Example"),
        state: JobState::Downloading,
        wanted_bytes: 100,
        verified_bytes: 50,
        total_payload_downloaded: 50,
        total_payload_uploaded: 0,
        down_bps: 10,
        up_bps: 0,
        connected_seeds: 1,
        connected_peers: 0,
        estimated_seeds: None,
        estimated_peers: None,
    };
    assert_eq!(row.eta_seconds(), Some(5));
    assert_eq!(window_title(&[]), "TemplarTorrent");
    assert_eq!(
        window_title(std::slice::from_ref(&row)),
        "Example — TemplarTorrent"
    );
    assert_eq!(window_title(&[row.clone(), row]), "2 jobs — TemplarTorrent");
    assert_eq!(clean_label("x\n\u{202e}y", 10), "xy");
}

/// DESIGN.md section 4: a clear sequence gets strict ranks; unrelated names
/// share one rank so the picker goes rarest-first; saved tiers round-trip;
/// a manual order is strict again; a finished tier promotes the next.
#[test]
fn automatic_order_uses_tiers_for_unclear_sets_and_strict_ranks_for_sequences() {
    let spec = |name: &str, length: u64| FileSpec {
        path: name.to_string(),
        length,
        wanted: true,
    };
    let unrelated = Sequence::new(
        vec![
            spec("setup.exe", 16),
            spec("readme.pdf", 16),
            spec("data.bin", 16),
        ],
        16,
        1024,
    )
    .unwrap();
    assert_eq!(unrelated.file_ranks(), &[0, 0, 0], "one tier, rarest-first");
    assert_eq!(unrelated.tier_starts(), &[0]);
    assert_eq!(unrelated.order(), &[2, 1, 0], "natural within the tier");

    let mut series = Sequence::new(
        vec![
            spec("Show.S01E02.mkv", 16),
            spec("Show.S01E01.mkv", 16),
            spec("Show.S01E01.srt", 16),
            spec("Show.S01E10.mkv", 16),
        ],
        16,
        1024,
    )
    .unwrap();
    assert_eq!(series.order(), &[1, 2, 0, 3]);
    assert_eq!(series.tier_starts(), &[0, 1, 2, 3]);
    assert_eq!(series.file_ranks(), &[2, 0, 1, 3]);
    assert_eq!(series.piece_ranks(), &[2, 0, 1, 3]);
    // Episode 1 verifies: its subtitle becomes first and the rest move up.
    assert!(series.mark_verified(1).unwrap());
    assert_eq!(series.file_ranks(), &[1, SKIP, 0, 2]);

    // A saved record with two files sharing the first tier.
    series.set_tiers(vec![0, 3, 1, 2], vec![0, 2, 3]).unwrap();
    // The verified file's tier is inactive and takes no rank.
    assert_eq!(series.file_ranks(), &[0, SKIP, 1, 0]);
    assert_eq!(series.tier_starts(), &[0, 2, 3]);
    assert!(series.set_tiers(vec![0, 3, 1, 2], vec![1, 2]).is_err());
    assert!(series.set_tiers(vec![0, 3, 1, 2], vec![0, 4]).is_err());
    assert!(series.set_tiers(vec![0, 3, 1], vec![0]).is_err());
    // A manual order is strict.
    series.set_order(vec![3, 0, 2, 1]).unwrap();
    assert_eq!(series.tier_starts(), &[0, 1, 2, 3]);
    assert_eq!(series.file_ranks(), &[1, SKIP, 2, 0]);
    // Back to automatic.
    series.set_automatic();
    assert_eq!(series.order(), &[1, 2, 0, 3]);
    assert_eq!(series.file_ranks(), &[1, SKIP, 0, 2]);
}
