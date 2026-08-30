//! PVAccess client library — search, connect, get, put, monitor.
//!
//! This crate provides both a **high-level API** ([`PvaClient`]) and the
//! lower-level protocol functions used to build it.
//!
//! # High-level API
//!
//! ```rust,ignore
//! use spvirit_client::PvaClient;
//! use std::ops::ControlFlow;
//!
//! let client = PvaClient::builder().build();
//!
//! // GET
//! let result = client.pvget("MY:PV").await?;
//!
//! // PUT
//! client.pvput("MY:PV", 42.0).await?;
//!
//! // MONITOR
//! client.pvmonitor("MY:PV", |val| {
//!     println!("{val:?}");
//!     ControlFlow::Continue(())
//! }).await?;
//!
//! // INFO (introspection)
//! let desc = client.pvinfo("MY:PV").await?;
//! ```
//!
//! # Key types
//!
//! - [`PvaClient`] — high-level client with `pvget`, `pvput`, `pvmonitor`, `pvinfo`
//! - [`PvOptions`] — configuration for PV operations (ports, timeout, auth)
//! - [`PvGetResult`] — decoded GET result with introspection
//! - [`ChannelConn`] — low-level established TCP channel
//! - [`SearchTarget`] — UDP/TCP search target address

pub mod auth;
pub mod byte_sink;
pub mod client;
pub mod format;
pub mod put_encode;
pub mod pva_client;
pub mod pvlist;
pub mod search;
pub mod transport;
pub mod types;

// --- Re-exports: upstream byte accounting ---
pub use byte_sink::ByteSink;

// --- Re-exports: high-level API ---
pub use pva_client::{
    MonitorOptions, PvaChannel, PvaClient, PvaClientBuilder, client_from_opts, pvinfo, pvmonitor,
    pvmonitor_fields, pvput, pvput_fields,
};
pub use pvlist::PvListSource;

// --- Re-exports: client core ---
pub use client::{ChannelConn, build_client_validation, establish_channel, pvget, pvget_fields};
pub use format::{OutputFormat, RenderOptions, format_output};
pub use search::{SearchTarget, build_auto_broadcast_targets, resolve_pv_server, search_pv};
pub use transport::{read_frame, read_packet, read_until};

// --- Re-exports: segmentation (so callers need not depend on spvirit-codec) ---
pub use spvirit_codec::{DecodeError, SegmentOutcome, SegmentReassembler};

// --- Re-exports: monitor updates (callbacks take a &MonitorUpdate) ---
pub use spvirit_codec::MonitorUpdate;
pub use types::{PvGetError, PvGetOptions, PvGetResult, PvMonitorEvent, PvOptions};
