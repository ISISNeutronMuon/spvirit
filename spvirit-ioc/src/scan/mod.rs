//! Scan infrastructure: periodic/event/IO-intr scan lists and their driver.
pub mod async_reg;
/// A `#[cfg(test)]` two-phase device used to exercise the async completion
/// path end-to-end on a `ManualClock`. Reached by the `process` tests as
/// `crate::scan::delay::DelaySupport`.
#[cfg(test)]
pub mod delay;
pub mod list;
pub mod menu;
pub mod scanner;
pub use async_reg::AsyncRegistry;
pub use list::ScanList;
pub use menu::{parse_scan, MAX_SCAN_PERIOD_SECS, MIN_SCAN_PERIOD_SECS, ScanParseError, ScanSpec};
pub use scanner::{ProcSink, Scanner};
