//! Bandwidth limiting using token bucket algorithm.
//!
//! This module provides rate limiting for download and upload traffic using
//! a token bucket algorithm with burst support.
//!
//! # Example
//!
//! ```
//! use rbit::BandwidthLimiter;
//!
//! # async fn example() {
//! // Create a limiter with 1MB/s download and 500KB/s upload
//! let mut limiter = BandwidthLimiter::new(1_000_000, 500_000);
//!
//! // Acquire bandwidth before sending/receiving data
//! limiter.acquire_download(16384).await;
//! limiter.acquire_upload(16384).await;
//!
//! // Create unlimited limiter
//! let unlimited = BandwidthLimiter::unlimited();
//! # }
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Semaphore;

/// A token bucket rate limiter.
///
/// Implements the token bucket algorithm for rate limiting. Tokens are added
/// at a fixed rate, and operations consume tokens. If not enough tokens are
/// available, the operation waits.
pub struct RateLimiter {
    tokens: Mutex<TokenBucket>,
    semaphore: Semaphore,
}

struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    tokens_per_sec: f64,
    last_update: Instant,
}

impl RateLimiter {
    /// Creates a new rate limiter with the specified bytes per second limit.
    ///
    /// The bucket size (max tokens) is set to 2x the rate to allow for bursts.
    pub fn new(bytes_per_sec: u64) -> Arc<Self> {
        let max_tokens = (bytes_per_sec * 2) as f64;
        Arc::new(Self {
            tokens: Mutex::new(TokenBucket {
                tokens: max_tokens,
                max_tokens,
                tokens_per_sec: bytes_per_sec as f64,
                last_update: Instant::now(),
            }),
            semaphore: Semaphore::new(1),
        })
    }

    /// Creates an unlimited rate limiter that never blocks.
    pub fn unlimited() -> Arc<Self> {
        Arc::new(Self {
            tokens: Mutex::new(TokenBucket {
                tokens: f64::MAX,
                max_tokens: f64::MAX,
                tokens_per_sec: f64::MAX,
                last_update: Instant::now(),
            }),
            semaphore: Semaphore::new(1),
        })
    }

    /// Updates the rate limit.
    pub fn set_rate(&self, bytes_per_sec: u64) {
        let mut bucket = self.tokens.lock();
        bucket.tokens_per_sec = bytes_per_sec as f64;
        bucket.max_tokens = (bytes_per_sec * 2) as f64;
        bucket.tokens = bucket.tokens.min(bucket.max_tokens);
    }

    /// Acquires the specified number of bytes from the bucket.
    ///
    /// Returns the duration to wait if not enough tokens are available.
    /// The caller should sleep for this duration before proceeding.
    pub async fn acquire(&self, bytes: usize) -> Duration {
        let _permit = self.semaphore.acquire().await.unwrap();

        let mut bucket = self.tokens.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_update).as_secs_f64();
        bucket.last_update = now;

        bucket.tokens = (bucket.tokens + elapsed * bucket.tokens_per_sec).min(bucket.max_tokens);

        let bytes_f = bytes as f64;
        if bucket.tokens >= bytes_f {
            bucket.tokens -= bytes_f;
            Duration::ZERO
        } else {
            let needed = bytes_f - bucket.tokens;
            let wait_secs = needed / bucket.tokens_per_sec;
            bucket.tokens = 0.0;
            Duration::from_secs_f64(wait_secs)
        }
    }

    /// Returns the currently available tokens (bytes).
    pub fn available(&self) -> usize {
        let bucket = self.tokens.lock();
        bucket.tokens as usize
    }
}

/// A combined download and upload bandwidth limiter.
///
/// Manages separate rate limiters for download and upload traffic.
///
/// # Example
///
/// ```
/// use rbit::BandwidthLimiter;
///
/// # async fn example() {
/// // 1MB/s download, 500KB/s upload
/// let limiter = BandwidthLimiter::new(1_000_000, 500_000);
///
/// // Before downloading data
/// limiter.acquire_download(16384).await;
///
/// // Before uploading data
/// limiter.acquire_upload(16384).await;
/// # }
/// ```
pub struct BandwidthLimiter {
    download: Arc<RateLimiter>,
    upload: Arc<RateLimiter>,
}

impl BandwidthLimiter {
    /// Creates a new bandwidth limiter with the specified limits.
    ///
    /// A limit of 0 means unlimited.
    pub fn new(download_limit: u64, upload_limit: u64) -> Self {
        Self {
            download: if download_limit == 0 {
                RateLimiter::unlimited()
            } else {
                RateLimiter::new(download_limit)
            },
            upload: if upload_limit == 0 {
                RateLimiter::unlimited()
            } else {
                RateLimiter::new(upload_limit)
            },
        }
    }

    /// Creates an unlimited bandwidth limiter.
    pub fn unlimited() -> Self {
        Self {
            download: RateLimiter::unlimited(),
            upload: RateLimiter::unlimited(),
        }
    }

    /// Sets the download rate limit. A limit of 0 means unlimited.
    pub fn set_download_limit(&mut self, bytes_per_sec: u64) {
        if bytes_per_sec == 0 {
            self.download = RateLimiter::unlimited();
        } else {
            self.download = RateLimiter::new(bytes_per_sec);
        }
    }

    /// Sets the upload rate limit. A limit of 0 means unlimited.
    pub fn set_upload_limit(&mut self, bytes_per_sec: u64) {
        if bytes_per_sec == 0 {
            self.upload = RateLimiter::unlimited();
        } else {
            self.upload = RateLimiter::new(bytes_per_sec);
        }
    }

    /// Acquires download bandwidth for the specified number of bytes.
    ///
    /// Blocks until the bandwidth is available.
    pub async fn acquire_download(&self, bytes: usize) {
        let wait = self.download.acquire(bytes).await;
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// Acquires upload bandwidth for the specified number of bytes.
    ///
    /// Blocks until the bandwidth is available.
    pub async fn acquire_upload(&self, bytes: usize) {
        let wait = self.upload.acquire(bytes).await;
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// Returns the download rate limiter.
    pub fn download_limiter(&self) -> Arc<RateLimiter> {
        self.download.clone()
    }

    /// Returns the upload rate limiter.
    pub fn upload_limiter(&self) -> Arc<RateLimiter> {
        self.upload.clone()
    }
}

impl Default for BandwidthLimiter {
    fn default() -> Self {
        Self::unlimited()
    }
}
