//! The engine behind the `Source` trait.
//!
//! Everything the engine does is synchronous and happens under a lock set's
//! mutex; this module is the only place that touches async. A `put` runs the
//! pass, drops the lock, and hands the accumulated monitors back to the
//! server, which is exactly the contract `Source::put` already has.

use crate::build::build_records;
use crate::ctx::ProcCtx;
use crate::graph::DependencyGraph;
use crate::lockset::RecordDb;
use crate::model::{Field, Value};
use crate::process::{process, write_field};
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::db::{DbRecord, load_db_records, parse_db_records};
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_server::simple_store::descriptor_for_payload;
use spvirit_types::NtPayload;
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
    subscribers: Mutex<HashMap<String, Vec<mpsc::Sender<NtPayload>>>>,
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
        let db = RecordDb::build(records);
        let source = IocSource {
            db,
            subscribers: Mutex::new(HashMap::new()),
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
            let is_pini = self.db.with_set(id.set, |set| set.get(id).common.pini);
            if !is_pini {
                continue;
            }
            let mut ctx = ProcCtx::new();
            let result = self.db.with_set(id.set, |set| process(set, id, &mut ctx));
            if let Err(e) = result {
                tracing::warn!(target: "spvirit_ioc", "PINI processing failed: {e}");
            }
            all.extend(self.flush(&mut ctx));
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

impl Source for IocSource {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let id = self.db.lookup(&name)?;
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
            let id = self.db.lookup(&name)?;
            Some(self.db.with_set(id.set, |set| set.get(id).to_payload()))
        })
    }

    fn put(
        &self,
        name: &str,
        value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        let name = name.to_string();
        let value = value.clone();
        Box::pin(async move {
            let id = self
                .db
                .lookup(&name)
                .ok_or_else(|| format!("no record named '{name}'"))?;
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
            self.db.lookup(&name)?;
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
