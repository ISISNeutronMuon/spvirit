//! `GatewaySource` — the [`spvirit_server::pvstore::Source`] implementation
//! that bridges downstream requests to upstream [`spvirit_client::PvaClient`]s.
//!
//! M1 wires up `claim`/`get`/`put`/`subscribe`/`names` (Tasks 10-13); `rpc`
//! remains an explicit "not implemented" stub (see its doc comment) and
//! `names` reports claimed bindings rather than a full upstream `pvlist`
//! fan-out (see its doc comment) — both documented M1/§14 gaps.

use std::collections::HashMap;
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::pvstore::{PvInfo, Source, TryClaim};
use spvirit_types::NtPayload;
use tokio::sync::mpsc;

use crate::access::{AccessControl, Decision, Identity, Op};
use crate::bridge::{merge_monitor_delta, nt_payload_from_decoded, nt_payload_from_get};
use crate::cache::monitor::{MonitorCache, MonitorKey};
use crate::convert::decoded_to_json;
use crate::cache::negative::NegativeCache;
use crate::loopguard::LoopGuard;
use crate::upstream::UpstreamPool;

/// Snapshots the current downstream connection's identity (socket peer host,
/// and decoded `ca` user if any) into an [`Identity`] for
/// [`AccessControl::decide`].
///
/// `host` is always the socket peer IP — it is the only value trusted for
/// HAG / pvlist-`FROM` matching (design spec §5.4, §6.3). The client-asserted
/// `host` string carried in `ca` connection-validation credentials is
/// advisory only and is deliberately never used for an access decision:
/// trusting it would let a client claim a trusted hostname it isn't actually
/// connecting from and bypass host-based rules (a spoofing hole). `user`, by
/// contrast, *is* the self-asserted `ca` value — spvirit matches p4p's
/// posture here, where ACF has always trusted the asserted user (UAG is
/// authorization, not authentication).
///
/// Returns a default (all-`None`) `Identity` when called outside a
/// [`spvirit_server::request_ctx`] scope (e.g. a unit test that calls
/// `claim`/`put`/`rpc` directly, off any connection task) — a permissive
/// `AccessControl` still behaves correctly in that case, and a restrictive
/// one fails closed (no host/user to match against). Whenever a request
/// scope *is* present, the peer IP is always present too, so `host` is only
/// `None` in that out-of-scope case.
///
/// Delegates to [`spvirit_server::request_ctx::request_identity`] rather than
/// reading `current_request` itself, so this crate's notion of "who is
/// asking" and `SourceRegistry`'s resolver-memo key can never drift apart.
fn current_identity() -> Identity {
    let (peer, user) = spvirit_server::request_ctx::request_identity();
    Identity {
        host: peer.map(|ip| ip.to_string()),
        user,
    }
}

/// Record of which upstream client resolved a downstream PV name, and under
/// what name it is known upstream.
///
/// Kept minimal (just the fields Tasks 10-13 need to route `get`/`put`/
/// `subscribe` back to the right client) so it doubles as the binding cache
/// value with no rework required.
///
/// `last_get` is the getholdoff cache for the *read* path (`get`, and later
/// `subscribe`'s initial value): the last successful upstream fetch plus the
/// instant it was fetched. It lives on the binding (rather than a parallel
/// map keyed by name) so it is naturally torn down together with the
/// binding and cannot desync from it; Task 12's `put` does not need a slot
/// here since a put's own reply value is not subject to getholdoff.
#[derive(Debug)]
pub struct Binding {
    pub client_name: String,
    pub real_name: String,
    /// The upstream introspection's `StructureDesc.struct_id` (e.g.
    /// `"epics:nt/NTScalar:1.0"`), captured at `claim` time so `subscribe`
    /// can stamp monitored payloads with the same struct id `get` already
    /// reports (`nt_payload_from_get` sources it from
    /// `PvGetResult::introspection` directly; `subscribe` has no per-tick
    /// introspection round-trip, so it must carry this forward from claim).
    struct_id: Option<String>,
    last_get: Mutex<Option<(Instant, NtPayload)>>,
}

/// A [`Source`] that resolves and forwards PVs to upstream PVA servers.
pub struct GatewaySource {
    pool: Arc<UpstreamPool>,
    client_order: Vec<String>,
    neg: Arc<NegativeCache>,
    /// Refuses to bind a name that resolves back into one of our own
    /// downstream server sockets (or an `ignoreaddr` host); consulted in
    /// [`GatewaySource::claim`].
    guard: Arc<LoopGuard>,
    getholdoff_ms: u32,
    /// Downstream name -> upstream binding.
    ///
    /// `Arc`-wrapped so `subscribe`'s spawned upstream-monitor task (a
    /// `'static` task that cannot borrow `&self`) can retire this PV's binding
    /// when its upstream dies. Every other call site dereferences through the
    /// `Arc` unchanged.
    bindings: Arc<Mutex<HashMap<String, Binding>>>,
    monitors: Arc<MonitorCache>,
    /// How many upstream monitors this source has torn down because their
    /// upstream connection ended (error or clean EOF) — NOT the normal
    /// last-subscriber-left retirement, which is not a fault. `Arc` so the
    /// spawned upstream task can bump it; surfaced on `/metrics`.
    deaths: Arc<std::sync::atomic::AtomicU64>,
    /// The readOnly/pvlist/ACF gate consulted at `claim` (`Op::Get`), `put`
    /// (`Op::Put`), and `rpc` (`Op::Rpc`). Precedence (readOnly > pvlist >
    /// ACF) lives entirely inside `AccessControl::decide` — this source only
    /// calls it and applies the returned `Decision`.
    access: Arc<AccessControl>,
}

impl GatewaySource {
    /// Build a source that searches `client_order` (in order) for each
    /// downstream name, backed by `pool`, remembering upstream misses in
    /// `neg`, refusing to resolve back into our own servers via `guard`, and
    /// gating every claim/put/rpc through `access`.
    pub fn new(
        pool: Arc<UpstreamPool>,
        client_order: Vec<String>,
        neg: Arc<NegativeCache>,
        guard: Arc<LoopGuard>,
        getholdoff_ms: u32,
        access: Arc<AccessControl>,
    ) -> Self {
        GatewaySource {
            pool,
            client_order,
            neg,
            guard,
            getholdoff_ms,
            bindings: Arc::new(Mutex::new(HashMap::new())),
            monitors: Arc::new(MonitorCache::new()),
            deaths: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            access,
        }
    }

    /// Number of distinct upstream monitors currently running. Exposed for
    /// tests proving that multiple `subscribe` calls for the same PV dedup
    /// onto a single upstream monitor task.
    pub fn upstream_monitor_count(&self) -> usize {
        self.monitors.upstream_count()
    }

    /// Names of the upstream channels currently held in the monitor cache —
    /// the live data behind the `cache` status PV (p4p's "upstream channel
    /// cache summary"). Cheap snapshot; no lock held across an await.
    pub fn upstream_monitor_names(&self) -> Vec<String> {
        self.monitors.names()
    }

    /// Forget the binding for downstream `name`.
    ///
    /// Called on the upstream-death teardown path. Leaving the binding in
    /// place would make `try_claim` keep answering `Yes` from memory (it
    /// consults the bindings map without contacting upstream), so a client
    /// that just received DESTROY_CHANNEL would re-search, be answered `Yes`,
    /// create a channel, hit the still-dead upstream and be destroyed again —
    /// an unbounded hot loop between client and gateway (design spec §4).
    /// With the binding gone `try_claim` returns `Unknown`, the background
    /// resolve runs a real `claim`, `pvinfo_full` fails, the negative cache
    /// records the miss, and the search simply goes unanswered — so the client
    /// falls back to normal PVA search backoff and recovers on its own when
    /// the IOC returns.
    pub fn retire_binding(&self, name: &str) {
        self.bindings.lock().unwrap().remove(name);
    }

    /// Number of upstream monitors torn down because their upstream ended.
    /// Mirrored onto `/metrics` as `spgateway_upstream_monitor_deaths_total`.
    pub fn upstream_monitor_deaths(&self) -> u64 {
        self.deaths.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Source for GatewaySource {
    fn try_claim(&self, name: &str) -> TryClaim {
        // Same order as `claim`: negative cache, then access, then bindings.
        // Anything this cannot settle is `Unknown` — the search path will have
        // it resolved in the background and answer the client's retry.
        if self.neg.is_missing(name, Instant::now()) {
            return TryClaim::No;
        }
        if let Decision::Deny = self.access.decide(Op::Get, name, &current_identity()) {
            return TryClaim::No;
        }
        let Ok(bindings) = self.bindings.try_lock() else {
            return TryClaim::Unknown;
        };
        if bindings.contains_key(name) {
            TryClaim::Yes
        } else {
            TryClaim::Unknown
        }
    }

    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let now = Instant::now();
            if self.neg.is_missing(&name, now) {
                return None;
            }

            // Access control gates visibility of the PV itself: a `Deny`
            // means the downstream name is invisible/unclaimable, and an
            // `AllowAliased` rewrites the *upstream* target we resolve
            // against while the binding (and everything downstream — the
            // registry key, the handler, `MonitorKey`) still keys off the
            // requested downstream `name`.
            let id = current_identity();
            let target = match self.access.decide(Op::Get, &name, &id) {
                Decision::Deny => return None,
                Decision::Allow => name.clone(),
                Decision::AllowAliased(real) => real,
            };

            for client_name in &self.client_order {
                let Some(client) = self.pool.client(client_name) else {
                    continue;
                };
                if let Ok((descriptor, server_addr, guid)) = client.pvinfo_full(&target).await {
                    // Loop / self-connection prevention: if this name resolved
                    // back into one of our own downstream server sockets (or an
                    // `ignoreaddr` host), or the responder's GUID matches one of
                    // our own servers' GUIDs, do NOT bind it — forwarding would
                    // loop a search into ourselves. Skip to the next client.
                    if self.guard.is_banned(server_addr) || self.guard.is_guid_banned(&guid) {
                        continue;
                    }
                    self.bindings.lock().unwrap().insert(
                        name.clone(),
                        Binding {
                            client_name: client_name.clone(),
                            real_name: target.clone(),
                            struct_id: descriptor.struct_id.clone(),
                            last_get: Mutex::new(None),
                        },
                    );
                    return Some(PvInfo {
                        descriptor,
                        writable: true,
                    });
                }
            }

            self.neg.record_miss(&name, now);
            None
        })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            // Resolve the binding and, while still holding the bindings-map
            // lock, check the per-binding getholdoff cache. Both locks
            // (outer map, inner `last_get`) are acquired and released
            // within this block, before any `.await` — never held across
            // one.
            let (client_name, real_name) = {
                let bindings = self.bindings.lock().unwrap();
                let binding = bindings.get(&name)?;

                let cache = binding.last_get.lock().unwrap();
                if let Some((t_last, payload)) = cache.as_ref()
                    && Instant::now() < *t_last + Duration::from_millis(self.getholdoff_ms as u64)
                {
                    return Some(payload.clone());
                }
                drop(cache);

                (binding.client_name.clone(), binding.real_name.clone())
            };

            let client = self.pool.client(&client_name)?;
            match client.pvget(&real_name).await {
                Ok(result) => {
                    let payload = nt_payload_from_get(&result);
                    if let Some(binding) = self.bindings.lock().unwrap().get(&name) {
                        *binding.last_get.lock().unwrap() = Some((Instant::now(), payload.clone()));
                    }
                    Some(payload)
                }
                Err(_) => None,
            }
        })
    }

    fn put(
        &self,
        name: &str,
        value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        let name = name.to_string();
        let json = decoded_to_json(value);
        Box::pin(async move {
            let json = json?;

            if let Decision::Deny = self.access.decide(Op::Put, &name, &current_identity()) {
                return Err("access denied".to_string());
            }

            // Resolve the binding and clone out what's needed before the
            // `.await` below — never hold the bindings-map MutexGuard across
            // an await point, matching `get`'s discipline.
            let (client_name, real_name) = {
                let bindings = self.bindings.lock().unwrap();
                let binding = bindings
                    .get(&name)
                    .ok_or_else(|| format!("no binding for unclaimed PV {name:?}"))?;
                (binding.client_name.clone(), binding.real_name.clone())
            };

            let client = self
                .pool
                .client(&client_name)
                .ok_or_else(|| format!("no upstream client named {client_name:?}"))?;

            client
                .pvput(&real_name, json)
                .await
                .map_err(|e| e.to_string())?;

            // A gateway just forwards the write; there is no forward-link
            // propagation to report back (that is server-side record
            // behavior, out of scope for a pass-through proxy).
            Ok(vec![])
        })
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let (client_name, real_name, struct_id) = {
                let bindings = self.bindings.lock().unwrap();
                let binding = bindings.get(&name)?;
                (
                    binding.client_name.clone(),
                    binding.real_name.clone(),
                    binding.struct_id.clone().unwrap_or_default(),
                )
            };

            let client = self.pool.client(&client_name)?;
            let key: MonitorKey = (client_name.clone(), real_name.clone());
            let monitors = self.monitors.clone();
            let bindings = self.bindings.clone();
            let deaths = self.deaths.clone();
            let down_name = name.clone();

            let rx = self.monitors.subscribe(key.clone(), move |entry| {
                tokio::spawn(async move {
                    let mut last_full = DecodedValue::Null;
                    // Set by the callback IMMEDIATELY BEFORE it returns
                    // `Break`, i.e. ONLY on the "last subscriber left" path.
                    //
                    // This flag is load-bearing and cannot be replaced by
                    // inspecting `pvmonitor`'s `Result`: a clean upstream EOF
                    // and a callback-requested stop BOTH surface as `Ok(())`,
                    // so a Result-based test would either miss real upstream
                    // deaths or double-retire an entry `dispatch_or_retire`
                    // has already removed (and possibly clobber a replacement
                    // entry created for the same key in the meantime).
                    let stopped_by_callback = Arc::new(AtomicBool::new(false));
                    let callback_stopped = stopped_by_callback.clone();
                    let callback_entry = entry.clone();
                    let callback_monitors = monitors.clone();
                    let callback_key = key.clone();
                    let result = client
                        .pvmonitor(&real_name, move |update| {
                            merge_monitor_delta(&mut last_full, update);
                            let payload = nt_payload_from_decoded(&last_full, struct_id.clone());
                            if callback_monitors.dispatch_or_retire(
                                &callback_key,
                                &callback_entry,
                                payload,
                            ) {
                                ControlFlow::Continue(())
                            } else {
                                callback_stopped.store(true, Ordering::SeqCst);
                                ControlFlow::Break(())
                            }
                        })
                        .await;

                    if stopped_by_callback.load(Ordering::SeqCst) {
                        // Last subscriber left. `dispatch_or_retire` already
                        // removed this key's entry from the map (atomically,
                        // under the map lock) the moment it decided the
                        // upstream loop should end. This is the existing,
                        // correct shutdown path: nothing to clean up, and no
                        // fault to report.
                        return;
                    }

                    // Upstream ended — an error, or a clean EOF. Either way
                    // there will never be another fan-out for this key, so
                    // nothing else will ever tear the entry down.
                    monitors.retire(&key, &entry);
                    // Removing the map's `Arc` is not enough: this task holds
                    // a clone, so the `Sender`s would survive and every
                    // downstream receiver would stay open and silent forever.
                    // Dropping them is what closes those receivers and makes
                    // the server-side pump observe `rx.recv() == None`.
                    entry.close_all();
                    // Without this, `try_claim` keeps answering `Yes` from the
                    // bindings map and a destroyed client hot-loops straight
                    // back into the dead upstream (design spec §4).
                    bindings.lock().unwrap().remove(&down_name);
                    deaths.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        "upstream monitor ended for real='{}' downstream='{}' via client='{}' \
                         ({:?}); retired the cache entry and binding and closed every \
                         downstream subscriber",
                        real_name,
                        down_name,
                        client_name,
                        result.err(),
                    );
                });
            });

            Some(rx)
        })
    }

    /// RPC forwarding to upstream servers is out of scope for M1:
    /// `PvaClient` (`spvirit-client`) exposes no general-purpose RPC call —
    /// the only "rpc" in the client is an internal `pvlist`-via-server-RPC
    /// helper, not a channel-addressed RPC primitive a gateway could forward
    /// arbitrary requests through. Rather than silently inheriting the
    /// `Source` trait's generic `"RPC not supported"` default, this override
    /// makes the gap explicit and greppable. Real forwarding is deferred
    /// until `spvirit-client` grows a general RPC call (spec §14 gap).
    fn rpc(
        &self,
        name: &str,
        _args: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<NtPayload, String>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            if let Decision::Deny = self.access.decide(Op::Rpc, &name, &current_identity()) {
                return Err("access denied".to_string());
            }
            Err("gateway RPC forwarding is not implemented in M1".to_string())
        })
    }

    /// Returns the sorted, deduplicated set of PV names this gateway has
    /// already successfully `claim`ed (the keys of `bindings`), *not* a full
    /// enumeration of every name available upstream.
    ///
    /// The plan's original design called for the union of `pvlist` results
    /// across `client_order`, but `PvaClient::pvlist`/`pvlist_with_fallback`
    /// both require a concrete upstream `SocketAddr`, and the gateway never
    /// caches one per client — `UpstreamPool` resolves upstreams dynamically
    /// via search per-name, so no single "the server address for this
    /// client" value exists to drive `pvlist` with. This is a documented M1
    /// divergence: full upstream namespace enumeration via `pvlist` fan-out
    /// is deferred until per-upstream server addresses are wired up (spec
    /// §14 / M2).
    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        let mut names: Vec<String> = {
            let bindings = self.bindings.lock().unwrap();
            bindings.keys().cloned().collect()
        };
        names.sort();
        names.dedup();
        Box::pin(async move { names })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;
    use std::time::Duration;

    fn test_source(neg: Arc<NegativeCache>, access: Arc<AccessControl>) -> GatewaySource {
        let cfg = GatewayConfig::from_json_str(r#"{"version":2,"clients":[],"servers":[]}"#)
            .unwrap();
        let pool = Arc::new(UpstreamPool::from_config(&cfg));
        let guard = Arc::new(LoopGuard::build(
            &cfg,
            &crate::config::ServerCfg {
                // (interface: vec![] below picks up the 0.0.0.0 local-IP
                // backstop; harmless for these no-clients-configured tests.)
                name: "s".into(),
                clients: vec![],
                interface: vec![],
                addrlist: String::new(),
                ignoreaddr: String::new(),
                autoaddrlist: true,
                serverport: 5075,
                bcastport: 5076,
                discovery_parity: true,
                getholdoff: 0,
                statusprefix: String::new(),
                access: String::new(),
                pvlist: String::new(),
                acf_client: None,
                x_spvirit: None,
            },
            std::collections::HashSet::new(),
        ));
        GatewaySource::new(pool, vec![], neg, guard, 0, access)
    }

    fn open_neg() -> Arc<NegativeCache> {
        Arc::new(NegativeCache::new(Duration::from_secs(30), 128))
    }

    fn allow_all() -> Arc<AccessControl> {
        Arc::new(AccessControl::new(false, None, None))
    }

    #[tokio::test]
    async fn claim_returns_none_when_no_clients_configured() {
        let src = test_source(open_neg(), allow_all());
        assert!(src.claim("ANY:PV").await.is_none());
    }

    /// Before a binding exists, `try_claim` cannot say `Yes` (that would be a
    /// search reply for a name that isn't actually bound yet); once one is
    /// inserted directly, it must. This before/after shape is what makes the
    /// test actually exercise the `bindings.contains_key` branch — the prior
    /// "unknown before a binding exists" test on its own would still pass
    /// even if that branch (the entire performance point of the task) were
    /// deleted.
    #[tokio::test]
    async fn gateway_try_claim_is_yes_once_a_binding_exists() {
        let src = test_source(open_neg(), allow_all());
        assert_eq!(
            src.try_claim("BOUND:PV"),
            TryClaim::Unknown,
            "before: no binding yet"
        );

        src.bindings.lock().unwrap().insert(
            "BOUND:PV".to_string(),
            Binding {
                client_name: "c".into(),
                real_name: "BOUND:PV".into(),
                struct_id: None,
                last_get: Mutex::new(None),
            },
        );

        assert_eq!(
            src.try_claim("BOUND:PV"),
            TryClaim::Yes,
            "after: a binding now exists"
        );
    }

    /// Pins the documented precedence inside `try_claim`: the negative cache
    /// and an access `Deny` must both outrank an existing binding, exactly as
    /// they do in `claim`. Without this, a future reordering could report a
    /// negatively-cached or denied name as `Yes` just because a stale binding
    /// still sits in the map.
    #[tokio::test]
    async fn gateway_try_claim_negative_cache_and_deny_outrank_an_existing_binding() {
        let neg = open_neg();
        let src = test_source(neg.clone(), allow_all());
        src.bindings.lock().unwrap().insert(
            "BOUND:PV".to_string(),
            Binding {
                client_name: "c".into(),
                real_name: "BOUND:PV".into(),
                struct_id: None,
                last_get: Mutex::new(None),
            },
        );
        neg.record_miss("BOUND:PV", Instant::now());
        assert_eq!(
            src.try_claim("BOUND:PV"),
            TryClaim::No,
            "a negative-cache hit must outrank an existing binding"
        );

        let access = Arc::new(AccessControl::new(
            false,
            Some(crate::access::pvlist::parse_pvlist("SECRET:.*  DENY\n.*  ALLOW\n").unwrap()),
            None,
        ));
        let src2 = test_source(open_neg(), access);
        src2.bindings.lock().unwrap().insert(
            "SECRET:PV".to_string(),
            Binding {
                client_name: "c".into(),
                real_name: "SECRET:PV".into(),
                struct_id: None,
                last_get: Mutex::new(None),
            },
        );
        assert_eq!(
            src2.try_claim("SECRET:PV"),
            TryClaim::No,
            "an access Deny must outrank an existing binding"
        );
    }

    #[tokio::test]
    async fn gateway_try_claim_is_no_for_a_negatively_cached_name() {
        let neg = open_neg();
        neg.record_miss("GONE:PV", Instant::now());
        let src = test_source(neg, allow_all());
        assert_eq!(src.try_claim("GONE:PV"), TryClaim::No);
        // A name the cache has never seen is still Unknown, not No.
        assert_eq!(src.try_claim("OTHER:PV"), TryClaim::Unknown);
    }

    #[tokio::test]
    async fn gateway_try_claim_is_no_for_a_denied_name() {
        let access = Arc::new(AccessControl::new(
            false,
            Some(crate::access::pvlist::parse_pvlist("SECRET:.*  DENY\n.*  ALLOW\n").unwrap()),
            None,
        ));
        let src = test_source(open_neg(), access);
        assert_eq!(src.try_claim("SECRET:PV"), TryClaim::No);
        assert_eq!(src.try_claim("PUBLIC:PV"), TryClaim::Unknown);
    }
}
