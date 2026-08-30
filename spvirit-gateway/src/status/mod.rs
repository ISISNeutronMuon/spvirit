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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_server::simple_store::descriptor_for_payload;
use spvirit_types::{
    NtPayload, NtScalar, NtScalarArray, NtTable, NtTableColumn, NtTimeStamp, PvValue,
    ScalarArrayValue, ScalarValue,
};
use tokio::sync::mpsc;

use crate::access::{AccessControl, Decision, Identity, Op};
use crate::proxy::GatewaySource;

/// How often the `subscribe` ticker refreshes a live PV's value.
const TICK_PERIOD: Duration = Duration::from_secs(1);

/// Snapshots the current downstream connection's identity (socket peer host,
/// and decoded `ca` user if any) into an [`Identity`] for
/// [`AccessControl::decide`].
///
/// This deliberately mirrors `proxy::current_identity` (which is private to
/// that module and cannot be reused) rather than reaching across a crate
/// boundary: the peer IP is authoritative for `host` (the self-asserted `ca`
/// host is advisory only and never used for a decision), while `user` is the
/// self-asserted `ca` value, matching p4p's posture. Returns a default
/// (all-`None`) [`Identity`] when called outside a
/// [`spvirit_server::request_ctx`] scope (e.g. a unit test that calls
/// `put`/`get` directly): a permissive `AccessControl` still behaves
/// correctly, and a restrictive one fails closed. In particular, for a pure
/// `readOnly` config `decide` short-circuits before any host/user match, so
/// enforcement holds even with a default `Identity`.
/// The current wall-clock time as an [`NtTimeStamp`] of UNIX seconds (NOT the
/// EPICS 1990-01-01 epoch — see the "1990 timestamp" gateway bug this helper
/// closes). Falls back to the zero duration (seconds/nanoseconds `0`) if the
/// system clock is set before the UNIX epoch, which never happens in
/// practice but keeps this infallible.
///
/// Not yet called from `value_payload`/`subscribe` — wiring status PVs to
/// stamp with a real time is a follow-up task; this and [`stamp`] land first
/// so that task is a pure call-site change.
#[allow(dead_code)]
fn now_ts() -> NtTimeStamp {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    NtTimeStamp {
        seconds_past_epoch: d.as_secs() as i64,
        nanoseconds: d.subsec_nanos() as i32,
        user_tag: 0,
    }
}

/// Attaches `ts` to a status payload's timestamp field(s), per variant:
/// - [`NtPayload::Scalar`]: sets the `Option<NtTimeStamp>` field.
/// - [`NtPayload::ScalarArray`]: sets the (non-optional) `NtTimeStamp` field.
/// - [`NtPayload::Table`]: sets the `Option<NtTimeStamp>` field.
/// - [`NtPayload::Generic`] (the `stats` structure): stamps every nested
///   `epics:nt/NTScalar:1.0` sub-structure's `timeStamp` sub-fields
///   (`secondsPastEpoch`/`nanoseconds`) in place.
/// - [`NtPayload::NdArray`]/[`NtPayload::Enum`]: not used by any status PV
///   today, so left untouched.
#[allow(dead_code)]
fn stamp(payload: &mut NtPayload, ts: NtTimeStamp) {
    match payload {
        NtPayload::Scalar(nt) => nt.time_stamp = Some(ts),
        NtPayload::ScalarArray(nt) => nt.time_stamp = ts,
        NtPayload::Table(nt) => nt.time_stamp = Some(ts),
        NtPayload::Generic { fields, .. } => {
            for (_, v) in fields.iter_mut() {
                let PvValue::Structure { fields: inner, .. } = v else {
                    continue;
                };
                for (n, tv) in inner.iter_mut() {
                    if n != "timeStamp" {
                        continue;
                    }
                    let PvValue::Structure { fields: tf, .. } = tv else {
                        continue;
                    };
                    for (fname, fval) in tf.iter_mut() {
                        match fname.as_str() {
                            "secondsPastEpoch" => {
                                *fval = PvValue::Scalar(ScalarValue::I64(ts.seconds_past_epoch))
                            }
                            "nanoseconds" => {
                                *fval = PvValue::Scalar(ScalarValue::I32(ts.nanoseconds))
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        NtPayload::NdArray(_) | NtPayload::Enum(_) => {}
    }
}

fn current_identity() -> Identity {
    let rc = spvirit_server::request_ctx::current_request();
    Identity {
        host: rc.as_ref().map(|c| c.peer.ip().to_string()),
        user: rc.and_then(|c| c.user),
    }
}

/// The live (get/subscribe) PVs, in the stable order `names()`/the banner
/// report them. `poke` is included here (it is readable — its value is an
/// internal generation counter — as well as the only writable status PV).
/// `threads` is NOT in this ticker set: it is a *static* string value (served
/// like the static bandwidth PVs — emit once, never changes) that *also*
/// responds to RPC (see `RPC_NAMES`), so it is not driven by the live ticker.
const LIVE: &[&str] = &["clients", "cache", "refs", "stats", "poke"];

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

/// Names that respond to `rpc`. `asTest` is pure RPC (no readable value) and
/// dry-runs an ACL decision. `threads` ALSO has a static get/subscribe value
/// (`"RPC only"`); listing it here additionally makes it an RPC target that
/// returns the richer thread-dump string — matching p4p, where `threads` is a
/// `SharedPV(NTScalar('s'), initial='RPC only')` that also serves RPC.
const RPC_NAMES: &[&str] = &["asTest", "threads"];

/// The full set of suffixes this source serves, in a stable order: live,
/// then static, then RPC. Both `StatusSource::names()` and
/// `banner::status_pv_lines` draw from this single iterator so the served
/// set and the banner cannot drift apart.
fn served_suffixes() -> impl Iterator<Item = &'static str> {
    LIVE.iter().chain(STATIC_ZERO.iter()).chain(RPC_NAMES.iter()).copied()
}

/// A cheap, cloneable "read the current name list" callback used by
/// [`StatusHandles`]'s live string-array PVs (`clients`, `cache`).
type ListHandle = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// A cheap, cloneable "read the current count" callback, used for the live
/// field(s) of the `stats` [`epics:p2p/Stats:1.0`] structure.
type CountHandle = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Live-value handles `StatusSource` reads for its `clients`/`cache`/`stats`
/// PVs.
///
/// Cheap to clone (each field is an `Arc`), so a clone can be handed to the
/// `subscribe` ticker task without borrowing the source. `refs` and the
/// bandwidth PVs are served as NTTables built from no live source yet (empty
/// rows), so they need no handle here; `threads` is a static string with no
/// gauge.
#[derive(Clone)]
pub struct StatusHandles {
    /// Downstream client names (p4p `clients`, an NTScalarArray-of-string).
    pub clients: ListHandle,
    /// Upstream channel-cache names (p4p `cache`, an NTScalarArray-of-string).
    pub cache: ListHandle,
    /// Monitor-cache size — the one `stats` field (`mcacheSize`) spvirit has a
    /// real live source for. The remaining `Stats` fields are stubbed to 0.
    pub mcache_size: CountHandle,
}

impl StatusHandles {
    /// All handles stubbed empty/zero — used by unit tests that don't need a
    /// real `GatewaySource`/`UpstreamPool`.
    pub fn test() -> Self {
        let empty: ListHandle = Arc::new(Vec::new);
        Self {
            clients: empty.clone(),
            cache: empty,
            mcache_size: Arc::new(|| 0),
        }
    }

    /// Wires the handles the runtime has a real M1 data source for:
    /// - `cache` <- [`GatewaySource::upstream_monitor_names`], the live list
    ///   of upstream channels currently held in the monitor cache. This
    ///   genuinely changes as monitors come and go, so its NTScalarArray
    ///   payload changes and the monitor pump forwards a fresh frame.
    /// - `stats.mcacheSize` <- [`GatewaySource::upstream_monitor_count`], the
    ///   number of distinct upstream monitors currently running (p4p's
    ///   monitor-cache size). This is live, so the `stats` structure updates.
    ///
    /// `clients` has no downstream-peer registry to read from in M1, so it is
    /// stubbed to an empty list (correct NTScalarArray shape, data-population
    /// follow-up). The other five `Stats` fields have no M1 collector and are
    /// stubbed to 0 — documented gaps, not oversights.
    pub fn from_gateway(src: &Arc<GatewaySource>) -> Self {
        let src_cache = src.clone();
        let src_stats = src.clone();
        Self {
            clients: Arc::new(Vec::new),
            cache: Arc::new(move || src_cache.upstream_monitor_names()),
            mcache_size: Arc::new(move || src_stats.upstream_monitor_count() as u64),
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

    /// An NTScalarArray-of-string payload (p4p's shape for `clients`/`cache`).
    fn string_array_payload(items: Vec<String>) -> NtPayload {
        NtPayload::ScalarArray(NtScalarArray::from_value(ScalarArrayValue::Str(items)))
    }

    /// A nested `epics:nt/NTScalar:1.0` sub-structure carrying an unsigned-long
    /// value, matching p4p's `NTScalar.buildType('L')` for each `Stats` field
    /// (`value` + default `alarm`/`timeStamp`). Built as a [`PvValue`] tree so
    /// it can be a field of the enclosing `Stats` structure.
    fn ntscalar_ulong_field(v: u64) -> PvValue {
        PvValue::Structure {
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            fields: vec![
                ("value".to_string(), PvValue::Scalar(ScalarValue::U64(v))),
                (
                    "alarm".to_string(),
                    PvValue::Structure {
                        struct_id: "alarm_t".to_string(),
                        fields: vec![
                            ("severity".to_string(), PvValue::Scalar(ScalarValue::I32(0))),
                            ("status".to_string(), PvValue::Scalar(ScalarValue::I32(0))),
                            ("message".to_string(), PvValue::Scalar(ScalarValue::Str(String::new()))),
                        ],
                    },
                ),
                (
                    "timeStamp".to_string(),
                    PvValue::Structure {
                        struct_id: "time_t".to_string(),
                        fields: vec![
                            ("secondsPastEpoch".to_string(), PvValue::Scalar(ScalarValue::I64(0))),
                            ("nanoseconds".to_string(), PvValue::Scalar(ScalarValue::I32(0))),
                            ("userTag".to_string(), PvValue::Scalar(ScalarValue::I32(0))),
                        ],
                    },
                ),
            ],
        }
    }

    /// The `stats` structure — p4p's `epics:p2p/Stats:1.0`: six unsigned-long
    /// cache/ban size fields, each a nested `NTScalar('L')`. Only `mcacheSize`
    /// (the upstream monitor-cache size) has a live M1 source; the other five
    /// are stubbed to 0 with the correct field names (shape-complete,
    /// data-pending). Field order matches p4p's `statsType`.
    fn stats_structure(mcache_size: u64) -> NtPayload {
        NtPayload::Generic {
            struct_id: "epics:p2p/Stats:1.0".to_string(),
            fields: vec![
                ("ccacheSize".to_string(), Self::ntscalar_ulong_field(0)),
                ("mcacheSize".to_string(), Self::ntscalar_ulong_field(mcache_size)),
                ("gcacheSize".to_string(), Self::ntscalar_ulong_field(0)),
                ("banHostSize".to_string(), Self::ntscalar_ulong_field(0)),
                ("banPVSize".to_string(), Self::ntscalar_ulong_field(0)),
                ("banHostPVSize".to_string(), Self::ntscalar_ulong_field(0)),
            ],
        }
    }

    /// The static `threads` get/subscribe value — p4p registers `threads` as a
    /// `SharedPV(nt=NTScalar('s'), initial='RPC only')` whose value stays the
    /// constant string `"RPC only"` forever (its RPC handler returns the stack
    /// dump via `op.done` and never posts). We mirror that: a plain readable /
    /// monitorable `NTScalar('s')` of `"RPC only"`.
    fn threads_static_value() -> NtPayload {
        NtPayload::Scalar(NtScalar::from_value(ScalarValue::Str("RPC only".into())))
    }

    /// The `threads` RPC response — p4p serves `threads` as an RPC that dumps
    /// thread stacks. Rust has no `faulthandler` equivalent and the gateway
    /// does no OS-thread introspection, so this returns a shape-correct
    /// best-effort string rather than a fabricated stack dump.
    fn threads_rpc_payload() -> NtPayload {
        let parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0);
        let msg = format!(
            "spvirit-gateway: OS thread stack-trace dump is not available (no \
             faulthandler equivalent in Rust). available_parallelism={parallelism}; \
             async work runs as tokio tasks, not dedicated OS threads."
        );
        NtPayload::Scalar(NtScalar::from_value(ScalarValue::Str(msg)))
    }

    /// The `refs` NTTable — p4p's `RefAdapter` shape: columns
    /// `type`/`count`/`delta` labelled `Type`/`Count`/`Delta`. No refcount
    /// collector exists in M1, so the rows are empty (correct shape, data
    /// follow-up).
    fn refs_table() -> NtPayload {
        NtPayload::Table(NtTable {
            labels: vec!["Type".into(), "Count".into(), "Delta".into()],
            columns: vec![
                NtTableColumn { name: "type".into(), values: ScalarArrayValue::Str(vec![]) },
                NtTableColumn { name: "count".into(), values: ScalarArrayValue::U32(vec![]) },
                NtTableColumn { name: "delta".into(), values: ScalarArrayValue::I32(vec![]) },
            ],
            descriptor: None,
            alarm: None,
            time_stamp: None,
        })
    }

    /// A bandwidth-counter NTTable matching p4p's `TableBuilder` shapes. The
    /// `bypv` tables carry `name`/`rate` (labels `PV` + direction); `us:byhost`
    /// carries `name`/`rate` (labels `Server` + direction); `ds:byhost` adds an
    /// `account` column (labels `Account`/`Client` + direction). No per-PV /
    /// per-host byte accounting exists in M1, so every table has empty rows
    /// (correct shape, `0` rows != "no traffic"; data follow-up).
    fn bandwidth_table(suffix: &str) -> NtPayload {
        let dir_label = if suffix.ends_with(":tx") { "TX (B/s)" } else { "RX (B/s)" };
        let (labels, columns) = if suffix.starts_with("ds:byhost:") {
            (
                vec!["Account".into(), "Client".into(), dir_label.into()],
                vec![
                    NtTableColumn { name: "account".into(), values: ScalarArrayValue::Str(vec![]) },
                    NtTableColumn { name: "name".into(), values: ScalarArrayValue::Str(vec![]) },
                    NtTableColumn { name: "rate".into(), values: ScalarArrayValue::F64(vec![]) },
                ],
            )
        } else {
            // `*:bypv:*` -> "PV"; `us:byhost:*` -> "Server".
            let name_label = if suffix.contains(":byhost:") { "Server" } else { "PV" };
            (
                vec![name_label.into(), dir_label.into()],
                vec![
                    NtTableColumn { name: "name".into(), values: ScalarArrayValue::Str(vec![]) },
                    NtTableColumn { name: "rate".into(), values: ScalarArrayValue::F64(vec![]) },
                ],
            )
        };
        NtPayload::Table(NtTable { labels, columns, descriptor: None, alarm: None, time_stamp: None })
    }

    /// Builds the current value payload for a non-RPC status PV suffix, in the
    /// p4p-matched shape. Free of `&self` borrows on anything but the two
    /// arguments so the `subscribe` ticker task can call it from a cloned
    /// [`StatusHandles`] + generation counter.
    fn value_payload(
        suffix: &str,
        handles: &StatusHandles,
        generation: &AtomicU64,
    ) -> Option<NtPayload> {
        Some(match suffix {
            "clients" => Self::string_array_payload((handles.clients)()),
            "cache" => Self::string_array_payload((handles.cache)()),
            "refs" => Self::refs_table(),
            "stats" => Self::stats_structure((handles.mcache_size)()),
            "poke" => Self::scalar_payload(generation.load(Ordering::Relaxed) as f64),
            "threads" => Self::threads_static_value(),
            s if STATIC_ZERO.contains(&s) => Self::bandwidth_table(s),
            _ => return None,
        })
    }

    /// Whether an access [`Decision`] grants the operation (an alias rewrite
    /// still grants it).
    fn allowed(d: &Decision) -> bool {
        matches!(d, Decision::Allow | Decision::AllowAliased(_))
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

    /// Builds the `asTest` RPC response in p4p's shape: the
    /// `epics:p2p/Permission:1.0` structure carrying the queried `pv`,
    /// `account`/`peer` echoed from the request, and a nested `permission`
    /// sub-structure of boolean verdicts (`put`/`rpc`, plus `uncached`/`audit`
    /// which spvirit does not model yet and reports `false`).
    ///
    /// spvirit does not expose `roles`/`asg`/`asl` from its `AccessControl`,
    /// so those p4p fields are present (shape parity) but empty/zero — a
    /// data-population follow-up, not a shape divergence.
    fn astest_response(
        pv: &str,
        account: &str,
        peer: &str,
        put_d: &Decision,
        rpc_d: &Decision,
    ) -> NtPayload {
        let permission = PvValue::Structure {
            struct_id: String::new(),
            fields: vec![
                ("put".to_string(), PvValue::Scalar(ScalarValue::Bool(Self::allowed(put_d)))),
                ("rpc".to_string(), PvValue::Scalar(ScalarValue::Bool(Self::allowed(rpc_d)))),
                ("uncached".to_string(), PvValue::Scalar(ScalarValue::Bool(false))),
                ("audit".to_string(), PvValue::Scalar(ScalarValue::Bool(false))),
            ],
        };
        NtPayload::Generic {
            struct_id: "epics:p2p/Permission:1.0".to_string(),
            fields: vec![
                ("pv".to_string(), PvValue::Scalar(ScalarValue::Str(pv.to_string()))),
                ("account".to_string(), PvValue::Scalar(ScalarValue::Str(account.to_string()))),
                ("peer".to_string(), PvValue::Scalar(ScalarValue::Str(peer.to_string()))),
                ("roles".to_string(), PvValue::ScalarArray(ScalarArrayValue::Str(vec![]))),
                ("asg".to_string(), PvValue::Scalar(ScalarValue::Str(String::new()))),
                ("asl".to_string(), PvValue::Scalar(ScalarValue::I32(0))),
                ("permission".to_string(), permission),
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
            // Hardening: a pvlist DENY hides the status PV entirely (claim
            // fails, so it is never registered for this identity). readOnly
            // does not affect Get, so this only bites on an explicit DENY.
            if let Decision::Deny = self.access.decide(Op::Get, &name, &current_identity()) {
                return None;
            }
            let payload = if RPC_NAMES.contains(&suffix) {
                // RPC-only PVs still advertise a result descriptor at claim
                // time, each in its own shape.
                match suffix {
                    // `threads` is both a value PV and an RPC target; advertise
                    // the get/subscribe value shape (identical NTScalar('s')).
                    "threads" => Self::threads_static_value(),
                    _ => Self::astest_response("", "", "", &Decision::Deny, &Decision::Deny),
                }
            } else {
                Self::value_payload(suffix, &self.handles, &self.generation)?
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
            // `asTest` is pure RPC (no readable value). `threads` is BOTH a
            // static gettable "RPC only" string AND an RPC target (p4p parity),
            // so it is NOT excluded here.
            if suffix == "asTest" {
                return None;
            }
            if let Decision::Deny = self.access.decide(Op::Get, &name, &current_identity()) {
                return None;
            }
            if !LIVE.contains(&suffix) && !STATIC_ZERO.contains(&suffix) && suffix != "threads" {
                return None;
            }
            Self::value_payload(suffix, &self.handles, &self.generation)
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
            // Gate the write through the same AccessControl the data-plane
            // GatewaySource uses, BEFORE mutating the generation counter.
            // For a pure `readOnly` config, `decide` short-circuits at step 1
            // (before any host/pvlist match), so even a default `Identity`
            // — as seen when called outside a request scope — enforces it.
            if let Decision::Deny = self.access.decide(Op::Put, &name, &current_identity()) {
                return Err("access denied".to_string());
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
            // `asTest` is pure RPC (nothing to monitor). `threads` IS
            // subscribable — a static "RPC only" string served like the static
            // bandwidth PVs (emit once, then close) — so it is NOT excluded.
            if suffix == "asTest" {
                return None;
            }
            if let Decision::Deny = self.access.decide(Op::Get, &name, &current_identity()) {
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
                        let Some(payload) = Self::value_payload(&suffix, &handles, &generation)
                        else {
                            break;
                        };
                        // The value is re-read every tick; the server's monitor
                        // pump suppresses byte-identical frames, so a client
                        // sees a new frame only when the underlying list/gauge
                        // actually changes — matching p4p's post-on-change.
                        if tx.send(payload).await.is_err() {
                            // Receiver dropped: stop ticking, don't leak the task.
                            break;
                        }
                    }
                });
            } else if STATIC_ZERO.contains(&suffix.as_str()) {
                tokio::spawn(async move {
                    let _ = tx.send(Self::bandwidth_table(&suffix)).await;
                    // Emit the (static, empty) bandwidth table once, then let
                    // `tx` drop — the receiver observes a closed channel (idle)
                    // rather than further updates, since no byte accounting
                    // exists to change the value.
                });
            } else if suffix == "threads" {
                tokio::spawn(async move {
                    let _ = tx.send(Self::threads_static_value()).await;
                    // Emit the static "RPC only" string once, then let `tx`
                    // drop. p4p's `threads` SharedPV value never changes (its
                    // RPC handler returns via `op.done` and never posts), so
                    // there are no further updates to send.
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
            if !RPC_NAMES.contains(&suffix) {
                return Err("RPC not supported".to_string());
            }
            // `threads` also responds to RPC (p4p's stack-dump handler) — its
            // get/subscribe value stays the static "RPC only" string, but RPC
            // returns the richer best-effort description. Gate it through the
            // same AccessControl (an RPC op on its own name).
            if suffix == "threads" {
                if let Decision::Deny = self.access.decide(Op::Rpc, &name, &current_identity()) {
                    return Err("access denied".to_string());
                }
                return Ok(Self::threads_rpc_payload());
            }
            let pv = Self::decoded_field_str(&args, "pv").unwrap_or_default();
            let id = Identity {
                host: Self::decoded_field_str(&args, "host"),
                user: Self::decoded_field_str(&args, "user"),
            };
            let account = id.user.clone().unwrap_or_default();
            let peer = id.host.clone().unwrap_or_default();
            let put_d = self.access.decide(Op::Put, &pv, &id);
            let rpc_d = self.access.decide(Op::Rpc, &pv, &id);
            Ok(Self::astest_response(&pv, &account, &peer, &put_d, &rpc_d))
        })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        let prefix = self.prefix.clone();
        Box::pin(async move { served_suffixes().map(|s| format!("{prefix}{s}")).collect() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "GW:STATUS:";

    #[test]
    fn now_ts_is_unix_seconds_not_epics_epoch() {
        let ts = now_ts();
        // Post-2020 UNIX seconds; proves no 1990-epoch offset and non-zero.
        assert!(ts.seconds_past_epoch > 1_577_836_800, "got {}", ts.seconds_past_epoch);
    }

    #[test]
    fn stamp_sets_scalar_array_and_table_and_generic_stats() {
        let ts = NtTimeStamp { seconds_past_epoch: 42, nanoseconds: 7, user_tag: 0 };

        let mut arr = StatusSource::string_array_payload(vec!["x".into()]);
        stamp(&mut arr, ts.clone());
        let NtPayload::ScalarArray(a) = &arr else { panic!() };
        assert_eq!(a.time_stamp.seconds_past_epoch, 42);

        let mut tbl = StatusSource::refs_table();
        stamp(&mut tbl, ts.clone());
        let NtPayload::Table(t) = &tbl else { panic!() };
        assert_eq!(t.time_stamp.as_ref().unwrap().seconds_past_epoch, 42);

        let mut st = StatusSource::stats_structure(5);
        stamp(&mut st, ts.clone());
        let NtPayload::Generic { fields, .. } = &st else { panic!() };
        // mcacheSize's nested timeStamp.secondsPastEpoch is now 42.
        let mc = fields.iter().find(|(n, _)| n == "mcacheSize").unwrap();
        let PvValue::Structure { fields: inner, .. } = &mc.1 else { panic!() };
        let tsf = inner.iter().find(|(n, _)| n == "timeStamp").unwrap();
        let PvValue::Structure { fields: tf, .. } = &tsf.1 else { panic!() };
        let secs = tf.iter().find(|(n, _)| n == "secondsPastEpoch").unwrap();
        assert!(matches!(&secs.1, PvValue::Scalar(ScalarValue::I64(42))));
    }

    fn source(read_only: bool) -> StatusSource {
        let access = Arc::new(AccessControl::new(read_only, None, None));
        StatusSource::new(PREFIX.to_string(), access, StatusHandles::test())
    }

    /// Item 1g: `poke` is the only writable status PV, but under a `readOnly`
    /// config a downstream `put` to it must be DENIED and must NOT advance the
    /// generation counter. Before the fix `put` mutated the counter without
    /// ever consulting `AccessControl` — a real `readOnly` bypass. `decide`
    /// short-circuits at step 1 for `readOnly`, so even the default `Identity`
    /// this off-connection unit test produces enforces the deny.
    #[tokio::test]
    async fn read_only_denies_poke_write() {
        let src = source(true);
        let name = format!("{PREFIX}poke");
        let value = DecodedValue::Structure(vec![]);

        let before = src.generation.load(Ordering::Relaxed);
        let res = src.put(&name, &value).await;

        assert!(res.is_err(), "readOnly must deny the poke write");
        assert_eq!(res.unwrap_err(), "access denied");
        assert_eq!(
            src.generation.load(Ordering::Relaxed),
            before,
            "a denied write must NOT advance the generation counter"
        );
    }

    /// Control: without `readOnly` the same `poke` write is allowed and bumps
    /// the generation counter — proving the gate denies only what it should.
    #[tokio::test]
    async fn writable_config_allows_poke_write() {
        let src = source(false);
        let name = format!("{PREFIX}poke");
        let value = DecodedValue::Structure(vec![]);

        let before = src.generation.load(Ordering::Relaxed);
        let res = src.put(&name, &value).await;

        assert!(res.is_ok(), "a writable config must allow the poke write");
        assert_eq!(
            src.generation.load(Ordering::Relaxed),
            before + 1,
            "an allowed write must advance the generation counter"
        );
    }

    /// A non-`poke` status PV stays read-only regardless of access config: the
    /// gate is reached only after the read-only-suffix check, so the error is
    /// the "read-only" one, not "access denied".
    #[tokio::test]
    async fn non_poke_status_pv_is_read_only() {
        let src = source(false);
        let name = format!("{PREFIX}clients");
        let value = DecodedValue::Structure(vec![]);

        let res = src.put(&name, &value).await;
        assert!(res.is_err());
        assert!(
            res.unwrap_err().contains("read-only"),
            "non-poke status PVs are rejected as read-only before the access gate"
        );
    }

    /// A `StatusSource` whose `cache` list is driven by a caller-controllable
    /// `Vec<String>` — lets a test change the underlying value without a real
    /// `GatewaySource`, so the ticker's post-on-change behaviour is testable.
    fn source_with_cache(
        list: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> StatusSource {
        let handles = StatusHandles {
            clients: Arc::new(Vec::new),
            cache: Arc::new(move || list.lock().unwrap().clone()),
            mcache_size: Arc::new(|| 0),
        };
        let access = Arc::new(AccessControl::new(false, None, None));
        StatusSource::new(PREFIX.to_string(), access, handles)
    }

    /// A `StatusSource` whose `stats.mcacheSize` is driven by a
    /// caller-controllable `u64`.
    fn source_with_mcache(count: Arc<std::sync::atomic::AtomicU64>) -> StatusSource {
        let handles = StatusHandles {
            clients: Arc::new(Vec::new),
            cache: Arc::new(Vec::new),
            mcache_size: Arc::new(move || count.load(Ordering::Relaxed)),
        };
        let access = Arc::new(AccessControl::new(false, None, None));
        StatusSource::new(PREFIX.to_string(), access, handles)
    }

    /// Digs the `value` (a `u64`) out of a nested `NTScalar('L')` `Stats`
    /// field.
    fn stats_field_u64(fields: &[(String, PvValue)], name: &str) -> u64 {
        let f = fields.iter().find(|(n, _)| n == name).map(|(_, v)| v).expect("field");
        let PvValue::Structure { fields: inner, .. } = f else {
            panic!("{name} must be a nested NTScalar structure, got {f:?}");
        };
        match inner.iter().find(|(n, _)| n == "value").map(|(_, v)| v) {
            Some(PvValue::Scalar(ScalarValue::U64(v))) => *v,
            other => panic!("{name}.value must be a U64 scalar, got {other:?}"),
        }
    }

    /// p4p shape: `clients`/`cache` are NTScalarArray-of-string, not scalars.
    #[tokio::test]
    async fn clients_and_cache_get_returns_string_array() {
        let src = source(false);
        for suffix in ["clients", "cache"] {
            let p = src.get(&format!("{PREFIX}{suffix}")).await.expect("get");
            match p {
                NtPayload::ScalarArray(a) => {
                    assert!(matches!(a.value, ScalarArrayValue::Str(_)), "{suffix} must be a string array");
                }
                other => panic!("{suffix} must be an NTScalarArray, got {other:?}"),
            }
        }
    }

    /// p4p shape: `refs` is an NTTable labelled `Type`/`Count`/`Delta`.
    #[tokio::test]
    async fn refs_get_returns_ntt_table_with_expected_labels() {
        let src = source(false);
        let p = src.get(&format!("{PREFIX}refs")).await.expect("get");
        let NtPayload::Table(t) = p else {
            panic!("refs must be an NTTable, got {p:?}");
        };
        assert_eq!(t.labels, vec!["Type", "Count", "Delta"]);
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["type", "count", "delta"]);
    }

    /// p4p shape: the bandwidth PVs are NTTables. `ds:byhost:*` has the extra
    /// `Account` column; the rate column's label is direction-specific.
    #[tokio::test]
    async fn bandwidth_pvs_are_tables_with_p4p_columns() {
        let src = source(false);

        let p = src.get(&format!("{PREFIX}us:bypv:tx")).await.expect("get");
        let NtPayload::Table(t) = p else { panic!("bandwidth PV must be a table") };
        assert_eq!(t.labels, vec!["PV", "TX (B/s)"]);

        let p = src.get(&format!("{PREFIX}ds:byhost:rx")).await.expect("get");
        let NtPayload::Table(t) = p else { panic!("bandwidth PV must be a table") };
        assert_eq!(t.labels, vec!["Account", "Client", "RX (B/s)"]);
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["account", "name", "rate"]);

        let p = src.get(&format!("{PREFIX}us:byhost:tx")).await.expect("get");
        let NtPayload::Table(t) = p else { panic!("bandwidth PV must be a table") };
        assert_eq!(t.labels, vec!["Server", "TX (B/s)"]);
    }

    /// asTest returns p4p's `epics:p2p/Permission:1.0` with a nested
    /// `permission` sub-structure, and `readOnly` reports `put=false`.
    #[tokio::test]
    async fn astest_returns_permission_shape() {
        let ac = Arc::new(AccessControl::new(true, None, None)); // readOnly
        let src = StatusSource::new(PREFIX.to_string(), ac, StatusHandles::test());
        let args = DecodedValue::Structure(vec![
            ("pv".to_string(), DecodedValue::String("SOMEPV".to_string())),
            ("user".to_string(), DecodedValue::String("alice".to_string())),
            ("host".to_string(), DecodedValue::String("10.0.0.1".to_string())),
        ]);
        let out = src.rpc(&format!("{PREFIX}asTest"), &args).await.expect("rpc");
        let NtPayload::Generic { struct_id, fields } = out else {
            panic!("asTest must return a Generic structure");
        };
        assert_eq!(struct_id, "epics:p2p/Permission:1.0");
        // account/peer echoed from the request args.
        let account = fields.iter().find(|(n, _)| n == "account").map(|(_, v)| v);
        assert!(matches!(account, Some(PvValue::Scalar(ScalarValue::Str(s))) if s == "alice"));
        // nested permission.put is false under readOnly.
        let perm = fields.iter().find(|(n, _)| n == "permission").map(|(_, v)| v).expect("permission");
        let PvValue::Structure { fields: pf, .. } = perm else {
            panic!("permission must be a sub-structure");
        };
        let put = pf.iter().find(|(n, _)| n == "put").map(|(_, v)| v);
        assert!(matches!(put, Some(PvValue::Scalar(ScalarValue::Bool(false)))), "readOnly => put denied");
    }

    /// The core of the "diag PVs don't update" fix: when the underlying list
    /// changes, the ticker produces a *different* frame and the subscribe
    /// receiver observes it. Driven deterministically with a paused clock —
    /// no wall-clock wait.
    #[tokio::test(start_paused = true)]
    async fn subscribe_delivers_a_new_frame_when_the_cache_list_changes() {
        let list = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let src = source_with_cache(list.clone());

        let mut rx = src.subscribe(&format!("{PREFIX}cache")).await.expect("subscribe");

        // A fresh interval's first tick completes immediately.
        let first = rx.recv().await.expect("first frame");
        assert_eq!(first, StatusSource::string_array_payload(vec![]));

        // Change the underlying value, then let one tick period elapse.
        list.lock().unwrap().push("UP:CHAN".to_string());
        tokio::time::advance(TICK_PERIOD).await;

        let second = rx.recv().await.expect("second frame");
        assert_eq!(
            second,
            StatusSource::string_array_payload(vec!["UP:CHAN".to_string()])
        );
        assert_ne!(first, second, "a changed value must produce a different frame");
    }

    /// p4p shape: `stats` is `epics:p2p/Stats:1.0` with six ulong cache/ban
    /// fields; `mcacheSize` carries the live monitor-cache size.
    #[tokio::test]
    async fn stats_get_returns_stats_structure() {
        let count = Arc::new(std::sync::atomic::AtomicU64::new(3));
        let src = source_with_mcache(count);
        let p = src.get(&format!("{PREFIX}stats")).await.expect("get");
        let NtPayload::Generic { struct_id, fields } = p else {
            panic!("stats must be a Generic structure, got {p:?}");
        };
        assert_eq!(struct_id, "epics:p2p/Stats:1.0");
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["ccacheSize", "mcacheSize", "gcacheSize", "banHostSize", "banPVSize", "banHostPVSize"]
        );
        assert_eq!(stats_field_u64(&fields, "mcacheSize"), 3, "mcacheSize is the live field");
        assert_eq!(stats_field_u64(&fields, "ccacheSize"), 0, "unwired fields stub to 0");
    }

    /// The `stats` structure updates: when `mcacheSize`'s source changes, the
    /// subscribe ticker delivers a second, different frame past the pump dedup.
    #[tokio::test(start_paused = true)]
    async fn subscribe_delivers_a_new_stats_frame_when_mcache_changes() {
        let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let src = source_with_mcache(count.clone());

        let mut rx = src.subscribe(&format!("{PREFIX}stats")).await.expect("subscribe");
        let first = rx.recv().await.expect("first frame");

        count.store(5, Ordering::Relaxed);
        tokio::time::advance(TICK_PERIOD).await;
        let second = rx.recv().await.expect("second frame");

        assert_ne!(first, second, "a changed mcacheSize must produce a different frame");
        let NtPayload::Generic { fields, .. } = &second else { panic!("stats must be Generic") };
        assert_eq!(stats_field_u64(fields, "mcacheSize"), 5);
    }

    /// p4p parity: `threads` is a static gettable/subscribable `NTScalar('s')`
    /// whose value is the constant `"RPC only"`, AND it responds to `rpc` with
    /// the richer best-effort thread-dump string.
    #[tokio::test]
    async fn threads_serves_static_string_and_rpc() {
        let src = source(false);
        let name = format!("{PREFIX}threads");

        // get -> static "RPC only" string.
        let got = src.get(&name).await.expect("threads must be a get value");
        match got {
            NtPayload::Scalar(nt) => {
                assert_eq!(nt.value, ScalarValue::Str("RPC only".into()));
            }
            other => panic!("threads get must be an NTScalar string, got {other:?}"),
        }

        // subscribe -> the same static "RPC only" string (emitted once).
        let mut rx = src.subscribe(&name).await.expect("threads must be subscribable");
        match rx.recv().await.expect("threads subscribe frame") {
            NtPayload::Scalar(nt) => {
                assert_eq!(nt.value, ScalarValue::Str("RPC only".into()));
            }
            other => panic!("threads subscribe must be an NTScalar string, got {other:?}"),
        }

        // rpc -> a (richer) string scalar, distinct from the static value.
        let out = src.rpc(&name, &DecodedValue::Structure(vec![])).await.expect("rpc");
        match out {
            NtPayload::Scalar(nt) => match nt.value {
                ScalarValue::Str(s) => assert_ne!(
                    s, "RPC only",
                    "threads RPC must return the richer description, not the static value"
                ),
                other => panic!("threads RPC must return a string scalar, got {other:?}"),
            },
            other => panic!("threads RPC must return an NTScalar string, got {other:?}"),
        }
    }
}
