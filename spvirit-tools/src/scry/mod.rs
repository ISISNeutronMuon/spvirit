//! EPICS packet capture and PVA traffic analysis.
//!
//! This module provides tools for capturing, decoding, and monitoring
//! EPICS/PVAccess network traffic. It includes a TUI-based interactive
//! viewer and an InfluxDB/Loki collector for production monitoring.

pub mod common;
pub mod spvirit_state;
pub mod ui;
pub mod utils;
