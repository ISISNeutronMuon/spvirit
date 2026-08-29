//! The scan driver: owns every scan list and processes records against the
//! record engine, never holding a scan-list lock across a process() call.

use std::sync::Arc;

use crate::clock::Clock;
use crate::ctx::ProcCtx;
use crate::lockset::{RecordDb, RecordId};
use crate::process::process;
use spvirit_types::NtPayload;

/// Where a completed pass's side effects go. The real impl (Task 15) forwards
/// into the server's subscribe fan-out, exactly as the put path does.
pub trait ProcSink: Send + Sync {
    fn flush(&self, events: Vec<(String, NtPayload)>, trace: Vec<String>);
}

pub struct Scanner {
    db: Arc<RecordDb>,
    clock: Arc<dyn Clock>,
    sink: Arc<dyn ProcSink>,
    // Scan lists are added in later tasks; the Mutexes they live behind all
    // obey the lock-ordering rule enforced by process_ids.
}

impl Scanner {
    pub fn new(db: Arc<RecordDb>, clock: Arc<dyn Clock>, sink: Arc<dyn ProcSink>) -> Self {
        Self { db, clock, sink }
    }

    /// Process a snapshot of record ids, in the given order. The caller has
    /// already released any scan-list lock. Each record is processed under
    /// its own lock set; a panic or ProcError on one record is contained and
    /// the batch continues.
    pub fn process_ids(&self, ids: &[RecordId]) {
        for &id in ids {
            let clock = self.clock.clone();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut ctx = ProcCtx::with_clock(clock.as_ref());
                let res = self.db.with_set(id.set, |set| process(set, id, &mut ctx));
                (
                    res,
                    ctx.take_events(),
                    std::mem::take(&mut ctx.trace),
                    std::mem::take(&mut ctx.deferred),
                )
            }));
            match outcome {
                Ok((res, events, trace, deferred)) => {
                    if let Err(e) = res {
                        tracing::warn!("scan processing error: {e}");
                    }
                    self.sink.flush(events, trace);
                    // Cross-lock-set requests A collects but cannot run:
                    // process them now, on this thread, after the lock drop.
                    if !deferred.is_empty() {
                        self.process_ids(&deferred);
                    }
                }
                Err(_) => {
                    tracing::warn!("scan processing panicked; continuing the pass");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::lockset::RecordDb;
    use crate::test_support::db as build_db;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingSink {
        posted: Mutex<Vec<String>>,
    }
    impl ProcSink for RecordingSink {
        fn flush(&self, events: Vec<(String, spvirit_types::NtPayload)>, _trace: Vec<String>) {
            let mut p = self.posted.lock().unwrap();
            for (name, _) in events {
                p.push(name);
            }
        }
    }

    fn scanner_over(src: &str) -> (Scanner, Arc<RecordingSink>, Arc<RecordDb>) {
        let db = Arc::new(build_db(src));
        let clock: Arc<dyn crate::clock::Clock> = Arc::new(ManualClock::new());
        let sink = Arc::new(RecordingSink::default());
        (Scanner::new(db.clone(), clock, sink.clone()), sink, db)
    }

    #[test]
    fn process_ids_processes_each_record_and_flushes_monitors() {
        let (scanner, sink, db) = scanner_over(
            "record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n\
             record(ai, \"PV:B\") {\n field(INP, \"2\")\n}\n",
        );
        let a = db.lookup("PV:A").unwrap();
        let b = db.lookup("PV:B").unwrap();
        scanner.process_ids(&[a, b]);
        assert_eq!(
            *sink.posted.lock().unwrap(),
            vec!["PV:A".to_string(), "PV:B".to_string()]
        );
    }

    // Panic-isolation test (Step 5): deliberately NOT added.
    //
    // The brief asks for a test proving a record whose `process()` panics
    // does not stop the following record from processing and flushing. There
    // is no reachable path in the current engine that makes `process()`
    // itself panic from `.db` configuration alone:
    //   - A self-referential FLNK/PP cycle does not panic or even error —
    //     `process_inner`'s PACT brake (see `process.rs`) returns `Ok(())`
    //     immediately on re-entry.
    //   - The one real failure path, `ProcError::TooDeep`, is only reachable
    //     today by manually calling `ctx.push_depth` 64 times through the
    //     `ProcCtx` API directly (see
    //     `process::tests::the_depth_cap_reports_the_record_rather_than_overflowing_the_stack`);
    //     `process_ids` builds its own private `ProcCtx` per record, so a
    //     test outside this module has no way to pre-load its depth counter
    //     to trigger that path through the public `Scanner` API.
    //   - Fabricating a panic some other way (e.g. inside the `ProcSink`, or
    //     via an unsafe/contrived hook) would not exercise the
    //     `catch_unwind` boundary around `process()` — it would test a
    //     different code path and give false confidence rather than real
    //     coverage.
    //
    // Per the task brief's ambiguity resolution, this specific assertion is
    // deferred to Task 7's thread-level panic test, once that task's real
    // fault-injection seam (or a genuinely panicking record body) exists.
    // What Step 3/4's `process_ids_processes_each_record_and_flushes_monitors`
    // test above already covers is the *shape* of the isolation contract:
    // `process_ids` iterates every id independently and flushes each one's
    // events regardless of the others' outcome.
}
