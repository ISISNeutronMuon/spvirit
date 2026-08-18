//! The [`Source`] trait — an object-safe abstraction over any PV data source,
//! and [`SourceRegistry`] — a dynamic, priority-ordered collection of sources.
//!
//! Protocol handlers use `SourceRegistry` to resolve PV names across multiple
//! registered sources, allowing different backends (in-memory records, hardware
//! drivers, proxies, etc.) to coexist in a single PVA server. basically what pvxs does with its provider registry.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spvirit_codec::spvd_decode::{DecodedValue, StructureDesc};
use spvirit_types::NtPayload;
use tokio::sync::{RwLock, mpsc};
use tracing::debug;

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
pub struct SourceRegistry {
    sources: RwLock<Vec<SourceEntry>>,
    /// PVs whose shadowing status has already been determined. Bounded by
    /// the number of distinct names clients search for, and only ever grown
    /// by a claim that a non-store source won.
    shadow_checked: RwLock<HashSet<String>>,
}

impl SourceRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            sources: RwLock::new(Vec::new()),
            shadow_checked: RwLock::new(HashSet::new()),
        }
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
    }

    /// Remove all sources with the given label.
    pub async fn remove(&self, label: &str) {
        debug!("SourceRegistry: removing source '{}'", label);
        let mut sources = self.sources.write().await;
        sources.retain(|e| e.label != label);
    }

    // ── Delegating operations ────────────────────────────────────────

    /// Find the first source that claims `name` and return its metadata.
    pub async fn claim(&self, name: &str) -> Option<PvInfo> {
        let sources = self.sources.read().await;
        for entry in sources.iter() {
            if let Some(info) = entry.source.claim(name).await {
                if !entry.is_store {
                    self.warn_if_shadowing_a_store(&sources, &entry.label, name)
                        .await;
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
    async fn warn_if_shadowing_a_store(&self, sources: &[SourceEntry], winner: &str, name: &str) {
        if self.shadow_checked.read().await.contains(name) {
            return;
        }
        if !self.shadow_checked.write().await.insert(name.to_string()) {
            // Another task got there between the read and the write.
            return;
        }
        for entry in sources.iter().filter(|e| e.is_store) {
            if entry.source.claim(name).await.is_some() {
                tracing::warn!(
                    "source '{winner}' shadows store '{}' for PV '{name}': the store's \
                     value will never be served",
                    entry.label
                );
                return;
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
    pub async fn names(&self) -> Vec<String> {
        let sources = self.sources.read().await;
        let mut seen = HashSet::new();
        let mut all = Vec::new();
        for entry in sources.iter() {
            for name in entry.source.names().await {
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
}
