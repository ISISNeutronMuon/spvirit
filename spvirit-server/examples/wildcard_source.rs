//! # Wildcard / Scratch-Pad Source
//!
//! A dynamic source that auto-creates PVs when they are first written.
//! Any PV whose name starts with `XYZ:` is claimed by this source.
//! On the first PUT the PV springs into existence; on subsequent GETs
//! the last-written value is returned.
//!
//! This pattern is useful for:
//! - Scratch-pad / mailbox PVs created by operators at runtime
//! - Auto-provisioning of device PVs based on naming conventions
//! - Testing and prototyping where a fixed PV list is inconvenient
//!
//! Run with:
//! ```sh
//! cargo run --example wildcard_source
//! ```
//! Then try:
//! ```sh
//! # Write creates the PV on the fly:
//! cargo run --bin spput -- XYZ:MyValue 42.0
//! cargo run --bin spget -- XYZ:MyValue
//!
//! # Any name under the XYZ: prefix works:
//! cargo run --bin spput -- XYZ:sensor/temp 22.5
//! cargo run --bin spget -- XYZ:sensor/temp
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spvirit_codec::spvd_decode::{DecodedValue, FieldDesc, FieldType, StructureDesc, TypeCode};
use spvirit_server::PvaServer;
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_types::{NtPayload, NtScalar, ScalarValue};
use tokio::sync::{RwLock, mpsc};

// ─── WildcardSource ──────────────────────────────────────────────────────────

/// A source that dynamically creates PVs matching a name prefix.
///
/// PVs are created lazily — `claim()` succeeds for any name starting with
/// the configured prefix, and values are stored in memory on first PUT.
struct WildcardSource {
    prefix: String,
    pvs: Arc<RwLock<HashMap<String, ScalarValue>>>,
    subscribers: Arc<RwLock<HashMap<String, Vec<mpsc::Sender<NtPayload>>>>>,
}

impl WildcardSource {
    fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            pvs: Arc::new(RwLock::new(HashMap::new())),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn matches(&self, name: &str) -> bool {
        name.starts_with(&self.prefix)
    }
}

/// Build a minimal NTScalar:1.0 descriptor for F64 PVs.
fn f64_scalar_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("epics:nt/NTScalar:1.0".to_string()),
        fields: vec![FieldDesc {
            name: "value".to_string(),
            field_type: FieldType::Scalar(TypeCode::Float64),
        }],
    }
}

impl Source for WildcardSource {
    // ANCHOR: claim
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            if !self.matches(&name) {
                return None;
            }
            // Every wildcard PV is writable and uses F64
            Some(PvInfo {
                descriptor: f64_scalar_desc(),
                writable: true,
            })
        })
    }
    // ANCHOR_END: claim

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            if !self.matches(&name) {
                return None;
            }
            let pvs = self.pvs.read().await;
            let val = pvs.get(&name).cloned().unwrap_or(ScalarValue::F64(0.0));
            Some(NtPayload::Scalar(NtScalar::from_value(val)))
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
            if !self.matches(&name) {
                return Err(format!("PV '{}' not in wildcard range", name));
            }

            let new_val = match &value {
                DecodedValue::Float64(v) => ScalarValue::F64(*v),
                DecodedValue::Int32(v) => ScalarValue::F64(*v as f64),
                DecodedValue::Structure(fields) => fields
                    .iter()
                    .find(|(k, _)| k == "value")
                    .and_then(|(_, v)| match v {
                        DecodedValue::Float64(f) => Some(ScalarValue::F64(*f)),
                        DecodedValue::Int32(i) => Some(ScalarValue::F64(*i as f64)),
                        _ => None,
                    })
                    .unwrap_or(ScalarValue::F64(0.0)),
                _ => return Err("unsupported value type".to_string()),
            };

            let created = {
                let mut pvs = self.pvs.write().await;
                let is_new = !pvs.contains_key(&name);
                pvs.insert(name.clone(), new_val.clone());
                is_new
            };

            if created {
                println!("[wildcard] created PV '{}'", name);
            } else {
                println!("[wildcard] updated PV '{}' = {:?}", name, new_val);
            }

            let payload = NtPayload::Scalar(NtScalar::from_value(new_val));

            // Notify subscribers.
            //
            // NOTE for anyone copying this template: distinguish the two
            // `try_send` failures. `Full` means the receiver is alive and
            // merely behind — drop the *update*, never the sender. `Closed`
            // means the receiver is genuinely gone, so the sender can never
            // deliver again and is pruned. Dropping a sender on `Full` would
            // close the stream, and for a source the server pumps (this one
            // keeps the default `pushes_own_updates() == false`) a closed
            // stream MEANS "this PV is dead": one episode of ordinary
            // backpressure would send DESTROY_CHANNEL to every subscriber.
            let mut subs = self.subscribers.write().await;
            if let Some(senders) = subs.get_mut(&name) {
                senders.retain(|tx| match tx.try_send(payload.clone()) {
                    Ok(()) => true,
                    Err(mpsc::error::TrySendError::Full(_)) => true,
                    Err(mpsc::error::TrySendError::Closed(_)) => false,
                });
            }

            Ok(vec![(name, payload)])
        })
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            if !self.matches(&name) {
                return None;
            }
            let (tx, rx) = mpsc::channel(64);
            self.subscribers
                .write()
                .await
                .entry(name)
                .or_default()
                .push(tx);
            Some(rx)
        })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async move {
            let pvs = self.pvs.read().await;
            pvs.keys().cloned().collect()
        })
    }
}

// ─── main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = PvaServer::builder()
        .ai("STATIC:HEARTBEAT", 0.0)
        // Wildcard source at order 10 — checked after built-in records
        .source("wildcard", 10, Arc::new(WildcardSource::new("XYZ:")))
        .build();

    let store = server.store().clone();
    tokio::spawn(async move {
        let mut tick = 0u64;
        loop {
            store
                .set_value("STATIC:HEARTBEAT", ScalarValue::F64(tick as f64))
                .await;
            tick += 1;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });

    println!("Wildcard source server running on port 5075");
    println!("  Built-in PV:  STATIC:HEARTBEAT");
    println!("  Wildcard PVs: XYZ:<anything> (created on first PUT)");
    println!();
    println!("Try: spput XYZ:MyValue 42.0");
    println!("     spget XYZ:MyValue");

    server.run().await
}
