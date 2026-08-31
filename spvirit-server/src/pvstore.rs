//! The [`Source`] trait — an object-safe abstraction over any PV data source,
//! and [`SourceRegistry`] — a dynamic, priority-ordered collection of sources.
//!
//! Protocol handlers use `SourceRegistry` to resolve PV names across multiple
//! registered sources, allowing different backends (in-memory records, hardware
//! drivers, proxies, etc.) to coexist in a single PVA server. basically what pvxs does with its provider registry.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use spvirit_codec::spvd_decode::{DecodedValue, StructureDesc};
use spvirit_types::NtPayload;
use tokio::sync::{RwLock, mpsc};
use tracing::debug;

/// How long a *positive* resolver outcome stays authoritative.
///
/// Short on purpose. The memo exists to make the client's *next* search
/// decisive, not to be a durable name cache: a PV that appears after the
/// server started must still become discoverable promptly, and a name whose
/// upstream has gone must stop being advertised. Worst-case discovery
/// latency for a brand-new PV that a client happened to search for just
/// before it was registered is one [`RESOLVED_NEGATIVE_TTL`], not this one —
/// see that constant.
const RESOLVED_TTL: Duration = Duration::from_secs(10);

/// How long a *negative* resolver outcome stays authoritative.
///
/// Deliberately much shorter than [`RESOLVED_TTL`]: a miss is cheap to
/// re-probe, and the negative memo's only job is to damp a retry storm, not
/// to hide a PV that appears moments later for up to ten seconds. This is
/// also the worst-case added discovery latency for a newly-registered PV
/// that a client's search already raced past once.
const RESOLVED_NEGATIVE_TTL: Duration = Duration::from_secs(2);

/// Upper bound on remembered outcomes, so an unbounded stream of distinct
/// miss-names cannot grow the memo without limit.
const RESOLVED_CAPACITY: usize = 4096;

/// Fraction of [`RESOLVED_CAPACITY`] evicted at once when the memo is full of
/// still-live entries, so the O(n) eviction scan is amortised over many
/// inserts instead of paid on every single one while the map sits at
/// capacity (as it will under a sustained flood of distinct miss-names).
const RESOLVED_EVICT_BATCH: usize = RESOLVED_CAPACITY / 4;

// ---------------------------------------------------------------------------
// PvInfo — metadata returned by Source::claim
// ---------------------------------------------------------------------------

/// Metadata about a PV as reported by the source that owns it.
#[derive(Debug, Clone)]
pub struct PvInfo {
    /// Structure descriptor for the PV.
    pub descriptor: StructureDesc,
    /// Whether the PV accepts PUT operations.
    pub writable: bool,
}

/// What a [`Source`] can say about a name **without doing I/O**.
///
/// This is the search path's question. `claim` is the authoritative one, but
/// it is allowed to be slow — for a proxying source it is a full upstream
/// round trip — and the search responder is a single task shared by every
/// client, so it must never await one. `try_claim` is the answer a source can
/// give from memory alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryClaim {
    /// This source serves the name. A search may answer `found=true` now.
    Yes,
    /// This source definitively does not serve it — an exhaustive local map
    /// that lacks the key, a live negative-cache entry, an access `Deny`.
    No,
    /// Cannot say without work that might block. The caller must not wait;
    /// it should start a background resolution and answer the retry.
    Unknown,
}

// ---------------------------------------------------------------------------
// Source — the object-safe provider trait
// ---------------------------------------------------------------------------

/// Object-safe trait for a PV data provider.
///
/// A source is responsible for a set of PV names. The server's
/// [`SourceRegistry`] iterates sources in priority order to find the first
/// that *claims* a given name.
///
/// # Implementing a custom source
///
/// ```rust,ignore
/// use spvirit_server::pvstore::{Source, PvInfo};
///
/// struct MySource { /* ... */ }
///
/// impl Source for MySource {
///     fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
///         Box::pin(async move { /* ... */ })
///     }
///     // ...other methods...
/// }
/// ```
pub trait Source: Send + Sync {
    /// Check whether this source owns `name` and, if so, return its metadata.
    ///
    /// Return `None` to let the registry try the next source.
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>>;

    /// Non-blocking counterpart to [`claim`](Self::claim), for the search path.
    ///
    /// Implementations **must not** perform network I/O, **must not** block on
    /// a lock, and **must** return promptly. Use `try_read`/`try_lock` and
    /// fall back to [`TryClaim::Unknown`] on contention.
    ///
    /// Returning `Unknown` is always correct and always safe: it costs one
    /// background resolution and one client search retry, never correctness.
    /// The default does exactly that, so a source that has no cheap answer
    /// needs no implementation. A source that *can* answer from memory should,
    /// because `Yes` is what lets a search be answered on the first datagram.
    /// This holds even for a source with no cache of its own: an `Unknown`
    /// still triggers [`SearchResolver`](crate::search_resolve::SearchResolver)
    /// to resolve the name in the background, and `SourceRegistry` remembers
    /// the outcome so the requester's retry is answered without another
    /// round trip. Under a resolution flood the name may instead be shed
    /// (the resolver has a hard concurrency cap) and answered on a later
    /// retry — still never incorrect, just not always the very next one.
    ///
    /// A source whose `claim` does no I/O should implement both in terms of
    /// one shared synchronous helper rather than duplicating the predicate —
    /// two copies of the same rule drift, and nothing here would catch it.
    fn try_claim(&self, _name: &str) -> TryClaim {
        TryClaim::Unknown
    }

    /// Read the current value of a PV.
    ///
    /// Only called for PVs this source has previously claimed.
    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>>;

    /// Apply a PUT value to a PV.
    ///
    /// Returns the list of `(pv_name, updated_payload)` pairs for all PVs
    /// that changed as a result (e.g. forward-link propagation).
    fn put(
        &self,
        name: &str,
        value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>>;

    /// Subscribe to value-change notifications on a PV.
    ///
    /// Returns `None` if the PV does not support subscription.
    fn subscribe(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>>;

    /// Whether this source delivers monitor updates on its own, by pushing
    /// into the server's [`MonitorRegistry`](crate::monitor::MonitorRegistry)
    /// (via `notify_monitors`, typically wired through
    /// [`StoreSource::set_monitor_registry`]) rather than only exposing them
    /// through [`subscribe`](Self::subscribe).
    ///
    /// The monitor handler consults this to decide whether it must pump this
    /// source's [`subscribe`](Self::subscribe) stream into the registry
    /// itself. Self-notifying sources (in-memory stores, IOC engines) return
    /// `true` so the handler does *not* pump — pumping a source that also
    /// self-notifies would deliver every update twice. Subscribe-only sources
    /// (gateway proxies, group PVs, async backends) keep the default `false`,
    /// so the handler drains their stream on their behalf.
    fn pushes_own_updates(&self) -> bool {
        false
    }

    /// Execute an RPC call on a channel.
    ///
    /// `name` is the channel name, `args` is the decoded request structure.
    /// Returns the response payload on success.
    ///
    /// The default implementation returns an error — override it in sources
    /// that provide RPC endpoints.
    fn rpc(
        &self,
        _name: &str,
        _args: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<NtPayload, String>> + Send + '_>> {
        Box::pin(async { Err("RPC not supported".to_string()) })
    }

    /// List all PV names provided by this source.
    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>>;
}

/// A source that owns a fixed, enumerable set of record names.
///
/// The distinction matters because the two tiers have different rules:
/// **stores must be disjoint** — two stores claiming the same name is a
/// configuration error caught at `build()` — while **sources may shadow**,
/// which is a legitimate way to override a PV and only warrants a warning.
/// `record_names` is synchronous because the disjointness check runs inside
/// `PvaServerBuilder::build`, which is not async.
///
/// The returned list must be deterministic (sorted) so overlap diagnostics
/// are stable between runs.
pub trait StoreSource: Source {
    fn record_names(&self) -> Vec<String>;

    /// Receive the server's [`MonitorRegistry`] so writes that originate in
    /// host code — rather than arriving as a client PUT through the handler —
    /// still reach monitor clients.
    ///
    /// Defaulted to a no-op because a store that offers no host-side write
    /// path needs nothing here. `PvaServer` calls it once, before serving.
    fn set_monitor_registry(&self, registry: Arc<crate::monitor::MonitorRegistry>) {
        let _ = registry;
    }

    /// Begin any scan-driven, self-clocked processing the store performs, now
    /// that the server is up and the monitor registry is set. `handle` is the
    /// server's runtime handle, for a store whose processing runs on its own
    /// (non-runtime) threads and must reach back into async publish paths.
    ///
    /// Defaulted to a no-op: a store with nothing self-clocked needs it not.
    /// `PvaServer` calls it once, after `set_monitor_registry`, before serving.
    fn start_scanning(&self, handle: tokio::runtime::Handle) {
        let _ = handle;
    }

    /// Stop what [`start_scanning`](StoreSource::start_scanning) began.
    /// Defaulted to a no-op. Idempotent by contract.
    fn stop_scanning(&self) {}

    /// The store's scan [`EventSink`](crate::events::EventSink), if it has one,
    /// so `PvaServer` can register it on the named-event fan-out and let
    /// PVAccess-posted events drive event scan lists. Defaulted to `None`.
    fn scanner_event_sink(&self) -> Option<Arc<dyn crate::events::EventSink>> {
        None
    }
}

// ---------------------------------------------------------------------------
// SourceEntry — one registered source with its priority
// ---------------------------------------------------------------------------

struct SourceEntry {
    /// Human-readable label for debugging / logging.
    label: String,
    /// Lower values are checked first.
    order: i32,
    /// The actual source implementation.
    source: Arc<dyn Source>,
    /// True for entries registered via [`SourceRegistry::add_store`].
    ///
    /// Consumed by `PvaServerBuilder::build`'s disjointness check and by
    /// `SourceRegistry::claim`'s shadow-warning.
    is_store: bool,
}

// ---------------------------------------------------------------------------
// SourceRegistry — ordered collection of sources
// ---------------------------------------------------------------------------

/// A dynamic, priority-ordered registry of [`Source`] providers.
///
/// PV name resolution iterates sources from lowest `order` to highest and
/// delegates to the first source that claims the name.
/// Memo key: the identity that asked, plus the name.
///
/// Keying on the bare name would let one client's successful resolution tell
/// a *different* client that a PV it may not see exists. `peer` and `user`
/// are the same two fields [`crate::request_ctx::request_identity`] exposes
/// — the ones `AccessControl::decide` in `spvirit-gateway` actually matches
/// on — so this memo can never disagree with the access policy about who is
/// asking. The client-asserted `ca` *host* string is deliberately not part
/// of the key: it is never trusted for an access decision either, so
/// including it would only fragment the memo without adding security.
///
/// A struct key (rather than a delimited `String`) means two different
/// identities can never collide by way of a crafted separator character in
/// a user-supplied field.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MemoKey {
    /// `None` only outside a request scope (e.g. a unit test calling
    /// `note_resolved`/`try_claim` directly). Whenever a request scope is
    /// present, the peer is always present too.
    peer: Option<IpAddr>,
    user: Option<String>,
    name: String,
}

pub struct SourceRegistry {
    sources: RwLock<Vec<SourceEntry>>,
    /// PVs whose shadowing status has already been determined. Bounded by
    /// the number of distinct names clients search for, and only ever grown
    /// by a claim that a non-store source won.
    shadow_checked: RwLock<HashSet<String>>,
    /// Outcomes of background resolutions, keyed by identity as well as name.
    ///
    /// `std::sync::Mutex`, never held across an `.await`, and only ever
    /// `try_lock`ed from the search path — the whole point of this type is
    /// that the search task cannot block.
    resolved: Mutex<HashMap<MemoKey, (Instant, bool)>>,
    positive_ttl: Duration,
    negative_ttl: Duration,
}

impl SourceRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            sources: RwLock::new(Vec::new()),
            shadow_checked: RwLock::new(HashSet::new()),
            resolved: Mutex::new(HashMap::new()),
            positive_ttl: RESOLVED_TTL,
            negative_ttl: RESOLVED_NEGATIVE_TTL,
        }
    }

    /// Create an empty registry whose resolver memo expires after `ttl`,
    /// for both positive and negative outcomes, instead of the defaults
    /// [`RESOLVED_TTL`] / [`RESOLVED_NEGATIVE_TTL`]. Test-only knob.
    #[cfg(test)]
    pub(crate) fn new_with_memo_ttl(ttl: Duration) -> Self {
        let mut reg = Self::new();
        reg.positive_ttl = ttl;
        reg.negative_ttl = ttl;
        reg
    }

    /// Number of outcomes currently held in the resolver memo. Test-only.
    #[cfg(test)]
    pub(crate) fn memo_len(&self) -> usize {
        self.lock_resolved().len()
    }

    /// Lock the resolver memo, recovering from poisoning rather than
    /// propagating it.
    ///
    /// The memo has no invariant a panicking holder could leave broken — the
    /// worst a poisoned lock could contain is a slightly stale entry — so
    /// treating a poison as fatal would silently and permanently disable the
    /// memo for the rest of the process, resurrecting the exact regression
    /// this task exists to fix.
    fn lock_resolved(&self) -> std::sync::MutexGuard<'_, HashMap<MemoKey, (Instant, bool)>> {
        self.resolved.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Memo key for `name` under the current request identity. The one
    /// builder both `note_resolved` and `recall` go through, so the two can
    /// never derive the identity differently.
    fn memo_key(name: &str) -> MemoKey {
        let (peer, user) = crate::request_ctx::request_identity();
        MemoKey {
            peer,
            user,
            name: name.to_string(),
        }
    }

    /// Record the outcome of a background resolution for the current identity.
    pub fn note_resolved(&self, name: &str, found: bool) {
        let key = Self::memo_key(name);
        let now = Instant::now();
        let mut memo = self.lock_resolved();
        if memo.len() >= RESOLVED_CAPACITY {
            let positive_ttl = self.positive_ttl;
            let negative_ttl = self.negative_ttl;
            memo.retain(|_, (at, found)| {
                now.duration_since(*at) < if *found { positive_ttl } else { negative_ttl }
            });
            if memo.len() >= RESOLVED_CAPACITY {
                // Still full of live entries: evict a batch of the oldest
                // rather than one entry per insert. One-at-a-time eviction
                // here would mean every insert pays a full O(n) scan for as
                // long as the map stays saturated — exactly the shape of a
                // sustained distinct-name flood.
                let mut by_age: Vec<(MemoKey, Instant)> =
                    memo.iter().map(|(k, (at, _))| (k.clone(), *at)).collect();
                by_age.sort_by_key(|(_, at)| *at);
                let evict = RESOLVED_EVICT_BATCH.max(1).min(by_age.len());
                for (k, _) in by_age.into_iter().take(evict) {
                    memo.remove(&k);
                }
            }
        }
        memo.insert(key, (now, found));
    }

    /// The remembered outcome for `name` under the current identity, if it has
    /// not expired. `try_lock` — a contended memo answers `None`, never
    /// blocks. A poisoned lock is treated like an uncontended one (see
    /// [`lock_resolved`](Self::lock_resolved)) rather than answering `None`
    /// forever.
    fn recall(&self, name: &str) -> Option<bool> {
        let key = Self::memo_key(name);
        let memo = match self.resolved.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        let (at, found) = memo.get(&key)?;
        let ttl = if *found { self.positive_ttl } else { self.negative_ttl };
        (Instant::now().duration_since(*at) < ttl).then_some(*found)
    }

    /// Register an ordinary source. Sources may shadow stores and each other.
    ///
    /// Lower `order` values are queried first.
    pub async fn add(&self, label: impl Into<String>, order: i32, source: Arc<dyn Source>) {
        self.insert(label.into(), order, source, false).await;
    }

    /// Register a store — a source whose record set is fixed and must not
    /// overlap another store's. See [`StoreSource`].
    pub async fn add_store(&self, label: impl Into<String>, order: i32, source: Arc<dyn Source>) {
        self.insert(label.into(), order, source, true).await;
    }

    async fn insert(&self, label: String, order: i32, source: Arc<dyn Source>, is_store: bool) {
        debug!(
            "SourceRegistry: adding source '{}' at order {} (store: {})",
            label, order, is_store
        );
        let mut sources = self.sources.write().await;
        sources.push(SourceEntry {
            label,
            order,
            source,
            is_store,
        });
        sources.sort_by_key(|e| e.order);
        self.invalidate_memo();
    }

    /// Remove all sources with the given label.
    pub async fn remove(&self, label: &str) {
        debug!("SourceRegistry: removing source '{}'", label);
        let mut sources = self.sources.write().await;
        sources.retain(|e| e.label != label);
        self.invalidate_memo();
    }

    /// Drop every remembered resolver outcome.
    ///
    /// The source set just changed, so any memo entry — positive or negative
    /// — may now be stale: a newly-added source can serve a name the memo
    /// says `No`, and a removed one can no longer serve a name the memo says
    /// `Yes`. The registry knows exactly when this happens; without this the
    /// memo would have no way to find out and a name could stay wrong for a
    /// full TTL after `add`/`add_store`/`remove`.
    fn invalidate_memo(&self) {
        self.lock_resolved().clear();
    }

    // ── Delegating operations ────────────────────────────────────────

    /// Find the first source that claims `name` and return its metadata.
    ///
    /// The sources list is snapshotted and the read guard released *before*
    /// any source's `claim` is awaited. A proxying source's `claim` is a full
    /// upstream round trip; holding the registry lock across one would block
    /// every concurrent add/remove for that long.
    pub async fn claim(&self, name: &str) -> Option<PvInfo> {
        let snapshot: Vec<(String, Arc<dyn Source>, bool)> = {
            let sources = self.sources.read().await;
            sources
                .iter()
                .map(|e| (e.label.clone(), e.source.clone(), e.is_store))
                .collect()
        };
        for (label, source, is_store) in &snapshot {
            if let Some(info) = source.claim(name).await {
                if !is_store {
                    self.warn_if_shadowing_a_store(&snapshot, label, name).await;
                }
                return Some(info);
            }
        }
        None
    }

    /// Log once if `winner` — an ordinary source — beat a store to `name`.
    ///
    /// Shadowing between sources is legal and often deliberate, so this
    /// warns rather than failing. Overlap between two *stores* is the error
    /// case, and `PvaServerBuilder::build` rejects it outright.
    ///
    /// Only `claim` calls this. `get`/`put`/`subscribe`/`rpc` re-resolve the
    /// same name through the same ordered list, and a client always searches
    /// before operating, so the first claim is where the diagnostic belongs.
    async fn warn_if_shadowing_a_store(
        &self,
        snapshot: &[(String, Arc<dyn Source>, bool)],
        winner: &str,
        name: &str,
    ) {
        if self.shadow_checked.read().await.contains(name) {
            return;
        }
        if !self.shadow_checked.write().await.insert(name.to_string()) {
            // Another task got there between the read and the write.
            return;
        }
        for (label, source, _) in snapshot.iter().filter(|(_, _, is_store)| *is_store) {
            if source.claim(name).await.is_some() {
                tracing::warn!(
                    "source '{winner}' shadows store '{label}' for PV '{name}': the store's \
                     value will never be served"
                );
                return;
            }
        }
    }

    /// Non-blocking aggregate of [`Source::try_claim`] across all sources.
    ///
    /// `Yes` as soon as any source owns the name (matching `claim`'s
    /// first-wins order). `No` only when *every* source is decisive that it
    /// does not. Otherwise the sources are collectively `Unknown` — a state
    /// this method resolves further by consulting the resolver memo (see
    /// [`note_resolved`](Self::note_resolved)) before finally answering
    /// `Unknown` itself. A source that actually knows always outranks the
    /// memo. Contention on the sources lock also answers `Unknown` — the
    /// registry must never assert a name is absent when a source it did not
    /// consult might serve it.
    ///
    /// Synchronous by design: this is called from the search responder, and
    /// the entire point is that it cannot await.
    pub fn try_claim(&self, name: &str) -> TryClaim {
        let Ok(sources) = self.sources.try_read() else {
            // Contention on the sources lock, not on a source's own state:
            // the memo can only turn this `Unknown` into a previously-
            // observed answer, never assert something no source has vouched
            // for, so consulting it here is strictly better than not.
            return match self.recall(name) {
                Some(true) => TryClaim::Yes,
                Some(false) => TryClaim::No,
                None => TryClaim::Unknown,
            };
        };
        let mut all_decisive = true;
        for entry in sources.iter() {
            match entry.source.try_claim(name) {
                TryClaim::Yes => return TryClaim::Yes,
                TryClaim::No => {}
                TryClaim::Unknown => all_decisive = false,
            }
        }
        if all_decisive {
            TryClaim::No
        } else {
            match self.recall(name) {
                Some(true) => TryClaim::Yes,
                Some(false) => TryClaim::No,
                None => TryClaim::Unknown,
            }
        }
    }

    /// Check whether any source claims the given PV name.
    pub async fn has_pv(&self, name: &str) -> bool {
        self.claim(name).await.is_some()
    }

    /// Get the value from the first source that claims the PV.
    pub async fn get(&self, name: &str) -> Option<NtPayload> {
        let sources = self.sources.read().await;
        for entry in sources.iter() {
            if entry.source.claim(name).await.is_some() {
                return entry.source.get(name).await;
            }
        }
        None
    }

    /// Get the structure descriptor from the first source that claims the PV.
    pub async fn get_descriptor(&self, name: &str) -> Option<StructureDesc> {
        self.claim(name).await.map(|info| info.descriptor)
    }

    /// Check if the PV is writable (via the first claiming source).
    pub async fn is_writable(&self, name: &str) -> bool {
        self.claim(name).await.is_some_and(|info| info.writable)
    }

    /// Delegate a PUT to the first source that claims the PV.
    pub async fn put(
        &self,
        name: &str,
        value: &DecodedValue,
    ) -> Result<Vec<(String, NtPayload)>, String> {
        let sources = self.sources.read().await;
        for entry in sources.iter() {
            if entry.source.claim(name).await.is_some() {
                return entry.source.put(name, value).await;
            }
        }
        Err(format!("PV '{}' not found", name))
    }

    /// Subscribe via the first source that claims the PV.
    pub async fn subscribe(&self, name: &str) -> Option<mpsc::Receiver<NtPayload>> {
        let sources = self.sources.read().await;
        for entry in sources.iter() {
            if entry.source.claim(name).await.is_some() {
                return entry.source.subscribe(name).await;
            }
        }
        None
    }

    /// Whether the first source that claims `name` delivers its own monitor
    /// updates (see [`Source::pushes_own_updates`]).
    ///
    /// Returns `false` for an unclaimed PV — there is nothing to pump and no
    /// source to ask, so the caller's decision (pump vs. skip) is moot.
    pub async fn pushes_own_updates(&self, name: &str) -> bool {
        let sources = self.sources.read().await;
        for entry in sources.iter() {
            if entry.source.claim(name).await.is_some() {
                return entry.source.pushes_own_updates();
            }
        }
        false
    }

    /// Execute an RPC call via the first source that claims the channel.
    pub async fn rpc(&self, name: &str, args: &DecodedValue) -> Result<NtPayload, String> {
        let sources = self.sources.read().await;
        for entry in sources.iter() {
            if entry.source.claim(name).await.is_some() {
                return entry.source.rpc(name, args).await;
            }
        }
        Err(format!("RPC channel '{}' not found", name))
    }

    /// Collect all PV names from every registered source.
    ///
    /// The sources list is snapshotted and the read guard released *before*
    /// any source's `names()` is awaited, for the same reason
    /// [`claim`](Self::claim) does it: a source's `names()` may be arbitrary
    /// third-party work (a Python coroutine under the GIL, say), and holding
    /// the registry lock across one blocks every concurrent add/remove — and
    /// every other `try_read`-based reader, since tokio's `RwLock` is
    /// writer-fair — for that whole duration.
    pub async fn names(&self) -> Vec<String> {
        let snapshot: Vec<Arc<dyn Source>> = {
            let sources = self.sources.read().await;
            sources.iter().map(|e| e.source.clone()).collect()
        };
        let mut seen = HashSet::new();
        let mut all = Vec::new();
        for source in &snapshot {
            for name in source.names().await {
                if seen.insert(name.clone()) {
                    all.push(name);
                }
            }
        }
        all.sort();
        all
    }
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal [`Source`] over a fixed name list, for registry tests.
    struct StubSource {
        names: Vec<String>,
        claims: std::sync::atomic::AtomicUsize,
    }

    impl StubSource {
        fn new(names: &[&str]) -> Self {
            Self {
                names: names.iter().map(|s| s.to_string()).collect(),
                claims: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn claim_count(&self) -> usize {
            self.claims.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Source for StubSource {
        fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
            self.claims.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let claimed = self.names.iter().any(|n| n == name);
            Box::pin(async move {
                claimed.then(|| PvInfo {
                    descriptor: StructureDesc::default(),
                    writable: true,
                })
            })
        }

        fn get(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn put(
            &self,
            _name: &str,
            _value: &DecodedValue,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>>
        {
            Box::pin(async { Ok(vec![]) })
        }

        fn subscribe(
            &self,
            _name: &str,
        ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
            let names = self.names.clone();
            Box::pin(async move { names })
        }
    }

    /// A source whose `names()` future takes `delay` to resolve — the shape
    /// of a proxying or Python-backed source enumerating over the network.
    struct SlowNamesSource {
        delay: Duration,
    }

    impl Source for SlowNamesSource {
        fn claim(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn get(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn put(
            &self,
            _name: &str,
            _value: &DecodedValue,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>>
        {
            Box::pin(async { Ok(vec![]) })
        }

        fn subscribe(
            &self,
            _name: &str,
        ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                vec!["SLOW:PV".to_string()]
            })
        }
    }

    /// `names()` must snapshot the source handles and release the read guard
    /// before awaiting any source's future, exactly as `claim` does. Holding
    /// it means one slow enumeration blocks every registration — and, because
    /// tokio's `RwLock` is writer-fair, the queued writer then also fails
    /// every `try_read` the search path depends on.
    #[tokio::test]
    async fn names_does_not_hold_the_sources_lock_across_a_source_future() {
        let reg = Arc::new(SourceRegistry::new());
        reg.add(
            "slow",
            0,
            Arc::new(SlowNamesSource {
                delay: Duration::from_millis(400),
            }),
        )
        .await;

        let enumerating = {
            let reg = reg.clone();
            tokio::spawn(async move { reg.names().await })
        };
        // Let the enumeration get as far as its source future.
        tokio::time::sleep(Duration::from_millis(80)).await;

        let before = Instant::now();
        reg.add("late", 1, Arc::new(StubSource::new(&["B"]))).await;
        let blocked = before.elapsed();
        assert!(
            blocked < Duration::from_millis(200),
            "registering a source waited {blocked:?} behind a slow names()"
        );

        // Anti-vacuity: the slow enumeration really was still in flight, and
        // really did produce its names.
        // (The snapshot predates the `late` registration, so `B` is correctly
        // absent from it.)
        assert_eq!(enumerating.await.unwrap(), vec!["SLOW:PV".to_string()]);
    }

    #[tokio::test]
    async fn stores_are_recorded_as_stores_and_sources_are_not() {
        let reg = SourceRegistry::new();
        reg.add_store("builtin", 0, Arc::new(StubSource::new(&["A"]))).await;
        reg.add("custom", 10, Arc::new(StubSource::new(&["B"]))).await;
        let flags: Vec<(String, bool)> = reg
            .sources
            .read()
            .await
            .iter()
            .map(|e| (e.label.clone(), e.is_store))
            .collect();
        assert_eq!(
            flags,
            vec![("builtin".to_string(), true), ("custom".to_string(), false)]
        );
    }

    #[tokio::test]
    async fn a_store_added_late_still_sorts_by_order() {
        let reg = SourceRegistry::new();
        reg.add("custom", 10, Arc::new(StubSource::new(&["B"]))).await;
        reg.add_store("builtin", 0, Arc::new(StubSource::new(&["A"]))).await;
        let labels: Vec<String> = reg
            .sources
            .read()
            .await
            .iter()
            .map(|e| e.label.clone())
            .collect();
        assert_eq!(labels, vec!["builtin".to_string(), "custom".to_string()]);
    }

    /// A source registered ahead of a store wins the claim — that is the
    /// documented behaviour, and it stays. The registry just says so.
    #[tokio::test]
    async fn a_source_shadowing_a_store_still_wins_the_claim() {
        let reg = SourceRegistry::new();
        reg.add("override", -1, Arc::new(StubSource::new(&["PV:X"]))).await;
        reg.add_store("builtin", 0, Arc::new(StubSource::new(&["PV:X"]))).await;
        assert!(reg.claim("PV:X").await.is_some());
    }

    /// The warning is emitted at most once per PV, so a client that searches
    /// in a loop does not flood the log.
    #[tokio::test]
    async fn the_shadow_check_runs_once_per_pv() {
        let reg = SourceRegistry::new();
        let store = Arc::new(StubSource::new(&["PV:X"]));
        reg.add("override", -1, Arc::new(StubSource::new(&["PV:X"]))).await;
        reg.add_store("builtin", 0, store.clone()).await;
        let before = store.claim_count();
        for _ in 0..5 {
            reg.claim("PV:X").await;
        }
        assert_eq!(
            store.claim_count() - before,
            1,
            "the shadowed store must be consulted exactly once"
        );
    }

    /// A source claiming a name no store owns is the ordinary case and must
    /// not cost a scan of every store on every search after the first.
    #[tokio::test]
    async fn an_unshadowed_source_claim_is_also_checked_only_once() {
        let reg = SourceRegistry::new();
        let store = Arc::new(StubSource::new(&["PV:OTHER"]));
        reg.add("plain", -1, Arc::new(StubSource::new(&["PV:X"]))).await;
        reg.add_store("builtin", 0, store.clone()).await;
        let before = store.claim_count();
        for _ in 0..5 {
            reg.claim("PV:X").await;
        }
        assert_eq!(store.claim_count() - before, 1);
    }

    /// A store winning its own claim triggers no check at all.
    #[tokio::test]
    async fn a_store_winning_its_own_claim_consults_nothing_else() {
        let reg = SourceRegistry::new();
        let other = Arc::new(StubSource::new(&["PV:X"]));
        reg.add_store("builtin", 0, Arc::new(StubSource::new(&["PV:X"]))).await;
        reg.add_store("second", 5, other.clone()).await;
        let before = other.claim_count();
        reg.claim("PV:X").await;
        assert_eq!(other.claim_count() - before, 0);
    }

    /// An `io::Write` sink over a shared buffer, for capturing `tracing`
    /// output during a test. Only ever touched synchronously by the
    /// subscriber's own formatting call — never held across an `.await`.
    #[derive(Clone, Default)]
    struct CaptureWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The counter-based tests above prove the shadow check runs at most
    /// once per name, but they infer "warned" from how many times the
    /// store's `claim` ran — which can't tell a real warning apart from a
    /// silent scan. This test observes the actual `tracing::warn!` event.
    #[tokio::test]
    async fn the_shadow_warning_is_emitted_once_not_just_counted() {
        let buffer = CaptureWriter::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();

        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let reg = SourceRegistry::new();
        reg.add("override", -1, Arc::new(StubSource::new(&["PV:X"]))).await;
        reg.add_store("builtin", 0, Arc::new(StubSource::new(&["PV:X"]))).await;

        reg.claim("PV:X").await;
        reg.claim("PV:X").await;

        let captured = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        let warnings: Vec<&str> = captured.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            warnings.len(),
            1,
            "expected exactly one warning event, got: {captured:?}"
        );
        assert!(warnings[0].contains("PV:X"), "missing PV name: {captured:?}");
        assert!(warnings[0].contains("override"), "missing source label: {captured:?}");
        assert!(warnings[0].contains("builtin"), "missing store label: {captured:?}");
    }

    /// A source that answers `try_claim` decisively from a fixed name set —
    /// the shape every local (non-proxying) source is expected to have.
    struct DecisiveSource {
        names: Vec<String>,
    }

    impl DecisiveSource {
        fn new(names: &[&str]) -> Self {
            Self {
                names: names.iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    impl Source for DecisiveSource {
        fn try_claim(&self, name: &str) -> TryClaim {
            if self.names.iter().any(|n| n == name) {
                TryClaim::Yes
            } else {
                TryClaim::No
            }
        }

        fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
            let hit = self.names.iter().any(|n| n == name);
            Box::pin(async move {
                hit.then(|| PvInfo {
                    descriptor: StructureDesc::default(),
                    writable: false,
                })
            })
        }

        fn get(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn put(
            &self,
            _name: &str,
            _value: &DecodedValue,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>>
        {
            Box::pin(async { Err("read-only".to_string()) })
        }

        fn subscribe(
            &self,
            _name: &str,
        ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
            let names = self.names.clone();
            Box::pin(async move { names })
        }
    }

    #[tokio::test]
    async fn try_claim_says_yes_when_a_source_owns_the_name() {
        let reg = SourceRegistry::new();
        reg.add("decisive", 0, Arc::new(DecisiveSource::new(&["A"])))
            .await;
        assert_eq!(reg.try_claim("A"), TryClaim::Yes);
    }

    #[tokio::test]
    async fn try_claim_says_no_only_when_every_source_is_decisive_about_it() {
        let reg = SourceRegistry::new();
        reg.add("decisive", 0, Arc::new(DecisiveSource::new(&["A"])))
            .await;
        assert_eq!(reg.try_claim("MISSING"), TryClaim::No);
    }

    #[tokio::test]
    async fn one_undecided_source_makes_the_whole_answer_unknown() {
        let reg = SourceRegistry::new();
        reg.add("decisive", 0, Arc::new(DecisiveSource::new(&["A"])))
            .await;
        // StubSource does not override try_claim, so it answers Unknown.
        reg.add("undecided", 1, Arc::new(StubSource::new(&["B"]))).await;
        // "A" is owned outright, so the undecided source never gets a say.
        assert_eq!(reg.try_claim("A"), TryClaim::Yes);
        // "MISSING" is unowned by the decisive source, but the undecided one
        // might still serve it — the registry must not claim it knows.
        assert_eq!(reg.try_claim("MISSING"), TryClaim::Unknown);
    }

    #[tokio::test]
    async fn an_undecided_source_that_owns_the_name_still_yields_unknown() {
        let reg = SourceRegistry::new();
        reg.add("undecided", 0, Arc::new(StubSource::new(&["B"]))).await;
        assert_eq!(reg.try_claim("B"), TryClaim::Unknown);
    }

    #[tokio::test]
    async fn an_empty_registry_is_decisive_that_it_has_nothing() {
        let reg = SourceRegistry::new();
        assert_eq!(reg.try_claim("ANYTHING"), TryClaim::No);
    }

    #[tokio::test]
    async fn a_resolved_name_becomes_decisive_even_for_a_source_that_never_probes() {
        // StubSource does not override try_claim, so it always answers Unknown —
        // the same shape as RecordFieldSource, GroupSource and IocSource.
        let reg = SourceRegistry::new();
        reg.add("stub", 0, Arc::new(StubSource::new(&["REAL:PV"]))).await;

        assert_eq!(reg.try_claim("REAL:PV"), TryClaim::Unknown);
        let found = reg.claim("REAL:PV").await.is_some();
        assert!(found, "control: claim must find it");
        reg.note_resolved("REAL:PV", found);

        assert_eq!(
            reg.try_claim("REAL:PV"),
            TryClaim::Yes,
            "a resolved name must be answerable without another round trip"
        );
    }

    #[tokio::test]
    async fn a_resolved_miss_is_remembered_as_no() {
        let reg = SourceRegistry::new();
        reg.add("stub", 0, Arc::new(StubSource::new(&["REAL:PV"]))).await;
        reg.note_resolved("GONE:PV", false);
        assert_eq!(reg.try_claim("GONE:PV"), TryClaim::No);
    }

    #[tokio::test]
    async fn a_decisive_source_outranks_the_memo() {
        // DecisiveSource (already in this module) answers Yes for its names.
        let reg = SourceRegistry::new();
        reg.add("dec", 0, Arc::new(DecisiveSource::new(&["LIVE:PV"]))).await;
        reg.note_resolved("LIVE:PV", false); // stale, contradicted by the source
        assert_eq!(reg.try_claim("LIVE:PV"), TryClaim::Yes);
    }

    #[tokio::test]
    async fn a_memo_entry_expires() {
        // Wall-clock, not `tokio::time::pause`/`advance`: the memo's
        // expiry check is built on `std::time::Instant`, which a paused
        // Tokio clock does not affect (only `tokio::time::Instant` is
        // virtualised), so switching this test to the virtual clock without
        // also changing the memo's timestamp type would make it pass
        // vacuously — actual real time is what has to elapse here.
        let reg = SourceRegistry::new_with_memo_ttl(Duration::from_millis(40));
        reg.add("stub", 0, Arc::new(StubSource::new(&["REAL:PV"]))).await;
        reg.note_resolved("REAL:PV", true);
        assert_eq!(reg.try_claim("REAL:PV"), TryClaim::Yes);
        tokio::time::sleep(Duration::from_millis(90)).await;
        assert_eq!(
            reg.try_claim("REAL:PV"),
            TryClaim::Unknown,
            "an expired memo must fall back to Unknown, never to a stale answer"
        );
    }

    #[tokio::test]
    async fn the_memo_is_bounded() {
        let reg = SourceRegistry::new();
        for i in 0..(RESOLVED_CAPACITY * 2) {
            reg.note_resolved(&format!("MISS:{i}"), false);
        }
        assert!(
            reg.memo_len() <= RESOLVED_CAPACITY,
            "memo grew past its bound: {}",
            reg.memo_len()
        );
    }

    /// This is the shape that actually occurs on the dominant discovery path:
    /// UDP search installs the peer (`request_ctx::scope`) but never any `ca`
    /// credentials, so `user` and `host` are `None` for every UDP searcher
    /// and only the peer IP tells two clients apart. A version of this test
    /// that also varied `user` (or `host`) would pass on that field alone
    /// and prove nothing about the peer-only case — which is exactly the gap
    /// the round-1 review found.
    #[tokio::test]
    async fn the_memo_does_not_leak_a_pv_across_identities() {
        use crate::request_ctx::RequestContext;

        let reg = Arc::new(SourceRegistry::new());
        reg.add("stub", 0, Arc::new(StubSource::new(&["SECRET:PV"]))).await;

        let a = RequestContext {
            peer: "10.0.0.1:5075".parse().unwrap(),
            user: None,
            host: None,
        };
        let b = RequestContext {
            peer: "10.0.0.2:5075".parse().unwrap(),
            user: None,
            host: None,
        };

        let r = reg.clone();
        crate::request_ctx::scope_with(a, async move { r.note_resolved("SECRET:PV", true) }).await;

        let r = reg.clone();
        let seen_by_bob =
            crate::request_ctx::scope_with(b, async move { r.try_claim("SECRET:PV") }).await;
        assert_eq!(
            seen_by_bob,
            TryClaim::Unknown,
            "a different peer's resolution must not tell this one the PV exists"
        );
    }

    /// The companion positive case to the leak test above: an identity must
    /// be able to read back its *own* memo entry. Without this, a `memo_key`
    /// that never agreed with itself (e.g. one that hashed something
    /// non-deterministic, or that the leak test's negative assertion would
    /// pass against by accident) would go undetected — a negative-only test
    /// suite is satisfied by a memo that is simply inert for everyone.
    #[tokio::test]
    async fn an_identity_can_read_back_its_own_resolution() {
        use crate::request_ctx::RequestContext;

        let reg = Arc::new(SourceRegistry::new());
        reg.add("stub", 0, Arc::new(StubSource::new(&["OWN:PV"]))).await;

        let alice = RequestContext {
            peer: "10.0.0.1:5075".parse().unwrap(),
            user: Some("alice".into()),
            host: None,
        };

        let r = reg.clone();
        let seen_by_alice = crate::request_ctx::scope_with(alice.clone(), async move {
            r.note_resolved("OWN:PV", true);
            r.try_claim("OWN:PV")
        })
        .await;
        assert_eq!(
            seen_by_alice,
            TryClaim::Yes,
            "an identity must be able to read back its own resolution"
        );

        // And a second, separate scope with the same identity fields must
        // see it too — the memo key must be a pure function of identity plus
        // name, not tied to the particular task instance that wrote it.
        let r = reg.clone();
        let seen_again =
            crate::request_ctx::scope_with(alice, async move { r.try_claim("OWN:PV") }).await;
        assert_eq!(seen_again, TryClaim::Yes);
    }
}
