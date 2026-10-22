use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug)]
struct Inner {
    limit: AtomicUsize,
    used: AtomicUsize,
    high: AtomicUsize,
}
#[derive(Debug, Clone)]
pub struct ByteBudget(Arc<Inner>);
/// A linear reservation. Moving a buffer to a disk worker must move its permit too.
/// Do not release this while an independently cloned Bytes still owns the allocation.
#[derive(Debug)]
pub struct Permit {
    owner: Arc<Inner>,
    bytes: usize,
}
impl ByteBudget {
    pub fn new(limit: usize) -> Self {
        Self(Arc::new(Inner {
            limit: AtomicUsize::new(limit),
            used: AtomicUsize::new(0),
            high: AtomicUsize::new(0),
        }))
    }
    pub fn try_acquire(&self, bytes: usize) -> Option<Permit> {
        let old = self
            .0
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|&sum| sum <= self.0.limit.load(Ordering::Acquire))
            })
            .ok()?;
        self.0.high.fetch_max(old + bytes, Ordering::Relaxed);
        Some(Permit {
            owner: Arc::clone(&self.0),
            bytes,
        })
    }
    pub fn used(&self) -> usize {
        self.0.used.load(Ordering::Acquire)
    }
    pub fn high_watermark(&self) -> usize {
        self.0.high.load(Ordering::Relaxed)
    }
    pub fn limit(&self) -> usize {
        self.0.limit.load(Ordering::Acquire)
    }
    /// Existing owners remain charged after a reduction. New admission waits
    /// until those owners drain below the new cap; changing settings cannot
    /// mint a fresh pool of permits.
    pub fn set_limit(&mut self, limit: usize) {
        self.0.limit.store(limit, Ordering::Release);
    }
}
impl Permit {
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.owner.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
