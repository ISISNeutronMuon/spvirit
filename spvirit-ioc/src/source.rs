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
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::db::{DbRecord, load_db_records, parse_db_records};
use spvirit_server::field_provider::{
    RecordFieldDesc, RecordFieldProvider, resolve_field_info, resolve_field_payload,
};
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_server::simple_store::descriptor_for_payload;
use spvirit_types::{NtPayload, ScalarValue};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use tokio::sync::mpsc;

/// Depth of a subscriber's monitor channel. Matches `SimplePvStore`'s
/// `mpsc::channel(64)` in `Source::subscribe`.
const SUBSCRIBER_QUEUE: usize = 64;

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
    /// tier 2.
    field_subs: Mutex<Vec<mpsc::Sender<NtPayload>>>,
}

impl IocSource {
    pub fn from_db_file(path: &str) -> Result<IocSource, String> {
        let raw = load_db_records(path, &HashMap::new()).map_err(|e| e.to_string())?;
        Self::from_raw(raw)
    }

    pub fn from_db_str(content: &str) -> Result<IocSource, String> {
        let raw =
            parse_db_records(content, "<memory>", &HashMap::new()).map_err(|e| e.to_string())?;
        Self::from_raw(raw)
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
        };
        // Report the load-time diagnostics once, here, rather than on every
        // pass. None of them is fatal.
        for line in source.graph().report() {
            tracing::warn!(target: "spvirit_ioc", "{line}");
        }
        Ok(source)
    }

    pub fn graph(&self) -> DependencyGraph {
        self.db.dependency_graph()
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
