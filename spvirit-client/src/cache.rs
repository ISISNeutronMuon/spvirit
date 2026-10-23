//! Resolution and connection caching for [`crate::pva_client::PvaClient`].

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use crate::client::{ValidatedConn, connect_and_validate, create_channel_on};
use crate::types::{PvGetError, PvOptions};

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

/// Record that an operation was retried on a fresh connection after a pooled
/// one turned out to be dead.
pub(crate) fn bump_retried_dead(counters: &CacheCounters) {
    bump(&counters.conn_retried_dead);
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
    /// The slot generation this entry was resolved under. An `invalidate`
    /// bumps the slot's generation, so an older entry reads as a miss.
    generation: u64,
}

/// One PV's cache slot. The map key is never removed by `invalidate`: that
/// would let a caller arriving mid-resolve create a fresh slot and start a
/// second, concurrent resolve. Instead `invalidate` bumps `generation`
/// (synchronously, without waiting for an in-flight resolver to release
/// `entry`), and any entry from an older generation is treated as absent.
#[derive(Debug, Default)]
struct Slot {
    generation: AtomicU64,
    entry: tokio::sync::Mutex<Option<Entry>>,
}

/// PV name -> [`Resolved`], with a TTL and single-flight.
///
/// The outer `std::sync::Mutex` guards only the map and is never held across
/// an `.await`; the per-PV `tokio::sync::Mutex` is what serialises racers so
/// that N concurrent lookups of one uncached PV perform exactly one search.
#[derive(Debug)]
pub(crate) struct ResolutionCache {
    ttl: Option<Duration>,
    slots: std::sync::Mutex<HashMap<String, Arc<Slot>>>,
}

impl ResolutionCache {
    pub(crate) fn new(ttl: Option<Duration>) -> Self {
        Self {
            ttl,
            slots: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn slot(&self, pv_name: &str) -> Arc<Slot> {
        let mut slots = self.slots.lock().expect("resolution cache mutex poisoned");
        Arc::clone(slots.entry(pv_name.to_string()).or_default())
    }

    pub(crate) fn invalidate(&self, pv_name: &str) {
        let slots = self.slots.lock().expect("resolution cache mutex poisoned");
        if let Some(slot) = slots.get(pv_name) {
            slot.generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Test hook: the cached address for `pv_name`, without touching the TTL
    /// or the counters.
    #[cfg(test)]
    pub(crate) fn peek_addr(&self, pv_name: &str) -> Option<SocketAddr> {
        let slots = self.slots.lock().expect("resolution cache mutex poisoned");
        let slot = slots.get(pv_name)?.clone();
        drop(slots);
        let guard = slot.entry.try_lock().ok()?;
        guard.map(|e| e.resolved.addr)
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
        let mut guard = slot.entry.lock().await;
        let generation = slot.generation.load(Ordering::SeqCst);

        if let Some(entry) = *guard
            && entry.generation == generation
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
            generation,
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
    pool: ConnPool,
}

impl ClientCache {
    pub(crate) fn new(cfg: CacheConfig) -> Self {
        Self {
            resolution: ResolutionCache::new(cfg.resolve_ttl),
            counters: CacheCounters::default(),
            pool: ConnPool::new(cfg.conn_idle_ttl, cfg.pool_max_per_addr),
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

    /// Take a connection to `resolved.addr` — from the pool if one is idle
    /// and fresh enough, otherwise newly dialled and validated.
    pub(crate) async fn acquire(
        &self,
        opts: &PvOptions,
        resolved: Resolved,
    ) -> Result<PooledConn, PvGetError> {
        if let Some(c) = self.pool.checkout(resolved.addr, &self.counters) {
            return Ok(c);
        }
        let conn = connect_and_validate(resolved.addr, resolved.guid, opts).await?;
        Ok(PooledConn::fresh(conn))
    }

    /// Dial a new connection to `resolved.addr`, bypassing the pool.
    ///
    /// Used for the retry after a *reused* connection failed: another idle
    /// connection to the same server may be just as dead (a server restart
    /// kills them all), so the retry must not draw from the pool again.
    /// Counted as a `conn_misses`, like any other dial.
    pub(crate) async fn acquire_fresh(
        &self,
        opts: &PvOptions,
        resolved: Resolved,
    ) -> Result<PooledConn, PvGetError> {
        bump(&self.counters.conn_misses);
        let conn = connect_and_validate(resolved.addr, resolved.guid, opts).await?;
        Ok(PooledConn::fresh(conn))
    }

    /// Return a connection that completed its operation successfully.
    pub(crate) fn release(&self, conn: PooledConn) {
        self.pool.checkin(conn);
    }

    pub(crate) fn counters(&self) -> &CacheCounters {
        &self.counters
    }

    /// Forget the resolution for `pv_name` *and* close every idle connection
    /// to the address it resolved to, if that address is known.
    pub(crate) fn invalidate_at(&self, pv_name: &str, addr: SocketAddr) {
        self.resolution.invalidate(pv_name);
        self.pool.purge_addr(addr);
    }

    /// Test hook: see `PvaClient::close_pooled_for_test`.
    ///
    /// `tokio::net::TcpStream` has no synchronous `shutdown(Shutdown)`, so this
    /// borrows the raw socket, shuts it down through a temporary
    /// `std::net::TcpStream`, and `mem::forget`s that temporary so the fd is
    /// not closed twice — the pool still owns it. The server sees EOF and drops
    /// its side, but the `PooledConn` stays in the pool and is handed out again,
    /// which is exactly the "reused a dead connection" case under test.
    #[cfg(test)]
    pub(crate) fn close_pooled_for_test(&self, pv_name: &str) {
        let Some(addr) = self.resolution.peek_addr(pv_name) else {
            return;
        };
        let idle = self.pool.idle.lock().expect("conn pool mutex poisoned");
        if let Some(stack) = idle.get(&addr) {
            for c in stack {
                #[cfg(unix)]
                {
                    use std::os::fd::{AsRawFd, FromRawFd};
                    let s = unsafe { std::net::TcpStream::from_raw_fd(c.conn.stream.as_raw_fd()) };
                    let _ = s.shutdown(std::net::Shutdown::Both);
                    std::mem::forget(s); // do not close the fd; the pool owns it
                }
                #[cfg(windows)]
                {
                    use std::os::windows::io::{AsRawSocket, FromRawSocket};
                    let s = unsafe {
                        std::net::TcpStream::from_raw_socket(c.conn.stream.as_raw_socket())
                    };
                    let _ = s.shutdown(std::net::Shutdown::Both);
                    std::mem::forget(s); // do not close the socket; the pool owns it
                }
            }
        }
    }

    /// Test hook: `(channels, next_cid)` for every idle pooled connection to
    /// the address `pv_name` currently resolves to.
    #[cfg(test)]
    pub(crate) fn pooled_channels_for_test(
        &self,
        pv_name: &str,
    ) -> Vec<(HashMap<String, u32>, u32)> {
        let Some(addr) = self.resolution.peek_addr(pv_name) else {
            return Vec::new();
        };
        let idle = self.pool.idle.lock().expect("conn pool mutex poisoned");
        idle.get(&addr)
            .map(|stack| {
                stack
                    .iter()
                    .map(|c| (c.channels.clone(), c.next_cid))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Test hook: on every idle pooled connection that carries a channel for
    /// `pv_name`, send the server a client DESTROY_CHANNEL for it while
    /// keeping the sid cached client-side. The next operation on that
    /// connection therefore uses a sid the server no longer knows — the
    /// "server destroyed a pooled channel" case.
    #[cfg(test)]
    pub(crate) async fn destroy_pooled_channel_for_test(&self, pv_name: &str) {
        use tokio::io::AsyncWriteExt;
        let Some(addr) = self.resolution.peek_addr(pv_name) else {
            return;
        };
        // Take the connections out so the std mutex is not held across the
        // writes, then put them back unchanged (same `idle_since`).
        let taken = {
            let mut idle = self.pool.idle.lock().expect("conn pool mutex poisoned");
            idle.remove(&addr).unwrap_or_default()
        };
        let mut back = Vec::with_capacity(taken.len());
        for mut c in taken {
            if let Some(&sid) = c.channels.get(pv_name) {
                let is_be = c.conn.is_be;
                let mut frame = spvirit_codec::spvirit_encode::encode_header(
                    false,
                    is_be,
                    false,
                    c.conn.version,
                    8,
                    8,
                );
                // DESTROY_CHANNEL payload: sid, then cid. cid 0 is NOT the
                // channel's real cid: the server's
                // `remove_channel_if_current(0, sid)` drops every sid-keyed
                // row (which is all an op lookup reads, so the next op gets
                // "Unknown SID") but leaves `cid_to_sid[real cid]` pointing
                // at the dead sid. Not byte-for-byte what a real destroy
                // leaves behind, but the state this test needs.
                let cid = 0u32;
                if is_be {
                    frame.extend_from_slice(&sid.to_be_bytes());
                    frame.extend_from_slice(&cid.to_be_bytes());
                } else {
                    frame.extend_from_slice(&sid.to_le_bytes());
                    frame.extend_from_slice(&cid.to_le_bytes());
                }
                c.conn
                    .stream
                    .write_all(&frame)
                    .await
                    .expect("write DESTROY_CHANNEL");
            }
            back.push(c);
        }
        let mut idle = self.pool.idle.lock().expect("conn pool mutex poisoned");
        idle.insert(addr, back);
    }
}

/// A validated connection plus the channels already created on it.
///
/// The pool's unit of storage. A connection is checked out *exclusively*:
/// only one operation uses it at a time, so no reader task and no ioid
/// demultiplexing is needed. (See "No multiplexing" in the Design section for
/// the response-matching rule that exclusivity alone does not give you.)
///
/// No `#[derive(Debug)]`: it holds a `ValidatedConn`, which holds a
/// `SegmentReassembler`, which does not implement `Debug`
/// (`spvirit-codec/src/segment.rs:34`). Same for `ConnPool` below, and for
/// `ClientCache`, which Task 2 already left underived. Nothing formats any of
/// them: `PvaClient`'s hand-written `Debug` prints `self.cache.stats()` (a
/// plain `CacheStats`), not the cache itself.
pub(crate) struct PooledConn {
    pub(crate) conn: ValidatedConn,
    /// PV name -> server-side sid, for channels already created here. The
    /// spvirit server allocates one sid per cid in a CreateChannel batch
    /// (spvirit-server/src/handler.rs:1383), so one connection can carry
    /// several PVs.
    channels: HashMap<String, u32>,
    /// Next client-side channel id to hand out on this connection. Must be
    /// unique for the connection's lifetime.
    next_cid: u32,
    /// When this connection was last checked in. Only meaningful while it
    /// sits in the pool.
    idle_since: Instant,
    /// False for a connection dialled for this operation, true for one taken
    /// from the pool. Only a reused connection may be retried after failure.
    reused: bool,
}

impl PooledConn {
    /// Wrap a just-dialled connection. Never eligible for a retry.
    pub(crate) fn fresh(conn: ValidatedConn) -> Self {
        Self {
            conn,
            channels: HashMap::new(),
            next_cid: 1,
            idle_since: Instant::now(),
            reused: false,
        }
    }

    pub(crate) fn was_reused(&self) -> bool {
        self.reused
    }

    /// The server-side sid for `pv_name` on this connection, creating the
    /// channel if this connection does not have one yet.
    pub(crate) async fn channel_sid(
        &mut self,
        pv_name: &str,
        timeout_dur: Duration,
    ) -> Result<u32, PvGetError> {
        if let Some(sid) = self.channels.get(pv_name) {
            return Ok(*sid);
        }
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        let sid = create_channel_on(&mut self.conn, pv_name, cid, timeout_dur).await?;
        self.channels.insert(pv_name.to_string(), sid);
        Ok(sid)
    }
}

/// Server address -> a small LIFO stack of idle validated connections.
///
/// The mutex guards only the map and is never held across an `.await`.
pub(crate) struct ConnPool {
    idle_ttl: Option<Duration>,
    max_per_addr: usize,
    idle: std::sync::Mutex<HashMap<SocketAddr, Vec<PooledConn>>>,
}

impl ConnPool {
    pub(crate) fn new(idle_ttl: Option<Duration>, max_per_addr: usize) -> Self {
        Self {
            idle_ttl,
            max_per_addr,
            idle: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Take an idle connection for `addr`, dropping any that have sat longer
    /// than the idle TTL. Returns `None` when the pool is cold or disabled.
    pub(crate) fn checkout(
        &self,
        addr: SocketAddr,
        counters: &CacheCounters,
    ) -> Option<PooledConn> {
        let Some(ttl) = self.idle_ttl else {
            bump(&counters.conn_misses);
            return None;
        };

        let mut idle = self.idle.lock().expect("conn pool mutex poisoned");
        // A `?` here would return `None` without bumping `conn_misses`, which
        // makes the counter the tests assert on wrong. Bump, then return.
        let Some(stack) = idle.get_mut(&addr) else {
            bump(&counters.conn_misses);
            return None;
        };

        // LIFO: the most recently checked-in connection is the most likely
        // to still be alive, so pop from the back.
        while let Some(mut c) = stack.pop() {
            if c.idle_since.elapsed() >= ttl {
                // Dropping `c` closes its socket.
                bump(&counters.conn_evicted_idle);
                continue;
            }
            c.reused = true;
            bump(&counters.conn_hits);
            return Some(c);
        }

        bump(&counters.conn_misses);
        None
    }

    /// Return a healthy connection to the pool. A connection that failed
    /// must simply be dropped instead of checked in.
    pub(crate) fn checkin(&self, mut conn: PooledConn) {
        if self.idle_ttl.is_none() || self.max_per_addr == 0 {
            return;
        }
        let addr = conn.conn.server_addr;
        conn.idle_since = Instant::now();
        conn.reused = false;
        let mut idle = self.idle.lock().expect("conn pool mutex poisoned");
        let stack = idle.entry(addr).or_default();
        if stack.len() < self.max_per_addr {
            stack.push(conn);
        }
        // Over the cap: `conn` is dropped here, closing the socket.
    }

    /// Close and forget every idle connection to `addr`.
    pub(crate) fn purge_addr(&self, addr: SocketAddr) {
        let mut idle = self.idle.lock().expect("conn pool mutex poisoned");
        idle.remove(&addr);
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

    /// Review M2: an `invalidate` while a resolve is in flight must not let
    /// a later caller start a second, concurrent resolve of the same PV. The
    /// later caller queues behind the leader and, because the leader's result
    /// predates the invalidate, resolves again only once the leader is done.
    #[tokio::test]
    async fn invalidate_during_an_in_flight_resolve_keeps_single_flight() {
        let cache = Arc::new(ResolutionCache::new(Some(Duration::from_secs(300))));
        let counters = Arc::new(CacheCounters::default());
        let fake = Arc::new(CountingResolver::default());
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
        while fake.calls() == 0 {
            tokio::task::yield_now().await;
        }

        cache.invalidate("A:PV");

        let t2 = {
            let (cache, counters, fake) = (cache.clone(), counters.clone(), fake.clone());
            tokio::spawn(async move {
                cache
                    .get_or_resolve("A:PV", &counters, || async {
                        fake.calls.fetch_add(1, Ordering::SeqCst);
                        Ok(resolved(5099))
                    })
                    .await
            })
        };
        // Give t2 every chance to run; it must park on the slot lock rather
        // than start its own resolve while the leader is still in flight.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            fake.calls(),
            1,
            "a caller arriving after invalidate must not resolve concurrently"
        );

        gate.notify_one();
        assert_eq!(t1.await.unwrap().unwrap(), resolved(5075));
        assert_eq!(
            t2.await.unwrap().unwrap(),
            resolved(5099),
            "the leader's pre-invalidate result must not be served as a hit"
        );
        assert_eq!(fake.calls(), 2);

        // The post-invalidate result is what is cached now.
        let got = cache
            .get_or_resolve("A:PV", &counters, || async {
                fake.calls.fetch_add(1, Ordering::SeqCst);
                Ok(resolved(1))
            })
            .await
            .unwrap();
        assert_eq!(got, resolved(5099));
        assert_eq!(fake.calls(), 2);
    }

    /// Review M4: a leader cancelled mid-resolve must not wedge its
    /// followers. The follower takes over and resolves itself. The timeout
    /// turns a hang into a failure instead of a stuck test run.
    #[tokio::test]
    async fn a_cancelled_leader_does_not_wedge_a_follower() {
        let cache = Arc::new(ResolutionCache::new(Some(Duration::from_secs(300))));
        let counters = Arc::new(CacheCounters::default());
        let fake = Arc::new(CountingResolver::default());

        let leader = {
            let (cache, counters, fake) = (cache.clone(), counters.clone(), fake.clone());
            tokio::spawn(async move {
                cache
                    .get_or_resolve("A:PV", &counters, || async {
                        fake.calls.fetch_add(1, Ordering::SeqCst);
                        std::future::pending::<()>().await;
                        Ok(resolved(1))
                    })
                    .await
            })
        };
        while fake.calls() == 0 {
            tokio::task::yield_now().await;
        }

        let follower = {
            let (cache, counters, fake) = (cache.clone(), counters.clone(), fake.clone());
            tokio::spawn(async move {
                cache
                    .get_or_resolve("A:PV", &counters, || async {
                        fake.calls.fetch_add(1, Ordering::SeqCst);
                        Ok(resolved(5075))
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;

        leader.abort();
        assert!(leader.await.unwrap_err().is_cancelled());

        let got = tokio::time::timeout(Duration::from_secs(5), follower)
            .await
            .expect("the follower must not hang after the leader is cancelled")
            .unwrap()
            .unwrap();
        assert_eq!(got, resolved(5075));
        assert_eq!(fake.calls(), 2, "the follower resolves for itself");
    }

    use tokio::net::{TcpListener, TcpStream};

    /// Build a `ValidatedConn` over a real loopback socket, with no PVA
    /// handshake. Enough for pool mechanics (which never read or write).
    async fn fake_conn(listener_addr: SocketAddr) -> crate::client::ValidatedConn {
        let stream = TcpStream::connect(listener_addr).await.expect("connect");
        crate::client::ValidatedConn {
            stream,
            version: 2,
            is_be: false,
            server_addr: listener_addr,
            guid: [0u8; 12],
            reassembler: spvirit_codec::SegmentReassembler::new(),
        }
    }

    async fn loopback_listener() -> (TcpListener, SocketAddr) {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let a = l.local_addr().expect("local_addr");
        (l, a)
    }

    #[tokio::test]
    async fn a_checked_in_connection_is_handed_back_on_the_next_checkout() {
        let (_l, addr) = loopback_listener().await;
        let pool = ConnPool::new(Some(Duration::from_secs(30)), 4);
        let counters = CacheCounters::default();

        assert!(
            pool.checkout(addr, &counters).is_none(),
            "cold pool is empty"
        );
        assert_eq!(counters.snapshot().conn_misses, 1);

        pool.checkin(PooledConn::fresh(fake_conn(addr).await));

        let got = pool.checkout(addr, &counters).expect("warm pool must hit");
        assert!(got.was_reused());
        assert_eq!(counters.snapshot().conn_hits, 1);
        assert_eq!(counters.snapshot().conn_misses, 1);
    }

    #[tokio::test]
    async fn connections_are_pooled_per_address() {
        let (_l1, a1) = loopback_listener().await;
        let (_l2, a2) = loopback_listener().await;
        let pool = ConnPool::new(Some(Duration::from_secs(30)), 4);
        let counters = CacheCounters::default();

        pool.checkin(PooledConn::fresh(fake_conn(a1).await));

        assert!(
            pool.checkout(a2, &counters).is_none(),
            "wrong address must miss"
        );
        assert!(
            pool.checkout(a1, &counters).is_some(),
            "right address must hit"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_connection_past_its_ttl_is_evicted_not_reused() {
        let (_l, addr) = loopback_listener().await;
        let pool = ConnPool::new(Some(Duration::from_secs(30)), 4);
        let counters = CacheCounters::default();

        pool.checkin(PooledConn::fresh(fake_conn(addr).await));
        tokio::time::advance(Duration::from_secs(31)).await;

        assert!(pool.checkout(addr, &counters).is_none());
        let s = counters.snapshot();
        assert_eq!(s.conn_evicted_idle, 1);
        assert_eq!(s.conn_misses, 1);
        assert_eq!(s.conn_hits, 0);
    }

    #[tokio::test]
    async fn the_pool_never_holds_more_than_max_per_addr() {
        let (_l, addr) = loopback_listener().await;
        let pool = ConnPool::new(Some(Duration::from_secs(30)), 2);
        let counters = CacheCounters::default();

        for _ in 0..5 {
            pool.checkin(PooledConn::fresh(fake_conn(addr).await));
        }

        assert!(pool.checkout(addr, &counters).is_some());
        assert!(pool.checkout(addr, &counters).is_some());
        assert!(pool.checkout(addr, &counters).is_none(), "cap is 2");
    }

    #[tokio::test]
    async fn a_disabled_pool_never_stores_anything() {
        let (_l, addr) = loopback_listener().await;
        let pool = ConnPool::new(None, 4);
        let counters = CacheCounters::default();

        pool.checkin(PooledConn::fresh(fake_conn(addr).await));
        assert!(pool.checkout(addr, &counters).is_none());
    }

    #[tokio::test]
    async fn purge_addr_drops_every_connection_to_that_server() {
        let (_l, addr) = loopback_listener().await;
        let pool = ConnPool::new(Some(Duration::from_secs(30)), 4);
        let counters = CacheCounters::default();

        pool.checkin(PooledConn::fresh(fake_conn(addr).await));
        pool.checkin(PooledConn::fresh(fake_conn(addr).await));
        pool.purge_addr(addr);

        assert!(pool.checkout(addr, &counters).is_none());
    }

    #[tokio::test]
    async fn a_fresh_connection_reports_that_it_was_not_reused() {
        let (_l, addr) = loopback_listener().await;
        let c = PooledConn::fresh(fake_conn(addr).await);
        assert!(!c.was_reused());
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
