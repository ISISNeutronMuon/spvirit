//! `spvirit-ioc` — a synchronous EPICS record-processing engine.
//!
//! Loads a `.db` file, partitions its records into lock sets joined by
//! `INP`/`OUT`/`DOL`/`FLNK`/`SDIS` links, and processes them with EPICS
//! `dbProcess` semantics: input links pull, forward links push, alarms
//! accumulate over a pass and commit at its end. [`IocSource`] exposes the
//! result as a `spvirit_server::pvstore::Source`, so it plugs into
//! `PvaServerBuilder::source()` like any other source.
//!
//! Sub-project A — this crate's current scope — covers six record types
//! (`ai`, `ao`, `bi`, `bo`, `longin`, `longout`) with no scanning, no
//! channel-access links, and no async device support; see
//! [`graph::DependencyGraph`]'s load-time diagnostics and the
//! `docs/book/src/06-dev-guide/09-processing-engine.md` chapter for what
//! that means in practice and what sub-projects B, C and D still owe.

pub mod alarm;
pub mod build;
pub mod clock;
pub mod ctx;
pub mod fields;
pub mod graph;
pub mod lockset;
pub mod model;
pub mod process;
pub mod scan;
pub mod source;
pub mod spec;
#[cfg(test)]
mod test_support;

pub use source::IocSource;
pub use spec::{RecordSpec, SpecError};
