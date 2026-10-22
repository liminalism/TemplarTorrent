//! Headless session engine: lease coordination, global arbitration, peer actors,
//! bounded disk/hash workers. Dependency-free and deterministic; transports and
//! storage arrive behind injected traits. See docs/INTEGRATION.md B-C.
#![forbid(unsafe_code)]

pub mod arbiter;
pub mod event;
pub mod lease;
pub mod peer;
pub mod session;
pub mod worker;

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    UnknownJob,
    UnknownPeer,
    QueueFull,
    NoCapacity,
    BudgetExhausted,
    LimitOverflow,
    TooManyJobs,
    Unsupported(&'static str),
    Sequence(templartorrent_core::sequence::SequenceError),
    Storage(crate::worker::SinkError),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sequence(inner) => write!(f, "sequence: {inner}"),
            Self::Storage(inner) => write!(f, "storage: {inner:?}"),
            Self::Unsupported(what) => write!(f, "unsupported in this engine stage: {what}"),
            other => write!(f, "{other:?}"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<templartorrent_core::sequence::SequenceError> for EngineError {
    fn from(value: templartorrent_core::sequence::SequenceError) -> Self {
        Self::Sequence(value)
    }
}
