//! Source fingerprinting: identify the process producing a channel's values
//! from passively observed PVAccess traffic alone.
//!
//! See `docs/superpowers/specs/2026-08-21-source-fingerprinting.md`.

pub mod cadence;
pub mod circular;
pub mod cluster;
pub mod detect;
pub mod engine;
pub mod export;
pub mod lattice;
pub mod nt_probe;
pub mod options;
pub mod running;
pub mod similarity;
pub mod types;
