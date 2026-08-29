//! Scan infrastructure: periodic/event/IO-intr scan lists and their driver.
pub mod list;
pub mod menu;
pub use list::ScanList;
pub use menu::{parse_scan, ScanParseError, ScanSpec};
