//! `StatusSource` — serves the gateway's status/introspection PVs (p4p
//! parity spec §8.1) under a configurable `<statusprefix>`, plus an `asTest`
//! diagnostic RPC.
//!
//! Registered in the runtime (`runtime.rs`) at a **lower** `.source()` order
//! than the `GatewaySource` (`-10` vs `0`), so its `<prefix>*` names claim
//! first and cannot be shadowed by an upstream PV that happens to share a
//! name.
//!
//! Never receives a `MonitorRegistry` (extra `.source()`s are m1-only —
//! see `pva_server.rs`), so `pushes_own_updates()` stays at its default
//! `false`: live PVs are exposed only through `subscribe`'s
//! `mpsc::Receiver`, fed by an internal `tokio::time::interval` ticker, and
//! the server's per-PV pump drains it.

pub mod banner;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_server::simple_store::descriptor_for_payload;
use spvirit_types::{NtPayload, NtScalar, PvValue, ScalarValue};
use tokio::sync::mpsc;

use crate::access::{AccessControl, Decision, Identity, Op};
use crate::proxy::GatewaySource;
use crate::upstream::UpstreamPool;

/// How often the `subscribe` ticker refreshes a live PV's value.
const TICK_PERIOD: Duration = Duration::from_secs(1);

/// The live PVs, in the stable order `names()`/the banner report them.
/// `poke` is included here (it is readable — its value is an internal
/// generation counter — as well as the only writable status PV).
const LIVE: &[&str] = &["clients", "cache", "refs", "threads", "stats", "poke"];

/// The static, always-zero bandwidth-counter PVs (spec §8.1) — not wired to
/// any real per-PV/per-host byte counter in M1.
const STATIC_ZERO: &[&str] = &[
    "ds:bypv:rx",
    "ds:bypv:tx",
    "ds:byhost:rx",
    "ds:byhost:tx",
    "us:bypv:rx",
    "us:bypv:tx",
    "us:byhost:rx",
    "us:byhost:tx",
];

/// RPC-only names (no `get`/`subscribe` value, only `rpc`).
const RPC_NAMES: &[&str] = &["asTest"];

/// The full set of suffixes this source serves, in a stable order: live,
/// then static, then RPC. Both `StatusSource::names()` and
/// `banner::status_pv_lines` draw from this single iterator so the served
/// set and the banner cannot drift apart.
fn served_suffixes() -> impl Iterator<Item = &'static str> {
    LIVE.iter().chain(STATIC_ZERO.iter()).chain(RPC_NAMES.iter()).copied()
}

/// A cheap, cloneable "read the current value" callback used by
/// [`StatusHandles`]'s live gauges.
type Gauge = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Live-value handles `StatusSource` reads for its `clients`/`cache`/
/// `refs`/`threads`/`stats` PVs.
///
/// Cheap to clone (each field is an `Arc`), so a clone can be handed to the
/// `subscribe` ticker task without borrowing the source.
#[derive(Clone)]
pub struct StatusHandles {
    pub clients: Gauge,
    pub cache: Gauge,
    pub refs: Gauge,
    pub threads: Gauge,
    pub stats: Gauge,
}

impl StatusHandles {
    /// All gauges stubbed to zero — used by unit tests that don't need a
    /// real `GatewaySource`/`UpstreamPool`.
    pub fn test() -> Self {
        let zero: Gauge = Arc::new(|| 0.0);
        Self {
            clients: zero.clone(),
            cache: zero.clone(),
            refs: zero.clone(),
            threads: zero.clone(),
            stats: zero,
        }
    }

    /// Wires the handles the runtime has a real M1 source for:
    /// - `clients` <- the number of upstream clients configured on this
    ///   server's shared [`UpstreamPool`].
    /// - `cache` <- [`GatewaySource::upstream_monitor_count`], the number of
    ///   distinct upstream monitors currently running.
    ///
    /// `refs`, `threads`, and `stats` have no obvious M1 data source yet
    /// (no per-binding refcount, no thread-pool introspection, no
    /// aggregate request-stats collector exists in the gateway) and are
    /// stubbed to zero — a documented M1 gap, not an oversight.
    pub fn from_gateway(src: &Arc<GatewaySource>, pool: &Arc<UpstreamPool>) -> Self {
        let n_clients = pool.names().len() as f64;
        let src = src.clone();
        let zero: Gauge = Arc::new(|| 0.0);
        Self {
            clients: Arc::new(move || n_clients),
            cache: Arc::new(move || src.upstream_monitor_count() as f64),
            refs: zero.clone(),
            threads: zero.clone(),
            stats: zero,
        }
    }

    fn read(&self, suffix: &str, generation: &AtomicU64) -> Option<f64> {
        match suffix {
            "clients" => Some((self.clients)()),
            "cache" => Some((self.cache)()),
            "refs" => Some((self.refs)()),
            "threads" => Some((self.threads)()),
            "stats" => Some((self.stats)()),
            "poke" => Some(generation.load(Ordering::Relaxed) as f64),
            _ => None,
        }
    }
}

/// A [`Source`] serving the gateway's status/introspection PVs under a
/// configurable prefix, gated through the same [`AccessControl`] the
/// gateway's data-plane `GatewaySource` uses.
pub struct StatusSource {
    prefix: String,
    access: Arc<AccessControl>,
    handles: StatusHandles,
    /// Bumped by `put("<prefix>poke", ..)`; also `poke`'s own readable
    /// value, so a client can observe the counter advance after poking it.
    generation: Arc<AtomicU64>,
}

impl StatusSource {
    pub fn new(prefix: String, access: Arc<AccessControl>, handles: StatusHandles) -> Self {
        Self {
            prefix,
            access,
            handles,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    fn suffix<'a>(&self, name: &'a str) -> Option<&'a str> {
        name.strip_prefix(self.prefix.as_str())
    }

    fn scalar_payload(v: f64) -> NtPayload {
        NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(v)))
    }

    fn decision_str(d: &Decision) -> &'static str {
        match d {
            Decision::Allow | Decision::AllowAliased(_) => "allow",
            Decision::Deny => "deny",
        }
    }

    /// Reads a `DecodedValue::Structure` field by name as a string, per the
    /// `asTest` RPC argument shape `{pv, user, host}`. Missing/non-string
    /// fields (and an empty string, which the wire encoding round-trips a
    /// missing optional-ish value as) map to `None`.
    fn decoded_field_str(v: &DecodedValue, field: &str) -> Option<String> {
        let DecodedValue::Structure(fields) = v else {
            return None;
        };
        fields.iter().find(|(n, _)| n == field).and_then(|(_, val)| match val {
            DecodedValue::String(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })
    }

    /// Builds the `asTest` NT response: the three per-op verdicts as
    /// allow/deny strings, plus a short summary string.
    fn astest_response(pv: &str, get_d: &Decision, put_d: &Decision, rpc_d: &Decision) -> NtPayload {
        let get_s = Self::decision_str(get_d);
        let put_s = Self::decision_str(put_d);
        let rpc_s = Self::decision_str(rpc_d);
        let summary = format!("asTest {pv:?}: get={get_s}, put={put_s}, rpc={rpc_s}");
        NtPayload::Generic {
            struct_id: "spvirit:gateway/AsTestResult:1.0".to_string(),
            fields: vec![
                ("get".to_string(), PvValue::Scalar(ScalarValue::Str(get_s.to_string()))),
                ("put".to_string(), PvValue::Scalar(ScalarValue::Str(put_s.to_string()))),
                ("rpc".to_string(), PvValue::Scalar(ScalarValue::Str(rpc_s.to_string()))),
                ("summary".to_string(), PvValue::Scalar(ScalarValue::Str(summary))),
            ],
        }
    }
}

impl Source for StatusSource {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let suffix = self.suffix(&name)?;
            if !served_suffixes().any(|s| s == suffix) {
                return None;
            }
            let payload = if RPC_NAMES.contains(&suffix) {
                Self::astest_response("", &Decision::Deny, &Decision::Deny, &Decision::Deny)
            } else {
                Self::scalar_payload(self.handles.read(suffix, &self.generation).unwrap_or(0.0))
            };
            Some(PvInfo {
                descriptor: descriptor_for_payload(&payload),
                writable: suffix == "poke",
            })
        })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let suffix = self.suffix(&name)?;
            if RPC_NAMES.contains(&suffix) {
                return None;
            }
            let v = if LIVE.contains(&suffix) {
                self.handles.read(suffix, &self.generation)?
            } else if STATIC_ZERO.contains(&suffix) {
                0.0
            } else {
                return None;
            };
            Some(Self::scalar_payload(v))
        })
    }

    fn put(
        &self,
        name: &str,
        _value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let suffix = self
                .suffix(&name)
                .ok_or_else(|| format!("unclaimed status PV {name:?}"))?;
            if suffix != "poke" {
                return Err(format!("status PV {name:?} is read-only"));
            }
            self.generation.fetch_add(1, Ordering::Relaxed);
            Ok(vec![])
        })
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let suffix = self.suffix(&name)?.to_string();
            if RPC_NAMES.contains(&suffix.as_str()) {
                return None;
            }
            let (tx, rx) = mpsc::channel(4);
            if LIVE.contains(&suffix.as_str()) {
                let handles = self.handles.clone();
                let generation = self.generation.clone();
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(TICK_PERIOD);
                    loop {
                        interval.tick().await;
                        let Some(v) = handles.read(&suffix, &generation) else {
                            break;
                        };
                        if tx.send(Self::scalar_payload(v)).await.is_err() {
                            // Receiver dropped: stop ticking, don't leak the task.
                            break;
                        }
                    }
                });
            } else if STATIC_ZERO.contains(&suffix.as_str()) {
                tokio::spawn(async move {
                    let _ = tx.send(Self::scalar_payload(0.0)).await;
                    // Emit once, then let `tx` drop — the receiver observes
                    // a closed channel (idle) rather than further updates.
                });
            } else {
                return None;
            }
            Some(rx)
        })
    }

    fn rpc(
        &self,
        name: &str,
        args: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<NtPayload, String>> + Send + '_>> {
        let name = name.to_string();
        let args = args.clone();
        Box::pin(async move {
            let suffix = self
                .suffix(&name)
                .ok_or_else(|| format!("unclaimed status PV {name:?}"))?;
            if suffix != "asTest" {
                return Err("RPC not supported".to_string());
            }
            let pv = Self::decoded_field_str(&args, "pv").unwrap_or_default();
            let id = Identity {
                host: Self::decoded_field_str(&args, "host"),
                user: Self::decoded_field_str(&args, "user"),
            };
            let get_d = self.access.decide(Op::Get, &pv, &id);
            let put_d = self.access.decide(Op::Put, &pv, &id);
            let rpc_d = self.access.decide(Op::Rpc, &pv, &id);
            Ok(Self::astest_response(&pv, &get_d, &put_d, &rpc_d))
        })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        let prefix = self.prefix.clone();
        Box::pin(async move { served_suffixes().map(|s| format!("{prefix}{s}")).collect() })
    }
}
