//! Scan infrastructure: periodic/event/IO-intr scan lists and their driver.
pub mod menu;
pub use menu::{parse_scan, ScanParseError, ScanSpec};
