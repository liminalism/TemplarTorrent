//! Sequence-group detection for the automatic file order (DESIGN.md section 4).
//!
//! A torrent's files form a clear sequence when most of them share one name
//! template that differs only in one or two numeric runs: `S01E01`, `S01E02`;
//! `Disc 1/01.flac`, `Disc 1/02.flac`; `part1`, `part2`; `.part01.rar`,
//! `.part02.rar`; `.r00`, `.r01`. Members are ordered by those numbers,
//! outer run first (season before episode, disc before track), so leading
//! zeros and widths do not matter, and ties fall back to the natural
//! comparator and then the wire index.
//!
//! Anything less clear is not a sequence. Three or more varying numeric runs
//! (dates), several templates of the same size, or fewer than two members
//! leave every file in one tier, which the picker schedules rarest-first:
//! a filename guess is not promoted to a download order. Sidecars (subtitles,
//! checksums, notes) whose stem begins with a member's stem are scheduled in
//! their own tier right after that member. Files that match nothing, and
//! BEP 47 pad files, go last in one tier.
//!
//! The output is tiers: files in one tier share a rank. A strict order is
//! the special case of one file per tier, and the user's manual order always
//! stays strict.

use crate::sequence::natural_cmp;

/// The automatic order: tiers in download order, each a non-empty list of
/// wire indices, together a partition of every index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Automatic {
    pub tiers: Vec<Vec<usize>>,
    /// A clear sequence was found; otherwise the tiers carry no claim.
    pub sequence: bool,
}

impl Automatic {
    /// Positions in the flattened order where a new tier starts.
    pub fn starts(&self) -> Vec<usize> {
        let mut starts = Vec::with_capacity(self.tiers.len());
        let mut at = 0;
        for tier in &self.tiers {
            starts.push(at);
            at += tier.len();
        }
        starts
    }

    pub fn order(&self) -> Vec<usize> {
        self.tiers.iter().flatten().copied().collect()
    }
}

/// Extensions that ride along with a primary file of the same stem.
const SIDECARS: &[&str] = &[
    "srt", "sub", "idx", "ass", "ssa", "vtt", "nfo", "txt", "md5", "sfv", "sha1", "sha256", "cue",
    "log", "m3u", "m3u8", "jpg", "jpeg", "png", "url",
];

/// Most numeric runs that may vary inside one template: outer and inner
/// (season/episode, disc/track). A third varying run reads as a date.
const MAX_VARYING: usize = 2;

struct Shape {
    /// Lower-cased non-digit runs, with the digit runs cut out.
    text: Vec<String>,
    /// The digit runs, as written.
    digits: Vec<String>,
}

fn shape(path: &str) -> Shape {
    let mut text = Vec::new();
    let mut digits = Vec::new();
    let mut current = String::new();
    let mut in_digits = false;
    for ch in path.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                text.push(std::mem::take(&mut current).to_ascii_lowercase());
                in_digits = true;
            }
            current.push(ch);
        } else {
            if in_digits {
                digits.push(std::mem::take(&mut current));
                in_digits = false;
            }
            current.push(ch);
        }
    }
    if in_digits {
        digits.push(current);
        text.push(String::new());
    } else {
        text.push(current.to_ascii_lowercase());
    }
    Shape { text, digits }
}

/// Numeric comparison of two digit runs without parsing into a fixed width.
fn digits_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn directory(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) => &path[..index],
        None => "",
    }
}

fn stem_and_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(index) if index > 0 => (&name[..index], &name[index + 1..]),
        _ => (name, ""),
    }
}

fn is_pad(path: &str) -> bool {
    path.starts_with(".pad/") || path.contains("_____padding_file")
}

fn is_sidecar(path: &str) -> bool {
    let (_, extension) = stem_and_extension(file_name(path));
    let extension = extension.to_ascii_lowercase();
    SIDECARS.contains(&extension.as_str())
}

/// Detect the automatic order for `paths` (wire order). `eligible[i]` false
/// keeps a file out of the detection and in the trailing tier: pad files at
/// construction, nothing else.
pub fn automatic(paths: &[&str], eligible: &[bool]) -> Automatic {
    let n = paths.len();
    let natural = |a: &usize, b: &usize| natural_cmp(paths[*a], paths[*b]).then(a.cmp(b));
    let one_tier = |mut indices: Vec<usize>| {
        indices.sort_by(natural);
        Automatic {
            tiers: if indices.is_empty() {
                Vec::new()
            } else {
                vec![indices]
            },
            sequence: false,
        }
    };
    if n < 2 {
        return one_tier((0..n).collect());
    }
    let shapes: Vec<Shape> = paths.iter().map(|path| shape(path)).collect();
    let candidate = |index: usize| {
        eligible.get(index).copied().unwrap_or(true)
            && !is_pad(paths[index])
            && !is_sidecar(paths[index])
            && !shapes[index].digits.is_empty()
    };
    // Cluster candidates by template: equal text runs and digit-run count.
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    for index in 0..n {
        if !candidate(index) {
            continue;
        }
        let same = clusters.iter_mut().find(|cluster| {
            let first = &shapes[cluster[0]];
            first.text == shapes[index].text && first.digits.len() == shapes[index].digits.len()
        });
        match same {
            Some(cluster) => cluster.push(index),
            None => clusters.push(vec![index]),
        }
    }
    // The largest cluster, and only when it is clearly the largest.
    clusters.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| natural(&a[0], &b[0])));
    let Some(group) = clusters.first().filter(|group| group.len() >= 2) else {
        return one_tier((0..n).collect());
    };
    if clusters
        .get(1)
        .is_some_and(|next| next.len() == group.len())
    {
        return one_tier((0..n).collect());
    }
    let runs = shapes[group[0]].digits.len();
    let varying: Vec<usize> = (0..runs)
        .filter(|run| {
            let first = &shapes[group[0]].digits[*run];
            group.iter().any(|index| {
                digits_cmp(&shapes[*index].digits[*run], first) != std::cmp::Ordering::Equal
            })
        })
        .collect();
    if varying.is_empty() || varying.len() > MAX_VARYING {
        return one_tier((0..n).collect());
    }
    let mut members = group.clone();
    members.sort_by(|a, b| {
        for run in &varying {
            let order = digits_cmp(&shapes[*a].digits[*run], &shapes[*b].digits[*run]);
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        natural(a, b)
    });
    let mut placed = vec![false; n];
    let mut tiers = Vec::with_capacity(members.len() * 2);
    for member in &members {
        placed[*member] = true;
        tiers.push(vec![*member]);
        let (stem, _) = stem_and_extension(file_name(paths[*member]));
        let stem = stem.to_ascii_lowercase();
        let dir = directory(paths[*member]);
        let mut sidecars: Vec<usize> = (0..n)
            .filter(|index| {
                !placed[*index]
                    && eligible.get(*index).copied().unwrap_or(true)
                    && is_sidecar(paths[*index])
                    && directory(paths[*index]) == dir
                    && file_name(paths[*index])
                        .to_ascii_lowercase()
                        .starts_with(&stem)
            })
            .collect();
        if !sidecars.is_empty() {
            sidecars.sort_by(natural);
            for index in &sidecars {
                placed[*index] = true;
            }
            tiers.push(sidecars);
        }
    }
    let mut rest: Vec<usize> = (0..n).filter(|index| !placed[*index]).collect();
    if !rest.is_empty() {
        rest.sort_by(natural);
        tiers.push(rest);
    }
    Automatic {
        tiers,
        sequence: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(paths: &[&str]) -> Automatic {
        automatic(paths, &vec![true; paths.len()])
    }

    #[test]
    fn episodes_order_by_season_then_episode_regardless_of_width() {
        let paths = [
            "Show/Show.S02E01.1080p.mkv",
            "Show/Show.S01E10.1080p.mkv",
            "Show/Show.S01E2.1080p.mkv",
            "Show/Show.S01E01.1080p.mkv",
        ];
        let found = detect(&paths);
        assert!(found.sequence);
        assert_eq!(found.order(), vec![3, 2, 1, 0]);
        assert_eq!(found.tiers.len(), 4, "strict tiers for a clear sequence");
        assert_eq!(found.starts(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn discs_and_tracks_nest_and_sidecars_follow_their_member() {
        let paths = [
            "Album/Disc 2/01 - Opening.flac",
            "Album/Disc 1/02 - Second.flac",
            "Album/Disc 1/01 - First.flac",
            "Album/Disc 1/01 - First.cue",
            "Album/cover.jpg",
            "Album/Disc 1/10 - Tenth.flac",
        ];
        // Titles differ, so the text runs of "NN - Title.flac" differ per
        // file: no shared template, no claimed sequence, one tier.
        let found = detect(&paths);
        assert!(!found.sequence);
        assert_eq!(found.tiers.len(), 1);
        let plain = [
            "Album/Disc 2/01.flac",
            "Album/Disc 1/02.flac",
            "Album/Disc 1/01.flac",
            "Album/Disc 1/01.cue",
            "Album/cover.jpg",
            "Album/Disc 1/10.flac",
        ];
        let found = detect(&plain);
        assert!(found.sequence);
        assert_eq!(
            found.tiers,
            vec![vec![2], vec![3], vec![1], vec![5], vec![0], vec![4]],
            "disc 1 tracks 1 (with its cue), 2, 10, then disc 2, then the cover"
        );
    }

    #[test]
    fn archive_volumes_and_parts_are_sequences() {
        let rar = ["x.part3.rar", "x.part1.rar", "x.part2.rar", "x.sfv"];
        let found = detect(&rar);
        assert!(found.sequence);
        assert_eq!(found.order(), vec![1, 2, 0, 3]);
        let old = ["x.r01", "x.rar", "x.r00", "x.r10", "x.r02"];
        let found = detect(&old);
        assert!(found.sequence);
        // `x.rar` has no digits: it is not a member and trails.
        assert_eq!(found.order(), vec![2, 0, 4, 3, 1]);
        let parts = ["movie.part2.mkv", "movie.part1.mkv", "movie.part10.mkv"];
        assert_eq!(detect(&parts).order(), vec![1, 0, 2]);
    }

    #[test]
    fn unclear_sets_fall_back_to_one_tier() {
        let unrelated = ["readme.pdf", "setup.exe", "data.bin"];
        let found = detect(&unrelated);
        assert!(!found.sequence);
        assert_eq!(found.tiers, vec![vec![2, 0, 1]], "natural order, one tier");
        let dates = [
            "log-2023-01-05.txt",
            "log-2023-02-01.txt",
            "log-2024-01-01.txt",
        ];
        let found = detect(&dates);
        assert!(!found.sequence, "three varying runs read as dates");
        assert_eq!(found.tiers.len(), 1);
        let tie = ["a1.mkv", "a2.mkv", "b1.mkv", "b2.mkv"];
        assert!(!detect(&tie).sequence, "two equal clusters are not clear");
        let lone = ["one.mkv"];
        assert_eq!(detect(&lone).tiers, vec![vec![0]]);
        assert_eq!(detect(&[]).tiers, Vec::<Vec<usize>>::new());
        let same = ["dup.001", "dup.001", "dup.002"];
        let found = detect(&same);
        assert!(found.sequence);
        assert_eq!(
            found.order(),
            vec![0, 1, 2],
            "equal numbers keep wire order"
        );
    }

    #[test]
    fn pads_samples_and_ineligible_files_trail() {
        let paths = [
            "S/S01E02.mkv",
            ".pad/4096",
            "S/S01E01.mkv",
            ".pad/8192",
            "S/S01E01.sample.mkv",
            "S/S01E01.en.srt",
        ];
        let found = automatic(&paths, &[true, false, true, false, true, true]);
        assert!(found.sequence);
        assert_eq!(
            found.tiers,
            vec![vec![2], vec![5], vec![0], vec![1, 3, 4]],
            "episode 1, its subtitle, episode 2, then pads and the sample"
        );
    }
}
