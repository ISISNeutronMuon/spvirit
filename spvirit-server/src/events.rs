//! Server-wide lifecycle hooks and named events.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use tracing::warn;

use crate::simple_store::SimplePvStore;

/// Maximum number of queued handler invocations before new ones are dropped.
pub const DISPATCH_QUEUE_CAPACITY: usize = 1024;

/// An inline, awaited consumer of named events.
///
/// Implementors are awaited by [`Events::post`], in registration order,
/// before any deferred handler is queued — when `post` returns, every sink
/// has finished its work. That is what makes the "records have processed
/// when `post_event` returns" guarantee true.
/// Sub-project B implements this on its `Scanner` to drive `EVNT` scan lists.
///
/// The method returns a boxed future rather than being a plain `fn`, for the
/// same reason [`EventHandler`] does: every store mutation a realistic sink
/// needs (`SimplePvStore::set_value` and friends) is `async`, and a sync
/// trait would force implementors to block on a future from inside `post` —
/// which is reachable from the dispatcher task on a `current_thread`
/// runtime, where `Handle::block_on` panics, `block_in_place` panics, and
/// `futures::executor::block_on` deadlocks against the store's tokio
/// `RwLock`. There is no correct way to honour a sync signature.
///
/// The returned future borrows `&self`, not `event` — copy the name in if
/// the future needs it (`let event = event.to_string();`).
///
/// A panicking sink is caught, counted in [`Events::failed_count`], and the
/// remaining sinks still run: one bad sink can neither truncate the fan-out
/// nor stop handlers from being queued.
pub trait EventSink: Send + Sync {
    /// Handle `event`. Awaited inline by `post`; keep it short.
    fn on_event(&self, event: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// A deferred event handler.
///
/// Receives the store and the event name. Returns a boxed future because
/// `SimplePvStore::set_value` is async — a plain `Fn` could not touch the
/// store at all.
pub type EventHandler = Arc<
    dyn Fn(Arc<SimplePvStore>, String) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// A startup hook. Runs once, to completion, before the server serves.
pub type StartHook =
    Arc<dyn Fn(Arc<SimplePvStore>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// One queued unit of work: a handler plus the event that triggered it.
struct Dispatch {
    handler: EventHandler,
    event: String,
}

/// Server-wide event registry.
///
/// Owns the inline sinks and the deferred handlers. Sinks are awaited by
/// `post` before it returns; handlers are queued and run one at a time on a
/// single dispatcher task.
pub struct Events {
    sinks: RwLock<Vec<Arc<dyn EventSink>>>,
    handlers: RwLock<HashMap<String, Vec<EventHandler>>>,
    tx: mpsc::Sender<Dispatch>,
    rx: RwLock<Option<mpsc::Receiver<Dispatch>>>,
    dropped: AtomicU64,
    failed: Arc<AtomicU64>,
    /// Set by `start_dispatcher`. `drain()` and `post()` read it so that
    /// "you forgot to start the server" is diagnosed immediately and by
    /// name, instead of as a 10 s spin blamed on a stuck dispatcher.
    dispatcher_started: std::sync::atomic::AtomicBool,
    /// Incremented on every successful enqueue, decremented after the handler
    /// finishes. `drain()` waits for this to reach zero.
    inflight: Arc<AtomicU64>,
}

impl Events {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(DISPATCH_QUEUE_CAPACITY);
        Self {
            sinks: RwLock::new(Vec::new()),
            handlers: RwLock::new(HashMap::new()),
            tx,
            rx: RwLock::new(Some(rx)),
            dropped: AtomicU64::new(0),
            failed: Arc::new(AtomicU64::new(0)),
            dispatcher_started: std::sync::atomic::AtomicBool::new(false),
            inflight: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Register an inline sink. Sinks are awaited in registration order.
    ///
    /// May be called at any time, including from inside another sink's
    /// call-out; the new sink takes effect from the next `post`.
    pub fn add_sink(&self, sink: Arc<dyn EventSink>) {
        self.sinks.write().unwrap().push(sink);
    }

    /// Register a deferred handler for `event`.
    ///
    /// Handlers for one event run in registration order.
    pub fn add_handler(&self, event: impl Into<String>, handler: EventHandler) {
        self.handlers
            .write()
            .unwrap()
            .entry(event.into())
            .or_default()
            .push(handler);
    }

    /// Number of handler invocations dropped because the queue was full.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Number of sink or handler invocations that panicked.
    pub fn failed_count(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Start the single dispatcher task. Call once, at server start.
    pub fn start_dispatcher(&self, store: Arc<SimplePvStore>) {
        let Some(mut rx) = self.rx.write().unwrap().take() else {
            warn!("Events::start_dispatcher called twice; ignoring");
            return;
        };
        let inflight = self.inflight.clone();
        let failed = self.failed.clone();
        self.dispatcher_started.store(true, Ordering::SeqCst);
        tokio::spawn(async move {
            while let Some(Dispatch { handler, event }) = rx.recv().await {
                let fut = handler(store.clone(), event.clone());
                // AssertUnwindSafe: on panic we drop the handler's state and
                // continue; the store's own invariants are upheld by its locks.
                let result =
                    futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut)).await;
                if result.is_err() {
                    warn!("event handler for '{}' panicked", event);
                    failed.fetch_add(1, Ordering::Relaxed);
                }
                inflight.fetch_sub(1, Ordering::SeqCst);
            }
        });
    }

    /// Post `event`: await every sink, then queue handlers and return.
    ///
    /// Sinks run in registration order and are each wrapped in
    /// `catch_unwind`, exactly as handlers are on the dispatcher: a
    /// panicking sink is logged, counted in [`Self::failed_count`], and the
    /// fan-out continues. Without that, one bad sink would unwind out of
    /// `post` and the remaining sinks *and every handler* would silently
    /// never see the event.
    ///
    /// An unknown event name is a no-op — events are a dynamic namespace.
    pub async fn post(&self, event: &str) {
        // Clone the list rather than holding the read guard across the
        // call-outs: a sink is allowed to post, and to register further
        // sinks, from inside its own call-out.
        let sinks = self.sinks.read().unwrap().clone();
        for sink in &sinks {
            // AssertUnwindSafe: on panic we drop the sink's in-progress
            // state and continue; the store's invariants are upheld by its
            // own locks. The call itself is inside the async block so a
            // panic while *building* the future is caught too.
            let fut = async { sink.on_event(event).await };
            let result = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut)).await;
            if result.is_err() {
                warn!("event sink for '{}' panicked; continuing fan-out", event);
                self.failed.fetch_add(1, Ordering::Relaxed);
            }
        }

        let handlers = {
            let map = self.handlers.read().unwrap();
            map.get(event).cloned().unwrap_or_default()
        };
        // Count the whole batch as in-flight before enqueueing any of it, so
        // a concurrent drain() can never observe inflight == 0 partway
        // through this loop (e.g. because the dispatcher already finished
        // handler 1 while handler 2 has not been enqueued yet).
        if !handlers.is_empty() && !self.dispatcher_started.load(Ordering::SeqCst) {
            warn!(
                "posted '{}' with {} handler(s) registered but the event dispatcher \
                 has not started — nothing will run them until the server starts \
                 (run()/start()/start_background())",
                event,
                handlers.len()
            );
        }
        self.inflight
            .fetch_add(handlers.len() as u64, Ordering::SeqCst);
        for handler in handlers {
            let queued = self.tx.try_send(Dispatch {
                handler,
                event: event.to_string(),
            });
            if queued.is_err() {
                self.inflight.fetch_sub(1, Ordering::SeqCst);
                let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    warn!(
                        "event dispatch queue full; dropped handler for '{}' ({} dropped so far)",
                        event, n
                    );
                }
            }
        }
    }

    /// Wait until every queued handler has finished. Test helper.
    ///
    /// Bounded rather than an unconditional spin: this doubles as a deadlock
    /// detector. If a future change ever breaks the invariant that the
    /// dispatcher always decrements `inflight` (e.g. removing the
    /// `catch_unwind` around handler futures), a hung dispatcher must show up
    /// as a clear, named test failure — not an indefinitely hanging CI job.
    pub async fn drain(&self) {
        const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
        // `build() -> post_event() -> drain_events()` is the natural test
        // shape, and forgetting to start the server is the natural mistake.
        // Diagnose it here in microseconds instead of spinning a core for
        // ten seconds and then blaming the dispatcher for being stuck.
        if !self.dispatcher_started.load(Ordering::SeqCst)
            && self.inflight.load(Ordering::SeqCst) > 0
        {
            panic!(
                "Events::drain() called with {} handler invocation(s) queued but the \
                 dispatcher was never started — nothing will ever run them. Start the \
                 server (run() / start() / start_background(), or \
                 Events::start_dispatcher) before posting events you intend to drain.",
                self.inflight.load(Ordering::SeqCst)
            );
        }
        let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;
        while self.inflight.load(Ordering::SeqCst) > 0 {
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "Events::drain() timed out after {DRAIN_TIMEOUT:?} with {} handler(s) still in flight — \
                     the dispatcher likely stopped consuming (e.g. a handler future \
                     that never returns, or a panic no longer being caught)",
                    self.inflight.load(Ordering::SeqCst)
                );
            }
            tokio::task::yield_now().await;
        }
    }
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Build a store with one f64 record, for handlers to write into.
    fn test_store() -> Arc<crate::simple_store::SimplePvStore> {
        use crate::pva_server::PvaServer;
        let server = PvaServer::builder().ai("T:X", 0.0).build();
        server.store().clone()
    }

    #[tokio::test]
    async fn handlers_run_on_the_dispatcher_not_inline() {
        let store = test_store();
        let events = Events::new();
        let ran = Arc::new(AtomicUsize::new(0));

        let r = ran.clone();
        events.add_handler(
            "GO",
            Arc::new(move |_store, _event| {
                let r = r.clone();
                Box::pin(async move {
                    r.fetch_add(1, Ordering::SeqCst);
                })
            }),
        );
        events.start_dispatcher(store);

        events.post("GO").await;
        // Deferred: must not have run yet at the moment post() returned.
        assert_eq!(ran.load(Ordering::SeqCst), 0, "handler ran inline");

        events.drain().await;
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn handlers_are_serialized_in_registration_order() {
        let store = test_store();
        let events = Events::new();
        let log = Arc::new(Mutex::new(Vec::new()));

        for label in ["first", "second", "third"] {
            let log = log.clone();
            events.add_handler(
                "GO",
                Arc::new(move |_store, _event| {
                    let log = log.clone();
                    let label = label.to_string();
                    Box::pin(async move {
                        log.lock().unwrap().push(format!("{label}:enter"));
                        tokio::task::yield_now().await;
                        log.lock().unwrap().push(format!("{label}:exit"));
                    })
                }),
            );
        }
        events.start_dispatcher(store);

        events.post("GO").await;
        events.drain().await;

        // Serialized: every enter is immediately followed by its own exit.
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[
                "first:enter".to_string(),
                "first:exit".to_string(),
                "second:enter".to_string(),
                "second:exit".to_string(),
                "third:enter".to_string(),
                "third:exit".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn only_handlers_for_the_posted_event_run() {
        let store = test_store();
        let events = Events::new();
        let a = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(AtomicUsize::new(0));

        let ac = a.clone();
        events.add_handler("A", Arc::new(move |_s, _e| {
            let ac = ac.clone();
            Box::pin(async move { ac.fetch_add(1, Ordering::SeqCst); })
        }));
        let bc = b.clone();
        events.add_handler("B", Arc::new(move |_s, _e| {
            let bc = bc.clone();
            Box::pin(async move { bc.fetch_add(1, Ordering::SeqCst); })
        }));
        events.start_dispatcher(store);

        events.post("A").await;
        events.drain().await;

        assert_eq!(a.load(Ordering::SeqCst), 1);
        assert_eq!(b.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn handler_receives_the_event_name() {
        let store = test_store();
        let events = Events::new();
        let seen = Arc::new(Mutex::new(Vec::new()));

        let s = seen.clone();
        events.add_handler("SHUTTER", Arc::new(move |_store, event| {
            let s = s.clone();
            Box::pin(async move { s.lock().unwrap().push(event); })
        }));
        events.start_dispatcher(store);

        events.post("SHUTTER").await;
        events.drain().await;

        assert_eq!(seen.lock().unwrap().as_slice(), &["SHUTTER".to_string()]);
    }

    #[tokio::test]
    async fn handler_can_write_the_store() {
        let store = test_store();
        let events = Events::new();

        events.add_handler("BUMP", Arc::new(|store, _event| {
            Box::pin(async move {
                store
                    .set_value("T:X", spvirit_types::ScalarValue::F64(42.0))
                    .await;
            })
        }));
        events.start_dispatcher(store.clone());

        events.post("BUMP").await;
        events.drain().await;

        assert_eq!(
            store.get_value("T:X").await,
            Some(spvirit_types::ScalarValue::F64(42.0))
        );
    }

    #[tokio::test]
    async fn full_queue_drops_and_counts() {
        let store = test_store();
        let events = Events::new();
        // A handler that blocks until released, so the queue backs up.
        let gate = Arc::new(tokio::sync::Notify::new());
        let g = gate.clone();
        events.add_handler("FLOOD", Arc::new(move |_s, _e| {
            let g = g.clone();
            Box::pin(async move { g.notified().await; })
        }));
        events.start_dispatcher(store);

        // #[tokio::test] defaults to current_thread, and with no sinks
        // registered `post()` completes without ever returning `Pending`, so
        // awaiting it never yields to the executor: the spawned dispatcher is
        // never polled and never dequeues anything before we assert below —
        // capacity is filled from the sender side alone, deterministically.
        // Post well past capacity.
        for _ in 0..(DISPATCH_QUEUE_CAPACITY + 50) {
            events.post("FLOOD").await;
        }

        assert!(
            events.dropped_count() > 0,
            "expected drops once the queue filled, got {}",
            events.dropped_count()
        );

        // Release everything so the test does not leak a blocked task.
        gate.notify_waiters();
    }

    #[tokio::test]
    async fn dispatcher_survives_a_panicking_handler() {
        let store = test_store();
        let events = Events::new();
        let after = Arc::new(AtomicUsize::new(0));

        events.add_handler("BOOM", Arc::new(|_s, _e| {
            Box::pin(async { panic!("handler blew up"); })
        }));
        let a = after.clone();
        events.add_handler("BOOM", Arc::new(move |_s, _e| {
            let a = a.clone();
            Box::pin(async move { a.fetch_add(1, Ordering::SeqCst); })
        }));
        events.start_dispatcher(store);

        events.post("BOOM").await;
        events.drain().await;

        assert_eq!(
            after.load(Ordering::SeqCst),
            1,
            "handler after the panicking one must still run"
        );
        assert_eq!(events.failed_count(), 1);
    }

    struct RecordingSink {
        seen: Mutex<Vec<String>>,
    }

    impl EventSink for RecordingSink {
        fn on_event(&self, event: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            let event = event.to_string();
            Box::pin(async move {
                self.seen.lock().unwrap().push(event);
            })
        }
    }

    #[tokio::test]
    async fn post_calls_sinks_in_registration_order() {
        let a = Arc::new(RecordingSink { seen: Mutex::new(Vec::new()) });
        let b = Arc::new(RecordingSink { seen: Mutex::new(Vec::new()) });
        let events = Events::new();
        events.add_sink(a.clone());
        events.add_sink(b.clone());

        events.post("SHUTTER").await;

        assert_eq!(a.seen.lock().unwrap().as_slice(), &["SHUTTER".to_string()]);
        assert_eq!(b.seen.lock().unwrap().as_slice(), &["SHUTTER".to_string()]);
    }

    #[tokio::test]
    async fn post_with_no_sinks_is_a_noop() {
        let events = Events::new();
        events.post("NOBODY:LISTENING").await;
    }

    #[tokio::test]
    #[should_panic(expected = "the dispatcher was never started")]
    async fn drain_without_a_dispatcher_fails_immediately_and_says_why() {
        // `build() -> post_event() -> drain_events()` with no start in
        // between: the old code spun the full 10 s DRAIN_TIMEOUT and then
        // blamed the dispatcher for being stuck.
        let events = Events::new();
        events.add_handler(
            "GO",
            Arc::new(|_s, _e| Box::pin(async {})),
        );
        events.post("GO").await;
        let t0 = std::time::Instant::now();
        let hit = std::panic::AssertUnwindSafe(events.drain());
        let result = futures::FutureExt::catch_unwind(hit).await;
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(1),
            "drain() must fail fast when the dispatcher never started, took {:?}",
            t0.elapsed()
        );
        std::panic::resume_unwind(result.expect_err("drain() must panic"));
    }

    #[tokio::test]
    async fn drain_with_nothing_queued_is_fine_without_a_dispatcher() {
        // No handler invocations in flight means nothing to wait for; the
        // dispatcher check must not turn that into a failure.
        let events = Events::new();
        events.post("NOBODY").await;
        events.drain().await;
    }

    #[tokio::test]
    async fn a_panicking_sink_does_not_truncate_the_fan_out() {
        struct BoomSink;
        impl EventSink for BoomSink {
            fn on_event(&self, _event: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
                Box::pin(async { panic!("sink blew up") })
            }
        }

        let store = test_store();
        let events = Events::new();
        let before = Arc::new(RecordingSink { seen: Mutex::new(Vec::new()) });
        let after = Arc::new(RecordingSink { seen: Mutex::new(Vec::new()) });
        events.add_sink(before.clone());
        events.add_sink(Arc::new(BoomSink));
        events.add_sink(after.clone());

        let handler_ran = Arc::new(AtomicUsize::new(0));
        let h = handler_ran.clone();
        events.add_handler(
            "BOOM",
            Arc::new(move |_s, _e| {
                let h = h.clone();
                Box::pin(async move {
                    h.fetch_add(1, Ordering::SeqCst);
                })
            }),
        );
        events.start_dispatcher(store);

        // Must not unwind out of post().
        events.post("BOOM").await;
        events.drain().await;

        assert_eq!(before.seen.lock().unwrap().as_slice(), &["BOOM".to_string()]);
        assert_eq!(
            after.seen.lock().unwrap().as_slice(),
            &["BOOM".to_string()],
            "a sink after the panicking one must still see the event"
        );
        assert_eq!(
            handler_ran.load(Ordering::SeqCst),
            1,
            "a panicking sink must not stop handlers from being queued"
        );
        assert_eq!(events.failed_count(), 1);
    }

    #[tokio::test]
    async fn a_handler_may_post_another_event() {
        let store = test_store();
        let events = Arc::new(Events::new());
        let log = Arc::new(Mutex::new(Vec::new()));

        let l = log.clone();
        let ev = events.clone();
        events.add_handler("FIRST", Arc::new(move |_s, _e| {
            let l = l.clone();
            let ev = ev.clone();
            Box::pin(async move {
                l.lock().unwrap().push("first:enter".to_string());
                ev.post("SECOND").await;
                l.lock().unwrap().push("first:exit".to_string());
            })
        }));

        let l = log.clone();
        events.add_handler("SECOND", Arc::new(move |_s, _e| {
            let l = l.clone();
            Box::pin(async move { l.lock().unwrap().push("second".to_string()); })
        }));

        events.start_dispatcher(store);
        events.post("FIRST").await;
        events.drain().await;

        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[
                "first:enter".to_string(),
                "first:exit".to_string(),
                "second".to_string(),
            ],
            "nested handler must queue behind the posting handler, not run inside it"
        );
    }

    #[tokio::test]
    async fn a_sink_may_post_another_event_without_deadlocking() {
        // A sink registered from inside another sink's call-out. Its
        // presence is the second half of the test: it proves the
        // registration made during the call-out actually took effect, not
        // just that the call-out itself returned.
        struct LateSink {
            fired: Arc<AtomicUsize>,
        }
        impl EventSink for LateSink {
            fn on_event(&self, _event: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
                Box::pin(async move {
                    self.fired.fetch_add(1, Ordering::SeqCst);
                })
            }
        }

        struct Reposter {
            events: Mutex<Option<std::sync::Weak<Events>>>,
            fired: AtomicUsize,
            late_fired: Arc<AtomicUsize>,
        }
        impl EventSink for Reposter {
            fn on_event(&self, event: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
                let event = event.to_string();
                Box::pin(async move {
                    if event != "OUTER" {
                        return;
                    }
                    self.fired.fetch_add(1, Ordering::SeqCst);
                    // Take and drop the guard before awaiting: a std
                    // MutexGuard held across an await would not be Send.
                    let ev = {
                        let g = self.events.lock().unwrap();
                        g.as_ref().and_then(|w| w.upgrade())
                    };
                    if let Some(ev) = ev {
                        ev.post("INNER").await;
                        // add_sink() takes the sinks lock for write. If
                        // post() ever held the sinks read lock across this
                        // call-out instead of cloning it first, this write
                        // request would deadlock the calling thread against
                        // itself deterministically — a read guard can never
                        // be upgraded to a write guard, on any platform,
                        // regardless of writer contention from other threads.
                        ev.add_sink(Arc::new(LateSink {
                            fired: self.late_fired.clone(),
                        }));
                    }
                })
            }
        }

        let events = Arc::new(Events::new());
        let late_fired = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(Reposter {
            events: Mutex::new(Some(Arc::downgrade(&events))),
            fired: AtomicUsize::new(0),
            late_fired: late_fired.clone(),
        });
        events.add_sink(sink.clone());

        // Would deadlock if post() held the sinks read lock across the call.
        // Run it on its own thread (with its own runtime, since the lock
        // deadlock would wedge the *thread*, not just the task, so an
        // in-runtime `tokio::time::timeout` could never fire) with a bounded
        // wait — the same discipline `drain()` applies to the dispatcher: a
        // regression that reintroduces a lock held across the call-out must
        // show up as a clear, named panic, not an indefinitely hanging
        // `cargo test`.
        const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
        let ev = events.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("current_thread runtime for the sink call-out");
            rt.block_on(ev.post("OUTER"));
            let _ = done_tx.send(());
        });
        if done_rx.recv_timeout(CALL_TIMEOUT).is_err() {
            panic!(
                "sink call-out deadlocked — `post()` is holding a lock across the call-out"
            );
        }
        // The thread finished; join it so a panic inside it (e.g. from the
        // sink call-out itself) surfaces here instead of being swallowed.
        handle.join().expect("post(\"OUTER\") thread panicked");

        assert_eq!(sink.fired.load(Ordering::SeqCst), 1);
        assert_eq!(
            late_fired.load(Ordering::SeqCst),
            0,
            "late sink registered but not yet posted to"
        );

        // Probe with a fresh event name: the sink added from inside the
        // "OUTER" call-out must now be live. (Re-posting "OUTER" itself
        // would also re-trigger the nested "INNER" post and double-count
        // through LateSink, so a distinct probe event keeps this precise.)
        events.post("PROBE").await;
        assert_eq!(
            late_fired.load(Ordering::SeqCst),
            1,
            "sink registered from inside a call-out must take effect"
        );
    }
}
