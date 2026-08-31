//! Background resolution of PV names the search path could not answer.
//!
//! The search responder is a single task shared by every client. Awaiting a
//! [`SourceRegistry::claim`] inside it — for a proxying source, a full
//! upstream round trip — stops it reading datagrams for the whole duration,
//! denying search to every client and every name, including purely local
//! ones. Measured: 1.4 miss-searches/s produced total denial in ~4s at 3% CPU,
//! and because blocked clients retry at ~5Hz the denial sustained itself
//! indefinitely.
//!
//! So the search path asks [`Source::try_claim`](crate::pvstore::Source::try_claim),
//! which never blocks, and hands anything undecided here. This resolver runs
//! the real `claim` on a bounded set of spawned tasks and **sends nothing**:
//! `claim`'s own side effects (a gateway binding on success, a negative-cache
//! entry on a miss) are what make the requester's next search retry decisive.
//!
//! There is deliberately no queue. A permit is taken before spawning, so a
//! name that cannot get one is dropped rather than delayed — shedding is the
//! correct response to a flood, and the client retries anyway.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use tokio::sync::Semaphore;

use crate::pvstore::SourceRegistry;
use crate::request_ctx;

/// Maximum simultaneous background resolutions.
///
/// Every one of these may be an upstream round trip, so this is also the cap
/// on load a search flood can put on upstream servers. Eight is ample for
/// legitimate traffic, where resolutions complete in milliseconds.
pub const RESOLVE_CONCURRENCY: usize = 8;

/// Counters for the resolver, snapshotted by [`SearchResolver::stats`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResolveStatsSnapshot {
    /// Resolutions actually started.
    pub started: u64,
    /// Enqueues suppressed because the same name was already resolving.
    pub deduped: u64,
    /// Enqueues dropped because the concurrency cap was exhausted.
    pub dropped_full: u64,
    /// Resolutions that found the PV.
    pub completed_found: u64,
    /// Resolutions that concluded the PV is absent.
    pub completed_missing: u64,
    /// Search names answered outright from memory.
    pub try_claim_yes: u64,
    /// Search names known to be absent without any I/O.
    pub try_claim_no: u64,
    /// Search names that needed a background resolution.
    pub try_claim_unknown: u64,
    /// Pattern/wildcard enumerations shed without being answered, because
    /// [`PATTERN_ENUM_CONCURRENCY`](crate::handler::PATTERN_ENUM_CONCURRENCY)
    /// permits were all in use.
    ///
    /// A shed pattern query is answered with *silence* (the client's retry is
    /// what gets a decisive answer), so without this counter a wildcard flood
    /// — or a single upstream hung in `names()` holding every permit — is
    /// completely invisible in production. Global-only, like the
    /// `try_claim_*` counters: it is recorded by a free function on the
    /// search path, which has no per-resolver handle.
    pub pattern_enum_shed: u64,
}

#[derive(Debug, Default)]
struct Stats {
    started: AtomicU64,
    deduped: AtomicU64,
    dropped_full: AtomicU64,
    completed_found: AtomicU64,
    completed_missing: AtomicU64,
    try_claim_yes: AtomicU64,
    try_claim_no: AtomicU64,
    try_claim_unknown: AtomicU64,
    pattern_enum_shed: AtomicU64,
}

impl Stats {
    const fn new() -> Self {
        Self {
            started: AtomicU64::new(0),
            deduped: AtomicU64::new(0),
            dropped_full: AtomicU64::new(0),
            completed_found: AtomicU64::new(0),
            completed_missing: AtomicU64::new(0),
            try_claim_yes: AtomicU64::new(0),
            try_claim_no: AtomicU64::new(0),
            try_claim_unknown: AtomicU64::new(0),
            pattern_enum_shed: AtomicU64::new(0),
        }
    }
}

/// Process-wide totals, read by the `/metrics` endpoint.
///
/// Global rather than per-resolver because the only consumer aggregates
/// across every server in the process anyway, and reaching a per-server
/// resolver from the gateway runtime would mean threading a handle through
/// three layers for diagnostics alone. Per-resolver counters still exist and
/// are what the unit tests assert on, so tests never contend on this.
static GLOBAL: Stats = Stats::new();

/// Snapshot the process-wide counters.
pub fn global_stats() -> ResolveStatsSnapshot {
    snapshot(&GLOBAL)
}

/// Record one `try_claim` outcome from the search path.
pub fn note_try_claim(outcome: crate::pvstore::TryClaim) {
    let counter = match outcome {
        crate::pvstore::TryClaim::Yes => &GLOBAL.try_claim_yes,
        crate::pvstore::TryClaim::No => &GLOBAL.try_claim_no,
        crate::pvstore::TryClaim::Unknown => &GLOBAL.try_claim_unknown,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Record one shed pattern-query enumeration.
///
/// Called from both search paths whenever a pattern query is dropped rather
/// than answered — no permit was free. Shedding is silent on the wire by
/// design, so this counter is the only way an operator can see it happening.
pub fn note_pattern_enum_shed() {
    GLOBAL.pattern_enum_shed.fetch_add(1, Ordering::Relaxed);
}

fn snapshot(s: &Stats) -> ResolveStatsSnapshot {
    ResolveStatsSnapshot {
        started: s.started.load(Ordering::Relaxed),
        deduped: s.deduped.load(Ordering::Relaxed),
        dropped_full: s.dropped_full.load(Ordering::Relaxed),
        completed_found: s.completed_found.load(Ordering::Relaxed),
        completed_missing: s.completed_missing.load(Ordering::Relaxed),
        try_claim_yes: s.try_claim_yes.load(Ordering::Relaxed),
        try_claim_no: s.try_claim_no.load(Ordering::Relaxed),
        try_claim_unknown: s.try_claim_unknown.load(Ordering::Relaxed),
        pattern_enum_shed: s.pattern_enum_shed.load(Ordering::Relaxed),
    }
}

/// Resolves PV names off the search task, under a hard concurrency cap.
pub struct SearchResolver {
    sources: Arc<SourceRegistry>,
    permits: Arc<Semaphore>,
    /// Names currently being resolved. Suppresses duplicate work from a
    /// client's own search retries — the property that stops a flood from
    /// feeding itself.
    inflight: Arc<Mutex<HashSet<Arc<str>>>>,
    stats: Arc<Stats>,
}

/// Take the `inflight` lock, recovering from poisoning rather than panicking.
///
/// Same ruling, and for the same reason, as
/// [`SourceRegistry::lock_resolved`](crate::pvstore): treating a poison as
/// fatal here would make every subsequent `enqueue` panic *on the search
/// task* — which is precisely the total search denial this module exists to
/// prevent. Worse, one of the two callers is `InflightGuard::drop`, where a
/// panic during unwind aborts the process.
///
/// The set is only ever `insert`ed into and `remove`d from, neither of which
/// can panic, so poisoning is not currently reachable — but the recovery is
/// free and the failure mode it averts is not.
fn lock_inflight(inflight: &Mutex<HashSet<Arc<str>>>) -> MutexGuard<'_, HashSet<Arc<str>>> {
    inflight.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Removes `name` from `inflight` when dropped — on normal completion, and,
/// crucially, on unwind if `claim` panics. Constructed before the `claim`
/// await so the removal runs unconditionally.
struct InflightGuard {
    inflight: Arc<Mutex<HashSet<Arc<str>>>>,
    name: Arc<str>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        lock_inflight(&self.inflight).remove(&self.name);
    }
}

impl SearchResolver {
    pub fn new(sources: Arc<SourceRegistry>) -> Self {
        Self {
            sources,
            permits: Arc::new(Semaphore::new(RESOLVE_CONCURRENCY)),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            stats: Arc::new(Stats::new()),
        }
    }

    /// Start resolving `name` in the background, unless it is already being
    /// resolved or the concurrency cap is exhausted.
    ///
    /// Synchronous and non-blocking by contract: this is called from the
    /// search responder, and anything that can wait here reintroduces exactly
    /// the defect this module exists to remove. It must stay free of `.await`
    /// and of any lock that could be held across one.
    pub fn enqueue(&self, name: &str) {
        // Permit first: an exhausted cap is the common case under a flood,
        // and taking it first means the inflight set is never touched for a
        // job that will not run.
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            self.stats.dropped_full.fetch_add(1, Ordering::Relaxed);
            GLOBAL.dropped_full.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // A single allocation, shared between the `inflight` set and the
        // spawned task via cheap `Arc` clones (refcount bumps, not copies).
        let name: Arc<str> = Arc::from(name);
        {
            let mut inflight = lock_inflight(&self.inflight);
            if !inflight.insert(name.clone()) {
                self.stats.deduped.fetch_add(1, Ordering::Relaxed);
                GLOBAL.deduped.fetch_add(1, Ordering::Relaxed);
                return; // `permit` drops here, releasing it.
            }
        }
        self.stats.started.fetch_add(1, Ordering::Relaxed);
        GLOBAL.started.fetch_add(1, Ordering::Relaxed);

        // Captured now, on the task that holds the request's task-local; a
        // spawned task does not inherit it.
        let ctx = request_ctx::current_request();
        let sources = self.sources.clone();
        let inflight = self.inflight.clone();
        let stats = self.stats.clone();

        tokio::spawn(async move {
            let _permit = permit;
            // RAII: guarantees the `inflight` entry is released even if
            // `claim` panics (plausible for a proxying source parsing
            // untrusted upstream bytes). Without this, a panicking claim
            // would strand the name in `inflight` forever, silently
            // deduping every future `enqueue` for it until process restart.
            let _guard = InflightGuard {
                inflight,
                name: name.clone(),
            };
            let started = Instant::now();
            // `note_resolved` must run under the same identity `try_claim` will
            // later be read under — inside `scope_with`, not after it. Called
            // after the scope ends, the memo key would derive from an empty
            // identity and the searching client's own `try_claim` (evaluated
            // under its real identity) would never see the entry.
            let n = name.clone();
            let resolve = async move {
                let found = sources.claim(&n).await.is_some();
                sources.note_resolved(&n, found);
                found
            };
            let found = match ctx {
                Some(ctx) => request_ctx::scope_with(ctx, resolve).await,
                None => resolve.await,
            };

            if found {
                stats.completed_found.fetch_add(1, Ordering::Relaxed);
                GLOBAL.completed_found.fetch_add(1, Ordering::Relaxed);
            } else {
                stats.completed_missing.fetch_add(1, Ordering::Relaxed);
                GLOBAL.completed_missing.fetch_add(1, Ordering::Relaxed);
            }
            tracing::debug!(
                "search resolve: '{name}' found={found} in {:?}",
                started.elapsed()
            );
        });
    }

    pub fn stats(&self) -> ResolveStatsSnapshot {
        snapshot(&self.stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvstore::{PvInfo, Source, TryClaim};
    use spvirit_codec::spvd_decode::DecodedValue;
    use spvirit_types::NtPayload;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// A source whose `claim` blocks for `delay`, counting calls and
    /// recording the `RequestContext` each call observed.
    struct SlowSource {
        delay: Duration,
        calls: AtomicUsize,
        live: AtomicUsize,
        peak_live: AtomicUsize,
        seen_ctx: StdMutex<Vec<Option<crate::request_ctx::RequestContext>>>,
    }

    impl SlowSource {
        fn new(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                delay,
                calls: AtomicUsize::new(0),
                live: AtomicUsize::new(0),
                peak_live: AtomicUsize::new(0),
                seen_ctx: StdMutex::new(Vec::new()),
            })
        }
    }

    impl Source for SlowSource {
        fn try_claim(&self, _name: &str) -> TryClaim {
            TryClaim::Unknown
        }

        fn claim(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_ctx
                .lock()
                .unwrap()
                .push(crate::request_ctx::current_request());
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_live.fetch_max(live, Ordering::SeqCst);
            Box::pin(async move {
                tokio::time::sleep(self.delay).await;
                self.live.fetch_sub(1, Ordering::SeqCst);
                None
            })
        }

        fn get(&self, _n: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn put(
            &self,
            _n: &str,
            _v: &DecodedValue,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>>
        {
            Box::pin(async { Err("no".to_string()) })
        }

        fn subscribe(
            &self,
            _n: &str,
        ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
            Box::pin(async { Vec::new() })
        }
    }

    async fn registry_with(src: Arc<SlowSource>) -> Arc<SourceRegistry> {
        let reg = Arc::new(SourceRegistry::new());
        reg.add("slow", 0, src).await;
        reg
    }

    /// A source whose `claim` always panics — stands in for a proxying
    /// source choking on malformed upstream bytes.
    struct PanicSource;

    impl Source for PanicSource {
        fn try_claim(&self, _name: &str) -> TryClaim {
            TryClaim::Unknown
        }

        fn claim(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
            Box::pin(async { panic!("PanicSource always panics") })
        }

        fn get(&self, _n: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn put(
            &self,
            _n: &str,
            _v: &DecodedValue,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>>
        {
            Box::pin(async { Err("no".to_string()) })
        }

        fn subscribe(
            &self,
            _n: &str,
        ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
            Box::pin(async { Vec::new() })
        }
    }

    async fn registry_with_panic() -> Arc<SourceRegistry> {
        let reg = Arc::new(SourceRegistry::new());
        reg.add("panic", 0, Arc::new(PanicSource)).await;
        reg
    }

    #[tokio::test]
    async fn enqueue_returns_immediately_and_resolves_in_the_background() {
        let src = SlowSource::new(Duration::from_millis(150));
        let resolver = SearchResolver::new(registry_with(src.clone()).await);

        let before = Instant::now();
        resolver.enqueue("SLOW:PV");
        // The whole point: enqueue does not wait for the 150ms claim.
        assert!(
            before.elapsed() < Duration::from_millis(20),
            "enqueue blocked for {:?}",
            before.elapsed()
        );

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(src.calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.stats().completed_missing, 1);
    }

    #[tokio::test]
    async fn the_same_name_in_flight_is_resolved_only_once() {
        let src = SlowSource::new(Duration::from_millis(200));
        let resolver = SearchResolver::new(registry_with(src.clone()).await);

        // A blocked client retries its search at roughly 5Hz. Those retries
        // must not multiply upstream work — that is what made the observed
        // outage self-sustaining.
        for _ in 0..20 {
            resolver.enqueue("SLOW:PV");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(src.calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.stats().deduped, 19);
    }

    #[tokio::test]
    async fn a_name_can_be_resolved_again_once_its_resolution_finishes() {
        let src = SlowSource::new(Duration::from_millis(50));
        let resolver = SearchResolver::new(registry_with(src.clone()).await);

        resolver.enqueue("SLOW:PV");
        tokio::time::sleep(Duration::from_millis(200)).await;
        resolver.enqueue("SLOW:PV");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(src.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_flood_of_distinct_names_is_capped_and_sheds_the_excess() {
        let src = SlowSource::new(Duration::from_millis(300));
        let resolver = SearchResolver::new(registry_with(src.clone()).await);

        for i in 0..200 {
            resolver.enqueue(&format!("FLOOD:{i}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(
            src.peak_live.load(Ordering::SeqCst) <= RESOLVE_CONCURRENCY,
            "peak concurrent claims {} exceeded cap {}",
            src.peak_live.load(Ordering::SeqCst),
            RESOLVE_CONCURRENCY
        );
        let stats = resolver.stats();
        assert_eq!(stats.started, RESOLVE_CONCURRENCY as u64);
        assert_eq!(stats.dropped_full, 200 - RESOLVE_CONCURRENCY as u64);
    }

    #[tokio::test]
    async fn the_worker_sees_the_identity_of_the_task_that_enqueued() {
        let src = SlowSource::new(Duration::from_millis(10));
        let resolver = SearchResolver::new(registry_with(src.clone()).await);
        let peer: std::net::SocketAddr = "10.1.2.3:5075".parse().unwrap();

        crate::request_ctx::scope(peer, async {
            crate::request_ctx::set_credentials(Some("operator1".into()), None);
            resolver.enqueue("SCOPED:PV");
        })
        .await;

        tokio::time::sleep(Duration::from_millis(200)).await;
        let seen = src.seen_ctx.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let ctx = seen[0]
            .as_ref()
            .expect("resolution ran with no request context — an ACL would fail closed");
        assert_eq!(ctx.peer, peer);
        assert_eq!(ctx.user.as_deref(), Some("operator1"));
    }

    #[tokio::test]
    async fn resolution_outside_a_request_scope_still_runs() {
        let src = SlowSource::new(Duration::from_millis(10));
        let resolver = SearchResolver::new(registry_with(src.clone()).await);
        resolver.enqueue("UNSCOPED:PV");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(src.calls.load(Ordering::SeqCst), 1);
        assert!(src.seen_ctx.lock().unwrap()[0].is_none());
    }

    #[tokio::test]
    async fn a_panicking_resolve_still_frees_its_inflight_slot() {
        let resolver = SearchResolver::new(registry_with_panic().await);

        resolver.enqueue("PANIC:PV");
        tokio::time::sleep(Duration::from_millis(100)).await;

        // If the `inflight` guard didn't run on unwind, this second enqueue
        // would dedup forever instead of starting a fresh resolution.
        resolver.enqueue("PANIC:PV");
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stats = resolver.stats();
        assert_eq!(
            stats.started, 2,
            "name was stranded in `inflight` after the first claim panicked"
        );
        assert_eq!(stats.deduped, 0);
    }

    /// A-F5: a poisoned `inflight` mutex must not disable the resolver.
    ///
    /// `.lock().unwrap()` here would panic on the *search task* for every
    /// subsequent `enqueue` — the total search denial this module exists to
    /// prevent — and in `InflightGuard::drop` it would panic during unwind,
    /// aborting the process. Same ruling as `SourceRegistry::lock_resolved`.
    #[tokio::test]
    async fn a_poisoned_inflight_lock_does_not_disable_the_resolver() {
        let src = SlowSource::new(Duration::from_millis(10));
        let resolver = SearchResolver::new(registry_with(src.clone()).await);

        // Poison the mutex the only way it can be poisoned: unwind while
        // holding the guard.
        let inflight = resolver.inflight.clone();
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = inflight.lock().unwrap();
            panic!("deliberate poison");
        }));
        std::panic::set_hook(prev_hook);
        assert!(inflight.is_poisoned(), "test failed to poison the mutex");

        // The enqueue path must still take the lock and start the work...
        resolver.enqueue("POISON:PV");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(resolver.stats().started, 1, "enqueue refused to run");
        assert_eq!(src.calls.load(Ordering::SeqCst), 1);

        // ...and `InflightGuard::drop` must still have released the name, or
        // this second enqueue dedups instead of starting.
        resolver.enqueue("POISON:PV");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let stats = resolver.stats();
        assert_eq!(
            stats.started, 2,
            "the guard did not release the name under a poisoned lock"
        );
        assert_eq!(stats.deduped, 0);
    }

    #[tokio::test]
    async fn a_completed_resolve_records_its_outcome_in_the_registry() {
        // SlowSource never answers try_claim (always Unknown), the same shape
        // as an uncached in-tree source. Build one that actually finds the
        // name so the memo has something true to remember.
        struct FoundSlowSource(Arc<SlowSource>);

        impl Source for FoundSlowSource {
            fn try_claim(&self, name: &str) -> TryClaim {
                self.0.try_claim(name)
            }

            fn claim(
                &self,
                _name: &str,
            ) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
                self.0.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    tokio::time::sleep(self.0.delay).await;
                    Some(PvInfo {
                        descriptor: spvirit_codec::spvd_decode::StructureDesc::default(),
                        writable: false,
                    })
                })
            }

            fn get(&self, n: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
                self.0.get(n)
            }

            fn put(
                &self,
                n: &str,
                v: &DecodedValue,
            ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>>
            {
                self.0.put(n, v)
            }

            fn subscribe(
                &self,
                n: &str,
            ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
                self.0.subscribe(n)
            }

            fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
                self.0.names()
            }
        }

        let inner = SlowSource::new(Duration::from_millis(20));
        let reg = Arc::new(SourceRegistry::new());
        reg.add("found-slow", 0, Arc::new(FoundSlowSource(inner.clone())))
            .await;

        assert_eq!(
            reg.try_claim("FOUND:PV"),
            TryClaim::Unknown,
            "control: the source cannot answer without I/O"
        );

        // Exercised inside a real request scope, not unscoped: an unscoped
        // enqueue takes the `ctx == None` arm in the resolver's spawned task
        // and never proves the `Some(ctx)` / `scope_with` arm actually writes
        // a memo entry readable back under that same identity. Every other
        // test in this module (bar `the_worker_sees_the_identity_...`, which
        // doesn't touch the memo at all) runs unscoped, so without this the
        // suite would stay green even if `scope_with`'s memo write were
        // silently inert for every credentialed client.
        let peer: std::net::SocketAddr = "10.9.9.9:5075".parse().unwrap();
        let resolver = SearchResolver::new(reg.clone());
        crate::request_ctx::scope(peer, async {
            resolver.enqueue("FOUND:PV");
            tokio::time::sleep(Duration::from_millis(200)).await;

            assert_eq!(resolver.stats().completed_found, 1);
            assert_eq!(
                reg.try_claim("FOUND:PV"),
                TryClaim::Yes,
                "the resolver's outcome must be readable, under the same identity, \
                 without another round trip"
            );
        })
        .await;
    }

    /// The `/metrics` endpoint reads a process-global rather than a
    /// per-resolver handle (see the plan's Task 6 ruling), so the global must
    /// actually advance when work happens.
    ///
    /// The global is shared with every other test in this binary, so a bare
    /// `after > before` proves nothing — it stayed true with this test's own
    /// enqueues deleted, and true under a mutant that removed the increment
    /// from `enqueue` entirely. Two things fix that. The per-resolver
    /// counters are *exact* and private to this resolver, so only this test's
    /// own work can satisfy them. And the global deltas are asserted against
    /// counts greater than one: neighbours can only push a delta up, never
    /// down, so if the production increment is gone the delta is zero no
    /// matter who else is running.
    #[tokio::test]
    async fn the_global_counters_advance_with_real_work() {
        const DISTINCT: u64 = 5; // under RESOLVE_CONCURRENCY, so none are shed
        const DUPES: u64 = 3;

        let before = global_stats();
        let src = SlowSource::new(Duration::from_millis(50));
        let resolver = SearchResolver::new(registry_with(src.clone()).await);
        for i in 0..DISTINCT {
            resolver.enqueue(&format!("GLOBAL:PV{i}"));
        }
        for _ in 0..DUPES {
            resolver.enqueue("GLOBAL:PV0"); // already inflight
        }
        tokio::time::sleep(Duration::from_millis(400)).await;

        // Exact, and attributable to nothing but this test.
        let own = resolver.stats();
        assert_eq!(own.started, DISTINCT, "own started");
        assert_eq!(own.deduped, DUPES, "own deduped");
        assert_eq!(own.dropped_full, 0, "own dropped_full");
        assert_eq!(own.completed_missing, DISTINCT, "own completed_missing");
        assert_eq!(src.calls.load(Ordering::SeqCst), DISTINCT as usize);

        // And the global carried at least that much through.
        let after = global_stats();
        for (label, delta, expected) in [
            ("started", after.started - before.started, DISTINCT),
            ("deduped", after.deduped - before.deduped, DUPES),
            (
                "completed_missing",
                after.completed_missing - before.completed_missing,
                DISTINCT,
            ),
        ] {
            assert!(
                delta >= expected,
                "global {label} advanced by {delta}, but this test alone did {expected}"
            );
        }
    }

    #[test]
    fn note_try_claim_counts_each_outcome_separately() {
        let before = global_stats();
        note_try_claim(TryClaim::Yes);
        note_try_claim(TryClaim::No);
        note_try_claim(TryClaim::No);
        note_try_claim(TryClaim::Unknown);
        let after = global_stats();
        assert!(after.try_claim_yes > before.try_claim_yes);
        assert!(after.try_claim_no >= before.try_claim_no + 2);
        assert!(after.try_claim_unknown > before.try_claim_unknown);
    }
}
