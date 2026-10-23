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

use spvirit_codec::spvd_decode::{DecodedValue, StructureDesc};
use spvirit_server::pvstore::{PvInfo, Source, TryClaim};
use spvirit_types::NtPayload;
use tokio::sync::mpsc;

use crate::access::{AccessControl, Decision, Identity, Op};
use crate::bridge::{
    merge_monitor_delta, nt_payload_from_decoded, nt_payload_from_get, patch_nt_payload,
};
use crate::cache::monitor::{MonitorCache, MonitorKey};
use crate::convert::decoded_to_json;
use crate::cache::negative::NegativeCache;
use crate::lockutil::{lock_or_recover, try_lock_or_recover};
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
    /// The upstream introspection captured at resolve time.
    ///
    /// Held in full (not just `struct_id`) so `claim` can answer a repeat call
    /// straight from this map instead of going back to the wire, and so
    /// `subscribe` can keep stamping monitored payloads with the same struct
    /// id `get` reports (`nt_payload_from_get` sources it from
    /// `PvGetResult::introspection`; `subscribe` has no per-tick introspection
    /// round trip, so it must carry this forward from claim).
    pub descriptor: StructureDesc,
    /// When this binding was last confirmed against upstream. Bounds how long
    /// the fast path in `claim` may answer from memory.
    pub bound_at: Instant,
    last_get: Mutex<Option<(Instant, NtPayload)>>,
}

/// Forget the binding for downstream `name`.
///
/// A free function over the map rather than a `GatewaySource` method because
/// `subscribe`'s spawned upstream task is `'static` and holds only the
/// `Arc<Mutex<..>>`, not `&self`. Both that task and
/// [`GatewaySource::retire_binding`] go through here, so the teardown rule
/// documented on `retire_binding` has exactly one implementation.
fn retire_binding_in(bindings: &Mutex<HashMap<String, Binding>>, name: &str) {
    lock_or_recover(bindings).remove(name);
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
    /// How long a binding may answer `claim` without going back upstream.
    ///
    /// A staleness bound, not an eviction policy: an expired binding is
    /// re-resolved and refreshed in place, never dropped. 30 s is the order of
    /// magnitude of a PVA client's own search backoff, so a genuinely
    /// restructured IOC is picked up in roughly the time a direct client would
    /// take to notice.
    binding_ttl: Duration,
    /// How many times this source has gone to the wire to resolve a name.
    /// Exposed for tests proving the binding fast path actually short-circuits.
    resolves: std::sync::atomic::AtomicU64,
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
            binding_ttl: Duration::from_secs(30),
            resolves: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Number of distinct upstream monitors currently running. Exposed for
    /// tests proving that multiple `subscribe` calls for the same PV dedup
    /// onto a single upstream monitor task.
    pub fn upstream_monitor_count(&self) -> usize {
        self.monitors.upstream_count()
    }

    /// Override the binding staleness bound. Test-serving builder; production
    /// takes the 30 s default from [`GatewaySource::new`].
    pub fn with_binding_ttl(mut self, ttl: Duration) -> Self {
        self.binding_ttl = ttl;
        self
    }

    /// How many upstream resolutions (`pvinfo_full` attempts) this source has
    /// issued. Exposed for tests proving repeat claims are answered from the
    /// binding map rather than from the wire.
    pub fn upstream_resolve_count(&self) -> u64 {
        self.resolves.load(std::sync::atomic::Ordering::Relaxed)
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
        retire_binding_in(&self.bindings, name);
    }

    /// Number of upstream monitors torn down because their upstream ended.
    /// Mirrored onto `/metrics` as `spgateway_upstream_monitor_deaths_total`.
    pub fn upstream_monitor_deaths(&self) -> u64 {
        self.deaths.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many bindings currently hold a cached `get` value. Exposed for
    /// tests proving the getholdoff cache is filled when the feature is on and
    /// left completely alone when `getholdoff = 0`.
    pub fn getholdoff_cache_len(&self) -> usize {
        lock_or_recover(&self.bindings)
            .values()
            .filter(|b| lock_or_recover(&b.last_get).is_some())
            .count()
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
        let Some(bindings) = try_lock_or_recover(&self.bindings) else {
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

            // Fast path. `SourceRegistry` calls `claim` before EVERY
            // get/put/subscribe/rpc/pushes_own_updates, so without this each
            // downstream operation pays a fresh UDP search + TCP connect +
            // CreateChannel + GET_FIELD whose only product — the
            // `StructureDesc` — this map already holds.
            //
            // Three guards keep the shortcut honest, and their order matters:
            //  * the negative cache and the access decision above already ran,
            //    so a denied or negatively-cached name is never answered from a
            //    stale binding (the same precedence `try_claim` uses);
            //  * `real_name == target`, because the binding is keyed on the
            //    *downstream* name while a pvlist ALIAS can map that name to a
            //    different upstream target for a different identity —
            //    answering with another identity's descriptor would be wrong;
            //  * `bound_at` within `binding_ttl`, so an upstream that
            //    restructured while nothing was monitoring it is re-resolved
            //    within a bounded window rather than never.
            //
            // The loop guard is deliberately NOT re-checked here, and the
            // honest reason is narrower than "the binding is already
            // vetted". A binding exists only because some earlier resolve
            // passed `is_banned`/`is_guid_banned` below — but `Binding` pins
            // no address and no GUID, so that guarantee covers the address
            // resolved *then*, not the address the next `pvget`/`pvput` will
            // find: the data path re-searches every operation
            // (`PvaClient::pvget` runs a fresh `low_level_pvget`). So a name
            // that starts resolving to a banned address while a binding is
            // live is answered unguarded until the binding expires.
            //
            // That TOCTOU is pre-existing — even before the fast path, claim
            // validated the address *its own* search found, not the one the
            // data path would re-resolve a moment later — but the fast path
            // widens the window from roughly one search to `binding_ttl`.
            // Accepted because `LoopGuard` is built once per server at
            // startup (`runtime.rs`) and never replaced, so the ban set
            // cannot change under a live binding; the only mover is upstream
            // itself migrating onto a banned address, which the TTL bounds.
            // Revisit this skip if either premise goes: a `LoopGuard` that
            // can be reloaded, a `Binding` that pins its resolved address or
            // GUID (then re-check it cheaply here), or client-side
            // connection caching that makes the data path stop re-searching.
            {
                let bindings = lock_or_recover(&self.bindings);
                if let Some(binding) = bindings.get(&name)
                    && binding.real_name == target
                    && now.duration_since(binding.bound_at) < self.binding_ttl
                {
                    return Some(PvInfo {
                        descriptor: binding.descriptor.clone(),
                        writable: true,
                    });
                }
            }

            for client_name in &self.client_order {
                let Some(client) = self.pool.client(client_name) else {
                    continue;
                };
                self.resolves
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let (descriptor, server_addr, guid) = match client.pvinfo_full(&target).await {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        // Downstream a failed resolve is indistinguishable from
                        // "no such PV", so this line is the only place the
                        // difference is recorded. `debug` rather than `warn`:
                        // on a multi-client gateway most clients legitimately
                        // do not host most names, so a miss here is routine.
                        tracing::debug!(
                            "gateway: upstream resolve failed for downstream={name:?} \
                             real={target:?} via client={client_name:?}: {e}"
                        );
                        continue;
                    }
                };
                // Loop / self-connection prevention: if this name resolved
                // back into one of our own downstream server sockets (or an
                // `ignoreaddr` host), or the responder's GUID matches one of
                // our own servers' GUIDs, do NOT bind it — forwarding would
                // loop a search into ourselves. Skip to the next client.
                if self.guard.is_banned(server_addr) || self.guard.is_guid_banned(&guid) {
                    continue;
                }
                {
                    let mut bindings = lock_or_recover(&self.bindings);
                    // Two statements, not one `match` with an `insert` in
                    // its fallback arm: `get_mut`'s borrow of `bindings`
                    // would still be live in that arm, and the borrow
                    // checker rejects it (NLL problem case 3). Reducing
                    // the match to a `bool` ends the borrow at the
                    // semicolon.
                    let refreshed = match bindings.get_mut(&name) {
                        // Refresh in place. Replacing the whole `Binding`
                        // (what this used to do) resets `last_get` to
                        // `None` — and `SourceRegistry` claims immediately
                        // before every `get`, so the getholdoff cache was
                        // wiped before every read it was meant to serve.
                        Some(existing)
                            if existing.client_name == *client_name
                                && existing.real_name == target =>
                        {
                            // An upstream that restructured invalidates the
                            // cached value: `handler.rs` encodes whatever
                            // `last_get` holds against the descriptor this
                            // binding currently advertises, so serving a
                            // payload cached against the superseded one
                            // emits a frame whose data does not match its
                            // own introspection — and in this protocol one
                            // malformed frame takes the whole TCP
                            // connection down, not just that value. Do NOT
                            // simplify this to an unconditional refresh:
                            // the equality check is what keeps a stale
                            // payload from outliving its structure. It is
                            // equally not an unconditional clear — that
                            // would wipe the cache on every claim and so
                            // disable getholdoff altogether, which is the
                            // very bug the in-place refresh above fixes.
                            if existing.descriptor != descriptor {
                                *lock_or_recover(&existing.last_get) = None;
                            }
                            existing.descriptor = descriptor.clone();
                            existing.bound_at = Instant::now();
                            true
                        }
                        // A different client or a different upstream target
                        // is a different binding: the cached value came
                        // from somewhere else and must not carry over.
                        _ => false,
                    };
                    if !refreshed {
                        bindings.insert(
                            name.clone(),
                            Binding {
                                client_name: client_name.clone(),
                                real_name: target.clone(),
                                descriptor: descriptor.clone(),
                                bound_at: Instant::now(),
                                last_get: Mutex::new(None),
                            },
                        );
                    }
                }
                return Some(PvInfo {
                    descriptor,
                    writable: true,
                });
            }

            // Stamped now, not with the `now` taken at entry: the loop above
            // just spent a full `PvaClient` operation timeout per configured
            // client, and charging that against this entry would make any TTL
            // shorter than the search expire the moment it is written.
            self.neg.record_miss(&name, Instant::now());
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
                let bindings = lock_or_recover(&self.bindings);
                let binding = bindings.get(&name)?;

                // `getholdoff = 0` (the default) means the feature is off: the
                // window can never be satisfied, so do not even take the inner
                // lock to compare against a zero-length window.
                if self.getholdoff_ms > 0 {
                    let cache = lock_or_recover(&binding.last_get);
                    if let Some((t_last, payload)) = cache.as_ref()
                        && Instant::now()
                            < *t_last + Duration::from_millis(self.getholdoff_ms as u64)
                    {
                        return Some(payload.clone());
                    }
                }

                (binding.client_name.clone(), binding.real_name.clone())
            };

            let client = self.pool.client(&client_name)?;
            match client.pvget(&real_name).await {
                Ok(result) => {
                    let payload = nt_payload_from_get(&result);
                    // Only fill the cache when the feature is on: with the
                    // default 0 the read above can never hit, so the clone and
                    // the two locks would be pure cost on every read.
                    if self.getholdoff_ms > 0
                        && let Some(binding) = lock_or_recover(&self.bindings).get(&name)
                    {
                        *lock_or_recover(&binding.last_get) = Some((Instant::now(), payload.clone()));
                    }
                    Some(payload)
                }
                Err(e) => {
                    // `warn`, not `debug`: unlike `claim`, this PV is already
                    // bound — the upstream answered a resolve for it moments
                    // ago — so a failing read is a real fault, not a routine
                    // miss.
                    tracing::warn!(
                        "gateway: upstream get failed for downstream={name:?} \
                             real={real_name:?} via client={client_name:?}: {e}"
                    );
                    None
                }
            }
        })
    }

    fn put(
        &self,
        name: &str,
        value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        let name = name.to_string();
        // Converted eagerly (before `Box::pin`) only because `value` is a
        // borrow the returned future must not capture; the RESULT is not
        // inspected until after the access decision below, so a denied put
        // never surfaces a conversion error.
        let json = decoded_to_json(value);
        Box::pin(async move {
            if let Decision::Deny = self.access.decide(Op::Put, &name, &current_identity()) {
                return Err("access denied".to_string());
            }

            let json = json?;

            // Resolve the binding and clone out what's needed before the
            // `.await` below — never hold the bindings-map MutexGuard across
            // an await point, matching `get`'s discipline.
            let (client_name, real_name) = {
                let bindings = lock_or_recover(&self.bindings);
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
                let bindings = lock_or_recover(&self.bindings);
                let binding = bindings.get(&name)?;
                (
                    binding.client_name.clone(),
                    binding.real_name.clone(),
                    binding.descriptor.struct_id.clone().unwrap_or_default(),
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
                    // The payload handed to subscribers, kept across ticks so a delta can
                    // be patched into it instead of rebuilding the whole structure.
                    let mut cached_payload: Option<NtPayload> = None;
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

                            // Patch in place when the delta names its changed
                            // fields; only rebuild (re-converting every field of
                            // the structure) when it cannot be applied
                            // incrementally — a whole-structure delta, the first
                            // tick, or a path the payload does not yet have.
                            let needs_rebuild = match cached_payload.as_mut() {
                                Some(cached) => !patch_nt_payload(cached, &last_full, update),
                                None => true,
                            };
                            if needs_rebuild {
                                cached_payload =
                                    Some(nt_payload_from_decoded(&last_full, struct_id.clone()));
                            }
                            let payload =
                                cached_payload.as_ref().expect("filled immediately above");

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
                    // Without this, `try_claim` keeps answering `Yes` from the
                    // bindings map and a destroyed client hot-loops straight
                    // back into the dead upstream (design spec §4).
                    retire_binding_in(&bindings, &down_name);
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
                    // LAST, deliberately: dropping the `Sender`s is the
                    // observable "upstream is gone" edge — it is what wakes
                    // every downstream receiver with `rx.recv() == None` and,
                    // once Layer 2 lands, what makes the server-side pump send
                    // DESTROY_CHANNEL. Everything a woken client might
                    // immediately observe (the retired cache entry, the retired
                    // binding, the death count) must already be committed
                    // before we signal, or a client re-searching the instant it
                    // is woken can still be answered `Yes` from a binding we
                    // were about to remove — the §4 hot loop.
                    //
                    // Honest note on why this call exists at all: today it is
                    // *equivalent* to simply returning from this task, because
                    // the only surviving `Arc<MonitorEntry>` clones are this
                    // task's `entry` and the one the `pvmonitor` callback held
                    // (already dropped with the callback), so the senders would
                    // die microseconds later anyway. The call earns its place
                    // by making the edge explicit and correctly ordered, and it
                    // becomes strictly load-bearing the moment anything holds
                    // an `Arc<MonitorEntry>` clone past this task's return.
                    // Do not delete it on the grounds that a test still passes
                    // without it.
                    entry.close_all();
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
            let bindings = lock_or_recover(&self.bindings);
            bindings.keys().cloned().collect()
        };
        names.sort();
        Box::pin(async move { names })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;
    use std::time::Duration;

    fn test_binding(client: &str, real: &str) -> Binding {
        Binding {
            client_name: client.to_string(),
            real_name: real.to_string(),
            descriptor: StructureDesc::default(),
            bound_at: Instant::now(),
            last_get: Mutex::new(None),
        }
    }

    /// The teardown rule documented on `retire_binding` (design spec §4) must
    /// have exactly one implementation, shared by the `&self` entry point and
    /// by `subscribe`'s `'static` spawned task. This exercises that shared
    /// helper directly, including the "already gone" case the spawned task can
    /// race into.
    #[test]
    fn retire_binding_in_removes_only_the_named_binding() {
        let bindings: Mutex<HashMap<String, Binding>> = Mutex::new(HashMap::new());
        bindings
            .lock()
            .unwrap()
            .insert("A".to_string(), test_binding("c", "A:REAL"));
        bindings
            .lock()
            .unwrap()
            .insert("B".to_string(), test_binding("c", "B:REAL"));

        retire_binding_in(&bindings, "A");
        assert!(!bindings.lock().unwrap().contains_key("A"));
        assert!(bindings.lock().unwrap().contains_key("B"));

        // Retiring a name that is already gone is a no-op, not a panic: the
        // upstream-death path can race a concurrent retirement.
        retire_binding_in(&bindings, "A");
        assert_eq!(bindings.lock().unwrap().len(), 1);
    }

    /// One panic while holding the bindings map must not take the whole
    /// gateway offline. Every critical section over this map is a short,
    /// panic-free `HashMap` operation, so the poison carries no broken
    /// invariant — recovering is strictly better than refusing every later
    /// search, get, subscribe and status read for the process's lifetime.
    #[tokio::test]
    async fn a_poisoned_bindings_map_does_not_take_the_gateway_down() {
        let src = test_source(open_neg(), allow_all());
        src.bindings
            .lock()
            .unwrap()
            .insert("BOUND:PV".to_string(), test_binding("c", "BOUND:REAL"));

        let bindings = src.bindings.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let _ = std::thread::spawn(move || {
            let _guard = bindings.lock().unwrap();
            panic!("poison the bindings map");
        })
        .join();
        std::panic::set_hook(previous);
        assert!(src.bindings.is_poisoned());

        assert_eq!(
            src.try_claim("BOUND:PV"),
            TryClaim::Yes,
            "a poisoned bindings map must not silence the search path"
        );
        assert_eq!(src.names().await, vec!["BOUND:PV".to_string()]);
        src.retire_binding("BOUND:PV");
        assert!(src.names().await.is_empty());
    }

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

    /// A source with one upstream client pointed at a UDP port nothing is
    /// listening on, so every `pvinfo_full`/`pvget` through it runs the full
    /// `PvaClient` operation timeout (5 s by default — `UpstreamPool` never
    /// sets one) and then fails. The port is bound and released just to obtain
    /// a number the OS is not currently handing out; nothing ever answers on
    /// it. `None` means this environment cannot bind a UDP port at all.
    fn dead_upstream_source(neg: Arc<NegativeCache>) -> Option<GatewaySource> {
        let port = std::net::UdpSocket::bind("127.0.0.1:0")
            .ok()?
            .local_addr()
            .ok()?
            .port();
        let cfg_json = format!(
            r#"{{
                "version": 2,
                "clients": [{{
                    "name": "dead-client",
                    "addrlist": "127.0.0.1",
                    "autoaddrlist": false,
                    "bcastport": {port},
                    "interface": ["127.0.0.1"]
                }}],
                "servers": [{{
                    "name": "s",
                    "clients": ["dead-client"]
                }}]
            }}"#
        );
        let cfg = GatewayConfig::from_json_str(&cfg_json).ok()?;
        let pool = Arc::new(UpstreamPool::from_config(&cfg));
        let guard = Arc::new(LoopGuard::build(
            &cfg,
            &cfg.servers[0],
            std::collections::HashSet::new(),
        ));
        Some(GatewaySource::new(
            pool,
            vec!["dead-client".to_string()],
            neg,
            guard,
            0,
            allow_all(),
        ))
    }

    /// The negative-cache TTL must be measured from when the upstream search
    /// FINISHED, not from when `claim` was entered: the search burns a full
    /// client timeout per configured client, and charging that against the
    /// miss entry makes a short TTL expire the instant it is written.
    #[tokio::test]
    async fn a_slow_miss_still_gets_its_full_negative_ttl() {
        let neg = Arc::new(NegativeCache::new(Duration::from_secs(2), 128));
        let Some(src) = dead_upstream_source(neg.clone()) else {
            eprintln!("Skipping test: cannot bind a UDP port in this environment");
            return;
        };

        let before = Instant::now();
        assert!(src.claim("GONE:PV").await.is_none());
        let search_took = before.elapsed();

        if search_took < Duration::from_secs(3) {
            eprintln!(
                "Skipping assertion: the upstream search failed in {search_took:?}, faster \
                 than the 2 s negative TTL, so this environment cannot tell the two apart"
            );
            return;
        }

        assert!(
            neg.is_missing("GONE:PV", Instant::now()),
            "the miss was recorded with the pre-search instant, so its 2 s TTL had already \
             expired by the time claim returned after {search_took:?}"
        );
    }

    /// The access decision must run before the value conversion. A read-only
    /// gateway asked to put an unrepresentable value must answer "access
    /// denied" — not a conversion error that reveals the request got past the
    /// gate. (The conversion itself still runs eagerly; only its outcome is
    /// held back until after the decision.)
    #[tokio::test]
    async fn put_decides_access_before_converting_the_value() {
        let read_only = Arc::new(AccessControl::new(true, None, None));
        let src = test_source(open_neg(), read_only);

        let err = src
            .put("ANY:PV", &DecodedValue::Raw(vec![0xde, 0xad]))
            .await
            .expect_err("a read-only gateway must reject every put");

        assert_eq!(
            err, "access denied",
            "expected the denial, got the JSON conversion's error instead"
        );
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
                descriptor: StructureDesc::default(),
                bound_at: Instant::now(),
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
                descriptor: StructureDesc::default(),
                bound_at: Instant::now(),
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
                descriptor: StructureDesc::default(),
                bound_at: Instant::now(),
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

    /// An `io::Write` sink over a shared buffer, for capturing `tracing`
    /// output during a test. Only ever touched synchronously by the
    /// subscriber's own formatting call — never held across an `.await`.
    #[derive(Clone, Default)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Both upstream failure paths must leave a record. Downstream they are
    /// indistinguishable from "no such PV", so without a log an upstream that
    /// is up but erroring cannot be told apart from one that is absent.
    ///
    /// The capture is a process-wide default subscriber installed once, not a
    /// thread-scoped `set_default`. With scoped dispatch, `tracing` evaluates
    /// each callsite's interest across every live dispatcher and caches it
    /// globally, and other tests in this binary running in parallel made the
    /// claim-path `debug!` intermittently invisible to the scoped capture
    /// (about half of full `--lib` runs, even with `rebuild_interest_cache`).
    /// A single global subscriber has no such race. Every other test in the
    /// binary then also logs into this buffer, which nobody else reads. The
    /// PV names here (`LOGTEST:*`) are used by no other test, and each
    /// assertion matches a `key="value"` fragment of the log line, so
    /// another test's output cannot satisfy it.
    #[tokio::test]
    async fn a_failing_upstream_is_logged_by_both_claim_and_get() {
        static CAPTURE: std::sync::OnceLock<CaptureWriter> = std::sync::OnceLock::new();
        let buffer = CAPTURE
            .get_or_init(|| {
                let buffer = CaptureWriter::default();
                let writer = buffer.clone();
                let subscriber = tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::DEBUG)
                    .with_ansi(false)
                    .without_time()
                    .with_writer(move || writer.clone())
                    .finish();
                tracing::subscriber::set_global_default(subscriber)
                    .expect("no other test in this binary installs a global subscriber");
                buffer
            })
            .clone();

        let Some(src) = dead_upstream_source(open_neg()) else {
            eprintln!("Skipping test: cannot bind a UDP port in this environment");
            return;
        };

        // claim: every configured client fails to resolve.
        assert!(src.claim("LOGTEST:GONE").await.is_none());

        // get: insert a binding by hand — the upstream is dead, so no claim
        // can create one — then `get` reaches `pvget` and fails there.
        src.bindings.lock().unwrap().insert(
            "LOGTEST:BOUND".to_string(),
            Binding {
                client_name: "dead-client".to_string(),
                real_name: "LOGTEST:REAL".to_string(),
                descriptor: StructureDesc::default(),
                bound_at: Instant::now(),
                last_get: Mutex::new(None),
            },
        );
        assert!(src.get("LOGTEST:BOUND").await.is_none());

        let captured = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert!(
            captured.contains(
                "downstream=\"LOGTEST:GONE\" real=\"LOGTEST:GONE\" via client=\"dead-client\""
            ),
            "claim's upstream failure must name the PV and the client: {captured:?}"
        );
        assert!(
            captured.contains(
                "downstream=\"LOGTEST:BOUND\" real=\"LOGTEST:REAL\" via client=\"dead-client\""
            ),
            "get's upstream failure must name the downstream and upstream names: {captured:?}"
        );
    }
}
