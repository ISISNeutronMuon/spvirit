//! `GatewaySource` — the [`spvirit_server::pvstore::Source`] implementation
//! that bridges downstream requests to upstream [`spvirit_client::PvaClient`]s.
//!
//! M1 only wires up `claim`: search each configured upstream client in order
//! and cache the first hit. `get`/`put`/`subscribe`/`names` are placeholder
//! stubs replaced in later tasks (10-13).

use std::collections::HashMap;
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_types::NtPayload;
use tokio::sync::mpsc;

use crate::bridge::{merge_monitor_delta, nt_payload_from_decoded, nt_payload_from_get};
use crate::cache::monitor::{MonitorCache, MonitorKey};
use crate::cache::negative::NegativeCache;
use crate::loopguard::LoopGuard;
use crate::upstream::UpstreamPool;

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
    #[allow(dead_code)] // consulted by the server-side search path, wired in Task 14
    guard: Arc<LoopGuard>,
    getholdoff_ms: u32,
    bindings: Mutex<HashMap<String, Binding>>,
    monitors: Arc<MonitorCache>,
}

impl GatewaySource {
    /// Build a source that searches `client_order` (in order) for each
    /// downstream name, backed by `pool`, remembering upstream misses in
    /// `neg` and (later) refusing to resolve back into our own servers via
    /// `guard`.
    pub fn new(
        pool: Arc<UpstreamPool>,
        client_order: Vec<String>,
        neg: Arc<NegativeCache>,
        guard: Arc<LoopGuard>,
        getholdoff_ms: u32,
    ) -> Self {
        GatewaySource {
            pool,
            client_order,
            neg,
            guard,
            getholdoff_ms,
            bindings: Mutex::new(HashMap::new()),
            monitors: Arc::new(MonitorCache::new()),
        }
    }

    /// Number of distinct upstream monitors currently running. Exposed for
    /// tests proving that multiple `subscribe` calls for the same PV dedup
    /// onto a single upstream monitor task.
    pub fn upstream_monitor_count(&self) -> usize {
        self.monitors.upstream_count()
    }
}

impl Source for GatewaySource {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let now = Instant::now();
            if self.neg.is_missing(&name, now) {
                return None;
            }

            for client_name in &self.client_order {
                let Some(client) = self.pool.client(client_name) else {
                    continue;
                };
                if let Ok(descriptor) = client.pvinfo(&name).await {
                    self.bindings.lock().unwrap().insert(
                        name.clone(),
                        Binding {
                            client_name: client_name.clone(),
                            real_name: name.clone(),
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
        _name: &str,
        _value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        Box::pin(async { Err("not implemented in Task 9".to_string()) })
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
            let key: MonitorKey = (client_name, real_name.clone());
            let monitors = self.monitors.clone();

            let rx = self.monitors.subscribe(key.clone(), move |entry| {
                tokio::spawn(async move {
                    let mut last_full = DecodedValue::Null;
                    let callback_entry = entry.clone();
                    let callback_monitors = monitors.clone();
                    let callback_key = key.clone();
                    let _ = client
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
                                ControlFlow::Break(())
                            }
                        })
                        .await;
                    // `dispatch_or_retire` already removed this key's entry
                    // from the map (atomically, under the map lock) the
                    // moment it decided the upstream loop should end, so
                    // there is nothing left to clean up here.
                });
            });

            Some(rx)
        })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async { vec![] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;
    use std::time::Duration;

    #[tokio::test]
    async fn claim_returns_none_when_no_clients_configured() {
        let cfg = GatewayConfig::from_json_str(r#"{"version":2,"clients":[],"servers":[]}"#)
            .unwrap();
        let pool = Arc::new(UpstreamPool::from_config(&cfg));
        let neg = Arc::new(NegativeCache::new(Duration::from_secs(30), 128));
        let guard = Arc::new(LoopGuard::build(
            &cfg,
            &crate::config::ServerCfg {
                name: "s".into(),
                clients: vec![],
                interface: vec![],
                addrlist: String::new(),
                ignoreaddr: String::new(),
                autoaddrlist: true,
                serverport: 5075,
                bcastport: 5076,
                getholdoff: 0,
                statusprefix: String::new(),
                access: String::new(),
                pvlist: String::new(),
                acf_client: None,
                x_spvirit: None,
            },
        ));
        let src = GatewaySource::new(pool, vec![], neg, guard, 0);
        assert!(src.claim("ANY:PV").await.is_none());
    }
}
