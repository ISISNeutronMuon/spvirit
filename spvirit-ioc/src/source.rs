//! The engine behind the `Source` trait.
//!
//! Everything the engine does is synchronous and happens under a lock set's
//! mutex; this module is the only place that touches async. A `put` runs the
//! pass, drops the lock, and hands the accumulated monitors back to the
//! server, which is exactly the contract `Source::put` already has.

use crate::build::build_records;
use crate::clock::{Clock, SystemClock};
use crate::ctx::ProcCtx;
use crate::fields::{record_field_kind, record_field_value};
use crate::graph::DependencyGraph;
use crate::lockset::{RecordDb, RecordId};
use crate::model::{Field, Value};
use crate::process::{process, write_field};
use crate::scan::{ProcSink, ScanSpec, Scanner, parse_scan};
use crate::spec::{RecordSpec, unmodelled_fields};
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::db::{DbRecord, load_db_records, parse_db_records};
use spvirit_server::field_provider::{
    RecordFieldDesc, RecordFieldProvider, resolve_field_info, resolve_field_payload,
};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::record_fields::parse_field_ref;
use spvirit_server::pvstore::{PvInfo, Source, StoreSource};
use spvirit_server::simple_store::descriptor_for_payload;
use spvirit_types::{NtPayload, ScalarValue};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc;

/// Depth of a subscriber's monitor channel. Matches `SimplePvStore`'s
/// `mpsc::channel(64)` in `Source::subscribe`.
const SUBSCRIBER_QUEUE: usize = 64;

/// The processing engine as a `Source`.
///
/// Immutable after construction. There is deliberately no `add_record`: see
/// [`IocSource::LOCK_SET_IMMUTABILITY_REASON`]. In Rust the absence of the
/// method is the refusal, checked by the compiler; Python, which has no such
/// check, raises with that same text from `Ioc.add_record`.
pub struct IocSource {
    /// Shared with the [`Scanner`] so both serve the same records: the put
    /// handler updates `Common.scan_raw`/`phas`/`evnt` through this handle and
    /// the Scanner processes the very same lock sets. `Scanner::new` requires
    /// `Arc<RecordDb>`; every `self.db.*` call reaches through the `Arc` deref
    /// unchanged.
    db: Arc<RecordDb>,
    /// The single authority for scan-list membership. Writable SCAN/EVNT/PHAS
    /// puts route add/remove calls here; egress/lifecycle wiring
    /// (start/shutdown/load_from_db/real ProcSink) is deferred to Task 15, so
    /// this Scanner is constructed with a no-op sink and never started here.
    scanner: Arc<Scanner>,
    /// Each record's kind, by name. Lets `field_descriptor` answer a channel
    /// search without taking the record's lock set, which is the whole point
    /// of the `dbNameToAddr`/`dbGetField` split.
    kinds: HashMap<String, crate::model::Kind>,
    /// Each record's name, by slot id — how a resolved link target gets
    /// rendered back to a name without reaching into another lock set.
    id_names: HashMap<crate::lockset::RecordId, String>,
    subscribers: Mutex<HashMap<String, Vec<mpsc::Sender<NtPayload>>>>,
    /// Senders for open *field* subscriptions. Field values are served as a
    /// one-shot snapshot in A2 — live field monitors arrive with the field
    /// writes in sub-project B — so these are retained only to keep the
    /// channels open, exactly as `RecordFieldSource::open_subs` does for
    /// tier 2 (`SimplePvStore`). Each subscribe first prunes senders whose
    /// receiver has been dropped (`is_closed()`), so the Vec stays bounded
    /// under subscribe/disconnect churn rather than growing forever.
    field_subs: Mutex<Vec<mpsc::Sender<NtPayload>>>,
    /// The server's monitor registry, handed over by
    /// `PvaServer::serve_after_start_hooks` before serving begins.
    ///
    /// A `std::sync::RwLock` rather than tokio's: it is written once at
    /// startup and read once per host-side write, and the guard is never held
    /// across an await — `set_value` clones the `Arc` out and drops the guard
    /// before it publishes. That is load-bearing, not incidental; A2's review
    /// established that no lock in this crate crosses an await.
    registry: RwLock<Option<Arc<MonitorRegistry>>>,
}

/// A minimal, opaque `Debug` — `RecordDb` and friends don't derive it, and
/// the only consumer of this impl is `Result::expect_err` in
/// `tests/programmatic.rs`, which never prints the `Ok` value anyway.
impl std::fmt::Debug for IocSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IocSource").finish_non_exhaustive()
    }
}

impl IocSource {
    /// Why records cannot be added after construction.
    ///
    /// Lives here as a constant so the Rust documentation and the Python
    /// exception message are the same words. Two wordings would drift, and
    /// the Python one is the only one most users will ever read.
    pub const LOCK_SET_IMMUTABILITY_REASON: &'static str =
        "records are fixed when the engine is built: RecordId is {set, index}, \
         assigned by partitioning the link graph into lock sets, so a record \
         whose links join two existing sets would invalidate every outstanding \
         id. EPICS Base has the same restriction — dbLoadRecords after iocInit \
         is unsupported. Pass every record to Ioc(records=[...]) at once.";

    pub fn from_db_file(path: &str) -> Result<IocSource, String> {
        let raw = load_db_records(path, &HashMap::new()).map_err(|e| e.to_string())?;
        Self::from_raw(raw)
    }

    pub fn from_db_str(content: &str) -> Result<IocSource, String> {
        let raw =
            parse_db_records(content, "<memory>", &HashMap::new()).map_err(|e| e.to_string())?;
        Self::from_raw(raw)
    }

    /// Build an engine from records described in host code.
    ///
    /// Equivalent to writing the same records as `.db` text and calling
    /// [`IocSource::from_db_str`] — the specs lower to the very `DbRecord`s
    /// the parser produces, so both paths converge here before any field is
    /// interpreted. `tests/programmatic.rs` pins the equivalence as an
    /// observable property.
    ///
    /// Returns an `Arc` because it binds every spec to the built engine: a
    /// `RecordSpec` the caller kept is a live handle afterwards (Ruling 6).
    pub fn from_records(records: Vec<RecordSpec>) -> Result<Arc<IocSource>, String> {
        let raws = records.iter().map(RecordSpec::to_db_record).collect();
        let source = Arc::new(Self::from_raw(raws)?);
        for spec in &records {
            spec.bind(&source);
        }
        Ok(source)
    }

    fn from_raw(raw: Vec<DbRecord>) -> Result<IocSource, String> {
        let records = build_records(&raw).map_err(|e| e.to_string())?;
        let kinds = records
            .iter()
            .map(|r| (r.name.clone(), r.kind))
            .collect::<HashMap<_, _>>();
        let db = Arc::new(RecordDb::build(records));
        let id_names = db
            .names()
            .into_iter()
            .filter_map(|name| db.lookup(&name).map(|id| (id, name)))
            .collect::<HashMap<_, _>>();
        // The Scanner shares `db` so it processes the same records the source
        // serves. In production SystemClock is correct; a no-op ProcSink is a
        // placeholder until Task 15 wires the real egress. It is never started
        // here, so no thread is spawned and no clock is ever read — Task 12
        // exercises only clock-independent membership add/remove.
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let sink: Arc<dyn ProcSink> = Arc::new(NoopSink);
        let scanner = Arc::new(Scanner::new(db.clone(), clock, sink));
        let source = IocSource {
            db,
            scanner,
            kinds,
            id_names,
            subscribers: Mutex::new(HashMap::new()),
            field_subs: Mutex::new(Vec::new()),
            registry: RwLock::new(None),
        };
        // Report the load-time diagnostics once, here, rather than on every
        // pass. None of them is fatal.
        //
        // The unmodelled-field warning is emitted here, on the shared path,
        // rather than in `from_records` — a `.db` carrying DRVH deserves the
        // same warning as a `RecordSpec` carrying it, and warning on only one
        // path would make the two paths distinguishable by their diagnostics
        // even though their behaviour is identical.
        for line in source.graph().report() {
            tracing::warn!(target: "spvirit_ioc", "{line}");
        }
        for r in &raw {
            let unmodelled = unmodelled_fields(r);
            if !unmodelled.is_empty() {
                tracing::warn!(
                    target: "spvirit_ioc",
                    "record '{}': the engine does not model {} — accepted and ignored, \
                     exactly as dbLoadRecords ignores a field no DSET reads",
                    r.name,
                    unmodelled.join(", "),
                );
            }
        }
        Ok(source)
    }

    pub fn graph(&self) -> DependencyGraph {
        self.db.dependency_graph()
    }

    /// Every record name, sorted. The inherent twin of
    /// [`StoreSource::record_names`], so callers do not need the trait in
    /// scope for a plain listing.
    pub fn record_names_sorted(&self) -> Vec<String> {
        self.db.names()
    }

    /// Attach the server's [`MonitorRegistry`]. Called automatically by
    /// `PvaServer` through [`StoreSource::set_monitor_registry`].
    pub fn set_registry(&self, registry: Arc<MonitorRegistry>) {
        *self.registry.write().expect("registry lock poisoned") = Some(registry);
    }

    /// The attached registry, if this engine has been handed to a server.
    ///
    /// Returns a clone rather than a guard so a caller cannot hold the lock
    /// across an await.
    pub fn monitor_registry(&self) -> Option<Arc<MonitorRegistry>> {
        self.registry.read().expect("registry lock poisoned").clone()
    }

    /// Process every PINI record, in `.db` definition order.
    pub fn process_pini(&self) -> Vec<(String, NtPayload)> {
        let mut all = Vec::new();
        for &id in self.db.order() {
            let mut ctx = ProcCtx::new();
            // A single lock acquisition: check PINI and, if set, run the pass
            // under the same guard, so there is no window between the check
            // and the process for another pass to slip in.
            let processed = self.db.with_set(id.set, |set| {
                if !set.get(id).common.pini {
                    return None;
                }
                Some(process(set, id, &mut ctx))
            });
            if let Some(result) = processed {
                if let Err(e) = result {
                    tracing::warn!(target: "spvirit_ioc", "PINI processing failed: {e}");
                }
                all.extend(self.flush(&mut ctx));
            }
        }
        all
    }

    /// Publish a context's monitors to subscribers and return them for the
    /// caller. Runs with no lock set held.
    fn flush(&self, ctx: &mut ProcCtx) -> Vec<(String, NtPayload)> {
        for line in std::mem::take(&mut ctx.trace) {
            tracing::debug!(target: "spvirit_ioc", "{line}");
        }
        let events = ctx.take_events();
        let mut subs = self.subscribers.lock().expect("subscriber map poisoned");
        for (name, payload) in &events {
            let Some(senders) = subs.get_mut(name) else {
                continue;
            };
            // `try_send` fails both when the channel is full and when it is
            // closed. Either way the subscriber cannot keep up (or is gone),
            // and a full channel must never block a processing pass, so it
            // is dropped exactly like a closed one — the same rule
            // `SimplePvStore` applies to its own subscriber lists.
            senders.retain(|tx| tx.try_send(payload.clone()).is_ok());
        }
        events
    }

    /// Run `f` while holding `name`'s lock set. Exists so a test can prove a
    /// field claim does not contend for it.
    #[doc(hidden)]
    pub fn with_lock_set_for_test<R>(
        &self,
        name: &str,
        f: impl FnOnce(&mut crate::lockset::LockSetData) -> R,
    ) -> Option<R> {
        let id = self.db.lookup(name)?;
        Some(self.db.with_set(id.set, f))
    }

    /// Write `VAL` from host code and process, exactly as a client PUT would.
    ///
    /// Returns the monitors the pass produced, and publishes them to the
    /// server's registry if this engine has been handed one.
    ///
    /// The publication is the reason this method exists rather than callers
    /// using [`Source::put`] directly. `put` returns its events for the
    /// *handler* to publish (`handler.rs`, `notify_changed_records`); a write
    /// that starts here never goes through the handler, so nothing would
    /// publish them. `flush` — which `put` already ran — only feeds
    /// `subscribers`, which is populated exclusively by the PV-group
    /// machinery, so a plain `spmonitor` is not in it.
    ///
    /// An engine that has not been handed to a server still processes; it
    /// simply has nobody to publish to, which is correct for a bare engine
    /// under test.
    pub async fn set_value(
        &self,
        name: &str,
        value: DecodedValue,
    ) -> Result<Vec<(String, NtPayload)>, String> {
        let events = Source::put(self, name, &value).await?;
        // Clone the Arc out and drop the guard before awaiting: no lock in
        // this crate crosses an await.
        if let Some(registry) = self.monitor_registry() {
            for (pv, payload) in &events {
                registry.notify_monitors(pv, payload).await;
            }
        }
        Ok(events)
    }

    fn value_of(decoded: &DecodedValue) -> Result<Value, String> {
        match decoded {
            DecodedValue::Float64(v) => Ok(Value::Double(*v)),
            DecodedValue::Float32(v) => Ok(Value::Double(*v as f64)),
            DecodedValue::Int32(v) => Ok(Value::Long(*v)),
            DecodedValue::Int64(v) => Ok(Value::Long(*v as i32)),
            DecodedValue::UInt16(v) => Ok(Value::Long(*v as i32)),
            DecodedValue::Boolean(v) => Ok(Value::Long(i32::from(*v))),
            other => Err(format!("cannot write {other:?} to a record")),
        }
    }

    /// Render a put value as the string a text field (`SCAN`, `EVNT`) stores.
    /// A client writes these as strings; a non-string arrives rendered.
    fn decoded_as_string(value: &DecodedValue) -> String {
        match value {
            DecodedValue::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    /// Parse a put value as the `i32` `PHAS` takes. Accepts the integer wire
    /// types directly and a string (as a client-typed field write would send),
    /// rejecting anything that is not a whole number.
    fn decoded_as_i32(value: &DecodedValue) -> Result<i32, String> {
        match value {
            DecodedValue::Int32(v) => Ok(*v),
            DecodedValue::Int16(v) => Ok(*v as i32),
            DecodedValue::Int64(v) => Ok(*v as i32),
            DecodedValue::UInt16(v) => Ok(*v as i32),
            DecodedValue::String(s) => s
                .trim()
                .parse::<i32>()
                .map_err(|_| format!("PHAS: '{s}' is not an integer")),
            other => Err(format!("PHAS: cannot write {other:?}")),
        }
    }

    /// Route a write to a *field* PV. Only `SCAN`, `EVNT`, and `PHAS` are
    /// writable and reach the Scanner; every other field PV stays read-only.
    ///
    /// Called only once `resolve_field_info` has confirmed the name is a real
    /// field PV, so `parse_field_ref` and the base lookup both succeed for a
    /// modelled field — the fallbacks below are defensive, not reachable via
    /// the normal `put` entry.
    fn put_field_pv(
        &self,
        name: &str,
        value: &DecodedValue,
    ) -> Result<Vec<(String, NtPayload)>, String> {
        let field_ref = parse_field_ref(name).ok_or_else(|| format!("no record named '{name}'"))?;
        let id = self
            .db
            .lookup(&field_ref.base)
            .ok_or_else(|| format!("no record named '{name}'"))?;
        match field_ref.field.as_str() {
            "SCAN" => self.put_scan(id, value),
            "PHAS" => self.put_phas(id, value),
            "EVNT" => self.put_evnt(id, value),
            _ => Err(format!("field PV '{name}' is read-only")),
        }
    }

    /// Apply a `SCAN` write. An unparseable value is rejected before anything
    /// changes, so both `scan_raw` and list membership are left untouched. A
    /// valid value updates `scan_raw`, drops the record from every scan list,
    /// then re-adds it per the new [`ScanSpec`]. `IoIntr` is never auto-added
    /// (binding is explicit registration only, Task 11); `Passive` stays off
    /// every list.
    fn put_scan(
        &self,
        id: RecordId,
        value: &DecodedValue,
    ) -> Result<Vec<(String, NtPayload)>, String> {
        let raw = Self::decoded_as_string(value);
        // Parse first: an error here must leave membership unchanged.
        let spec = parse_scan(&raw).map_err(|e| e.to_string())?;
        // Take the lock only to read PHAS/EVNT and update scan_raw, then drop
        // it before any Scanner call — the Scanner takes its own list locks and
        // must never be called while the record lock set is held.
        let (phas, evnt) = self.db.with_set(id.set, |set| {
            let c = &mut set.get_mut(id).common;
            c.scan_raw = raw;
            (c.phas, c.evnt.clone())
        });
        self.scanner.remove_periodic(id);
        self.scanner.remove_event(id);
        self.scanner.unregister_io_intr(id);
        match spec {
            ScanSpec::Periodic(period) => self.scanner.add_periodic(id, phas, period),
            ScanSpec::Event => self.scanner.add_event(id, phas, evnt),
            // Explicit registration only (Task 11): a SCAN write never binds an
            // I/O-Intr record to a source.
            ScanSpec::IoIntr => {}
            // Off every list.
            ScanSpec::Passive => {}
        }
        Ok(Vec::new())
    }

    /// Apply a `PHAS` write. An unparseable value is rejected. On success the
    /// new priority is stored and the record is re-inserted into whichever
    /// scan list its current `SCAN` puts it on, so the list re-orders in place
    /// (`ScanList::insert` updates an existing member's PHAS). An I/O-Intr
    /// record cannot be re-ordered from here — its source key is unknown to
    /// the put path — so its PHAS is stored but its list order is left to the
    /// next explicit `register_io_intr`.
    fn put_phas(
        &self,
        id: RecordId,
        value: &DecodedValue,
    ) -> Result<Vec<(String, NtPayload)>, String> {
        let phas = Self::decoded_as_i32(value)?;
        let (spec, evnt) = self.db.with_set(id.set, |set| {
            let c = &mut set.get_mut(id).common;
            c.phas = phas;
            (parse_scan(&c.scan_raw), c.evnt.clone())
        });
        // A record whose stored SCAN no longer parses is on no list; nothing to
        // re-order.
        if let Ok(spec) = spec {
            match spec {
                ScanSpec::Periodic(period) => self.scanner.add_periodic(id, phas, period),
                ScanSpec::Event => self.scanner.add_event(id, phas, evnt),
                ScanSpec::IoIntr | ScanSpec::Passive => {}
            }
        }
        Ok(Vec::new())
    }

    /// Apply an `EVNT` write. The value is always stored. If the record's
    /// `SCAN` is `Event`, it is moved off its old event list and onto the one
    /// keyed by the new value.
    fn put_evnt(
        &self,
        id: RecordId,
        value: &DecodedValue,
    ) -> Result<Vec<(String, NtPayload)>, String> {
        let evnt = Self::decoded_as_string(value);
        let (spec, phas) = self.db.with_set(id.set, |set| {
            let c = &mut set.get_mut(id).common;
            c.evnt = evnt.clone();
            (parse_scan(&c.scan_raw), c.phas)
        });
        if let Ok(ScanSpec::Event) = spec {
            // `remove_event` clears the old key; `add_event` joins the new one.
            self.scanner.remove_event(id);
            self.scanner.add_event(id, phas, evnt);
        }
        Ok(Vec::new())
    }

    /// Reach the source's [`Scanner`] so a test can assert list membership
    /// after a put (membership is not observable from `Common`).
    #[cfg(test)]
    pub fn scanner_for_test(&self) -> &Arc<Scanner> {
        &self.scanner
    }
}

/// A placeholder [`ProcSink`] for the source-owned Scanner. The Scanner's real
/// egress (forwarding processed monitors into the server fan-out) is wired in
/// Task 15; until then it is never started, so this sink is never called.
struct NoopSink;

impl ProcSink for NoopSink {
    fn flush(&self, _events: Vec<(String, NtPayload)>, _trace: Vec<String>) {}
}

impl RecordFieldProvider for IocSource {
    fn field_value(
        &self,
        base: &str,
        field: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ScalarValue>> + Send + '_>> {
        let (base, field) = (base.to_string(), field.to_string());
        Box::pin(async move {
            let id = self.db.lookup(&base)?;
            let names = |target: &crate::lockset::RecordId| self.id_names.get(target).cloned();
            self.db
                .with_set(id.set, |set| record_field_value(set.get(id), &field, &names))
        })
    }

    /// Answers from the name-to-kind map and the static field table, so a
    /// channel search never contends for a lock set that a processing pass
    /// may be holding.
    fn field_descriptor(
        &self,
        base: &str,
        field: &str,
    ) -> Pin<Box<dyn Future<Output = Option<RecordFieldDesc>> + Send + '_>> {
        let (base, field) = (base.to_string(), field.to_string());
        Box::pin(async move {
            let kind = *self.kinds.get(&base)?;
            record_field_kind(kind, &field).map(|kind| RecordFieldDesc { kind })
        })
    }
}

impl Source for IocSource {
    /// The IOC engine self-notifies: record processing pushes updates into the
    /// monitor registry via `notify_monitors` (wired through
    /// `set_monitor_registry`), so the monitor handler must not also pump
    /// `subscribe` — doing so would double-deliver.
    fn pushes_own_updates(&self) -> bool {
        true
    }

    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            // A record name wins over a field reference: an exact record
            // called `A.B` is still that record, not `A`'s `B` field.
            let Some(id) = self.db.lookup(&name) else {
                return resolve_field_info(self, &name).await;
            };
            let payload = self.db.with_set(id.set, |set| set.get(id).to_payload());
            Some(PvInfo {
                descriptor: descriptor_for_payload(&payload),
                writable: true,
            })
        })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let Some(id) = self.db.lookup(&name) else {
                return resolve_field_payload(self, &name).await;
            };
            Some(self.db.with_set(id.set, |set| set.get(id).to_payload()))
        })
    }

    /// Write `VAL` and then process the record, returning every monitor the
    /// put caused.
    ///
    /// A put can produce *two* events for the same record. `write_field`
    /// posts the raw written value immediately, and processing then posts
    /// again if the body recomputes `VAL` — an input record whose `INP`
    /// names another record overwrites the put value from its link, so a
    /// subscriber sees the written value followed by the linked one. This is
    /// EPICS Base's behaviour, not an artifact: `dbPut` posts the field
    /// write before `dbProcess` runs, so the intermediate value goes on the
    /// wire there too. `a_put_to_a_linked_input_posts_the_written_value_then_the_linked_one`
    /// in `tests/source_integration.rs` pins the sequence.
    fn put(
        &self,
        name: &str,
        value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        let name = name.to_string();
        let value = value.clone();
        Box::pin(async move {
            let id = match self.db.lookup(&name) {
                Some(id) => id,
                // A field PV: SCAN/EVNT/PHAS are writable and route into the
                // Scanner; every other field PV stays read-only.
                None if resolve_field_info(self, &name).await.is_some() => {
                    return self.put_field_pv(&name, &value);
                }
                None => return Err(format!("no record named '{name}'")),
            };
            let parsed = Self::value_of(&value)?;
            let mut ctx = ProcCtx::new();
            self.db
                .with_set(id.set, |set| {
                    write_field(set, id, Field::Val, parsed, &mut ctx)?;
                    process(set, id, &mut ctx)
                })
                .map_err(|e| e.to_string())?;
            Ok(self.flush(&mut ctx))
        })
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            if self.db.lookup(&name).is_none() {
                let initial = resolve_field_payload(self, &name).await?;
                let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
                let _ = tx.try_send(initial);
                let mut field_subs = self
                    .field_subs
                    .lock()
                    .expect("field subscriber list poisoned");
                // Prune senders whose receiver has been dropped before pushing
                // the new one, so subscribe/disconnect churn cannot grow this
                // Vec without bound. `is_closed()` is the right test: the
                // channel only ever carries the single one-shot snapshot, so a
                // closed channel means the subscriber is gone — never merely
                // backpressured — and a `try_send` probe would be spurious.
                field_subs.retain(|tx| !tx.is_closed());
                field_subs.push(tx);
                return Some(rx);
            }
            let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
            self.subscribers
                .lock()
                .expect("subscriber map poisoned")
                .entry(name)
                .or_default()
                .push(tx);
            Some(rx)
        })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async move { self.db.names() })
    }
}

impl StoreSource for IocSource {
    /// `RecordDb::names` is already sorted, which the trait requires.
    fn record_names(&self) -> Vec<String> {
        self.db.names()
    }

    fn set_monitor_registry(&self, registry: Arc<MonitorRegistry>) {
        self.set_registry(registry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `tracing::Subscriber` that captures every event's
    /// formatted `message` field, so a test can assert on log content
    /// without pulling in `tracing-subscriber` as a dependency just for
    /// this one check.
    #[derive(Clone, Default)]
    struct CapturingSubscriber(Arc<Mutex<Vec<String>>>);

    struct MessageVisitor(String);
    impl tracing::field::Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    impl tracing::Subscriber for CapturingSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = MessageVisitor(String::new());
            event.record(&mut visitor);
            self.0.lock().expect("captured-message lock poisoned").push(visitor.0);
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    fn raw_with_field(name: &str, record_type: &str, field: &str, value: &str) -> DbRecord {
        let mut fields = HashMap::new();
        fields.insert(field.to_string(), value.to_string());
        DbRecord { name: name.to_string(), record_type: record_type.to_string(), fields }
    }

    /// The `Debug` impl is opaque by design (see the doc comment on it), but
    /// it must still name the type rather than rendering as nothing — a
    /// caller matching `{:?}` output against a broken record, or a test
    /// using `expect_err`, needs to see *something* identifiable.
    #[test]
    fn debug_names_the_type() {
        let source = IocSource::from_raw(vec![raw_with_field("X", "ai", "EGU", "C")])
            .expect("must build");
        let rendered = format!("{source:?}");
        assert!(rendered.contains("IocSource"), "got: {rendered}");
    }

    /// Ruling 3: a field the engine doesn't model is accepted, carried, and
    /// warned about exactly once at load — never silently. Assert the
    /// warning actually fires and actually names the field, not just that
    /// *some* event happens (load also emits an unrelated graph-report
    /// warning for a record nothing ever processes, which must not be
    /// mistaken for this one).
    #[test]
    fn an_unmodelled_field_is_warned_about_by_name_at_load() {
        let messages: Arc<Mutex<Vec<String>>> = Arc::default();
        let subscriber = CapturingSubscriber(messages.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let _source = IocSource::from_raw(vec![raw_with_field("X", "ai", "DRVH", "100")])
            .expect("must build");

        let msgs = messages.lock().expect("captured-message lock poisoned");
        assert!(
            msgs.iter().any(|m| m.contains("DRVH") && m.contains("does not model")),
            "expected a warning naming the unmodelled DRVH field, got: {msgs:?}"
        );
    }

    /// `record_names_sorted` is the inherent listing a host calls without
    /// pulling `StoreSource` into scope; it must be every record's real
    /// name, sorted — not an empty, placeholder, or made-up list.
    #[test]
    fn record_names_sorted_lists_every_real_name_in_order() {
        let source = IocSource::from_raw(vec![
            raw_with_field("Z:PV", "ai", "EGU", "C"),
            raw_with_field("A:PV", "ai", "EGU", "C"),
        ])
        .expect("must build");
        assert_eq!(source.record_names_sorted(), vec!["A:PV".to_string(), "Z:PV".to_string()]);
    }

    /// Item 3a: repeated subscribe/disconnect churn on a field PV must keep the
    /// `field_subs` Vec bounded. The Vec used to be append-only — one entry per
    /// subscribe, never pruned — so churn grew it without limit. Pruning senders
    /// whose receiver has been dropped (`is_closed()`) on each subscribe holds
    /// it at the count of *live* subscriptions, which here is one at a time.
    #[tokio::test]
    async fn field_subscription_churn_keeps_field_subs_bounded() {
        let source = IocSource::from_raw(vec![raw_with_field("PV:A", "ai", "EGU", "C")])
            .expect("must build");
        for _ in 0..100 {
            let rx = source
                .subscribe("PV:A.EGU")
                .await
                .expect("a field PV is subscribable");
            // Dropping the receiver closes the channel; the next subscribe must
            // prune this now-dead sender rather than accumulate it.
            drop(rx);
        }
        let len = source
            .field_subs
            .lock()
            .expect("field subscriber list poisoned")
            .len();
        assert!(
            len <= 1,
            "field_subs must stay bounded under churn (an append-only Vec would \
             hold 100 here), got {len}"
        );
    }

    // ----- Task 12: writable SCAN/EVNT/PHAS routed into the Scanner -----

    use std::time::Duration;

    fn s(v: &str) -> DecodedValue {
        DecodedValue::String(v.to_string())
    }

    #[tokio::test]
    async fn writing_scan_moves_the_record_between_lists() {
        let source =
            IocSource::from_raw(vec![raw_with_field("PV:A", "ai", "EGU", "C")]).expect("build");
        let id = source.db.lookup("PV:A").expect("record exists");
        // Passive by default: on no periodic list.
        assert!(source
            .scanner_for_test()
            .periodic_members(Duration::from_secs(1))
            .is_empty());
        Source::put(&source, "PV:A.SCAN", &s("1 second"))
            .await
            .expect("SCAN put succeeds");
        assert_eq!(
            source.scanner_for_test().periodic_members(Duration::from_secs(1)),
            vec![id],
            "the record must now be on the 1 Hz periodic list"
        );
    }

    #[tokio::test]
    async fn writing_passive_takes_the_record_off_every_list() {
        let source =
            IocSource::from_raw(vec![raw_with_field("PV:A", "ai", "EGU", "C")]).expect("build");
        let id = source.db.lookup("PV:A").expect("record exists");
        Source::put(&source, "PV:A.SCAN", &s("1 second")).await.expect("put");
        assert_eq!(
            source.scanner_for_test().periodic_members(Duration::from_secs(1)),
            vec![id]
        );
        Source::put(&source, "PV:A.SCAN", &s("Passive")).await.expect("put");
        assert!(
            source
                .scanner_for_test()
                .periodic_members(Duration::from_secs(1))
                .is_empty(),
            "Passive must take the record off the periodic list"
        );
    }

    #[tokio::test]
    async fn unparseable_scan_is_rejected_and_membership_unchanged() {
        let source =
            IocSource::from_raw(vec![raw_with_field("PV:A", "ai", "EGU", "C")]).expect("build");
        let id = source.db.lookup("PV:A").expect("record exists");
        Source::put(&source, "PV:A.SCAN", &s("1 second")).await.expect("put");
        let err = Source::put(&source, "PV:A.SCAN", &s("banana"))
            .await
            .expect_err("an unparseable SCAN must be rejected");
        assert!(err.contains("banana"), "error must name the bad value, got: {err}");
        // Membership is untouched: still on the 1 Hz list.
        assert_eq!(
            source.scanner_for_test().periodic_members(Duration::from_secs(1)),
            vec![id]
        );
        // And scan_raw is untouched too.
        assert_eq!(
            source.field_value("PV:A", "SCAN").await,
            Some(ScalarValue::Str("1 second".into())),
            "a rejected SCAN put must not have altered scan_raw"
        );
    }

    #[tokio::test]
    async fn writing_phas_reorders_the_list() {
        let source = IocSource::from_raw(vec![
            raw_with_field("PV:A", "ai", "EGU", "C"),
            raw_with_field("PV:B", "ai", "EGU", "C"),
        ])
        .expect("build");
        let a = source.db.lookup("PV:A").expect("A");
        let b = source.db.lookup("PV:B").expect("B");
        Source::put(&source, "PV:A.SCAN", &s("1 second")).await.expect("put");
        Source::put(&source, "PV:B.SCAN", &s("1 second")).await.expect("put");
        // Both PHAS 0: insertion order A, B.
        assert_eq!(
            source.scanner_for_test().periodic_members(Duration::from_secs(1)),
            vec![a, b]
        );
        // Raise A's PHAS: B (still 0) must now sort ahead of A.
        Source::put(&source, "PV:A.PHAS", &DecodedValue::Int32(5)).await.expect("put");
        assert_eq!(
            source.scanner_for_test().periodic_members(Duration::from_secs(1)),
            vec![b, a],
            "a PHAS write must re-order the list in place"
        );
        // And the stored PHAS reads back.
        assert_eq!(source.field_value("PV:A", "PHAS").await, Some(ScalarValue::I32(5)));
    }

    #[tokio::test]
    async fn unparseable_phas_is_rejected_and_membership_unchanged() {
        let source =
            IocSource::from_raw(vec![raw_with_field("PV:A", "ai", "EGU", "C")]).expect("build");
        let id = source.db.lookup("PV:A").expect("record exists");
        Source::put(&source, "PV:A.SCAN", &s("1 second")).await.expect("put");
        let err = Source::put(&source, "PV:A.PHAS", &s("not-a-number"))
            .await
            .expect_err("an unparseable PHAS must be rejected");
        assert!(!err.is_empty(), "must return a non-empty error");
        assert_eq!(
            source.scanner_for_test().periodic_members(Duration::from_secs(1)),
            vec![id],
            "a rejected PHAS put must not change membership"
        );
        assert_eq!(source.field_value("PV:A", "PHAS").await, Some(ScalarValue::I32(0)));
    }

    #[tokio::test]
    async fn writing_evnt_moves_the_event_list_membership() {
        let source =
            IocSource::from_raw(vec![raw_with_field("PV:A", "ai", "EGU", "C")]).expect("build");
        let id = source.db.lookup("PV:A").expect("record exists");
        Source::put(&source, "PV:A.EVNT", &s("a")).await.expect("put");
        Source::put(&source, "PV:A.SCAN", &s("Event")).await.expect("put");
        assert_eq!(source.scanner_for_test().event_members("a"), vec![id]);
        assert!(source.scanner_for_test().event_members("b").is_empty());
        // Move the record to a new event list by re-writing EVNT.
        Source::put(&source, "PV:A.EVNT", &s("b")).await.expect("put");
        assert!(
            source.scanner_for_test().event_members("a").is_empty(),
            "the record must leave its old event list"
        );
        assert_eq!(
            source.scanner_for_test().event_members("b"),
            vec![id],
            "the record must join the new event list"
        );
    }

    #[tokio::test]
    async fn writing_evnt_does_not_join_a_list_when_scan_is_not_event() {
        // A Passive record's EVNT is stored but joins no event list.
        let source =
            IocSource::from_raw(vec![raw_with_field("PV:A", "ai", "EGU", "C")]).expect("build");
        Source::put(&source, "PV:A.EVNT", &s("a")).await.expect("put");
        assert!(source.scanner_for_test().event_members("a").is_empty());
        assert_eq!(source.field_value("PV:A", "EVNT").await, Some(ScalarValue::Str("a".into())));
    }

    #[tokio::test]
    async fn evnt_field_reads_back_the_stored_value() {
        // Loaded from the .db, then updated by a put.
        let source =
            IocSource::from_raw(vec![raw_with_field("PV:A", "ai", "EVNT", "seven")]).expect("build");
        assert_eq!(
            source.field_value("PV:A", "EVNT").await,
            Some(ScalarValue::Str("seven".into())),
            "EVNT must be read back from the .db-loaded value"
        );
        Source::put(&source, "PV:A.EVNT", &s("eight")).await.expect("put");
        assert_eq!(
            source.field_value("PV:A", "EVNT").await,
            Some(ScalarValue::Str("eight".into())),
            "EVNT must reflect the written value"
        );
    }

    #[tokio::test]
    async fn a_non_writable_field_pv_is_still_read_only() {
        let source =
            IocSource::from_raw(vec![raw_with_field("PV:A", "ai", "EGU", "C")]).expect("build");
        let err = Source::put(&source, "PV:A.EGU", &s("V"))
            .await
            .expect_err("EGU must stay read-only");
        assert!(err.contains("read-only"), "got: {err}");
    }

    /// An unmodelled field on a real record is not a field PV at all: the
    /// `resolve_field_info` guard in `put` must reject it as "no record named"
    /// rather than falling into the field-PV path and mislabelling it
    /// read-only. Kills the mutant that replaces that match guard with `true`.
    #[tokio::test]
    async fn an_unmodelled_field_is_not_treated_as_a_field_pv() {
        let source =
            IocSource::from_raw(vec![raw_with_field("PV:A", "ai", "EGU", "C")]).expect("build");
        let err = Source::put(&source, "PV:A.NOTAFIELD", &s("x"))
            .await
            .expect_err("an unmodelled field must be rejected");
        assert!(
            err.contains("no record named"),
            "an unmodelled field must not be treated as a (read-only) field PV, got: {err}"
        );
    }
}
