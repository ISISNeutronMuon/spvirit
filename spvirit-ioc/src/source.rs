//! The engine behind the `Source` trait.
//!
//! Everything the engine does is synchronous and happens under a lock set's
//! mutex; this module is the only place that touches async. A `put` runs the
//! pass, drops the lock, and hands the accumulated monitors back to the
//! server, which is exactly the contract `Source::put` already has.

use crate::build::build_records;
use crate::ctx::ProcCtx;
use crate::fields::{record_field_kind, record_field_value};
use crate::graph::DependencyGraph;
use crate::lockset::RecordDb;
use crate::model::{Field, Value};
use crate::process::{process, write_field};
use crate::spec::{RecordSpec, unmodelled_fields};
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::db::{DbRecord, load_db_records, parse_db_records};
use spvirit_server::field_provider::{
    RecordFieldDesc, RecordFieldProvider, resolve_field_info, resolve_field_payload,
};
use spvirit_server::monitor::MonitorRegistry;
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
    db: RecordDb,
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
    /// tier 2 (`SimplePvStore`).
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
        let db = RecordDb::build(records);
        let id_names = db
            .names()
            .into_iter()
            .filter_map(|name| db.lookup(&name).map(|id| (id, name)))
            .collect::<HashMap<_, _>>();
        let source = IocSource {
            db,
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
                None if resolve_field_info(self, &name).await.is_some() => {
                    return Err(format!("field PV '{name}' is read-only"));
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
                self.field_subs
                    .lock()
                    .expect("field subscriber list poisoned")
                    .push(tx);
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
}
