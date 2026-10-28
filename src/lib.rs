//! agenthand — the Rust build of the agenthand MCP server.
//!
//! agenthand gives an AI agent three tools for helping a person sell a
//! physical item: `identify` (what is this, and how would you list it),
//! `cut_out` (the item alone, as a transparent PNG), and `draw` (the
//! item staged into a generated scene, composited deterministically so
//! the product pixels are never regenerated — the Fidelity Lock). A
//! Python package of the same name remains available as a fallback
//! install; this crate is the primary build, and it holds parity with
//! the Python reference through the frozen replay gate (see the
//! `parity` CLI and `tests/parity.rs`).
//!
//! Module map:
//! - `server` — the MCP stdio server and the three tool handlers.
//! - `meta` — the Meta Model API client (identify and draw contracts).
//! - `sam`, `segmentation` — SAM 3.1 mask decoding and mask quality
//!   control.
//! - `composite` — the Fidelity Lock compositor and receipt
//!   verification.
//! - `image`, `resample`, `ingest`, `assets` — image primitives,
//!   ingest limits, and the on-disk asset store behind `asset://`
//!   handles.
//! - `billing`, `ledger`, `sqlledger`, `pricing`, `config` — the
//!   spend cap and its two ledger stores (JSONL and SQLite, one
//!   accounting semantics).
//! - `measure` — the prototype scale-card dimension estimator (not a
//!   fourth tool).
//! - `heic` — the from-scratch HEIC decoder (in progress; see
//!   STATUS.md).

pub mod assets;
pub mod billing;
pub mod composite;
pub mod config;
pub mod heic;
pub mod image;
pub mod ingest;
pub mod ledger;
pub mod measure;
pub mod meta;
pub mod pricing;
pub mod resample;
pub mod roomrun;
pub mod sam;
pub mod segmentation;
pub mod server;
pub mod sqlledger;

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
