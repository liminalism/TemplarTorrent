//! One session arbiter per process. It owns every global cap — runnable jobs,
//! connections, payload memory and per-direction rate buckets — so no peer or
//! job can exceed its share regardless of what remotes do.
use std::collections::HashMap;

use templartorrent_core::{
    bandwidth::{SharedBucket, TokenBucket},
    budget::ByteBudget,
    model::{JobState, Limits, TorrentId, admitted_jobs},
};

use crate::EngineError;

#[derive(Debug)]
pub struct SessionArbiter {
    limits: Limits,
    connections: HashMap<TorrentId, usize>,
    total_connections: usize,
    budget: ByteBudget,
    down: SharedBucket,
    up: TokenBucket,
}

impl SessionArbiter {
    pub fn new(limits: Limits, now_ns: u64) -> Result<Self, EngineError> {
        let down =
            TokenBucket::new(limits.download_kb_s, now_ns).ok_or(EngineError::LimitOverflow)?;
        let up = TokenBucket::new(limits.upload_kb_s, now_ns).ok_or(EngineError::LimitOverflow)?;
        Ok(Self {
            budget: ByteBudget::new(limits.payload_budget_bytes),
            limits,
            connections: HashMap::new(),
            total_connections: 0,
            down: SharedBucket::new(down),
            up,
        })
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn set_limits(&mut self, limits: Limits, now_ns: u64) -> Result<(), EngineError> {
        // Preflight both directions before mutating either bucket.
        TokenBucket::new(limits.download_kb_s, now_ns).ok_or(EngineError::LimitOverflow)?;
        TokenBucket::new(limits.upload_kb_s, now_ns).ok_or(EngineError::LimitOverflow)?;
        self.down
            .set_limit(limits.download_kb_s, now_ns)
            .ok_or(EngineError::LimitOverflow)?;
        self.up
            .set_limit(limits.upload_kb_s, now_ns)
            .ok_or(EngineError::LimitOverflow)?;
        self.budget.set_limit(limits.payload_budget_bytes);
        self.limits = limits;
        Ok(())
    }

    /// Stable-order runnable set; see core `admitted_jobs` for the policy.
    pub fn admit(&self, jobs: &[(TorrentId, JobState)]) -> Vec<TorrentId> {
        admitted_jobs(jobs, &self.limits)
    }

    /// Open one connection for a job if both the global and per-job caps allow.
    pub fn try_open(&mut self, job: TorrentId) -> bool {
        if self.total_connections >= self.limits.connections_total {
            return false;
        }
        let used = self.connections.get(&job).copied().unwrap_or(0);
        if used >= self.limits.connections_per_job {
            return false;
        }
        self.connections.insert(job, used + 1);
        self.total_connections += 1;
        true
    }

    pub fn close(&mut self, job: TorrentId) {
        if let Some(used) = self.connections.get_mut(&job) {
            if *used > 0 {
                *used -= 1;
                self.total_connections = self.total_connections.saturating_sub(1);
            }
            if *used == 0 {
                self.connections.remove(&job);
            }
        }
    }

    pub fn connections_for(&self, job: TorrentId) -> usize {
        self.connections.get(&job).copied().unwrap_or(0)
    }

    pub fn total_connections(&self) -> usize {
        self.total_connections
    }

    pub fn budget(&self) -> &ByteBudget {
        &self.budget
    }

    /// The session's download bucket as a handle. Spenders outside the
    /// session (HTTP file downloads) reserve from it directly, so one limit
    /// covers peers and URL connections.
    pub fn download_rate(&self) -> SharedBucket {
        self.down.clone()
    }

    pub fn budget_used(&self) -> usize {
        self.budget.used()
    }

    pub fn budget_high_watermark(&self) -> usize {
        self.budget.high_watermark()
    }

    /// Reserve download bandwidth for an outgoing block request batch.
    pub fn reserve_down(&mut self, bytes: u64, now_ns: u64) -> bool {
        self.down.reserve(bytes, now_ns)
    }

    /// Reserve upload bandwidth for an outgoing bounded payload write.
    pub fn reserve_up(&mut self, bytes: u64, now_ns: u64) -> bool {
        self.up.reserve(bytes, now_ns)
    }

    pub fn down_wait_ns(&mut self, bytes: u64, now_ns: u64) -> Option<u64> {
        self.down.wait_ns(bytes, now_ns)
    }
}
