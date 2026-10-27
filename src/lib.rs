//! agenthand — Rust port (parallel experiment).
//!
//! The Python build in `~/workspace/agenthand` is the shipped
//! product; this crate is a port under evaluation. It earns parity
//! only through the replay-package gate (see `parity` CLI / tests).

pub mod assets;
pub mod billing;
pub mod composite;
pub mod config;
pub mod image;
pub mod ingest;
pub mod ledger;
pub mod meta;
pub mod pricing;
pub mod resample;
pub mod sam;
pub mod segmentation;
pub mod server;

/// Python's `round(x, 6)`: round-half-even at the 6th decimal.
pub fn round6(x: f64) -> f64 {
    let s = x * 1e6;
    let f = s.floor();
    let diff = s - f;
    let r = if diff > 0.5 {
        f + 1.0
    } else if diff < 0.5 {
        f
    } else if (f as i64) % 2 == 0 {
        f
    } else {
        f + 1.0
    };
    r / 1e6
}
