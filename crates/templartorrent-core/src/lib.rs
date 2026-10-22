//! Deterministic application policy, independent of networking and GUI.
//! This crate does not download files. See docs/IMPLEMENTATION.md for the session engine.
#![forbid(unsafe_code)]
pub mod bandwidth;
pub mod budget;
pub mod model;
pub mod paths;
pub mod picker;
pub mod sequence;
pub mod tracker;
