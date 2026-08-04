//! Demonstrates registering multiple `Source` providers with different
//! priorities via the `.source()` builder method.
//!
//! Two custom sources are shown:
//!
//! * `ConstSource` — a high-priority source (order -10) that serves a
//!   handful of read-only PVs with constant values. Because its order is
//!   negative it is checked *before* the built-in SimplePvStore.
//!
//! * `ComputedSource` — a low-priority source (order 10) that
//!   computes values on each GET. It is checked *after* the built-in store.
//!
//! The built-in SimplePvStore (from `.ai()` / `.ao()` etc.) sits at order 0
//! in the middle.
//!
//! Run with:
//! ```sh
//! cargo run --example multi_source
//! ```
//! Then try:
//! ```sh
//! cargo run --bin spget -- CONST:PI CONST:E SIM:COUNTER COMPUTED:TIME
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::PvaServer;
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_types::{NtPayload, NtScalar, ScalarValue};

use spvirit_server::simple_store::descriptor_for_payload;
use tokio::sync::mpsc;

// ─── ConstSource ─────────────────────────────────────────────────────────────

/// A source that serves a fixed set of read-only constants.
struct ConstSource {
    pvs: Vec<(&'static str, f64)>,
}

impl ConstSource {
    fn new() -> Self {
        Self {
            pvs: vec![
                ("CONST:PI", std::f64::consts::PI),
                ("CONST:E", std::f64::consts::E),
            ],
        }
    }

    fn lookup(&self, name: &str) -> Option<f64> {
        self.pvs.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }
}

impl Source for ConstSource {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let val = self.lookup(&name)?;
            let payload = NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(val)));
            Some(PvInfo {
                descriptor: descriptor_for_payload(&payload),
                writable: false,
            })
        })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let val = self.lookup(&name)?;
            Some(NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(
                val,
            ))))
        })
    }

    fn put(
        &self,
        _name: &str,
        _value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        Box::pin(async { Err("read-only source".to_string()) })
    }

    fn subscribe(
        &self,
        _name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        Box::pin(async { None })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async move { self.pvs.iter().map(|(n, _)| n.to_string()).collect() })
    }
}

// ─── ComputedSource ──────────────────────────────────────────────────────────

/// A source that computes values on the fly — e.g. wall-clock time.
struct ComputedSource;

impl Source for ComputedSource {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            if name == "COMPUTED:TIME" {
                let payload = NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(0.0)));
                Some(PvInfo {
                    descriptor: descriptor_for_payload(&payload),
                    writable: false,
                })
            } else {
                None
            }
        })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            if name == "COMPUTED:TIME" {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64();
                Some(NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(
                    now,
                ))))
            } else {
                None
            }
        })
    }

    fn put(
        &self,
        _name: &str,
        _value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        Box::pin(async { Err("read-only source".to_string()) })
    }

    fn subscribe(
        &self,
        _name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        Box::pin(async { None })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async { vec!["COMPUTED:TIME".to_string()] })
    }
}

// ─── main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // tracing_subscriber::fmt::init();

    let counter = Arc::new(AtomicU64::new(0));
    let c = counter.clone();

    // ANCHOR: register
    let server = PvaServer::builder()
        .ai("SIM:COUNTER", 0.0)
        // ConstSource at order -10 — checked before the built-in store
        .source("constants", -10, Arc::new(ConstSource::new()))
        // ComputedSource at order 10 — checked after the built-in store
        .source("computed", 10, Arc::new(ComputedSource))
        .build();
    // ANCHOR_END: register

    let store = server.store().clone();

    tokio::spawn(async move {
        loop {
            let n = c.fetch_add(1, Ordering::Relaxed);
            store
                .set_value("SIM:COUNTER", ScalarValue::F64(n as f64))
                .await;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });

    println!("multi_source server running — PVs: CONST:PI, CONST:E, SIM:COUNTER, COMPUTED:TIME");
    server.run().await
}
