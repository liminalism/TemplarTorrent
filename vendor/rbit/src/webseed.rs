//! BEP-19: WebSeed - HTTP/FTP Seeding
//!
//! This module provides HTTP/FTP seeding support for BitTorrent,
//! allowing clients to download torrent data from web servers
//! in addition to peers.
//!
//! [BEP-19]: http://bittorrent.org/beps/bep_0019.html

use std::collections::HashSet;
use std::time::Duration;

use bytes::Bytes;
use reqwest::{header, Client, StatusCode};
use thiserror::Error;
use tracing::{debug, warn};

/// Default timeout for HTTP connection.
pub const WEBSEED_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default timeout for HTTP read operations.
pub const WEBSEED_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum number of retries for failed requests.
pub const WEBSEED_MAX_RETRIES: u32 = 3;

/// File completion threshold for gap-filling mode (50%).
pub const WEBSEED_GAP_FILL_THRESHOLD: f64 = 0.5;

/// Errors that can occur during web seeding.
#[derive(Debug, Error)]
pub enum WebSeedError {
    /// HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// Server returned an error status code.
    #[error("HTTP status error: {0}")]
    StatusError(StatusCode),

    /// Downloaded data failed hash verification.
    #[error("hash verification failed")]
    HashMismatch,

    /// Server is temporarily busy (should retry later).
    #[error("server temporarily busy")]
    TemporarilyBusy,

    /// URL is blacklisted due to previous failures.
    #[error("URL is blacklisted")]
    Blacklisted,

    /// Invalid URL format.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    /// No available URLs to download from.
    #[error("no available URLs")]
    NoAvailableUrls,

    /// Range request not supported by server.
    #[error("range requests not supported")]
    RangeNotSupported,
}

/// Information about a torrent for URL construction.
#[derive(Debug, Clone)]
pub struct TorrentInfo {
    /// The torrent name (top-level directory or file name).
    pub name: String,
    /// Whether this is a single-file torrent.
    pub is_single_file: bool,
    /// Total torrent size.
    pub total_length: u64,
    /// Piece length in bytes.
    pub piece_length: u64,
}

/// Information about a file within a torrent.
#[derive(Debug, Clone)]
pub struct FileInfo {
    /// File path relative to torrent root.
    pub path: String,
    /// File length in bytes.
    pub length: u64,
    /// Byte offset of file within the torrent.
    pub offset: u64,
}

/// Client for downloading from HTTP/FTP web seeds.
pub struct WebSeedClient {
    /// HTTP client for making requests.
    client: Client,
    /// Base URLs for web seeds.
    base_urls: Vec<String>,
    /// URLs that have been blacklisted due to failures.
    blacklisted: HashSet<String>,
}

impl WebSeedClient {
    /// Creates a new web seed client with the given URLs.
    pub fn new(urls: Vec<String>) -> Result<Self, WebSeedError> {
        let client = Client::builder()
            .connect_timeout(WEBSEED_CONNECT_TIMEOUT)
            .read_timeout(WEBSEED_READ_TIMEOUT)
            .build()?;

        Ok(Self {
            client,
            base_urls: urls,
            blacklisted: HashSet::new(),
        })
    }

    /// Creates a new web seed client with a custom HTTP client.
    pub fn with_client(client: Client, urls: Vec<String>) -> Self {
        Self {
            client,
            base_urls: urls,
            blacklisted: HashSet::new(),
        }
    }

    /// Returns the number of available (non-blacklisted) URLs.
    pub fn available_url_count(&self) -> usize {
        self.base_urls
            .iter()
            .filter(|url| !self.blacklisted.contains(*url))
            .count()
    }

    /// Returns true if there are any available URLs.
    pub fn has_available_urls(&self) -> bool {
        self.available_url_count() > 0
    }

    /// Blacklists a URL due to failures (e.g., hash mismatch).
    pub fn blacklist_url(&mut self, url: &str) {
        warn!("WebSeed: Blacklisting URL due to failures: {}", url);
        self.blacklisted.insert(url.to_string());
    }

    /// Clears all blacklisted URLs.
    pub fn clear_blacklist(&mut self) {
        self.blacklisted.clear();
    }

    /// Constructs the full URL for a file in a single-file torrent.
    ///
    /// Per BEP-19:
    /// - If base URL ends with '/', append the torrent name
    /// - Otherwise, use the base URL as-is (it points directly to the file)
    pub fn construct_single_file_url(&self, base_url: &str, torrent_name: &str) -> String {
        if base_url.ends_with('/') {
            format!("{}{}", base_url, torrent_name)
        } else {
            base_url.to_string()
        }
    }

    /// Constructs the full URL for a file in a multi-file torrent.
    ///
    /// Per BEP-19:
    /// - Append {name}/{path/to/file} to the base URL
    pub fn construct_multi_file_url(
        &self,
        base_url: &str,
        torrent_name: &str,
        file_path: &str,
    ) -> String {
        let base = base_url.strip_suffix('/').unwrap_or(base_url);
        format!("{}/{}/{}", base, torrent_name, file_path)
    }

    /// Fetches a byte range from a URL.
    ///
    /// Uses HTTP Range header to request partial content.
    pub async fn fetch_range(
        &self,
        url: &str,
        start: u64,
        end: u64,
    ) -> Result<Bytes, WebSeedError> {
        if self.blacklisted.contains(url) {
            return Err(WebSeedError::Blacklisted);
        }

        debug!("WebSeed: Fetching range {}-{} from {}", start, end, url);

        let response = self
            .client
            .get(url)
            .header(header::RANGE, format!("bytes={}-{}", start, end))
            .send()
            .await?;

        match response.status() {
            StatusCode::PARTIAL_CONTENT => {
                // Server supports range requests
                Ok(response.bytes().await?)
            }
            StatusCode::OK => {
                // Server doesn't support range requests, returned full file
                // We need to slice the response
                let bytes = response.bytes().await?;
                let start = start as usize;
                let end = (end + 1).min(bytes.len() as u64) as usize;
                if start >= bytes.len() {
                    return Err(WebSeedError::RangeNotSupported);
                }
                Ok(bytes.slice(start..end))
            }
            StatusCode::SERVICE_UNAVAILABLE | StatusCode::TOO_MANY_REQUESTS => {
                // Server is busy, don't blacklist
                Err(WebSeedError::TemporarilyBusy)
            }
            status => Err(WebSeedError::StatusError(status)),
        }
    }

    /// Downloads a piece from any available web seed.
    ///
    /// Tries each non-blacklisted URL in order until one succeeds.
    pub async fn download_piece(
        &self,
        torrent: &TorrentInfo,
        files: &[FileInfo],
        piece_index: u32,
    ) -> Result<Bytes, WebSeedError> {
        if !self.has_available_urls() {
            return Err(WebSeedError::NoAvailableUrls);
        }

        let piece_start = piece_index as u64 * torrent.piece_length;
        let piece_end = (piece_start + torrent.piece_length).min(torrent.total_length) - 1;

        // For single-file torrents, the piece is within the single file
        if torrent.is_single_file {
            for base_url in &self.base_urls {
                if self.blacklisted.contains(base_url) {
                    continue;
                }

                let url = self.construct_single_file_url(base_url, &torrent.name);
                match self.fetch_range(&url, piece_start, piece_end).await {
                    Ok(data) => return Ok(data),
                    Err(WebSeedError::TemporarilyBusy) => continue,
                    Err(e) => {
                        debug!("WebSeed: Error from {}: {}", url, e);
                        continue;
                    }
                }
            }
        } else {
            // For multi-file torrents, a piece may span multiple files
            // We need to fetch from each file and concatenate
            let piece_data = self
                .download_piece_multi_file(torrent, files, piece_start, piece_end)
                .await?;
            return Ok(piece_data);
        }

        Err(WebSeedError::NoAvailableUrls)
    }

    /// Downloads a piece that may span multiple files.
    async fn download_piece_multi_file(
        &self,
        torrent: &TorrentInfo,
        files: &[FileInfo],
        piece_start: u64,
        piece_end: u64,
    ) -> Result<Bytes, WebSeedError> {
        let mut result = Vec::new();
        let mut current_pos = piece_start;

        // Find which files this piece spans
        for file in files {
            let file_start = file.offset;
            let file_end = file.offset + file.length;

            // Skip files before this piece
            if file_end <= piece_start {
                continue;
            }

            // Stop if we've passed this piece
            if file_start > piece_end {
                break;
            }

            // Calculate the range within this file
            let range_start = current_pos.saturating_sub(file_start);
            let range_end = (piece_end + 1).min(file_end) - file_start - 1;

            // Try each URL
            let mut success = false;
            for base_url in &self.base_urls {
                if self.blacklisted.contains(base_url) {
                    continue;
                }

                let url = self.construct_multi_file_url(base_url, &torrent.name, &file.path);
                match self.fetch_range(&url, range_start, range_end).await {
                    Ok(data) => {
                        result.extend_from_slice(&data);
                        success = true;
                        break;
                    }
                    Err(WebSeedError::TemporarilyBusy) => continue,
                    Err(e) => {
                        debug!("WebSeed: Error from {}: {}", url, e);
                        continue;
                    }
                }
            }

            if !success {
                return Err(WebSeedError::NoAvailableUrls);
            }

            current_pos = file_end;
            if current_pos > piece_end {
                break;
            }
        }

        Ok(Bytes::from(result))
    }
}

/// BEP-19 piece selection strategy.
///
/// Modifies "rarest first" to create gaps that HTTP/FTP can fill efficiently.
#[derive(Debug, Clone)]
pub struct GapAwarePieceSelector {
    /// Number of connected peers (used in rarity calculation).
    peer_count: usize,
}

impl GapAwarePieceSelector {
    /// Creates a new gap-aware piece selector.
    pub fn new(peer_count: usize) -> Self {
        Self { peer_count }
    }

    /// Updates the peer count.
    pub fn set_peer_count(&mut self, count: usize) {
        self.peer_count = count;
    }

    /// Calculates X value for BEP-19 formula: X = sqrt(peers) - 1
    fn x_value(&self) -> f64 {
        (self.peer_count as f64).sqrt() - 1.0
    }

    /// Selects a piece using BEP-19 gap-aware strategy.
    ///
    /// Priority logic:
    /// 1. If piece_rarity < (cur_rarest - X), select it
    /// 2. Else if piece_rarity <= (cur_rarest + X) AND gap > cur_gap, select it
    ///
    /// Arguments:
    /// - `piece_availability`: For each piece, how many peers have it
    /// - `our_pieces`: Which pieces we already have
    /// - `peer_pieces`: Which pieces the current peer has
    ///
    /// Returns: Selected piece index, or None if no suitable piece found
    pub fn select_piece(
        &self,
        piece_availability: &[usize],
        our_pieces: &[bool],
        peer_pieces: &[bool],
    ) -> Option<u32> {
        let x = self.x_value().max(0.0);
        let _num_pieces = piece_availability.len();

        // Find the rarest pieces we need that the peer has
        let mut candidates: Vec<(u32, usize, usize)> = Vec::new(); // (index, rarity, gap)

        for (i, (&availability, (&have, &peer_has))) in piece_availability
            .iter()
            .zip(our_pieces.iter().zip(peer_pieces.iter()))
            .enumerate()
        {
            // Skip pieces we already have or peer doesn't have
            if have || !peer_has {
                continue;
            }

            // Calculate gap (distance to nearest completed piece)
            let gap = self.calculate_gap(i, our_pieces);
            candidates.push((i as u32, availability, gap));
        }

        if candidates.is_empty() {
            return None;
        }

        // Find the rarest piece
        let cur_rarest = candidates.iter().map(|(_, r, _)| *r).min().unwrap_or(0);

        // Apply BEP-19 selection logic
        let mut best: Option<(u32, usize, usize)> = None;

        for (idx, rarity, gap) in candidates {
            let rarity_threshold_low = (cur_rarest as f64 - x).max(0.0) as usize;
            let rarity_threshold_high = cur_rarest + x as usize;

            if rarity < rarity_threshold_low {
                // Very rare piece, select immediately
                return Some(idx);
            } else if rarity <= rarity_threshold_high {
                // Within acceptable rarity range, prefer larger gaps
                match best {
                    None => best = Some((idx, rarity, gap)),
                    Some((_, _, best_gap)) if gap > best_gap => {
                        best = Some((idx, rarity, gap));
                    }
                    _ => {}
                }
            }
        }

        best.map(|(idx, _, _)| idx)
    }

    /// Calculates the gap (distance to nearest completed piece).
    fn calculate_gap(&self, piece_index: usize, our_pieces: &[bool]) -> usize {
        let num_pieces = our_pieces.len();
        let mut min_distance = num_pieces;

        // Search left
        for i in (0..piece_index).rev() {
            if our_pieces[i] {
                min_distance = min_distance.min(piece_index - i);
                break;
            }
        }

        // Search right
        for (i, &have) in our_pieces.iter().enumerate().skip(piece_index + 1) {
            if have {
                min_distance = min_distance.min(i - piece_index);
                break;
            }
        }

        min_distance
    }

    /// Selects a piece for gap-filling mode (used when > 50% complete).
    ///
    /// Finds the piece with the smallest gap to a completed section.
    pub fn select_gap_filling_piece(
        &self,
        our_pieces: &[bool],
        peer_pieces: &[bool],
    ) -> Option<u32> {
        let mut best: Option<(u32, usize)> = None; // (index, gap)

        for (i, (&have, &peer_has)) in our_pieces.iter().zip(peer_pieces.iter()).enumerate() {
            if have || !peer_has {
                continue;
            }

            let gap = self.calculate_gap(i, our_pieces);

            match best {
                None => best = Some((i as u32, gap)),
                Some((_, best_gap)) if gap < best_gap => {
                    best = Some((i as u32, gap));
                }
                _ => {}
            }
        }

        best.map(|(idx, _)| idx)
    }

    /// Returns true if we should use gap-filling mode.
    ///
    /// Gap-filling is activated when completion > 50%.
    pub fn should_use_gap_filling(&self, our_pieces: &[bool]) -> bool {
        if our_pieces.is_empty() {
            return false;
        }

        let completed = our_pieces.iter().filter(|&&x| x).count();
        let ratio = completed as f64 / our_pieces.len() as f64;

        ratio > WEBSEED_GAP_FILL_THRESHOLD
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_construct_single_file_url_with_trailing_slash() {
        let client = WebSeedClient::new(vec!["http://example.com/".to_string()]).unwrap();
        let url = client.construct_single_file_url("http://example.com/", "file.txt");
        assert_eq!(url, "http://example.com/file.txt");
    }

    #[test]
    fn test_construct_single_file_url_without_trailing_slash() {
        let client = WebSeedClient::new(vec!["http://example.com/file.txt".to_string()]).unwrap();
        let url = client.construct_single_file_url("http://example.com/file.txt", "file.txt");
        assert_eq!(url, "http://example.com/file.txt");
    }

    #[test]
    fn test_construct_multi_file_url() {
        let client = WebSeedClient::new(vec!["http://example.com/".to_string()]).unwrap();
        let url = client.construct_multi_file_url("http://example.com/", "torrent", "dir/file.txt");
        assert_eq!(url, "http://example.com/torrent/dir/file.txt");
    }

    #[test]
    fn test_blacklist() {
        let mut client = WebSeedClient::new(vec![
            "http://a.com/".to_string(),
            "http://b.com/".to_string(),
        ])
        .unwrap();

        assert_eq!(client.available_url_count(), 2);

        client.blacklist_url("http://a.com/");
        assert_eq!(client.available_url_count(), 1);

        client.clear_blacklist();
        assert_eq!(client.available_url_count(), 2);
    }

    #[test]
    fn test_gap_calculation() {
        let selector = GapAwarePieceSelector::new(10);

        // Gap should be distance to nearest completed piece
        let pieces = vec![true, false, false, false, true];
        assert_eq!(selector.calculate_gap(1, &pieces), 1); // 1 away from piece 0
        assert_eq!(selector.calculate_gap(2, &pieces), 2); // 2 away from piece 0 or 4
        assert_eq!(selector.calculate_gap(3, &pieces), 1); // 1 away from piece 4
    }

    #[test]
    fn test_gap_filling_threshold() {
        let selector = GapAwarePieceSelector::new(10);

        // Less than 50% complete
        let pieces = vec![true, false, false, false];
        assert!(!selector.should_use_gap_filling(&pieces));

        // More than 50% complete
        let pieces = vec![true, true, true, false];
        assert!(selector.should_use_gap_filling(&pieces));
    }

    #[test]
    fn test_x_value() {
        let selector = GapAwarePieceSelector::new(16);
        assert!((selector.x_value() - 3.0).abs() < 0.001); // sqrt(16) - 1 = 3
    }
}
