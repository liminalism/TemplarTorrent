use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

#[allow(dead_code)]
pub const DEFAULT_CACHE_MEMORY: usize = 256 * 1024 * 1024;
pub const MAX_CACHE_MEMORY: usize = 1024 * 1024 * 1024;
pub const BLOCK_CACHE_RATIO: f32 = 0.6;
pub const PIECE_CACHE_RATIO: f32 = 0.3;

pub struct MemoryBudget {
    total_limit: usize,
    current_usage: AtomicUsize,
    block_cache_limit: usize,
    piece_cache_limit: usize,
    pressure_notify: Notify,
}

impl MemoryBudget {
    pub fn new(total_limit: usize) -> Arc<Self> {
        let limit = total_limit.min(MAX_CACHE_MEMORY);
        Arc::new(Self {
            total_limit: limit,
            current_usage: AtomicUsize::new(0),
            block_cache_limit: (limit as f32 * BLOCK_CACHE_RATIO) as usize,
            piece_cache_limit: (limit as f32 * PIECE_CACHE_RATIO) as usize,
            pressure_notify: Notify::new(),
        })
    }

    pub fn try_allocate(self: &Arc<Self>, bytes: usize) -> Option<MemoryPermit> {
        let mut current = self.current_usage.load(Ordering::Relaxed);
        loop {
            if current + bytes > self.total_limit {
                return None;
            }
            match self.current_usage.compare_exchange_weak(
                current,
                current + bytes,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(MemoryPermit {
                        budget: Arc::clone(self),
                        bytes,
                    })
                }
                Err(actual) => current = actual,
            }
        }
    }

    pub async fn allocate(self: &Arc<Self>, bytes: usize) -> MemoryPermit {
        loop {
            if let Some(permit) = self.try_allocate(bytes) {
                return permit;
            }
            self.pressure_notify.notify_waiters();
            tokio::task::yield_now().await;
        }
    }

    pub fn release(&self, bytes: usize) {
        self.current_usage.fetch_sub(bytes, Ordering::SeqCst);
    }

    pub fn current_usage(&self) -> usize {
        self.current_usage.load(Ordering::Relaxed)
    }

    pub fn total_limit(&self) -> usize {
        self.total_limit
    }

    pub fn block_cache_limit(&self) -> usize {
        self.block_cache_limit
    }

    pub fn piece_cache_limit(&self) -> usize {
        self.piece_cache_limit
    }

    pub fn is_under_pressure(&self) -> bool {
        let usage = self.current_usage.load(Ordering::Relaxed);
        usage > (self.total_limit as f32 * 0.9) as usize
    }

    pub async fn wait_for_pressure(&self) {
        self.pressure_notify.notified().await;
    }
}

pub struct MemoryPermit {
    budget: Arc<MemoryBudget>,
    bytes: usize,
}

impl MemoryPermit {
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn resize(&mut self, new_bytes: usize) {
        if new_bytes > self.bytes {
            let diff = new_bytes - self.bytes;
            self.budget.current_usage.fetch_add(diff, Ordering::SeqCst);
        } else {
            let diff = self.bytes - new_bytes;
            self.budget.current_usage.fetch_sub(diff, Ordering::SeqCst);
        }
        self.bytes = new_bytes;
    }
}

impl Drop for MemoryPermit {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}
