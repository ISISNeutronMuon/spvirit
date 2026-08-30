//! The scan driver: owns every scan list and processes records against the
//! record engine, never holding a scan-list lock across a process() call.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

/// Bound on the `scan_once` request queue (mirrors the event dispatcher's
/// `DISPATCH_QUEUE_CAPACITY`). At module scope so the drop-count test can
/// reference it as `super::SCAN_ONCE_CAPACITY`.
pub(crate) const SCAN_ONCE_CAPACITY: usize = 1024;

use crate::clock::Clock;
use crate::ctx::ProcCtx;
use crate::lockset::{RecordDb, RecordId};
use crate::process::process;
use crate::scan::ScanList;
use spvirit_server::events::EventSink;
use spvirit_types::NtPayload;

/// Where a completed pass's side effects go. The real impl (Task 15) forwards
/// into the server's subscribe fan-out, exactly as the put path does.
pub trait ProcSink: Send + Sync {
    fn flush(&self, events: Vec<(String, NtPayload)>, trace: Vec<String>);
}

/// A periodic scan list's key: the period in whole nanoseconds, so two
/// periodic scan rates that resolve to the same duration share one thread.
type PeriodKey = u64;

pub struct Scanner {
    db: Arc<RecordDb>,
    clock: Arc<dyn Clock>,
    sink: Arc<dyn ProcSink>,
    periodic: Mutex<HashMap<PeriodKey, ScanList>>,
    /// Event-driven scan lists, keyed by the record's `EVNT` string. Fired
    /// synchronously (on the caller's thread) by [`Self::fire_event`], never
    /// spawned onto a background thread the way periodic lists are.
    events: Mutex<HashMap<String, ScanList>>,
    stop: Arc<AtomicBool>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// Sender end of the bounded `scan_once` queue. `None` once `shutdown()`
    /// has closed it (dropping the sender is what makes the drain thread's
    /// blocked `recv()` return `Err` and exit).
    scan_once_tx: Mutex<Option<SyncSender<RecordId>>>,
    /// Receiver end, parked here until `start()` moves it into the drain
    /// thread. Kept alive (not dropped) until then so `try_send` on a full
    /// queue reports `Full`, not `Disconnected`.
    scan_once_rx: Mutex<Option<Receiver<RecordId>>>,
    /// Count of `scan_once` requests dropped because the queue was full.
    scan_once_dropped: AtomicU64,
    /// The `scan_once` drain/callback thread's handle. Kept out of `threads`
    /// (which stays the periodic-thread set several tests assert on), but
    /// joined by the SAME self-join-guarded loop in `shutdown()`.
    drain_thread: Mutex<Option<JoinHandle<()>>>,
}

impl Scanner {
    pub fn new(db: Arc<RecordDb>, clock: Arc<dyn Clock>, sink: Arc<dyn ProcSink>) -> Self {
        let (tx, rx) = sync_channel(SCAN_ONCE_CAPACITY);
        Self {
            db,
            clock,
            sink,
            periodic: Mutex::new(HashMap::new()),
            events: Mutex::new(HashMap::new()),
            stop: Arc::new(AtomicBool::new(false)),
            threads: Mutex::new(Vec::new()),
            scan_once_tx: Mutex::new(Some(tx)),
            scan_once_rx: Mutex::new(Some(rx)),
            scan_once_dropped: AtomicU64::new(0),
            drain_thread: Mutex::new(None),
        }
    }

    /// Enqueue a single-record process request onto the bounded `scan_once`
    /// queue. Non-blocking (uses `try_send`) and safe to call from inside a
    /// process pass: it only enqueues, never processes. When the queue is
    /// full the request is dropped, [`Self::scan_once_dropped`] is
    /// incremented, and a power-of-two rate-limited warning is emitted
    /// (mirroring the event dispatcher's overflow handling).
    pub fn scan_once(&self, id: RecordId) {
        let guard = self.scan_once_tx.lock().unwrap();
        // `None` after shutdown: the queue is closed, so silently drop --
        // there is no drain thread left to process the request.
        let Some(tx) = guard.as_ref() else { return };
        if tx.try_send(id).is_err() {
            let n = self.scan_once_dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::warn!(
                    "scan_once queue full; dropped scan request for {:?} ({} dropped so far)",
                    id,
                    n
                );
            }
        }
    }

    /// Number of `scan_once` requests dropped so far because the queue was
    /// full.
    pub fn scan_once_dropped(&self) -> u64 {
        self.scan_once_dropped.load(Ordering::Relaxed)
    }

    /// Sweep every record whose `PINI` field is true, in ascending `PHAS`
    /// (ties broken by `.db` definition order — `RecordDb::order()`'s
    /// index), and process each one via [`Self::process_ids`]. Must run to
    /// completion before any periodic thread is spawned (see `start`), so
    /// startup-time initialization records finish before periodic scanning
    /// can observe/depend on their output.
    ///
    /// Reuses `process_ids` (which flushes per record) rather than a
    /// batch-flush variant: the spec's "flushed once at the end" wording
    /// describes an equivalent, not a required, batching -- each posted
    /// `(name, payload)` is its own monitor update regardless of how the
    /// flushes are grouped, so per-record flushing here keeps lock
    /// ordering, per-record panic isolation, and deferred-recursion handling
    /// uniform with every other caller of `process_ids` (R-T8-FLUSH).
    ///
    /// This is a NEW, PHAS-ordered sweep distinct from the pre-existing
    /// `IocSource::process_pini` (`source.rs`), which sweeps in `.db`
    /// definition order only; that older path is left untouched (R-T8-DUP).
    pub fn process_pini(&self) {
        let mut sweep: Vec<(i32, usize, RecordId)> = self
            .db
            .order()
            .iter()
            .enumerate()
            .filter_map(|(order_index, &id)| {
                let (pini, phas) = self
                    .db
                    .with_set(id.set, |set| {
                        let common = &set.get(id).common;
                        (common.pini, common.phas)
                    });
                pini.then_some((phas, order_index, id))
            })
            .collect();
        sweep.sort_by_key(|&(phas, order_index, _)| (phas, order_index));
        let ids: Vec<RecordId> = sweep.into_iter().map(|(_, _, id)| id).collect();
        self.process_ids(&ids);
    }

    /// Add `id` (with scan priority `phas`) to the periodic scan list for
    /// `period`. Lists are created lazily; `start()` spawns one thread per
    /// non-empty list.
    ///
    /// A zero period is refused (warned once, not registered): `periodic_loop`
    /// computes deadlines as `start + n*period`, so a zero period makes every
    /// deadline equal to `start`, and the overrun catch-up loop
    /// (`while deadline_for(n) <= now`) would then spin `n` all the way to
    /// `u64::MAX` on the very first tick instead of ever sleeping again --
    /// a livelock that pegs a CPU core forever.
    pub fn add_periodic(&self, id: RecordId, phas: i32, period: Duration) {
        if period.is_zero() {
            tracing::warn!(
                "add_periodic: ignoring zero-period scan request for {:?} (would livelock)",
                id
            );
            return;
        }
        let key = period.as_nanos() as u64;
        self.periodic
            .lock()
            .unwrap()
            .entry(key)
            .or_default()
            .insert(id, phas);
    }

    /// Remove `id` from every periodic scan list it belongs to.
    pub fn remove_periodic(&self, id: RecordId) {
        let mut periodic = self.periodic.lock().unwrap();
        for list in periodic.values_mut() {
            list.remove(id);
        }
    }

    /// Add `id` (with scan priority `phas`) to the event-driven scan list
    /// keyed by `event` (the record's `EVNT` field). An empty `event` string
    /// means "no list" -- such a record is never added (Task 12 leaves
    /// `EVNT` at its default, empty value for every non-event-scanned
    /// record, so this guard is what keeps them all out of a `""`-keyed
    /// list).
    pub fn add_event(&self, id: RecordId, phas: i32, event: String) {
        if event.is_empty() {
            return;
        }
        self.events.lock().unwrap().entry(event).or_default().insert(id, phas);
    }

    /// Remove `id` from every event-driven scan list it belongs to.
    pub fn remove_event(&self, id: RecordId) {
        let mut events = self.events.lock().unwrap();
        for list in events.values_mut() {
            list.remove(id);
        }
    }

    /// Fire the event-driven scan list keyed by `event`: snapshot it
    /// (lock/clone/unlock), then run `process_ids` in PHAS order on the
    /// caller's thread. Synchronous by design -- unlike periodic scanning,
    /// event scanning must have completed by the time this call (and thus
    /// `EventSink::on_event`'s returned future, and thus `Events::post`)
    /// returns, so callers can rely on "records have processed when post
    /// returns". An unknown event name is a no-op.
    pub fn fire_event(&self, event: &str) {
        let ids = {
            self.events
                .lock()
                .unwrap()
                .get(event)
                .map(|l| l.snapshot())
                .unwrap_or_default()
        };
        self.process_ids(&ids);
    }

    /// Spawn one thread per non-empty periodic scan list. Call once; `self`
    /// must be held in an `Arc` so each thread can hold a strong reference.
    pub fn start(self: &Arc<Self>) {
        // PINI runs to completion before any periodic thread is spawned
        // (spec §PINI): startup-time initialization must finish first, not
        // race the first periodic pass.
        self.process_pini();
        let keys: Vec<PeriodKey> = {
            let periodic = self.periodic.lock().unwrap();
            periodic
                .iter()
                .filter(|(_, list)| !list.is_empty())
                .map(|(key, _)| *key)
                .collect()
        };
        // Anchor every thread's schedule to `now` at the moment `start()` is
        // called, not to whenever the OS happens to actually run the spawned
        // thread body. Capturing `self.clock.now()` lazily inside the thread
        // would race a caller that advances the clock immediately after
        // `start()` returns (the thread could see an already-advanced clock
        // and silently compute deadlines from the wrong origin).
        let anchor = self.clock.now();
        let mut threads = self.threads.lock().unwrap();
        for key in keys {
            // `weak` (not a strong clone) so the thread never keeps `Scanner`
            // alive: if it did, `Scanner::drop` could never run while any
            // thread was alive (a strong-ref cycle), leaking the thread
            // whenever a caller drops its handle without calling
            // `shutdown()` explicitly. `clock` and `stop` are independent
            // `Arc`s (not `Arc<Self>`), so cloning them does not re-create
            // that cycle -- they let the thread sleep and observe shutdown
            // without ever needing to upgrade `weak` for that.
            let weak = Arc::downgrade(self);
            let clock = self.clock.clone();
            let stop = self.stop.clone();
            let period = Duration::from_nanos(key);
            threads.push(std::thread::spawn(move || {
                Self::periodic_loop(weak, clock, stop, key, period, anchor)
            }));
        }
        // The `scan_once` drain/callback thread: drains the bounded queue and
        // processes each request via `process_ids`. Like the periodic threads
        // it holds only a `Weak<Self>` and upgrades it fresh per drained
        // request -- never holding a strong `Arc<Scanner>` across the blocking
        // `recv()` -- so an owner's `drop()` can still reclaim it. `recv()`
        // returns `Err` (and the loop exits) once `shutdown()` drops the
        // sender. `rx` is `take()`n so a second `start()` spawns no second
        // drain thread.
        if let Some(rx) = self.scan_once_rx.lock().unwrap().take() {
            let weak = Arc::downgrade(self);
            *self.drain_thread.lock().unwrap() = Some(std::thread::spawn(move || {
                while let Ok(id) = rx.recv() {
                    // `Scanner` may already be gone (owner dropped their last
                    // handle without calling `shutdown()`): exit exactly as a
                    // closed sender would. The strong ref is dropped at the end
                    // of each iteration, before the next blocking `recv()`.
                    let Some(this) = weak.upgrade() else { break };
                    this.process_ids(&[id]);
                }
            }));
        }
    }

    /// `n`th tick's offset from the thread's start instant, in nanoseconds.
    /// Saturates instead of wrapping/panicking so an astronomically long-
    /// running thread degrades (a saturated, effectively-never deadline)
    /// rather than panicking on overflow.
    fn deadline_nanos(period_nanos: u64, n: u64) -> u64 {
        period_nanos.saturating_mul(n)
    }

    /// One periodic thread's body: wait for each absolute deadline on the
    /// original grid (`start + n*period`), snapshot + process, then advance
    /// past any deadlines the pass overran (rate-limited warning on skips).
    ///
    /// Takes `Weak<Self>` rather than `Arc<Self>` deliberately: holding a
    /// strong reference here would keep `Scanner` alive for as long as this
    /// thread runs, so `Scanner::drop` could never fire while the thread was
    /// alive (a strong-ref cycle) -- a caller who dropped their handle
    /// without calling `shutdown()` would leak the thread forever. `clock`
    /// and `stop` are separate `Arc`s (not derived from the weak handle), so
    /// the thread can always sleep and observe a shutdown signal even after
    /// `Scanner` itself is gone; `weak` is upgraded fresh each iteration,
    /// strictly after waking and strictly before touching `periodic` or
    /// calling `process_ids`, and the resulting strong ref is dropped again
    /// (falls out of scope) before the next `sleep_until` -- so this thread
    /// never holds a strong `Arc<Scanner>` while parked, which is exactly
    /// what lets an owner's `drop()` reclaim it.
    fn periodic_loop(
        weak: Weak<Self>,
        clock: Arc<dyn Clock>,
        stop: Arc<AtomicBool>,
        key: PeriodKey,
        period: Duration,
        start: std::time::Instant,
    ) {
        // `key` is already the period in whole nanoseconds (see
        // `add_periodic`); using it directly (rather than
        // `period * n as u32`) avoids the u32-cast overflow a naive
        // multiplication would hit after ~4 billion ticks, and keeps the
        // schedule anchored to `start` (an absolute grid) instead of
        // `now + period` (which would drift under a slow pass).
        let deadline_for = |n: u64| start + Duration::from_nanos(Self::deadline_nanos(key, n));
        let mut n: u64 = 1;
        while !stop.load(Ordering::Relaxed) {
            let deadline = deadline_for(n);
            clock.sleep_until(deadline);
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // `Scanner` may already be gone (owner dropped their last handle
            // without calling `shutdown()`): in that case there is nothing
            // left to scan, so exit exactly as a `stop` signal would.
            let Some(this) = weak.upgrade() else { break };
            let ids = {
                this.periodic
                    .lock()
                    .unwrap()
                    .get(&key)
                    .map(|l| l.snapshot())
                    .unwrap_or_default()
            };
            this.process_ids(&ids);
            // `this` (the only strong ref this thread ever holds) is dropped
            // here, at the end of this block, before the loop goes back to
            // `sleep_until` -- never held across a sleep.

            n += 1;

            // Overrun / missed-tick catch-up on the absolute grid: if the
            // pass took long enough that the next deadline already lies in
            // the past, skip forward to the next future deadline instead of
            // rescheduling from `now` (which would let a slow pass drift the
            // whole schedule forward).
            let now = clock.now();
            let mut skipped = 0u64;
            while deadline_for(n) <= now {
                n += 1;
                skipped += 1;
            }
            if skipped > 0 {
                tracing::warn!(
                    "periodic scan (period={:?}) overran and skipped {} tick(s)",
                    period,
                    skipped
                );
            }
        }
    }

    /// Signal every periodic thread to stop and join them. Idempotent.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.clock.wake_all();
        // Close the `scan_once` queue BEFORE joining: the drain thread is
        // blocked in `recv()` (a channel wait, not a clock sleep, so
        // `wake_all()` cannot release it). Dropping the sender makes that
        // `recv()` return `Err(RecvError)` and the thread exits; joining it
        // before this drop would hang forever. Idempotent: `None` on the
        // second call. The drain thread's handle sits in the same `threads`
        // vec below, so the self-join guard covers it too.
        *self.scan_once_tx.lock().unwrap() = None;
        let mut handles: Vec<JoinHandle<()>> = std::mem::take(&mut *self.threads.lock().unwrap());
        // Join the drain thread through the SAME loop (and its self-join
        // guard) as the periodic threads. Taken so a second `shutdown()` sees
        // `None` and stays a no-op.
        handles.extend(self.drain_thread.lock().unwrap().take());
        // If the last strong `Arc<Scanner>` happens to be dropped from inside
        // one of our own periodic threads (e.g. mid-pass, after `weak.upgrade()`
        // -- see `periodic_loop`), `Scanner::drop` -> `shutdown()` runs ON that
        // thread, and its own `JoinHandle` is one of `handles`. Joining your own
        // thread from itself never returns (`JoinHandle::join` blocks forever
        // rather than erroring), so we must skip it: that thread is already in
        // the process of exiting on its own, so simply detaching its handle is
        // correct and leaks nothing (the OS thread still runs to completion).
        let self_id = std::thread::current().id();
        for handle in handles {
            if handle.thread().id() == self_id {
                continue;
            }
            let _ = handle.join();
        }
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

impl EventSink for Scanner {
    /// Drives an event-scan list from the server's named-event fan-out
    /// (`Events::post`). The returned future does purely synchronous work
    /// (`fire_event`) and awaits nothing, so by the time `Events::post`'s
    /// `sink.on_event(event).await` resolves, every record in the list has
    /// already been processed on `post`'s calling task -- preserving the
    /// "records have processed when post returns" contract documented on
    /// `EventSink` itself.
    fn on_event(&self, event: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let event = event.to_string();
        Box::pin(async move { self.fire_event(&event) })
    }
}

impl Drop for Scanner {
    /// A genuine RAII safety net: periodic threads hold only a `Weak<Self>`
    /// (see `periodic_loop`), never a strong `Arc<Self>`, so dropping the
    /// last strong handle -- even without ever calling `shutdown()` -- runs
    /// this `drop`, which sets `stop`, wakes every thread, and joins them
    /// before returning. If `shutdown()` was already called explicitly,
    /// `threads` is already empty and this is a cheap idempotent no-op.
    fn drop(&mut self) {
        self.shutdown();
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
        // Every `process()` entry line ("{name}: process entered"), only
        // recorded for records with TPRO set. Unlike `posted` (monitor
        // posts, which the engine correctly suppresses across passes that
        // don't change VAL or alarm state, per MDEL/ADEL deadbanding — see
        // `post_monitors`), this fires on every completed pass regardless of
        // whether the value moved, which is what the periodic-thread tests
        // below need: they scan a record whose INP is a constant, so VAL
        // never changes pass-to-pass and a monitor-post count would plateau
        // at 1 after the very first pass.
        traces: Mutex<Vec<String>>,
    }
    impl ProcSink for RecordingSink {
        fn flush(&self, events: Vec<(String, spvirit_types::NtPayload)>, trace: Vec<String>) {
            let mut p = self.posted.lock().unwrap();
            for (name, _) in events {
                p.push(name);
            }
            self.traces.lock().unwrap().extend(trace);
        }
    }
    impl RecordingSink {
        /// Total number of monitor posts flushed so far for `name`.
        fn count(&self, name: &str) -> usize {
            self.posted.lock().unwrap().iter().filter(|n| *n == name).count()
        }
        /// Total number of completed `process()` passes over `name` so far
        /// (requires `field(TPRO, "1")` on that record in the test's `.db`).
        fn passes(&self, name: &str) -> usize {
            let needle = format!("{name}: process entered");
            self.traces.lock().unwrap().iter().filter(|t| **t == needle).count()
        }
    }

    fn scanner_over(src: &str) -> (Scanner, Arc<RecordingSink>, Arc<RecordDb>) {
        let db = Arc::new(build_db(src));
        let clock: Arc<dyn crate::clock::Clock> = Arc::new(ManualClock::new());
        let sink = Arc::new(RecordingSink::default());
        (Scanner::new(db.clone(), clock, sink.clone()), sink, db)
    }

    /// Spin (yielding, not sleeping) until `pred` is true or a real, short
    /// timeout expires. This is a *test* waiting for a background thread to
    /// observe a `ManualClock` advance — it is not a scan-engine
    /// synchronization primitive, so a bounded real timeout is fine here.
    fn wait_until(mut pred: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if pred() {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("wait_until: condition did not become true within 5s");
            }
            std::thread::yield_now();
            std::thread::sleep(Duration::from_millis(1));
        }
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

    // Task 7 resolves the panic-isolation gap left open above: `process()`
    // reads `ctx.clock().unix_nanos()` (see `process.rs`'s
    // `set.get_mut(id).time_ns = ctx.clock().unix_nanos();`) inside the very
    // `catch_unwind` closure `process_ids` wraps around each record, so a
    // test-only `Clock` double that panics on its first `unix_nanos()` call
    // gives a real, reachable panic at that boundary without touching
    // `ProcSink` (whose `flush` runs outside `catch_unwind` and so would not
    // exercise the guard at all).
    struct PanicOnceClock {
        inner: ManualClock,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl PanicOnceClock {
        fn new(inner: ManualClock) -> Self {
            Self { inner, calls: std::sync::atomic::AtomicUsize::new(0) }
        }
    }
    impl crate::clock::Clock for PanicOnceClock {
        fn now(&self) -> std::time::Instant {
            self.inner.now()
        }
        fn sleep_until(&self, deadline: std::time::Instant) {
            self.inner.sleep_until(deadline)
        }
        fn unix_nanos(&self) -> u64 {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("PanicOnceClock: injected panic on first unix_nanos() call");
            }
            self.inner.unix_nanos()
        }
        fn wake_all(&self) {
            self.inner.wake_all()
        }
    }

    #[test]
    fn a_panicking_record_does_not_stop_the_pass_or_the_thread() {
        // R-T6-PACT: a panic inside process() strands that record with PACT
        // set forever (record_body only clears PACT on Err, not on unwind),
        // so the panicked record (A) never recovers and must NOT be asserted
        // to recover here. What this test proves is that the *thread* and
        // the *rest of the batch* (B) survive: B keeps processing and
        // flushing across passes despite A's panic on pass 1.
        // Both records are TPRO'd so `sink.passes()` (a per-pass "process
        // entered" trace count, unaffected by monitor deadbanding) can prove
        // B keeps running across multiple passes below, even though PV:B's
        // constant INP never actually changes VAL after the first pass —
        // see `RecordingSink::passes`'s doc comment.
        let db = Arc::new(build_db(
            "record(ai, \"PV:A\") {\n field(INP, \"1\")\n field(TPRO, \"1\")\n}\n\
             record(ai, \"PV:B\") {\n field(INP, \"2\")\n field(TPRO, \"1\")\n}\n",
        ));
        let manual = ManualClock::new();
        let clock: Arc<dyn crate::clock::Clock> = Arc::new(PanicOnceClock::new(manual.clone()));
        let sink = Arc::new(RecordingSink::default());
        let scanner = Arc::new(Scanner::new(db.clone(), clock, sink.clone()));
        let a = db.lookup("PV:A").unwrap();
        let b = db.lookup("PV:B").unwrap();
        // PHAS order puts A first, so A's process() (and the injected panic)
        // runs before B's on pass 1.
        scanner.add_periodic(a, 0, Duration::from_millis(100));
        scanner.add_periodic(b, 1, Duration::from_millis(100));
        scanner.start();

        manual.advance(Duration::from_millis(100));
        wait_until(|| sink.passes("PV:B") >= 1);
        assert_eq!(sink.passes("PV:B"), 1, "B must flush despite A panicking first");

        // The thread survives: B keeps accumulating on later passes too.
        manual.advance(Duration::from_millis(100));
        wait_until(|| sink.passes("PV:B") >= 2);
        assert_eq!(sink.passes("PV:B"), 2);

        scanner.shutdown();
    }

    #[test]
    fn ten_hz_fires_ten_times_per_simulated_second() {
        // TPRO'd so `sink.passes` counts every completed pass, not just the
        // first (VAL never changes again after pass 1 for a constant INP, so
        // a monitor-post count would plateau at 1 — see
        // `RecordingSink::passes`).
        let db = Arc::new(build_db(
            "record(ai, \"PV:A\") {\n field(INP, \"1\")\n field(TPRO, \"1\")\n}\n",
        ));
        let clock = Arc::new(ManualClock::new());
        let sink = Arc::new(RecordingSink::default());
        let scanner = Arc::new(Scanner::new(db.clone(), clock.clone(), sink.clone()));
        let a = db.lookup("PV:A").unwrap();
        scanner.add_periodic(a, 0, Duration::from_millis(100)); // 10 Hz
        scanner.start();

        // Advance one simulated second in 100ms steps; yield so the thread runs.
        for i in 1..=10 {
            clock.advance(Duration::from_millis(100));
            wait_until(|| sink.passes("PV:A") >= i);
        }
        // Exactly ten passes for one second at 10 Hz (allow the thread to catch up).
        wait_until(|| sink.passes("PV:A") == 10);
        assert_eq!(sink.passes("PV:A"), 10);
        scanner.shutdown();
    }

    #[test]
    fn phas_order_within_a_periodic_pass() {
        let db = Arc::new(build_db(
            "record(ai, \"PV:A\") {\n field(INP, \"1\")\n field(PHAS, \"1\")\n}\n\
             record(ai, \"PV:B\") {\n field(INP, \"2\")\n field(PHAS, \"0\")\n}\n",
        ));
        let clock = Arc::new(ManualClock::new());
        let sink = Arc::new(RecordingSink::default());
        let scanner = Arc::new(Scanner::new(db.clone(), clock.clone(), sink.clone()));
        let a = db.lookup("PV:A").unwrap();
        let b = db.lookup("PV:B").unwrap();
        // Both at 1 Hz; PV:B (PHAS 0) must be posted before PV:A (PHAS 1).
        scanner.add_periodic(a, 1, Duration::from_secs(1));
        scanner.add_periodic(b, 0, Duration::from_secs(1));
        scanner.start();

        clock.advance(Duration::from_secs(1));
        wait_until(|| sink.count("PV:A") >= 1 && sink.count("PV:B") >= 1);
        assert_eq!(
            *sink.posted.lock().unwrap(),
            vec!["PV:B".to_string(), "PV:A".to_string()]
        );
        scanner.shutdown();
    }

    #[test]
    fn overrun_lands_on_the_absolute_schedule_not_now_plus_period() {
        // A pass that consumes >1 period (simulated by advancing the
        // ManualClock from inside the sink, mimicking a slow scan) must
        // schedule the next pass on start + n*period, skipping the missed
        // tick(s) rather than drifting forward from `now`.
        struct SlowSink {
            inner: Arc<RecordingSink>,
            clock: ManualClock,
            // Only the first flush is slow, to observe one clean skip.
            armed: std::sync::atomic::AtomicBool,
        }
        impl ProcSink for SlowSink {
            fn flush(&self, events: Vec<(String, spvirit_types::NtPayload)>, trace: Vec<String>) {
                if !self.armed.swap(false, Ordering::SeqCst) {
                    self.inner.flush(events, trace);
                    return;
                }
                // Simulate a pass slow enough to burn through 2.5 periods
                // (period is 100ms) before the thread computes its next
                // deadline.
                self.clock.advance(Duration::from_millis(250));
                self.inner.flush(events, trace);
            }
        }

        // TPRO'd so `inner_sink.passes` counts every completed pass, not
        // just the first (VAL never changes again for a constant INP — see
        // `RecordingSink::passes`).
        let db = Arc::new(build_db(
            "record(ai, \"PV:A\") {\n field(INP, \"1\")\n field(TPRO, \"1\")\n}\n",
        ));
        let manual = ManualClock::new();
        let clock: Arc<dyn crate::clock::Clock> = Arc::new(manual.clone());
        let inner_sink = Arc::new(RecordingSink::default());
        let sink = Arc::new(SlowSink {
            inner: inner_sink.clone(),
            clock: manual.clone(),
            armed: std::sync::atomic::AtomicBool::new(true),
        });
        let scanner = Arc::new(Scanner::new(db.clone(), clock, sink.clone()));
        let a = db.lookup("PV:A").unwrap();
        scanner.add_periodic(a, 0, Duration::from_millis(100));
        scanner.start();

        // Cross the first deadline (t=100ms); the pass itself then advances
        // the clock a further 250ms (to t=350ms) before returning. On the
        // absolute grid (0, 100, 200, 300, 400, ...) that means deadlines at
        // 200 and 300 were already in the past the moment the pass finished,
        // so the thread must skip straight to 400 rather than scheduling
        // 350+100=450.
        manual.advance(Duration::from_millis(100));
        wait_until(|| inner_sink.passes("PV:A") >= 1);

        // No further clock advance needed: t is already 350ms, past the 400ms
        // deadline only once we push it there.
        manual.advance(Duration::from_millis(50)); // t = 400ms: next deadline due
        wait_until(|| inner_sink.passes("PV:A") >= 2);
        assert_eq!(
            inner_sink.passes("PV:A"),
            2,
            "must have landed on the 400ms grid tick, not drifted to 450/460"
        );

        // Advancing only to 490ms (short of the *next* grid point at 500ms)
        // must NOT trigger a third pass — proves the schedule really is
        // anchored to the original grid rather than now+period from the
        // overrun.
        manual.advance(Duration::from_millis(90)); // t = 490ms
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(inner_sink.passes("PV:A"), 2, "490ms must not yet reach the 500ms grid tick");

        manual.advance(Duration::from_millis(10)); // t = 500ms
        wait_until(|| inner_sink.passes("PV:A") >= 3);

        scanner.shutdown();
    }

    #[test]
    fn deadline_nanos_saturates_instead_of_overflowing_for_large_n() {
        // Guards the u64 arithmetic Task 7's absolute-deadline formula uses
        // in place of `period * n as u32` (which would overflow long before
        // n reaches u64::MAX). saturating_mul must never panic.
        assert_eq!(Scanner::deadline_nanos(u64::MAX, u64::MAX), u64::MAX);
        assert_eq!(Scanner::deadline_nanos(1_000_000_000, 10), 10_000_000_000);
        assert_eq!(Scanner::deadline_nanos(0, u64::MAX), 0);
    }

    #[test]
    fn shutdown_joins_the_periodic_thread() {
        let db = Arc::new(build_db("record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n"));
        let clock = Arc::new(ManualClock::new());
        let sink = Arc::new(RecordingSink::default());
        let scanner = Arc::new(Scanner::new(db.clone(), clock.clone(), sink.clone()));
        let a = db.lookup("PV:A").unwrap();
        scanner.add_periodic(a, 0, Duration::from_millis(50));
        scanner.start();
        assert_eq!(scanner.threads.lock().unwrap().len(), 1);
        // shutdown() must return (i.e. actually join) even though the thread
        // is currently parked in sleep_until with no deadline crossed yet.
        scanner.shutdown();
        assert!(scanner.threads.lock().unwrap().is_empty());
    }

    #[test]
    fn remove_periodic_stops_further_fires_but_leaves_the_thread_running() {
        // TPRO'd so `sink.passes` counts every completed pass regardless of
        // monitor deadbanding (constant INP never changes VAL again after
        // pass 1) -- see `RecordingSink::passes`.
        let db = Arc::new(build_db(
            "record(ai, \"PV:A\") {\n field(INP, \"1\")\n field(TPRO, \"1\")\n}\n",
        ));
        let clock = Arc::new(ManualClock::new());
        let sink = Arc::new(RecordingSink::default());
        let scanner = Arc::new(Scanner::new(db.clone(), clock.clone(), sink.clone()));
        let a = db.lookup("PV:A").unwrap();
        scanner.add_periodic(a, 0, Duration::from_millis(100));
        scanner.start();

        clock.advance(Duration::from_millis(100));
        wait_until(|| sink.passes("PV:A") >= 1);
        assert_eq!(sink.passes("PV:A"), 1);

        // `remove_periodic` must actually drop the id from the scan list
        // (not be a no-op stub): once removed, further deadlines on the same
        // thread must snapshot an empty id set and post nothing further, even
        // though the periodic thread itself keeps running (it is not torn
        // down just because its list became empty).
        scanner.remove_periodic(a);
        for _ in 0..3 {
            clock.advance(Duration::from_millis(100));
        }
        // No wait_until here: we are asserting the ABSENCE of further
        // fires, so a bounded real sleep (not a synchronization primitive,
        // just giving the background thread a chance to have wrongly fired
        // if `remove_periodic` were a no-op) is what we want, not a
        // predicate to wait on.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            sink.passes("PV:A"),
            1,
            "remove_periodic must stop further fires for the removed id"
        );

        scanner.shutdown();
    }

    #[test]
    fn drop_without_explicit_shutdown_reclaims_the_thread() {
        // Fix 1: the periodic thread holds only a `Weak<Scanner>` (see
        // `periodic_loop`'s doc comment), never a strong `Arc<Scanner>`, so
        // dropping the last strong handle -- with no `shutdown()` call at
        // all -- must still run `Scanner::drop`, which sets `stop`, wakes
        // the thread via `ManualClock::wake_all` (no real sleep or clock
        // advance needed: the thread is parked with no deadline crossed
        // yet, and only a wake can release it), and joins it before
        // `drop()` returns. Proven with a bounded channel recv rather than
        // just letting a hang fail the whole test binary.
        let db = Arc::new(build_db("record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n"));
        let clock = Arc::new(ManualClock::new());
        let sink = Arc::new(RecordingSink::default());
        let scanner = Arc::new(Scanner::new(db.clone(), clock.clone(), sink.clone()));
        let a = db.lookup("PV:A").unwrap();
        scanner.add_periodic(a, 0, Duration::from_millis(50));
        scanner.start();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(scanner); // no explicit shutdown() call
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("Scanner::drop did not return within 5s: thread was not reclaimed");
    }

    #[test]
    fn add_periodic_with_zero_period_is_ignored_not_livelocked() {
        // Fix 2: a zero period makes every deadline equal `start`
        // (`deadline_for(n) = start + n*0`), so the overrun catch-up loop
        // (`while deadline_for(n) <= now`) would spin `n` to `u64::MAX` on
        // the very first tick instead of ever sleeping again -- a
        // CPU-pegging livelock. `add_periodic` must refuse to register it
        // (and must not even spawn a thread for it in `start()`).
        let db = Arc::new(build_db("record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n"));
        let clock = Arc::new(ManualClock::new());
        let sink = Arc::new(RecordingSink::default());
        let scanner = Arc::new(Scanner::new(db.clone(), clock.clone(), sink.clone()));
        let a = db.lookup("PV:A").unwrap();
        scanner.add_periodic(a, 0, Duration::ZERO);
        assert!(
            scanner.periodic.lock().unwrap().is_empty(),
            "a zero-period request must not be registered"
        );

        scanner.start();
        assert!(
            scanner.threads.lock().unwrap().is_empty(),
            "no thread should be spawned for a period with nothing registered"
        );
        scanner.shutdown();
    }

    #[test]
    fn pini_records_process_at_startup_in_phas_order() {
        let db = Arc::new(build_db(
            "record(ai, \"PV:A\") {\n field(INP, \"1\")\n field(PINI, \"YES\")\n field(PHAS, \"1\")\n}\n\
             record(ai, \"PV:B\") {\n field(INP, \"2\")\n field(PINI, \"YES\")\n field(PHAS, \"0\")\n}\n\
             record(ai, \"PV:C\") {\n field(INP, \"3\")\n}\n",
        ));
        let clock: Arc<dyn crate::clock::Clock> = Arc::new(ManualClock::new());
        let sink = Arc::new(RecordingSink::default());
        let scanner = Arc::new(Scanner::new(db, clock, sink.clone()));
        scanner.process_pini();
        // PV:B (PHAS 0) before PV:A (PHAS 1); PV:C (no PINI) absent.
        assert_eq!(*sink.posted.lock().unwrap(), vec!["PV:B".to_string(), "PV:A".to_string()]);
    }

    #[test]
    fn pini_ties_break_by_db_definition_order_not_name() {
        // Both PHAS 0: PV:Z is defined first in the .db, so it must post
        // first despite sorting after PV:A alphabetically -- proving the
        // sort key is really (phas, order_index), not (phas, name).
        let db = Arc::new(build_db(
            "record(ai, \"PV:Z\") {\n field(INP, \"1\")\n field(PINI, \"YES\")\n field(PHAS, \"0\")\n}\n\
             record(ai, \"PV:A\") {\n field(INP, \"2\")\n field(PINI, \"YES\")\n field(PHAS, \"0\")\n}\n",
        ));
        let clock: Arc<dyn crate::clock::Clock> = Arc::new(ManualClock::new());
        let sink = Arc::new(RecordingSink::default());
        let scanner = Arc::new(Scanner::new(db, clock, sink.clone()));
        scanner.process_pini();
        assert_eq!(*sink.posted.lock().unwrap(), vec!["PV:Z".to_string(), "PV:A".to_string()]);
    }

    #[test]
    fn start_runs_pini_before_any_periodic_thread_even_with_no_periodic_list() {
        // Mutation-informed: kills a mutant that drops the `process_pini()`
        // call from `start()` (or only calls it when a periodic list
        // exists). No periodic scan list is registered at all here, so the
        // only way PV:A can post is if `start()` itself runs the PINI sweep.
        let db = Arc::new(build_db(
            "record(ai, \"PV:A\") {\n field(INP, \"1\")\n field(PINI, \"YES\")\n}\n",
        ));
        let clock = Arc::new(ManualClock::new());
        let sink = Arc::new(RecordingSink::default());
        let scanner = Arc::new(Scanner::new(db, clock, sink.clone()));
        scanner.start();
        assert_eq!(*sink.posted.lock().unwrap(), vec!["PV:A".to_string()]);
        assert!(
            scanner.threads.lock().unwrap().is_empty(),
            "no periodic list was registered, so no thread should be spawned"
        );
        scanner.shutdown();
    }

    #[test]
    fn scan_once_processes_the_record_on_the_callback_thread() {
        let db = Arc::new(build_db("record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n"));
        let (clock, sink) = (Arc::new(ManualClock::new()), Arc::new(RecordingSink::default()));
        let scanner = Arc::new(Scanner::new(db.clone(), clock, sink.clone()));
        scanner.start();
        let a = db.lookup("PV:A").unwrap();
        scanner.scan_once(a);
        wait_until(|| sink.count("PV:A") == 1);
        assert_eq!(sink.count("PV:A"), 1);
        scanner.shutdown();
    }

    #[test]
    fn scan_once_dropped_starts_at_zero_and_stays_zero_without_drops() {
        // A fresh Scanner has dropped nothing; and a single enqueued+drained
        // request (queue never full) must not increment the drop counter.
        // Pins `scan_once_dropped()` to the real counter rather than a
        // constant (kills a mutant that hard-codes its return value).
        let db = Arc::new(build_db("record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n"));
        let (clock, sink) = (Arc::new(ManualClock::new()), Arc::new(RecordingSink::default()));
        let scanner = Arc::new(Scanner::new(db.clone(), clock, sink.clone()));
        assert_eq!(scanner.scan_once_dropped(), 0);
        scanner.start();
        let a = db.lookup("PV:A").unwrap();
        scanner.scan_once(a);
        wait_until(|| sink.count("PV:A") == 1);
        assert_eq!(scanner.scan_once_dropped(), 0);
        scanner.shutdown();
    }

    #[test]
    fn scan_once_drops_and_counts_when_the_queue_is_full() {
        // Do NOT start the callback thread, so nothing drains: fill past capacity
        // from the caller side and assert scan_once_dropped() > 0.
        let db = Arc::new(build_db("record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n"));
        let (clock, sink) = (Arc::new(ManualClock::new()), Arc::new(RecordingSink::default()));
        let scanner = Scanner::new(db.clone(), clock, sink);
        let a = db.lookup("PV:A").unwrap();
        for _ in 0..(super::SCAN_ONCE_CAPACITY + 50) {
            scanner.scan_once(a);
        }
        assert!(scanner.scan_once_dropped() > 0);
    }

    #[test]
    fn fire_event_processes_the_named_list_in_phas_order_on_caller_thread() {
        let db = Arc::new(build_db(
            "record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n\
             record(ai, \"PV:B\") {\n field(INP, \"2\")\n}\n",
        ));
        let (clock, sink) = (Arc::new(ManualClock::new()), Arc::new(RecordingSink::default()));
        let scanner = Arc::new(Scanner::new(db.clone(), clock, sink.clone()));
        let (a, b) = (db.lookup("PV:A").unwrap(), db.lookup("PV:B").unwrap());
        scanner.add_event(a, 1, "shutter".into());
        scanner.add_event(b, 0, "shutter".into());
        scanner.fire_event("shutter"); // synchronous: results visible immediately
        assert_eq!(*sink.posted.lock().unwrap(), vec!["PV:B".to_string(), "PV:A".to_string()]);
    }

    #[test]
    fn firing_an_unknown_event_is_a_noop() {
        let db = Arc::new(build_db("record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n"));
        let (clock, sink) = (Arc::new(ManualClock::new()), Arc::new(RecordingSink::default()));
        let scanner = Scanner::new(db, clock, sink.clone());
        scanner.fire_event("nobody");
        assert!(sink.posted.lock().unwrap().is_empty());
    }

    #[test]
    fn add_event_with_empty_evnt_string_is_a_noop() {
        let db = Arc::new(build_db("record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n"));
        let (clock, sink) = (Arc::new(ManualClock::new()), Arc::new(RecordingSink::default()));
        let scanner = Scanner::new(db.clone(), clock, sink.clone());
        let a = db.lookup("PV:A").unwrap();
        scanner.add_event(a, 0, String::new());
        assert!(
            scanner.events.lock().unwrap().is_empty(),
            "an empty EVNT string must never create a list"
        );
    }

    #[test]
    fn remove_event_stops_further_fires() {
        let db = Arc::new(build_db("record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n"));
        let (clock, sink) = (Arc::new(ManualClock::new()), Arc::new(RecordingSink::default()));
        let scanner = Scanner::new(db.clone(), clock, sink.clone());
        let a = db.lookup("PV:A").unwrap();
        scanner.add_event(a, 0, "shutter".into());
        scanner.remove_event(a);
        scanner.fire_event("shutter");
        assert!(sink.posted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn on_event_drives_the_list_via_the_event_sink_seam() {
        // Register the Scanner as a sink on a real Events, post, assert processed.
        use spvirit_server::events::Events;

        let db = Arc::new(build_db("record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n"));
        let (clock, sink) = (Arc::new(ManualClock::new()), Arc::new(RecordingSink::default()));
        let scanner = Arc::new(Scanner::new(db.clone(), clock, sink.clone()));
        let a = db.lookup("PV:A").unwrap();
        scanner.add_event(a, 0, "shutter".into());

        let events = Events::new();
        events.add_sink(scanner.clone());
        events.post("shutter").await;

        assert_eq!(*sink.posted.lock().unwrap(), vec!["PV:A".to_string()]);
    }

    #[test]
    fn shutdown_skips_self_join_when_its_own_thread_triggers_it() {
        // Reproduces the deadlock Fix 1 (Weak-based Drop reclaim) introduced:
        // a periodic thread upgrades `weak` to a strong `this: Arc<Scanner>`
        // and holds it across `process_ids` (see `periodic_loop`). If the
        // owner has already dropped its own external `Arc<Scanner>` down to
        // strong-count 1 while that thread holds `this` (count 2), then when
        // `this` drops at the end of that thread's loop iteration, the count
        // goes 1 -> 0 *on that thread*, so `Scanner::drop` -> `shutdown()`
        // runs there. `shutdown()`'s `handles` then contains that very
        // thread's own `JoinHandle` (pushed by `start()`), and joining your
        // own thread from itself blocks forever -- a silent thread leak with
        // no panic and no test failure, unless something bounds the wait.
        //
        // Driving this end-to-end through the real weak-upgrade/refcount race
        // (ManualClock advance + a helper thread racing to drop the last
        // external Arc mid-pass) is not reliably deterministic: whichever
        // thread's decrement happens to observe the count hitting zero is a
        // genuine data race, so a test built that way could pass even on
        // unfixed code simply because the owner's drop -- not the periodic
        // thread's -- won the race that particular run.
        //
        // Instead, this constructs the exact precondition the race produces
        // -- a thread's own `JoinHandle` present in `threads` when that same
        // thread calls `shutdown()` -- deterministically, via a rendezvous
        // channel that guarantees the handle is pushed into `threads` before
        // `shutdown()` is invoked. This exercises the identical code path in
        // `shutdown()` (including the real `self_id` guard) without any
        // timing dependency.
        //
        // Without the `self_id` skip: `handle.join()` on itself never
        // returns, `done_tx.send(())` is never reached, and `recv_timeout`
        // below times out and this test fails. With the fix: `shutdown()`
        // returns promptly and the test passes.
        let db = Arc::new(build_db("record(ai, \"PV:A\") {\n field(INP, \"1\")\n}\n"));
        let clock = Arc::new(ManualClock::new());
        let sink = Arc::new(RecordingSink::default());
        let scanner = Arc::new(Scanner::new(db, clock, sink));

        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let scanner_for_thread = scanner.clone();
        let handle = std::thread::spawn(move || {
            // Wait until our own JoinHandle is in `threads` before calling
            // shutdown() on ourselves -- otherwise we might race ahead and
            // call it while `threads` is still empty, which would trivially
            // "pass" without ever exercising the self-join path.
            go_rx.recv().unwrap();
            scanner_for_thread.shutdown();
            let _ = done_tx.send(());
        });
        scanner.threads.lock().unwrap().push(handle);
        go_tx.send(()).unwrap();

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("shutdown() did not return within 5s: self-join likely deadlocked");
    }
}
