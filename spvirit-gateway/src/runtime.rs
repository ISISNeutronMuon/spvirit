//! `Runtime` — wires a validated [`GatewayConfig`] into a running gateway:
//! one shared [`UpstreamPool`] plus one [`spvirit_server::PvaServer`] (backed
//! by a [`GatewaySource`]) per `servers[]` entry.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spvirit_server::diag::{BandwidthCounters, ClientRegistry};
use spvirit_server::{PvaServer, rand_guid};

use crate::cache::negative::NegativeCache;
use crate::config::{ConfigError, GatewayConfig};
use crate::loopguard::LoopGuard;
use crate::proxy::GatewaySource;
use crate::status::{BandwidthSampler, RateSnapshot, StatusHandles, StatusSource, banner};
use crate::upstream::UpstreamPool;

/// How often the [`BandwidthSampler`] converts cumulative byte counters into
/// a fresh [`RateSnapshot`].
const SAMPLER_TICK_PERIOD: Duration = Duration::from_secs(1);

/// Default negative-search-cache TTL used when a server has no `x-spvirit
/// .negativeCache` override.
const DEFAULT_NEG_CACHE_TTL: Duration = Duration::from_secs(30);
/// Default negative-search-cache capacity used when a server has no
/// `x-spvirit.negativeCache` override.
const DEFAULT_NEG_CACHE_CAPACITY: usize = 128;

/// A fully wired gateway runtime: one [`PvaServer`] per configured
/// `servers[]` entry, all sharing a single upstream [`UpstreamPool`].
pub struct Runtime {
    servers: Vec<PvaServer>,
    /// The shared upstream pool (retained so the metrics endpoint can read
    /// `UpstreamPool::names().len()` live).
    pool: Arc<UpstreamPool>,
    /// One `GatewaySource` per server, retained so the metrics endpoint can
    /// sum `upstream_monitor_count()` across servers.
    sources: Vec<Arc<GatewaySource>>,
    /// `Some((listen, path))` when the effective config has metrics enabled
    /// (`x-spvirit.metrics.enabled`, possibly forced on by `--metrics`); the
    /// endpoint is bound and served in [`Runtime::run`].
    metrics: Option<(String, String)>,
    /// The single [`ClientRegistry`] shared by every server this gateway
    /// builds and by the status source's `clients` PV — retained so it lives
    /// as long as the gateway (a server holds its own clone via
    /// `.client_registry()`, but this keeps the registry alive even before
    /// any server starts, and is available for future runtime-level readers).
    client_registry: Arc<ClientRegistry>,
    /// The single [`BandwidthCounters`] shared by every upstream
    /// [`spvirit_client::PvaClient`]'s `ByteSink` (via
    /// `UpstreamPool::from_config_with_counters`) AND by every server this
    /// gateway builds (via `PvaServer::bandwidth_counters`) — the
    /// unification point where upstream (`us_*`) and downstream (`ds_*`)
    /// wire-byte accounting land in ONE counter set. Retained here so a
    /// future status-PV reader (Task 13) can read it without a server
    /// reference.
    bandwidth_counters: Arc<BandwidthCounters>,
    /// The latest 1 Hz [`RateSnapshot`] produced by the [`BandwidthSampler`]
    /// spawned in [`Runtime::run`], converting `bandwidth_counters` +
    /// `client_registry`'s cumulative byte counts into B/s rows. `std::sync
    /// ::Mutex` is correct here (not a tokio mutex): every read/write is a
    /// short, synchronous swap, never held across an `.await`. Retained on
    /// `Runtime` (rather than only inside the spawned task) so a future
    /// status-PV/`/metrics` reader (Tasks 13/14) can read it without a
    /// server reference, mirroring `bandwidth_counters`/`client_registry`.
    rate_snapshot: Arc<Mutex<RateSnapshot>>,
}

impl Runtime {
    /// The single [`ClientRegistry`] shared by every server this gateway
    /// built and (when configured) by the status source's `clients` PV.
    /// Exposed so tests can confirm the same `Arc` reached every consumer
    /// (via `Arc::strong_count`/`Arc::ptr_eq`) without exposing per-server
    /// internals.
    pub fn client_registry(&self) -> &Arc<ClientRegistry> {
        &self.client_registry
    }

    /// The single [`BandwidthCounters`] shared by every upstream client's
    /// `ByteSink` and every server's `ds_*` accounting. Exposed so tests
    /// (and a future status-PV reader) can confirm the same `Arc` reached
    /// every consumer, mirroring [`client_registry`](Self::client_registry).
    pub fn bandwidth_counters(&self) -> &Arc<BandwidthCounters> {
        &self.bandwidth_counters
    }

    /// The latest 1 Hz [`RateSnapshot`] produced by the [`BandwidthSampler`],
    /// shared with a future status-PV/`/metrics` reader. Exposed so tests
    /// (and Tasks 13/14) can read the current rate rows without a server
    /// reference, mirroring [`bandwidth_counters`](Self::bandwidth_counters).
    pub fn rate_snapshot(&self) -> &Arc<Mutex<RateSnapshot>> {
        &self.rate_snapshot
    }

    /// Validate `cfg` and build a `Runtime` from it.
    ///
    /// Builds one shared [`UpstreamPool`] for the whole configuration (every
    /// `clients[]` entry becomes one upstream [`spvirit_client::PvaClient`]),
    /// then for each `servers[]` entry builds a [`LoopGuard`], a
    /// [`NegativeCache`] (from that server's `x-spvirit.negativeCache`, or
    /// the defaults above; a configured `capacity: 0` is clamped to `1` per
    /// the Task 8 deferred note since a zero-capacity cache cannot hold
    /// anything), and a [`GatewaySource`] wired to that server's
    /// `clients[]` order — then a [`PvaServer`] listening on that server's
    /// `serverport`/`bcastport`, bound (and advertised) on the first
    /// `interface` entry.
    ///
    /// If a server has no `interface` entries, it binds to `0.0.0.0`
    /// (all interfaces) — the safest default for "unspecified", and
    /// consistent with p4p's own gateway defaulting to a wildcard bind when
    /// no interface list is given.
    pub fn from_config(cfg: GatewayConfig) -> Result<Self, ConfigError> {
        cfg.validate()?;

        // One shared BandwidthCounters for the whole gateway, built BEFORE
        // the upstream pool so the SAME `Arc` (never a second instance) is
        // threaded into: every upstream `PvaClient`'s `ByteSink` (via
        // `from_config_with_counters` below) and every server's `ds_*`
        // accounting (via `.bandwidth_counters()` on each builder below) —
        // the unification step that lets upstream and downstream byte
        // counts land in one counter set.
        let bandwidth_counters = Arc::new(BandwidthCounters::new());
        let pool = Arc::new(UpstreamPool::from_config_with_counters(
            &cfg,
            Some(&bandwidth_counters),
        ));
        let mut servers = Vec::with_capacity(cfg.servers.len());
        let mut sources: Vec<Arc<GatewaySource>> = Vec::with_capacity(cfg.servers.len());
        // One shared registry for the whole gateway: every server built below
        // is injected with a clone of this SAME `Arc`, and the status source
        // (when a `statusprefix` is configured) reads from another clone —
        // so a connection recorded by any server's lifecycle hooks is
        // visible to that server's own `clients` PV.
        let client_registry = Arc::new(ClientRegistry::new());

        // One shared rate snapshot for the whole gateway, built BEFORE the
        // servers loop so the status source (below) can wire its
        // `bandwidth` handle to the SAME `Arc<Mutex<RateSnapshot>>` the
        // `BandwidthSampler` (started in `run`, below) writes into.
        let rate_snapshot = Arc::new(Mutex::new(RateSnapshot::default()));

        // One GUID per server, generated before anything so the ban-set is
        // complete by the time any server's LoopGuard consults it.
        let server_guids: Vec<[u8; 12]> = cfg.servers.iter().map(|_| rand_guid()).collect();
        let all_guids: HashSet<[u8; 12]> = server_guids.iter().copied().collect();

        for (i, server_cfg) in cfg.servers.iter().enumerate() {
            let guard = Arc::new(LoopGuard::build(&cfg, server_cfg, all_guids.clone()));

            let (ttl, capacity) = server_cfg
                .x_spvirit
                .as_ref()
                .and_then(|ext| ext.negative_cache.as_ref())
                .map(|nc| (Duration::from_millis(nc.ttl_ms), nc.capacity.max(1)))
                .unwrap_or((DEFAULT_NEG_CACHE_TTL, DEFAULT_NEG_CACHE_CAPACITY));
            let neg = Arc::new(NegativeCache::new(ttl, capacity));
            let access = Arc::new(cfg.build_access_control(server_cfg)?);

            let src_arc = Arc::new(GatewaySource::new(
                pool.clone(),
                server_cfg.clients.clone(),
                neg,
                guard,
                server_cfg.getholdoff,
                access.clone(),
            ));

            let interface_ip: IpAddr = server_cfg
                .interface
                .first()
                .and_then(|s| s.parse::<IpAddr>().ok())
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));

            let mut builder = PvaServer::builder()
                .port(server_cfg.serverport)
                .udp_port(server_cfg.bcastport)
                .listen_ip(interface_ip)
                .advertise_ip(interface_ip)
                .discovery_parity(server_cfg.discovery_parity)
                .guid(server_guids[i])
                .client_registry(client_registry.clone())
                .bandwidth_counters(bandwidth_counters.clone())
                .source("gateway", 0, src_arc.clone());

            // Status PVs claim under a lower `.source()` order (-10 <
            // gateway's 0) so `<statusprefix>*` names always resolve here
            // first, before the gateway ever gets a chance to shadow them
            // by forwarding to an upstream PV of the same name.
            if !server_cfg.statusprefix.is_empty() {
                let status = Arc::new(StatusSource::new(
                    server_cfg.statusprefix.clone(),
                    access.clone(),
                    StatusHandles::from_gateway_with(
                        &src_arc,
                        client_registry.clone(),
                        rate_snapshot.clone(),
                    ),
                ));
                for line in banner::status_pv_lines(&server_cfg.statusprefix) {
                    tracing::info!("{line}");
                }
                builder = builder.source("status", -10, status);
            }

            let server = builder.build();

            servers.push(server);
            sources.push(src_arc);

            tracing::info!(
                "spgateway: server '{}' -> {}:{} (udp {}), upstreams [{}]",
                server_cfg.name,
                interface_ip,
                server_cfg.serverport,
                server_cfg.bcastport,
                server_cfg.clients.join(", "),
            );
        }

        tracing::info!(
            "spgateway: {} server(s), {} upstream(s) configured",
            cfg.servers.len(),
            cfg.clients.len(),
        );

        // Effective metrics setting: enabled by `x-spvirit.metrics.enabled`
        // (the CLI's `--metrics`/`--metrics-listen` mutate this same config
        // block before `from_config`, so both the `-T` and serving paths see
        // the same effective value).
        let metrics = cfg
            .x_spvirit
            .as_ref()
            .and_then(|t| t.metrics.as_ref())
            .filter(|m| m.enabled)
            .map(|m| (m.listen.clone(), m.path.clone()));

        Ok(Runtime {
            servers,
            pool,
            sources,
            metrics,
            client_registry,
            bandwidth_counters,
            rate_snapshot,
        })
    }

    /// Run every server's serve loop concurrently until either all of them
    /// exit, one of them errors, or Ctrl-C is received (in which case `run`
    /// returns `Ok(())` — a graceful stop, not a failure).
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        tracing::info!(
            "spgateway: starting {} server(s); press Ctrl-C to stop",
            self.servers.len()
        );

        // Metrics endpoint: bind up front so a bind failure is a FATAL
        // startup error (the user explicitly asked for the endpoint). When
        // disabled, `metrics_fut` is a never-completing `pending()` so it is
        // simply an inert `select!` arm.
        let metrics_fut = {
            let metrics = self.metrics.clone();
            let pool = self.pool.clone();
            let sources = self.sources.clone();
            let bandwidth_counters = self.bandwidth_counters.clone();
            let client_registry = self.client_registry.clone();
            async move {
                match metrics {
                    Some((listen, path)) => {
                        let listener = crate::metrics::bind(&listen).await.map_err(|e| {
                            tracing::error!("spgateway: metrics bind to {listen} failed: {e}");
                            format!("metrics bind to {listen} failed: {e}")
                        })?;
                        let bound = listener.local_addr().map_err(|e| e.to_string())?;
                        tracing::info!("spgateway: metrics endpoint on http://{bound}{path}");
                        // Covered end-to-end by
                        // `spvirit-gateway/tests/it_metrics.rs`, which stands
                        // up this `Runtime` and scrapes the endpoint over a
                        // real socket. Two earlier attempts tested a helper
                        // one frame up instead, and both left this line free
                        // to be replaced by a provider that reports zeros.
                        let provider = build_snapshot_provider(
                            pool,
                            sources,
                            bandwidth_counters,
                            client_registry,
                        );
                        crate::metrics::serve(listener, path, provider).await;
                        Ok(())
                    }
                    None => std::future::pending::<Result<(), String>>().await,
                }
            }
        };
        tokio::pin!(metrics_fut);

        let mut set = tokio::task::JoinSet::new();
        for server in self.servers {
            // `PvaServer::run`'s error type (`Box<dyn std::error::Error>`,
            // without a `Send` bound) can't cross a `JoinSet`'s task
            // boundary directly, so it is stringified inside the spawned
            // task, before it would otherwise need to be held across an
            // await point in the joiner.
            set.spawn(async move { server.run().await.map_err(|e| e.to_string()) });
        }

        // The 1 Hz BandwidthSampler: joined into the SAME `JoinSet` as the
        // servers above, rather than a bare `tokio::spawn`, so it shares
        // their exact shutdown mechanism — this task, like a server's
        // `run()`, loops forever (never returns `Ok`), and is dropped
        // (cancelled) the moment the outer `select!` below resolves via
        // Ctrl-C or a fatal metrics error, precisely mirroring how a server
        // task is torn down. No new shutdown path is introduced.
        {
            let counters = self.bandwidth_counters.clone();
            let registry = self.client_registry.clone();
            let snapshot = self.rate_snapshot.clone();
            set.spawn(async move {
                let mut sampler = BandwidthSampler::new(counters, registry, snapshot);
                let mut interval = tokio::time::interval(SAMPLER_TICK_PERIOD);
                loop {
                    interval.tick().await;
                    sampler.tick();
                }
            });
        }

        tokio::select! {
            result = join_all(&mut set) => result,
            _ = tokio::signal::ctrl_c() => Ok(()),
            // Only completes on a fatal metrics error (e.g. bind failure);
            // the serve loop itself runs until the runtime is dropped.
            res = &mut metrics_fut => res.map_err(Into::into),
        }
    }
}

/// Build the `/metrics` scrape callback.
///
/// A named function rather than a closure written inline in [`Runtime::run`]:
/// inline, the only thing a test could reach was the pure
/// [`metrics::apply_resolve_stats`](crate::metrics::apply_resolve_stats)
/// helper it calls, so *deleting the call* left every resolver counter
/// reading zero on a live `/metrics` with the whole suite green. The call site
/// is the deliverable; this makes it the thing under test.
fn build_snapshot_provider(
    pool: Arc<UpstreamPool>,
    sources: Vec<Arc<GatewaySource>>,
    bandwidth_counters: Arc<BandwidthCounters>,
    client_registry: Arc<ClientRegistry>,
) -> crate::metrics::SnapshotProvider {
    Arc::new(move || {
        // Read at scrape time, not cached: `upstream_monitors` was once found
        // reporting a stale value straight through a total outage, which is
        // exactly the failure a counter is supposed to make visible.
        let r = spvirit_server::search_resolve::global_stats();
        let mut snap = crate::metrics::MetricsSnapshot {
            clients: pool.names().len() as u64,
            upstream_monitors: sources
                .iter()
                .map(|s| s.upstream_monitor_count() as u64)
                .sum(),
            upstream_monitor_deaths: sources
                .iter()
                .map(|s| s.upstream_monitor_deaths())
                .sum(),
            ..crate::metrics::snapshot_from_bandwidth(&bandwidth_counters, &client_registry)
        };
        crate::metrics::apply_resolve_stats(&mut snap, &r);
        snap
    })
}

/// Drain `set`, returning the first error encountered (from either a failed
/// server or a panicked/cancelled task), or `Ok(())` once every server's
/// `run()` has returned successfully.
async fn join_all(
    set: &mut tokio::task::JoinSet<Result<(), String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    while let Some(res) = set.join_next().await {
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e.into()),
            Err(join_err) => return Err(join_err.to_string().into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIDI: &str = include_str!("../tests/fixtures/p4p_bidirectional.json");

    /// V2-3: the `/metrics` scrape callback must actually *call*
    /// `apply_resolve_stats`. Testing the helper alone left the call site
    /// uncovered — deleting it made every resolver counter on a live
    /// `/metrics` read zero with a green suite, which is the original
    /// "nothing can call this closure" finding moved rather than closed.
    ///
    /// The counters are process-wide and monotonic, so this asserts on a
    /// *delta* the test itself produces; a neighbour bumping them concurrently
    /// can only make the delta larger.
    #[test]
    fn the_metrics_provider_reports_the_resolver_counters() {
        use spvirit_server::pvstore::TryClaim;
        use spvirit_server::search_resolve::note_try_claim;

        let cfg = GatewayConfig::from_json_str(r#"{ "version":2 }"#).unwrap();
        let provider = build_snapshot_provider(
            Arc::new(UpstreamPool::from_config(&cfg)),
            Vec::new(),
            Arc::new(BandwidthCounters::new()),
            Arc::new(ClientRegistry::new()),
        );

        let before = provider();
        for _ in 0..7 {
            note_try_claim(TryClaim::Yes);
        }
        for _ in 0..3 {
            note_try_claim(TryClaim::No);
        }
        let after = provider();

        assert!(
            after.search_try_claim_yes >= before.search_try_claim_yes + 7,
            "/metrics did not carry the resolver's try_claim_yes counter \
             (before={}, after={}); the snapshot provider is not applying \
             `apply_resolve_stats`",
            before.search_try_claim_yes,
            after.search_try_claim_yes
        );
        assert!(
            after.search_try_claim_no >= before.search_try_claim_no + 3,
            "/metrics did not carry the resolver's try_claim_no counter \
             (before={}, after={})",
            before.search_try_claim_no,
            after.search_try_claim_no
        );
    }

    #[test]
    fn from_config_rejects_invalid_config() {
        let cfg = GatewayConfig::from_json_str(
            r#"{ "version":2, "clients":[{"name":"a"}], "servers":[{"name":"s","clients":["nope"]}] }"#,
        )
        .unwrap();
        match Runtime::from_config(cfg) {
            Err(ConfigError::Validation(_)) => {}
            Err(other) => panic!("expected a validation error, got {other:?}"),
            Ok(_) => panic!("expected a validation error, got Ok"),
        }
    }

    #[test]
    fn from_config_builds_one_server_per_entry() {
        // BIDI's `pvlist` path (`/etc/pvagw/pvacl.conf`) is illustrative of a
        // real p4p deployment, not a file present in this checkout; clear it
        // so this test exercises server construction, not fail-closed file
        // loading (Task 10's `validate()` now loads it for real).
        let mut cfg = GatewayConfig::from_json_str(BIDI).unwrap();
        for s in &mut cfg.servers {
            s.pvlist.clear();
            s.access.clear();
        }
        let n_servers = cfg.servers.len();
        let rt = Runtime::from_config(cfg).expect("valid config builds");
        assert_eq!(rt.servers.len(), n_servers);
    }

    /// The gateway's `client_registry` is one shared `Arc`, injected into
    /// every server (via `.client_registry()`) AND into the status source's
    /// `clients` handle (when `statusprefix` is set). This is a same-Arc
    /// wiring check (not a full end-to-end connection test): a config with
    /// `n` servers, each with a non-empty `statusprefix`, should leave the
    /// registry's strong count at more than just the `Runtime`'s own clone —
    /// proving the servers/status sources hold clones of it rather than each
    /// building their own registry.
    #[test]
    fn client_registry_is_shared_with_every_server() {
        let mut cfg = GatewayConfig::from_json_str(BIDI).unwrap();
        for s in &mut cfg.servers {
            s.pvlist.clear();
            s.access.clear();
            s.statusprefix = format!("{}:STATUS:", s.name);
        }
        let n_servers = cfg.servers.len();
        assert!(n_servers > 0, "fixture must have at least one server for this check");

        let rt = Runtime::from_config(cfg).expect("valid config builds");
        // The `Runtime`'s own clone, plus at least one clone per server
        // (injected via `.client_registry()`) and one per status source
        // (`from_gateway_with`): strictly more than 1, and consistent with
        // `n_servers` servers each holding onto their own clone(s).
        let strong = Arc::strong_count(rt.client_registry());
        assert!(
            strong > n_servers,
            "expected the shared registry's strong count ({strong}) to exceed the \
             server count ({n_servers}) once every server + status source holds a clone"
        );
    }

    /// The gateway's `bandwidth_counters` is ONE shared `Arc`, built once in
    /// `from_config` and injected as the SAME instance into every upstream
    /// client's `CountersSink` (via `UpstreamPool::from_config_with_counters`)
    /// AND into every server's `ds_*` accounting (via
    /// `PvaServer::bandwidth_counters`) — never a second
    /// `BandwidthCounters::new()`. Like `client_registry_is_shared_with_every_server`,
    /// this is a same-Arc wiring check via `Arc::strong_count`: the
    /// `Runtime`'s own clone, plus one per server, plus one per upstream
    /// client's sink, must exceed the server count alone.
    #[test]
    fn bandwidth_counters_is_shared_with_every_server_and_upstream_client() {
        let mut cfg = GatewayConfig::from_json_str(BIDI).unwrap();
        for s in &mut cfg.servers {
            s.pvlist.clear();
            s.access.clear();
        }
        let n_servers = cfg.servers.len();
        assert!(n_servers > 0, "fixture must have at least one server for this check");
        assert!(
            !cfg.clients.is_empty(),
            "fixture must have at least one client for this check"
        );

        let rt = Runtime::from_config(cfg).expect("valid config builds");
        // Runtime's own clone + at least one clone per server + at least one
        // clone per upstream client's CountersSink: strictly more than just
        // the server count.
        let strong = Arc::strong_count(rt.bandwidth_counters());
        assert!(
            strong > n_servers,
            "expected the shared BandwidthCounters' strong count ({strong}) to exceed the \
             server count ({n_servers}) once every server + upstream client sink holds a clone"
        );
    }
}
