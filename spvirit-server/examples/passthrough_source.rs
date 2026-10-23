//! # Passthrough / Middleware Source
//!
//! A source that wraps another source and adds cross-cutting concerns:
//! access control, logging, rate limiting, or value transformation.
//! This is the **decorator pattern** applied to PV providers.
//!
//! In this example `AccessControlSource` wraps an inner source and:
//! - Logs every GET, PUT, and subscribe operation
//! - Enforces a simple allow-list for PUT operations
//! - Tracks per-PV access counts
//!
//! This pattern is useful for:
//! - Adding authentication / authorization in front of any source
//! - Auditing and logging PV access
//! - Rate limiting or throttling
//! - Value clamping or unit conversion on reads/writes
//!
//! Run with:
//! ```sh
//! cargo run --example passthrough_source
//! ```
//! Then try:
//! ```sh
//! cargo run --bin spget  -- CTRL:SETPOINT
//! cargo run --bin spput  -- CTRL:SETPOINT 42.0     # allowed — on the allow-list
//! cargo run --bin spput  -- CTRL:READBACK 99.0     # denied  — not on the allow-list
//! cargo run --bin spget  -- CTRL:ACCESS_COUNT      # see how many operations were logged
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spvirit_codec::spvd_decode::{DecodedValue, FieldDesc, FieldType, StructureDesc, TypeCode};
use spvirit_server::PvaServer;
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_types::{NtPayload, NtScalar, ScalarValue};
use tokio::sync::{RwLock, mpsc};

// ─── AccessControlSource ─────────────────────────────────────────────────────

/// A decorator source that wraps an inner [`Source`] and adds access control
/// and logging.
struct AccessControlSource {
    inner: Arc<dyn Source>,
    /// PV names that are allowed to receive PUT operations.
    put_allow_list: Vec<String>,
    /// Per-PV access counters.
    access_counts: Arc<RwLock<HashMap<String, AtomicU64>>>,
}

impl AccessControlSource {
    fn new(inner: Arc<dyn Source>, put_allow_list: Vec<String>) -> Self {
        Self {
            inner,
            put_allow_list,
            access_counts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn bump(&self, name: &str) {
        let counts = self.access_counts.read().await;
        if let Some(counter) = counts.get(name) {
            counter.fetch_add(1, Ordering::Relaxed);
        } else {
            drop(counts);
            let mut counts = self.access_counts.write().await;
            counts
                .entry(name.to_string())
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn total_accesses(&self) -> u64 {
        let counts = self.access_counts.read().await;
        counts.values().map(|c| c.load(Ordering::Relaxed)).sum()
    }
}

impl Source for AccessControlSource {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            // Special virtual PV: access count
            if name == "CTRL:ACCESS_COUNT" {
                return Some(PvInfo {
                    descriptor: StructureDesc {
                        struct_id: Some("epics:nt/NTScalar:1.0".to_string()),
                        fields: vec![FieldDesc {
                            name: "value".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Float64),
                        }],
                    },
                    writable: false,
                });
            }

            // Delegate to the inner source, but override writability
            // based on the allow-list.
            let mut info = self.inner.claim(&name).await?;
            if info.writable && !self.put_allow_list.contains(&name) {
                info.writable = false; // hide writability from unauthorized PVs
            }
            Some(info)
        })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            self.bump(&name).await;

            // Virtual PV: total access count
            if name == "CTRL:ACCESS_COUNT" {
                let total = self.total_accesses().await;
                return Some(NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(
                    total as f64,
                ))));
            }

            println!("[acl] GET '{}'", name);
            self.inner.get(&name).await
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
            self.bump(&name).await;

            // Enforce the allow-list
            if !self.put_allow_list.contains(&name) {
                println!("[acl] PUT '{}' -> DENIED", name);
                return Err(format!(
                    "access denied: '{}' is not on the PUT allow-list",
                    name
                ));
            }

            println!("[acl] PUT '{}' -> ALLOWED", name);
            self.inner.put(&name, &value).await
        })
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            self.bump(&name).await;
            println!("[acl] SUBSCRIBE '{}'", name);
            self.inner.subscribe(&name).await
        })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async move {
            let mut names = self.inner.names().await;
            names.push("CTRL:ACCESS_COUNT".to_string());
            names.sort();
            names
        })
    }
}

// ─── main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Build the server with some base PVs.
    let mut server = PvaServer::builder()
        .ao("CTRL:SETPOINT", 0.0)
        .ai("CTRL:READBACK", 0.0)
        .ai("CTRL:STATUS", 0.0)
        .build();

    let store = server.store().clone();

    // Wrap the store in an access-control layer:
    //   Only CTRL:SETPOINT is allowed to be written.
    //   CTRL:READBACK and CTRL:STATUS are read-only through the ACL even
    //   though CTRL:READBACK might be writable at the store level.
    let acl = Arc::new(AccessControlSource::new(
        store.clone(),
        vec!["CTRL:SETPOINT".to_string()],
    ));

    // Replace the built-in store (order 0) with the ACL layer at order -1
    // so the ACL is checked first and intercepts all operations.
    server.add_source("acl", -1, acl);

    // Background: simulate readback tracking the setpoint
    tokio::spawn(async move {
        loop {
            if let Some(sp) = store.get_value("CTRL:SETPOINT").await
                && let ScalarValue::F64(target) = sp
                && let Some(ScalarValue::F64(current)) = store.get_value("CTRL:READBACK").await
            {
                // First-order exponential approach
                let next = current + 0.1 * (target - current);
                store
                    .set_value("CTRL:READBACK", ScalarValue::F64(next))
                    .await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });

    println!("Passthrough / access-control source server running on port 5075");
    println!("  Writable (allowed):  CTRL:SETPOINT");
    println!("  Read-only (denied):  CTRL:READBACK, CTRL:STATUS");
    println!("  Virtual PV:          CTRL:ACCESS_COUNT");
    println!();
    println!("Try: spput CTRL:SETPOINT 42.0     # succeeds");
    println!("     spput CTRL:READBACK 99.0     # access denied");

    server.run().await
}
