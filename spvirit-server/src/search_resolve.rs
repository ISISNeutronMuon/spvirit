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
use std::sync::{Arc, Mutex};
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
}

#[derive(Debug, Default)]
struct Stats {
    started: AtomicU64,
    deduped: AtomicU64,
    dropped_full: AtomicU64,
    completed_found: AtomicU64,
    completed_missing: AtomicU64,
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

/// Removes `name` from `inflight` when dropped — on normal completion, and,
/// crucially, on unwind if `claim` panics. Constructed before the `claim`
/// await so the removal runs unconditionally.
struct InflightGuard {
    inflight: Arc<Mutex<HashSet<Arc<str>>>>,
    name: Arc<str>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.inflight.lock().unwrap().remove(&self.name);
    }
}

impl SearchResolver {
    pub fn new(sources: Arc<SourceRegistry>) -> Self {
        Self {
            sources,
            permits: Arc::new(Semaphore::new(RESOLVE_CONCURRENCY)),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            stats: Arc::new(Stats::default()),
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
            return;
        };
        // A single allocation, shared between the `inflight` set and the
        // spawned task via cheap `Arc` clones (refcount bumps, not copies).
        let name: Arc<str> = Arc::from(name);
        {
            let mut inflight = self.inflight.lock().unwrap();
            if !inflight.insert(name.clone()) {
                self.stats.deduped.fetch_add(1, Ordering::Relaxed);
                return; // `permit` drops here, releasing it.
            }
        }
        self.stats.started.fetch_add(1, Ordering::Relaxed);

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
            } else {
                stats.completed_missing.fetch_add(1, Ordering::Relaxed);
            }
            tracing::debug!(
                "search resolve: '{name}' found={found} in {:?}",
                started.elapsed()
            );
        });
    }

    pub fn stats(&self) -> ResolveStatsSnapshot {
        ResolveStatsSnapshot {
            started: self.stats.started.load(Ordering::Relaxed),
            deduped: self.stats.deduped.load(Ordering::Relaxed),
            dropped_full: self.stats.dropped_full.load(Ordering::Relaxed),
            completed_found: self.stats.completed_found.load(Ordering::Relaxed),
            completed_missing: self.stats.completed_missing.load(Ordering::Relaxed),
        }
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
}
