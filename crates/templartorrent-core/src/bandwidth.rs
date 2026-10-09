//! Global payload token bucket. Uses caller-supplied monotonic nanoseconds for reproducibility.
//! Apply once per direction per session, not a separate copy per peer.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    bytes_per_second: u64,
    capacity: u64,
    tokens: u64,
    remainder: u128,
    last_ns: u64,
}
impl TokenBucket {
    /// kB/s is decimal: 1 kB = 1,000 bytes. 0 means unlimited, never "pause".
    pub fn new(kb_per_second: u64, now_ns: u64) -> Option<Self> {
        let rate = kb_per_second.checked_mul(1000)?;
        let capacity = (rate / 4).max(16 * 1024);
        Some(Self {
            bytes_per_second: rate,
            capacity,
            tokens: capacity,
            remainder: 0,
            last_ns: now_ns,
        })
    }
    pub fn rate(&self) -> u64 {
        self.bytes_per_second
    }
    pub fn capacity(&self) -> u64 {
        self.capacity
    }
    fn refill(&mut self, now_ns: u64) {
        let elapsed = now_ns.saturating_sub(self.last_ns);
        self.last_ns = self.last_ns.max(now_ns);
        let credit = u128::from(elapsed) * u128::from(self.bytes_per_second) + self.remainder;
        let whole = credit / 1_000_000_000;
        self.remainder = credit % 1_000_000_000;
        self.tokens = (u128::from(self.tokens) + whole).min(u128::from(self.capacity)) as u64;
        if self.tokens == self.capacity {
            self.remainder = 0;
        }
    }
    /// Reserve a block before a download request, or a bounded upload write before sending.
    /// Caller must split batches larger than capacity. Failed reservations consume nothing.
    pub fn reserve(&mut self, bytes: u64, now_ns: u64) -> bool {
        self.refill(now_ns);
        if self.bytes_per_second == 0 {
            return true;
        }
        if bytes > self.tokens {
            return false;
        }
        self.tokens -= bytes;
        true
    }
    /// None: reservation exceeds capacity; otherwise monotonic wait in ns.
    pub fn wait_ns(&mut self, bytes: u64, now_ns: u64) -> Option<u64> {
        self.refill(now_ns);
        if self.bytes_per_second == 0 || bytes <= self.tokens {
            return Some(0);
        }
        if bytes > self.capacity {
            return None;
        }
        let required = u128::from(bytes - self.tokens) * 1_000_000_000;
        let net = required.saturating_sub(self.remainder);
        u64::try_from(net.div_ceil(u128::from(self.bytes_per_second))).ok()
    }
    /// Limit changes do not refill the bucket, avoiding repeated-settings burst exploits.
    pub fn set_limit(&mut self, kb_per_second: u64, now_ns: u64) -> Option<()> {
        let rate = kb_per_second.checked_mul(1000)?;
        self.refill(now_ns);
        self.bytes_per_second = rate;
        self.capacity = (rate / 4).max(16 * 1024);
        self.tokens = self.tokens.min(self.capacity);
        self.remainder = 0;
        Some(())
    }
}
