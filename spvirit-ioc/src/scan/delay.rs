//! A `#[cfg(test)]` two-phase device: initiate-then-defer on a `Clock`.
//!
//! [`DelaySupport`] is the minimal [`AsyncSupport`] implementation the async
//! processing path needs to be exercised end-to-end without a real hardware
//! device or any wall-clock sleep. It mirrors an EPICS device whose read takes
//! longer than a processing pass:
//!
//! - The **initiating** pass (`pact == false`, entered through `process`)
//!   records when the operation becomes due — `clock.now() + delay` — and
//!   reports [`AsyncOutcome::Pending`], so `record_body` leaves PACT set and
//!   returns without running the synchronous second half.
//! - The **completion** pass (`pact == true`, entered through
//!   [`complete_async`](crate::process::complete_async)) collects the finished
//!   operation and reports [`AsyncOutcome::Complete`], so `record_body` falls
//!   through to the ordinary body (value/limits/monitors/FLNK) and clears PACT.
//!
//! A test drives completion deterministically: advance the `ManualClock` past
//! the recorded [`deadline`](DelaySupport::deadline), then call
//! `complete_async` (or `Scanner::scan_once`). No real sleep, no timing
//! tolerance — the clock is the only source of time.
//!
//! This lives in a real module (not the `process` test module) so several test
//! modules can bind it, per ruling R-T14-MODREG.

use crate::clock::Clock;
use crate::ctx::ProcCtx;
use crate::process::{AsyncOutcome, AsyncSupport};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A two-phase device that reports its operation outstanding for `delay` of
/// simulated time before completing.
pub struct DelaySupport {
    /// The single source of time — shared with the processing `ProcCtx` so the
    /// recorded deadline is on the same timeline the test advances.
    clock: Arc<dyn Clock>,
    /// How long after initiation the operation is due.
    delay: Duration,
    /// Set once the completion pass has collected the operation.
    completed: AtomicBool,
    /// The instant the operation became due, recorded on the initiating pass.
    /// `None` until then.
    deadline: Mutex<Option<Instant>>,
}

impl DelaySupport {
    /// A device that stays outstanding for `delay` of `clock` time.
    pub fn new(clock: Arc<dyn Clock>, delay: Duration) -> DelaySupport {
        DelaySupport {
            clock,
            delay,
            completed: AtomicBool::new(false),
            deadline: Mutex::new(None),
        }
    }

    /// The instant the outstanding operation became due, or `None` before the
    /// initiating pass has run. A test compares the `ManualClock` against this
    /// to decide when to drive completion.
    pub fn deadline(&self) -> Option<Instant> {
        *self.deadline.lock().expect("deadline lock poisoned")
    }

    /// Whether the completion pass has collected the operation yet.
    pub fn is_completed(&self) -> bool {
        self.completed.load(Ordering::SeqCst)
    }
}

impl AsyncSupport for DelaySupport {
    fn start(&self, _record: &str, pact: bool, _ctx: &mut ProcCtx) -> AsyncOutcome {
        if pact {
            // Completion pass: the outstanding operation is collected here, so
            // `record_body` runs its synchronous second half and clears PACT.
            self.completed.store(true, Ordering::SeqCst);
            AsyncOutcome::Complete
        } else {
            // Initiating pass: record when the operation becomes due and report
            // it outstanding, so PACT stays set for the completion path.
            let deadline = self.clock.now() + self.delay;
            *self.deadline.lock().expect("deadline lock poisoned") = Some(deadline);
            AsyncOutcome::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    #[test]
    fn initiate_records_the_deadline_and_reports_pending() {
        let clock = ManualClock::new();
        clock.advance(Duration::from_secs(1)); // a non-zero base
        let support_clock: Arc<dyn Clock> = Arc::new(clock.clone());
        let sup = DelaySupport::new(support_clock, Duration::from_millis(50));
        // Captured on the same (un-advanced) clock the support reads inside
        // `start`, so this is exactly `now() + delay`.
        let expected = clock.now() + Duration::from_millis(50);
        let mut ctx = ProcCtx::with_clock(&clock);

        let outcome = sup.start("PV:SLOW", false, &mut ctx);

        assert_eq!(
            outcome,
            AsyncOutcome::Pending,
            "the initiating pass leaves the operation outstanding"
        );
        assert_eq!(
            sup.deadline(),
            Some(expected),
            "the initiating pass records now() + delay"
        );
        assert!(
            !sup.is_completed(),
            "an outstanding operation is not yet completed"
        );
    }

    #[test]
    fn completion_pass_reports_complete_and_marks_completed() {
        let clock = ManualClock::new();
        let support_clock: Arc<dyn Clock> = Arc::new(clock.clone());
        let sup = DelaySupport::new(support_clock, Duration::from_millis(50));
        let mut ctx = ProcCtx::with_clock(&clock);

        let outcome = sup.start("PV:SLOW", true, &mut ctx);

        assert_eq!(
            outcome,
            AsyncOutcome::Complete,
            "the pact=true collect pass completes the operation"
        );
        assert!(
            sup.is_completed(),
            "the collect pass marks the operation completed"
        );
    }
}
