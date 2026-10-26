//! Validate original v1 bencode components before rbit flattens paths or casts
//! signed lengths. Unsupported layouts fail before opening any payload files.
use crate::{
    metadata::{FetchedMetadata, MAX_METADATA_SIZE},
    preflight,
};
use rbit::{InfoHash, Metainfo};
use sha1::{Digest, Sha1};
use std::collections::HashSet;
use templartorrent_core::{
    budget::{ByteBudget, Permit},
    paths::{RelativeFile, validate_component},
    sequence::FileSpec,
};
use templartorrent_engine::{session::MAX_PIECE_LENGTH, worker::PieceVerifier};

/// FileStore keeps one handle per real file for the job's life; storage
/// admission also checks the process's open-file budget.
pub const MAX_DRIVER_FILES: usize = 4096;
pub const MAX_DRIVER_PIECES: usize = 262_144;

/// A v1 torrent, or the v1 view of a hybrid one (BEP 52): v1 piece hashes,
/// the v1 `files` list including BEP 47 pad files, and the SHA-1 info hash.
/// Hybrid swarms are joined through that hash; v2 Merkle data is ignored.
pub struct ValidatedV1 {
    pub(crate) meta: Metainfo,
    pub(crate) paths: Vec<(Vec<String>, u64)>,
    /// `pads[i]` marks `paths[i]` as a BEP 47 pad file.
    pub(crate) pads: Vec<bool>,
    hash: [u8; 20],
    _permits: Vec<Permit>,
}

/// File list and pad flags from a v1 or hybrid info dictionary.
pub(crate) struct Layout {
    pub paths: Vec<(Vec<String>, u64)>,
    pub pads: Vec<bool>,
}

fn text(value: &[u8]) -> Result<&str, &'static str> {
    preflight::string(value)
        .and_then(|v| std::str::from_utf8(v).ok())
        .ok_or("invalid UTF-8 path component")
}
fn number(value: Option<&[u8]>) -> Result<u64, &'static str> {
    value
        .and_then(preflight::integer)
        .and_then(|v| u64::try_from(v).ok())
        .ok_or("invalid nonnegative v1 length")
}

pub(crate) fn layout(raw: &[u8]) -> Result<Vec<(Vec<String>, u64)>, &'static str> {
    Ok(layout_with_pads(raw)?.paths)
}

/// Validate the v1 view of `raw`. A hybrid dictionary (meta version 2 with a
/// file tree and v1 `pieces`) is read through its v1 keys; a pure v2 one,
/// without `pieces`, is refused.
pub(crate) fn layout_with_pads(raw: &[u8]) -> Result<Layout, &'static str> {
    let v2 = preflight::field(raw, b"meta version").is_some()
        || preflight::field(raw, b"file tree").is_some();
    if v2 && preflight::field(raw, b"pieces").is_none() {
        return Err("pure v2 torrents are not supported; hybrid and v1 torrents are");
    }
    let name = text(preflight::field(raw, b"name").ok_or("torrent name missing")?)?;
    validate_component(name).map_err(|_| "unsafe torrent name")?;
    if let Some(private) = preflight::field(raw, b"private")
        && !matches!(preflight::integer(private), Some(0 | 1))
    {
        return Err("invalid private flag");
    }
    let piece_length = number(preflight::field(raw, b"piece length"))?;
    if piece_length == 0 || piece_length > MAX_PIECE_LENGTH {
        return Err("unsupported v1 piece length");
    }
    let lengths = preflight::field(raw, b"length");
    let files = preflight::field(raw, b"files");
    let mut paths = Vec::new();
    let mut pads = Vec::new();
    match (lengths, files) {
        (Some(length), None) => {
            paths.push((vec![name.into()], number(Some(length))?));
            pads.push(false);
        }
        (None, Some(files)) => {
            for file in preflight::list(files, MAX_DRIVER_FILES)? {
                // BEP 47: p = pad, x = executable, h = hidden, l = symlink.
                let attr = match preflight::field(file, b"attr") {
                    Some(value) => text(value).map_err(|_| "invalid file attributes")?,
                    None => "",
                };
                if attr.contains('l') || preflight::field(file, b"symlink path").is_some() {
                    return Err("symlink entries are not supported");
                }
                pads.push(attr.contains('p'));
                let mut components = vec![name.to_string()];
                for component in preflight::list(
                    preflight::field(file, b"path").ok_or("file path missing")?,
                    63,
                )? {
                    components.push(text(component)?.to_string());
                }
                if components.len() == 1 {
                    return Err("empty file path");
                }
                paths.push((components, number(preflight::field(file, b"length"))?));
            }
        }
        _ => return Err("v1 layout requires exactly one of length or files"),
    }
    if paths.is_empty() {
        return Err("empty torrent file list");
    }
    let mut files = HashSet::new();
    let mut directories = HashSet::new();
    let mut total = 0u64;
    for ((components, length), pad) in paths.iter().zip(&pads) {
        if *pad {
            // Pad files are never created, and creators reuse names such as
            // `.pad/16384`, so they take part in no path check.
            total = total
                .checked_add(*length)
                .ok_or("v1 total length overflow")?;
            continue;
        }
        RelativeFile::new(components.clone()).map_err(|_| "unsafe original path component")?;
        let key = components.join("/").to_ascii_lowercase();
        if directories.contains(&key) || !files.insert(key) {
            return Err("duplicate or conflicting torrent paths");
        }
        for index in 1..components.len() {
            let parent = components[..index].join("/").to_ascii_lowercase();
            if files.contains(&parent) {
                return Err("file-directory path conflict");
            }
            directories.insert(parent);
        }
        total = total
            .checked_add(*length)
            .ok_or("v1 total length overflow")?;
    }
    let hashes = preflight::field(raw, b"pieces")
        .and_then(preflight::string)
        .ok_or("v1 piece hashes missing")?;
    if hashes.len() % 20 != 0
        || hashes.len() / 20 > MAX_DRIVER_PIECES
        || total.div_ceil(piece_length) != (hashes.len() / 20) as u64
    {
        return Err("v1 piece hashes disagree with file sizes or exceed limits");
    }
    if pads.iter().all(|pad| *pad) {
        return Err("torrent has no real files");
    }
    Ok(Layout { paths, pads })
}

impl ValidatedV1 {
    pub(crate) fn from_fetched(
        fetched: FetchedMetadata,
        hash: [u8; 20],
        budget: &ByteBudget,
    ) -> Result<(Self, Vec<rbit::Message>), &'static str> {
        let FetchedMetadata {
            raw_info,
            buffered,
            piece_count: _,
            permits,
        } = fetched;
        Ok((
            Self::from_raw_info(raw_info, hash, budget, permits)?,
            buffered,
        ))
    }

    /// Stored metainfo bytes: a `.torrent` file or the `d4:info…e` envelope
    /// that [`crate::driver::Driver::export_job`] writes. Same gates as the
    /// network path; the raw info copy is charged to the metadata budget.
    pub(crate) fn from_torrent_bytes(
        bytes: &[u8],
        budget: &ByteBudget,
    ) -> Result<(Self, [u8; 20]), &'static str> {
        if bytes.len() > MAX_METADATA_SIZE {
            return Err("torrent file exceeds 16 MiB limit");
        }
        let meta = Metainfo::from_bytes(bytes).map_err(|_| "invalid torrent file")?;
        let hash = meta
            .info_hash
            .v1_hash()
            .ok_or("pure v2 torrents are not supported; hybrid and v1 torrents are")?
            .0;
        let raw_info = meta.raw_info().to_vec();
        let permit = budget
            .try_acquire(raw_info.len())
            .ok_or("metadata budget exhausted")?;
        Ok((
            Self::from_raw_info(raw_info, hash, budget, vec![permit])?,
            hash,
        ))
    }

    fn from_raw_info(
        raw_info: Vec<u8>,
        hash: [u8; 20],
        budget: &ByteBudget,
        mut permits: Vec<Permit>,
    ) -> Result<Self, &'static str> {
        // Check again at the admission boundary; callers cannot mutate raw_info
        // and accidentally retain the resolver's authority for different bytes.
        if raw_info.len() > MAX_METADATA_SIZE || <[u8; 20]>::from(Sha1::digest(&raw_info)) != hash {
            return Err("metadata infohash mismatch");
        }
        let items = preflight::validate(&raw_info)?;
        // Conservative parse working-set admission includes rbit's raw copy,
        // piece hashes and tree/container overhead. Kept charged for job life.
        let additional = raw_info
            .len()
            .checked_mul(3)
            .and_then(|v| v.checked_add(items.checked_mul(256)?))
            .ok_or("metadata working set overflow")?;
        permits.push(
            budget
                .try_acquire(additional)
                .ok_or("metadata parse working set exceeds budget")?,
        );
        let Layout { paths, pads } = layout_with_pads(&raw_info)?;
        let mut envelope = Vec::with_capacity(raw_info.len() + 8);
        envelope.extend_from_slice(b"d4:info");
        envelope.extend_from_slice(&raw_info);
        envelope.push(b'e');
        let meta = Metainfo::from_bytes(&envelope).map_err(|_| "invalid v1 metadata")?;
        if meta.info_hash.v1_hash().map(|h| h.0) != Some(hash)
            || !matches!(meta.info_hash, InfoHash::V1(_) | InfoHash::Hybrid { .. })
        {
            return Err("pure v2 torrents are not supported; hybrid and v1 torrents are");
        }
        if meta.info.piece_count() != paths_pieces(&paths, meta.info.piece_length) {
            return Err("v1 piece hashes disagree with file sizes or exceed limits");
        }
        Ok(Self {
            meta,
            paths,
            pads,
            hash,
            _permits: permits,
        })
    }

    pub fn name(&self) -> &str {
        &self.meta.info.name
    }
    /// The SHA-1 info hash peers and trackers use, for v1 and hybrid alike.
    pub fn hash(&self) -> [u8; 20] {
        self.hash
    }
    pub fn is_pad(&self, file: usize) -> bool {
        self.pads.get(file).copied().unwrap_or(false)
    }
    pub fn is_private(&self) -> bool {
        self.meta.info.private
    }
    pub fn piece_count(&self) -> usize {
        self.meta.info.piece_count()
    }
    pub fn piece_length(&self) -> u64 {
        self.meta.info.piece_length
    }
    pub fn files(&self) -> Vec<FileSpec> {
        self.paths
            .iter()
            .zip(&self.pads)
            .map(|((parts, length), pad)| FileSpec {
                path: parts.join("/"),
                length: *length,
                // Pad files are never fetched for their own sake: a piece
                // is wanted only through the real files it overlaps.
                wanted: !pad,
            })
            .collect()
    }
}
fn paths_pieces(paths: &[(Vec<String>, u64)], piece_length: u64) -> usize {
    let total: u64 = paths.iter().map(|(_, length)| *length).sum();
    total.div_ceil(piece_length.max(1)) as usize
}

impl PieceVerifier for ValidatedV1 {
    fn verify(&self, piece: u32, data: &[u8]) -> bool {
        self.meta
            .info
            .get_v1_piece_hash(piece as usize)
            .is_some_and(|expected| expected == <[u8; 20]>::from(Sha1::digest(data)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn info(paths: &[Vec<String>]) -> Vec<u8> {
        use bytes::Bytes;
        use rbit::Value;
        let files = paths
            .iter()
            .map(|path| {
                Value::Dict(
                    [
                        (Bytes::from_static(b"length"), Value::Integer(0)),
                        (
                            Bytes::from_static(b"path"),
                            Value::List(
                                path.iter()
                                    .map(|s| Value::Bytes(Bytes::copy_from_slice(s.as_bytes())))
                                    .collect(),
                            ),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                )
            })
            .collect();
        rbit::encode(&Value::Dict(
            [
                (Bytes::from_static(b"files"), Value::List(files)),
                (
                    Bytes::from_static(b"name"),
                    Value::Bytes(Bytes::from_static(b"owned")),
                ),
                (Bytes::from_static(b"piece length"), Value::Integer(16384)),
                (Bytes::from_static(b"pieces"), Value::Bytes(Bytes::new())),
            ]
            .into_iter()
            .collect(),
        ))
        .unwrap()
    }
    #[test]
    fn validates_original_components_before_path_normalization() {
        for raw in [
            &b"d5:filesld6:lengthi0e4:pathl3:a/beee4:name1:x12:piece lengthi16e6:pieces0:e"[..],
            b"d6:lengthi-1e4:name1:x12:piece lengthi16e6:pieces0:e",
            b"d6:lengthi0e4:name1:x12:piece lengthi16e6:pieces0:7:privatei2ee",
        ] {
            preflight::validate(raw).unwrap();
            assert!(layout(raw).is_err());
        }
    }

    #[test]
    fn caps_file_handles_and_rejects_aliases_and_file_directory_conflicts() {
        for paths in [
            vec![vec!["A".into()], vec!["a".into()]],
            vec![vec!["a".into()], vec!["a".into(), "b".into()]],
            vec![vec!["a".into(), "b".into()], vec!["a".into()]],
            vec![vec!["..".into(), "escape".into()]],
            vec![vec!["a/b".into()]],
            vec![vec!["CON".into()]],
        ] {
            let raw = info(&paths);
            preflight::validate(&raw).unwrap();
            assert!(layout(&raw).is_err());
        }
        let mut paths: Vec<_> = (0..MAX_DRIVER_FILES)
            .map(|i| vec![format!("file{i}")])
            .collect();
        assert_eq!(layout(&info(&paths)).unwrap().len(), MAX_DRIVER_FILES);
        paths.push(vec!["one-too-many".into()]);
        assert!(layout(&info(&paths)).is_err());
    }

    #[test]
    fn seeded_malformed_metainfo_mutations_do_not_panic() {
        let raw = info(&[vec!["a".into()]]);
        let mut torrent = b"d4:info".to_vec();
        torrent.extend(raw);
        torrent.push(b'e');
        let mut state = 0x243f_6a88_85a3_08d3u64;
        for _ in 0..4096 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut mutated = torrent.clone();
            let at = state as usize % mutated.len();
            mutated[at] = (state >> 32) as u8;
            if state & 1 == 0 {
                mutated.truncate((state >> 16) as usize % mutated.len());
            }
            let _ = crate::parse_torrent(&mutated);
        }
    }
}
