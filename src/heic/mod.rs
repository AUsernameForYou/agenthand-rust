//! From-scratch HEIC/HEIF decoding (rust-100 build; see
//! `~/workspace/meta-hackathon/rust-100/` for the milestone docs,
//! SPEC-SUBSET.md scope contract, and PARITY-GATES.md).
//!
//! No libheif, libde265, ffmpeg, or any existing HEVC/HEIF decode
//! code in this path — reference tools are test-oracle duty only.
//!
//! M1: the HEIF container layer (this module). CABAC and
//! reconstruction land in M2/M3; grid/tile reconstruction is the
//! M3.5 junction. Grid was confirmed as the real corpus's primary
//! structure at the M1 hour-0 check (2026-10-28) and Daniel
//! approved the grid scope widening the same day (decision 52), so
//! grid composition (the `grid` module: canvas assembly from
//! decoded tile pictures) is in scope and implemented; what M3.5
//! still owes is the bit-exact tile decode feeding it.

pub mod container;
pub mod grid;
