//! `spvirit-ioc` — a synchronous EPICS record-processing engine.

pub mod alarm;
pub mod build;
pub mod ctx;
pub mod graph;
pub mod lockset;
pub mod model;
pub mod process;
pub mod source;
#[cfg(test)]
mod test_support;

pub use source::IocSource;
