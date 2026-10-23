//! Resolution and connection caching for [`crate::pva_client::PvaClient`].

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use crate::types::PvGetError;

/// Tuning for [`crate::pva_client::PvaClient`]'s resolution cache and
/// connection pool. A `None` TTL disables that layer completely, which
/// restores the pre-cache behaviour exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheConfig {
    /// How long a PV -> (addr, guid) resolution stays valid.
    pub resolve_ttl: Option<Duration>,
    /// How long an idle pooled TCP connection is kept before being closed.
    /// Deliberately below the ~60 s idle reaper common in EPICS servers, so
    /// we close first rather than discover a half-closed socket.
    pub conn_idle_ttl: Option<Duration>,
    /// Maximum idle connections retained per server address.
    pub pool_max_per_addr: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            resolve_ttl: Some(Duration::from_secs(300)),
            conn_idle_ttl: Some(Duration::from_secs(30)),
            pool_max_per_addr: 4,
        }
    }
}

impl CacheConfig {
    /// Both layers off: every operation performs a full search and dial.
    pub fn disabled() -> Self {
        Self {
            resolve_ttl: None,
            conn_idle_ttl: None,
            pool_max_per_addr: 0,
        }
    }
}

/// A point-in-time snapshot of the cache counters. Monotonic and saturating.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub resolve_hits: u64,
    pub resolve_misses: u64,
    pub conn_hits: u64,
    pub conn_misses: u64,
    pub conn_evicted_idle: u64,
    pub conn_retried_dead: u64,
}

/// The live counters behind [`CacheStats`].
#[derive(Debug, Default)]
pub(crate) struct CacheCounters {
    pub(crate) resolve_hits: AtomicU64,
    pub(crate) resolve_misses: AtomicU64,
    pub(crate) conn_hits: AtomicU64,
    pub(crate) conn_misses: AtomicU64,
    pub(crate) conn_evicted_idle: AtomicU64,
    pub(crate) conn_retried_dead: AtomicU64,
}

impl CacheCounters {
    pub(crate) fn snapshot(&self) -> CacheStats {
        CacheStats {
            resolve_hits: self.resolve_hits.load(Ordering::Relaxed),
            resolve_misses: self.resolve_misses.load(Ordering::Relaxed),
            conn_hits: self.conn_hits.load(Ordering::Relaxed),
            conn_misses: self.conn_misses.load(Ordering::Relaxed),
            conn_evicted_idle: self.conn_evicted_idle.load(Ordering::Relaxed),
            conn_retried_dead: self.conn_retried_dead.load(Ordering::Relaxed),
        }
    }
}

fn bump(c: &AtomicU64) {
    // Saturating rather than wrapping: a diagnostic counter that rolls over
    // to 0 is worse than one that sticks at u64::MAX.
    let _ = c.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
        Some(v.saturating_add(1))
    });
}

/// A resolved PV location: where the server is and which server it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub addr: SocketAddr,
    /// All-zero when resolved via the unicast shortcut, which has no search
    /// response to read a guid from.
    pub guid: [u8; 12],
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    resolved: Resolved,
    stored_at: Instant,
}

/// PV name -> [`Resolved`], with a TTL and single-flight.
///
/// The outer `std::sync::Mutex` guards only the map and is never held across
/// an `.await`; the per-PV `tokio::sync::Mutex` is what serialises racers so
/// that N concurrent lookups of one uncached PV perform exactly one search.
#[derive(Debug)]
pub(crate) struct ResolutionCache {
    ttl: Option<Duration>,
    slots: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<Entry>>>>>,
}

impl ResolutionCache {
    pub(crate) fn new(ttl: Option<Duration>) -> Self {
        Self {
            ttl,
            slots: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn slot(&self, pv_name: &str) -> Arc<tokio::sync::Mutex<Option<Entry>>> {
        let mut slots = self.slots.lock().expect("resolution cache mutex poisoned");
        Arc::clone(
            slots
                .entry(pv_name.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))),
        )
    }

    pub(crate) fn invalidate(&self, pv_name: &str) {
        let mut slots = self.slots.lock().expect("resolution cache mutex poisoned");
        slots.remove(pv_name);
    }

    /// Return the cached resolution for `pv_name`, or run `resolve` and cache
    /// its result. Concurrent callers for the same name are serialised, so
    /// only the first runs `resolve`.
    pub(crate) async fn get_or_resolve<F, Fut>(
        &self,
        pv_name: &str,
        counters: &CacheCounters,
        resolve: F,
    ) -> Result<Resolved, PvGetError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Resolved, PvGetError>>,
    {
        let Some(ttl) = self.ttl else {
            bump(&counters.resolve_misses);
            return resolve().await;
        };

        let slot = self.slot(pv_name);
        let mut guard = slot.lock().await;

        if let Some(entry) = *guard
            && entry.stored_at.elapsed() < ttl
        {
            bump(&counters.resolve_hits);
            return Ok(entry.resolved);
        }

        bump(&counters.resolve_misses);
        // A failure leaves the slot as it was (or empty), so the next caller
        // retries rather than inheriting a poisoned entry.
        let resolved = resolve().await?;
        *guard = Some(Entry {
            resolved,
            stored_at: Instant::now(),
        });
        Ok(resolved)
    }
}

/// The per-`PvaClient` cache: one resolution cache plus (from Task 4) one
/// connection pool, sharing one set of counters.
///
/// Deliberately no `#[derive(Debug)]`: Task 4 gives this a `ConnPool`, which
/// transitively holds a `spvirit_codec::SegmentReassembler`, and that type does
/// not implement `Debug` (`spvirit-codec/src/segment.rs:34`). Nothing formats
/// `ClientCache` — `PvaClient`'s hand-written `Debug` prints
/// `self.cache.stats()`, a plain `CacheStats`.
pub(crate) struct ClientCache {
    // Read only through `config()`, which nothing calls yet; no later task in
    // plan 2026-09-02-client-connection-cache names a caller either.
    #[allow(dead_code)]
    cfg: CacheConfig,
    counters: CacheCounters,
    resolution: ResolutionCache,
}

impl ClientCache {
    pub(crate) fn new(cfg: CacheConfig) -> Self {
        Self {
            resolution: ResolutionCache::new(cfg.resolve_ttl),
            counters: CacheCounters::default(),
            cfg,
        }
    }

    // Specified by Task 2 of plan 2026-09-02-client-connection-cache, but no
    // task in that plan calls it yet.
    #[allow(dead_code)]
    pub(crate) fn config(&self) -> CacheConfig {
        self.cfg
    }

    pub(crate) fn stats(&self) -> CacheStats {
        self.counters.snapshot()
    }

    pub(crate) fn invalidate(&self, pv_name: &str) {
        self.resolution.invalidate(pv_name);
    }

    /// Resolve `opts.pv_name` through the cache.
    pub(crate) async fn resolve(
        &self,
        opts: &crate::types::PvOptions,
    ) -> Result<Resolved, PvGetError> {
        self.resolution
            .get_or_resolve(&opts.pv_name, &self.counters, || async {
                let (addr, guid) = crate::search::resolve_pv_server(opts).await?;
                Ok(Resolved { addr, guid })
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    fn resolved(port: u16) -> Resolved {
        Resolved {
            addr: addr(port),
            guid: [7u8; 12],
        }
    }

    /// A counting fake resolver: records how many times it actually ran.
    #[derive(Default)]
    struct CountingResolver {
        calls: AtomicUsize,
    }
    impl CountingResolver {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[tokio::test]
    async fn second_lookup_of_the_same_pv_does_not_call_the_resolver() {
        let cache = ResolutionCache::new(Some(Duration::from_secs(300)));
        let counters = CacheCounters::default();
        let fake = CountingResolver::default();

        for _ in 0..2 {
            let got = cache
                .get_or_resolve("A:PV", &counters, || async {
                    fake.calls.fetch_add(1, Ordering::SeqCst);
                    Ok(resolved(5075))
                })
                .await
                .expect("resolve");
            assert_eq!(got, resolved(5075));
        }

        assert_eq!(
            fake.calls(),
            1,
            "the second lookup must be served from cache"
        );
        let stats = counters.snapshot();
        assert_eq!(stats.resolve_misses, 1);
        assert_eq!(stats.resolve_hits, 1);
    }

    #[tokio::test]
    async fn distinct_pv_names_are_cached_independently() {
        let cache = ResolutionCache::new(Some(Duration::from_secs(300)));
        let counters = CacheCounters::default();
        let fake = CountingResolver::default();

        let a = cache
            .get_or_resolve("A:PV", &counters, || async {
                fake.calls.fetch_add(1, Ordering::SeqCst);
                Ok(resolved(5075))
            })
            .await
            .unwrap();
        let b = cache
            .get_or_resolve("B:PV", &counters, || async {
                fake.calls.fetch_add(1, Ordering::SeqCst);
                Ok(resolved(5076))
            })
            .await
            .unwrap();

        assert_eq!(a.addr, addr(5075));
        assert_eq!(b.addr, addr(5076));
        assert_eq!(fake.calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn an_entry_older_than_the_ttl_is_re_resolved() {
        let cache = ResolutionCache::new(Some(Duration::from_secs(300)));
        let counters = CacheCounters::default();
        let fake = CountingResolver::default();

        cache
            .get_or_resolve("A:PV", &counters, || async {
                fake.calls.fetch_add(1, Ordering::SeqCst);
                Ok(resolved(5075))
            })
            .await
            .unwrap();

        tokio::time::advance(Duration::from_secs(301)).await;

        let got = cache
            .get_or_resolve("A:PV", &counters, || async {
                fake.calls.fetch_add(1, Ordering::SeqCst);
                Ok(resolved(5099))
            })
            .await
            .unwrap();

        assert_eq!(got.addr, addr(5099), "the stale entry must be replaced");
        assert_eq!(fake.calls(), 2);
        assert_eq!(counters.snapshot().resolve_misses, 2);
    }

    #[tokio::test]
    async fn two_racing_lookups_of_one_pv_produce_exactly_one_resolve() {
        let cache = Arc::new(ResolutionCache::new(Some(Duration::from_secs(300))));
        let counters = Arc::new(CacheCounters::default());
        let fake = Arc::new(CountingResolver::default());

        // A barrier the resolver waits on, so the second caller is
        // guaranteed to arrive while the first is still in flight. This is a
        // synchronisation point, not a sleep. `notify_one` (not
        // `notify_waiters`) is required: it stores a permit, so the wake-up
        // is not lost if the resolver has not yet parked on `notified()`.
        let gate = Arc::new(tokio::sync::Notify::new());

        let t1 = {
            let (cache, counters, fake, gate) =
                (cache.clone(), counters.clone(), fake.clone(), gate.clone());
            tokio::spawn(async move {
                cache
                    .get_or_resolve("A:PV", &counters, || async {
                        fake.calls.fetch_add(1, Ordering::SeqCst);
                        gate.notified().await;
                        Ok(resolved(5075))
                    })
                    .await
            })
        };

        // Wait until the first caller is inside the resolver.
        while fake.calls() == 0 {
            tokio::task::yield_now().await;
        }

        let t2 = {
            let (cache, counters, fake) = (cache.clone(), counters.clone(), fake.clone());
            tokio::spawn(async move {
                cache
                    .get_or_resolve("A:PV", &counters, || async {
                        fake.calls.fetch_add(1, Ordering::SeqCst);
                        Ok(resolved(9999))
                    })
                    .await
            })
        };

        // Let the in-flight resolve finish; the waiter must then see its value.
        tokio::task::yield_now().await;
        gate.notify_one();

        let a = t1.await.unwrap().unwrap();
        let b = t2.await.unwrap().unwrap();

        assert_eq!(a, resolved(5075));
        assert_eq!(
            b,
            resolved(5075),
            "the racer must observe the winner's result"
        );
        assert_eq!(fake.calls(), 1, "single-flight: exactly one resolve");
        assert_eq!(counters.snapshot().resolve_hits, 1);
    }

    #[tokio::test]
    async fn a_disabled_cache_resolves_every_time() {
        let cache = ResolutionCache::new(None);
        let counters = CacheCounters::default();
        let fake = CountingResolver::default();

        for _ in 0..3 {
            cache
                .get_or_resolve("A:PV", &counters, || async {
                    fake.calls.fetch_add(1, Ordering::SeqCst);
                    Ok(resolved(5075))
                })
                .await
                .unwrap();
        }

        assert_eq!(fake.calls(), 3);
        assert_eq!(counters.snapshot().resolve_hits, 0);
        assert_eq!(counters.snapshot().resolve_misses, 3);
    }

    #[tokio::test]
    async fn invalidate_forces_the_next_lookup_to_resolve_again() {
        let cache = ResolutionCache::new(Some(Duration::from_secs(300)));
        let counters = CacheCounters::default();
        let fake = CountingResolver::default();

        cache
            .get_or_resolve("A:PV", &counters, || async {
                fake.calls.fetch_add(1, Ordering::SeqCst);
                Ok(resolved(5075))
            })
            .await
            .unwrap();

        cache.invalidate("A:PV");

        let got = cache
            .get_or_resolve("A:PV", &counters, || async {
                fake.calls.fetch_add(1, Ordering::SeqCst);
                Ok(resolved(5099))
            })
            .await
            .unwrap();

        assert_eq!(got.addr, addr(5099));
        assert_eq!(fake.calls(), 2);
    }

    #[tokio::test]
    async fn a_failed_resolve_is_not_cached() {
        let cache = ResolutionCache::new(Some(Duration::from_secs(300)));
        let counters = CacheCounters::default();
        let fake = CountingResolver::default();

        let err = cache
            .get_or_resolve("A:PV", &counters, || async {
                fake.calls.fetch_add(1, Ordering::SeqCst);
                Err(PvGetError::Search("no responder"))
            })
            .await;
        assert!(err.is_err());

        let ok = cache
            .get_or_resolve("A:PV", &counters, || async {
                fake.calls.fetch_add(1, Ordering::SeqCst);
                Ok(resolved(5075))
            })
            .await
            .unwrap();

        assert_eq!(ok, resolved(5075));
        assert_eq!(fake.calls(), 2, "a failure must not poison the slot");
    }

    #[test]
    fn default_config_enables_both_layers_and_disabled_turns_both_off() {
        let d = CacheConfig::default();
        assert_eq!(d.resolve_ttl, Some(Duration::from_secs(300)));
        assert_eq!(d.conn_idle_ttl, Some(Duration::from_secs(30)));
        assert_eq!(d.pool_max_per_addr, 4);

        let off = CacheConfig::disabled();
        assert_eq!(off.resolve_ttl, None);
        assert_eq!(off.conn_idle_ttl, None);
    }
}
