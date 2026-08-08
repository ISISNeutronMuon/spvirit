//! Side effects a processing pass accumulates but must not perform while
//! holding the lock.
//!
//! Posting a monitor calls into the server's subscription fan-out, which may
//! block; requesting a scan in another lock set would need a second lock and
//! risks a deadlock. EPICS Base has the same problem and solves it the same
//! way — `dbCaPutLink` defers. Here the outermost `process()` caller drops
//! the lock, then flushes the context.

use crate::lockset::RecordId;
use spvirit_types::NtPayload;

/// The recursion cap. A `.db` with a PP cycle that PACT somehow fails to
/// break must fail loudly rather than blow the stack.
pub const MAX_DEPTH: usize = 64;

/// A processing pass could not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcError {
    TooDeep { depth: usize, record: String },
}

impl std::fmt::Display for ProcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcError::TooDeep { depth, record } => write!(
                f,
                "processing '{record}' exceeded the recursion cap of {depth}; \
                 the link graph almost certainly contains a PP cycle"
            ),
        }
    }
}

impl std::error::Error for ProcError {}

/// Accumulated side effects for one top-level processing pass.
#[derive(Debug, Default)]
pub struct ProcCtx {
    /// Monitors to publish, in post order.
    events: Vec<(String, NtPayload)>,
    /// Records in *other* lock sets that must be processed after this pass.
    /// A is single-set, so nothing fills this yet; sub-project B drains it.
    pub deferred: Vec<RecordId>,
    /// TPRO trace lines, emitted after the lock is dropped.
    pub trace: Vec<String>,
    depth: usize,
}

impl ProcCtx {
    pub fn new() -> ProcCtx {
        ProcCtx::default()
    }

    /// Queue a monitor. Ordering is observable to clients, so this appends.
    pub fn post(&mut self, name: &str, payload: NtPayload) {
        self.events.push((name.to_string(), payload));
    }

    /// Request processing of a record in another lock set, after this pass.
    pub fn defer(&mut self, id: RecordId) {
        self.deferred.push(id);
    }

    pub fn trace_line(&mut self, line: String) {
        self.trace.push(line);
    }

    /// Take the queued monitors, leaving the context empty.
    pub fn take_events(&mut self) -> Vec<(String, NtPayload)> {
        std::mem::take(&mut self.events)
    }

    /// Enter one level of recursion, or fail if the cap is reached.
    ///
    /// A drop guard would need to hold `&mut ProcCtx` for its whole
    /// lifetime, which `process()` also needs, so the depth is tracked by
    /// an explicit pair instead. Every successful `push_depth` must be
    /// matched by a `pop_depth`, including on the error return paths.
    pub fn push_depth(&mut self, record: &str) -> Result<(), ProcError> {
        if self.depth >= MAX_DEPTH {
            return Err(ProcError::TooDeep {
                depth: MAX_DEPTH,
                record: record.to_string(),
            });
        }
        self.depth += 1;
        Ok(())
    }

    /// Leave one level of recursion.
    pub fn pop_depth(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    pub fn depth(&self) -> usize {
        self.depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_types::{NtPayload, NtScalar, ScalarValue};

    fn payload(v: f64) -> NtPayload {
        NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(v)))
    }

    #[test]
    fn posted_events_come_back_in_post_order() {
        let mut ctx = ProcCtx::new();
        ctx.post("PV:A", payload(1.0));
        ctx.post("PV:B", payload(2.0));
        let events = ctx.take_events();
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["PV:A", "PV:B"],
            "monitor order is observable, so it must be insertion order"
        );
    }

    #[test]
    fn take_events_empties_the_buffer() {
        let mut ctx = ProcCtx::new();
        ctx.post("PV:A", payload(1.0));
        assert_eq!(ctx.take_events().len(), 1);
        assert!(
            ctx.take_events().is_empty(),
            "a second flush must not replay"
        );
    }

    #[test]
    fn depth_is_restored_by_pop() {
        let mut ctx = ProcCtx::new();
        ctx.push_depth("PV:A").expect("depth 1 is fine");
        assert_eq!(ctx.depth(), 1);
        ctx.pop_depth();
        assert_eq!(ctx.depth(), 0);
    }

    #[test]
    fn exceeding_the_depth_cap_names_the_record() {
        let mut ctx = ProcCtx::new();
        for _ in 0..MAX_DEPTH {
            ctx.push_depth("PV:DEEP").expect("within the cap");
        }
        let err = ctx
            .push_depth("PV:DEEP")
            .expect_err("one past the cap must fail");
        assert_eq!(
            err,
            ProcError::TooDeep {
                depth: MAX_DEPTH,
                record: "PV:DEEP".to_string()
            }
        );
    }
}
