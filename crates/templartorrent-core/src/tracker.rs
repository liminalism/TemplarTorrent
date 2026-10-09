#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Privacy {
    Unknown,
    Public,
    Private,
}
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Discovery {
    OriginalTracker,
    AddedTracker,
    Dht,
    Pex,
    Local,
}

/// Unknown metadata never authorizes automatic trackers. Public bootstrap is an explicit
/// add-dialog choice; private=true learned later immediately revokes it.
pub fn discovery_allowed(
    privacy: Privacy,
    source: Discovery,
    public_magnet_bootstrap: bool,
) -> bool {
    match (privacy, source) {
        (_, Discovery::OriginalTracker) => true,
        (Privacy::Public, _) => true,
        (Privacy::Unknown, Discovery::Dht | Discovery::Pex) => public_magnet_bootstrap,
        _ => false,
    }
}

#[derive(Debug, Clone)]
pub struct TrackerClock {
    periodic_due: u64,
    manual_due: u64,
    failures: u8,
    in_flight: bool,
}
impl Default for TrackerClock {
    fn default() -> Self {
        Self::new()
    }
}
impl TrackerClock {
    pub fn new() -> Self {
        Self {
            periodic_due: 0,
            manual_due: 0,
            failures: 0,
            in_flight: false,
        }
    }
    /// Monotonic seconds. Calling again while an announce is running never duplicates it.
    pub fn begin(&mut self, now: u64, manual: bool) -> bool {
        let due = if manual {
            self.manual_due
        } else {
            self.periodic_due
        };
        if self.in_flight || now < due {
            return false;
        }
        self.in_flight = true;
        true
    }
    pub fn success(&mut self, now: u64, interval: u32, min_interval: Option<u32>) {
        let min = u64::from(min_interval.unwrap_or(0)).max(60);
        self.manual_due = now.saturating_add(min);
        self.periodic_due = now.saturating_add(u64::from(interval).max(min));
        self.in_flight = false;
        self.failures = 0;
    }
    /// Seeded jitter is supplied by the engine, allowing deterministic fixture replays.
    pub fn failure(&mut self, now: u64, jitter_seconds: u64) {
        let delay = (30u64 << self.failures.min(7)).min(3600);
        self.failures = self.failures.saturating_add(1);
        let next = now
            .saturating_add(delay)
            .saturating_add(jitter_seconds.min(delay / 10));
        self.periodic_due = next;
        self.manual_due = next;
        self.in_flight = false;
    }
    pub fn next_due(&self) -> u64 {
        self.periodic_due
    }
    pub fn manual_due(&self) -> u64 {
        self.manual_due
    }
}
