//! # JSON File-Backed Source
//!
//! A source that persists PV values to a JSON file on disk.  Values
//! survive server restarts — the file is read at startup and written
//! on every PUT.
//!
//! This pattern is useful for:
//! - Persisting operator setpoints across IOC reboots
//! - Simple key/value stores that don't need a full database
//! - Save/restore of configuration PVs
//!
//! Run with:
//! ```sh
//! cargo run --example json_source
//! ```
//! Then try:
//! ```sh
//! cargo run --bin spput  -- JSON:SETPOINT_A 123.4
//! cargo run --bin spput  -- JSON:SETPOINT_B 567.8
//! cargo run --bin spget  -- JSON:SETPOINT_A JSON:SETPOINT_B
//!
//! # Restart the server — values are preserved:
//! cargo run --example json_source
//! cargo run --bin spget  -- JSON:SETPOINT_A
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use spvirit_codec::spvd_decode::{DecodedValue, FieldDesc, FieldType, StructureDesc, TypeCode};
use spvirit_server::PvaServer;
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_types::{NtPayload, NtScalar, ScalarValue};
use tokio::sync::{RwLock, mpsc};

// ─── JsonSource ──────────────────────────────────────────────────────────────

/// A source that persists PV values as a JSON file.
struct JsonSource {
    path: PathBuf,
    pvs: Arc<RwLock<HashMap<String, f64>>>,
}

impl JsonSource {
    /// Load (or create) the JSON file and return the source.
    fn load(path: impl AsRef<Path>, defaults: &[(&str, f64)]) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut pvs: HashMap<String, f64> = if path.exists() {
            let contents = std::fs::read_to_string(&path).unwrap_or_default();
            serde_json::from_str(&contents).unwrap_or_default()
        } else {
            HashMap::new()
        };

        // Fill in any defaults that aren't already persisted
        for &(name, val) in defaults {
            pvs.entry(name.to_string()).or_insert(val);
        }

        // Persist the merged state
        if let Ok(json) = serde_json::to_string_pretty(&pvs) {
            let _ = std::fs::write(&path, json);
        }

        println!(
            "[json_source] loaded {} PVs from {}",
            pvs.len(),
            path.display()
        );
        Self {
            path,
            pvs: Arc::new(RwLock::new(pvs)),
        }
    }

    /// Write the current PV map back to disk.
    async fn persist(&self) {
        let pvs = self.pvs.read().await;
        if let Ok(json) = serde_json::to_string_pretty(&*pvs) {
            if let Err(e) = std::fs::write(&self.path, json) {
                eprintln!("[json_source] failed to persist: {e}");
            }
        }
    }
}

fn f64_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("epics:nt/NTScalar:1.0".to_string()),
        fields: vec![FieldDesc {
            name: "value".to_string(),
            field_type: FieldType::Scalar(TypeCode::Float64),
        }],
    }
}

// ANCHOR: impl
impl Source for JsonSource {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            if self.pvs.read().await.contains_key(&name) {
                Some(PvInfo {
                    descriptor: f64_desc(),
                    writable: true,
                })
            } else {
                None
            }
        })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let pvs = self.pvs.read().await;
            let val = *pvs.get(&name)?;
            Some(NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(
                val,
            ))))
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
            let new_val = match &value {
                DecodedValue::Float64(v) => *v,
                DecodedValue::Int32(v) => *v as f64,
                DecodedValue::Structure(fields) => fields
                    .iter()
                    .find(|(k, _)| k == "value")
                    .and_then(|(_, v)| match v {
                        DecodedValue::Float64(f) => Some(*f),
                        DecodedValue::Int32(i) => Some(*i as f64),
                        _ => None,
                    })
                    .ok_or("missing numeric 'value' field")?,
                _ => return Err("unsupported value type".to_string()),
            };

            {
                let mut pvs = self.pvs.write().await;
                if !pvs.contains_key(&name) {
                    return Err(format!("PV '{}' not found in JSON store", name));
                }
                pvs.insert(name.clone(), new_val);
            }

            // Persist to disk after every write
            self.persist().await;
            println!("[json_source] persisted {} = {}", name, new_val);

            let payload = NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(new_val)));
            Ok(vec![(name, payload)])
        })
    }
    // ANCHOR_END: impl

    fn subscribe(
        &self,
        _name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        // File-backed source does not support push updates.
        // Clients use GET (polling) to read the latest persisted value.
        Box::pin(async { None })
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
    let json_src = JsonSource::load(
        "pvstore.json",
        &[
            ("JSON:SETPOINT_A", 0.0),
            ("JSON:SETPOINT_B", 0.0),
            ("JSON:LIMIT_HI", 100.0),
            ("JSON:LIMIT_LO", -100.0),
        ],
    );

    let server = PvaServer::builder()
        .ai("SIM:HEARTBEAT", 0.0)
        .source("json", -10, Arc::new(json_src))
        .build();

    let store = server.store().clone();
    tokio::spawn(async move {
        let mut tick = 0u64;
        loop {
            store
                .set_value("SIM:HEARTBEAT", ScalarValue::F64(tick as f64))
                .await;
            tick += 1;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });

    println!("JSON file-backed source server running on port 5075");
    println!("  Persistent PVs: JSON:SETPOINT_A, JSON:SETPOINT_B, JSON:LIMIT_HI, JSON:LIMIT_LO");
    println!("  In-memory PV:   SIM:HEARTBEAT");
    println!("  Storage file:   pvstore.json");
    println!();
    println!("Values survive server restarts.");

    server.run().await
}
