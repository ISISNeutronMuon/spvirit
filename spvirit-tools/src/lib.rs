//! PVAccess client/server tools for EPICS.
//!
//! This crate provides a PVA client library (via `spvirit-client`) and server
//! library (`spvirit_server`), along with command-line tools: `pvget`,
//! `pvput`, `pvmonitor`, `pvlist`, `pvinfo`, `pvexplore`, `pvsine`,
//! `spvirit_server`, and `spvirit_get_compare`.
//!
//! Commonly used types are re-exported at the crate root for convenience.
//! The full module paths remain available for less common items.

pub mod spvirit_client;
pub mod spvirit_server;
#[cfg(feature = "tui")]
pub mod tui;

// --- Re-exports: client core ---
pub use spvirit_client::client::{build_client_validation, pvget};
pub use spvirit_client::format::{OutputFormat, RenderOptions, format_output};
pub use spvirit_client::search::{SearchTarget, build_auto_broadcast_targets, search_pv};
pub use spvirit_client::transport::read_packet;
pub use spvirit_client::types::{PvGetError, PvGetOptions, PvGetResult};
