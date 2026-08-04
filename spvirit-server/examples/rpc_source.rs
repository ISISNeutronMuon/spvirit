//! # RPC Source Example
//!
//! A custom source that exposes an RPC channel named `RPC:add`.
//! Clients send a structure with two numeric fields (`a` and `b`),
//! and the server responds with their sum.
//!
//! Run with:
//! ```sh
//! cargo run --example rpc_source
//! ```
//! Then call the RPC from a PVA client, e.g. with pvxs or p4p:
//! ```python
//! from p4p.client.thread import Context
//! ctx = Context('pva')
//! result = ctx.rpc('RPC:add', {'a': 3.0, 'b': 4.0})
//! print(result)  # 7.0
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spvirit_codec::spvd_decode::{DecodedValue, FieldDesc, FieldType, StructureDesc, TypeCode};
use spvirit_server::PvaServer;
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_types::{NtPayload, NtScalar, ScalarValue};
use tokio::sync::mpsc;

// ─── RpcAddSource ────────────────────────────────────────────────────────────

/// A source that claims the `RPC:add` channel and implements an adder RPC.
struct RpcAddSource;

fn f64_scalar_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("epics:nt/NTScalar:1.0".to_string()),
        fields: vec![FieldDesc {
            name: "value".to_string(),
            field_type: FieldType::Scalar(TypeCode::Float64),
        }],
    }
}

/// Extract a numeric field from a decoded structure.
fn extract_f64(args: &DecodedValue, field: &str) -> Option<f64> {
    match args {
        DecodedValue::Structure(fields) => {
            fields
                .iter()
                .find(|(k, _)| k == field)
                .and_then(|(_, v)| match v {
                    DecodedValue::Float64(f) => Some(*f),
                    DecodedValue::Float32(f) => Some(*f as f64),
                    DecodedValue::Int32(i) => Some(*i as f64),
                    DecodedValue::Int64(i) => Some(*i as f64),
                    DecodedValue::UInt32(i) => Some(*i as f64),
                    _ => None,
                })
        }
        _ => None,
    }
}

impl Source for RpcAddSource {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            if name == "RPC:add" {
                Some(PvInfo {
                    descriptor: f64_scalar_desc(),
                    writable: false,
                })
            } else {
                None
            }
        })
    }

    fn get(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        Box::pin(async { None })
    }

    fn put(
        &self,
        _name: &str,
        _value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        Box::pin(async { Err("PUT not supported on RPC channel".to_string()) })
    }

    fn subscribe(
        &self,
        _name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        Box::pin(async { None })
    }

    // ANCHOR: rpc
    fn rpc(
        &self,
        name: &str,
        args: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<NtPayload, String>> + Send + '_>> {
        let name = name.to_string();
        let args = args.clone();
        Box::pin(async move {
            if name != "RPC:add" {
                return Err(format!("unknown RPC channel '{}'", name));
            }

            let a = extract_f64(&args, "a").unwrap_or(0.0);
            let b = extract_f64(&args, "b").unwrap_or(0.0);
            let sum = a + b;
            println!("[rpc] {a} + {b} = {sum}");

            Ok(NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(
                sum,
            ))))
        })
    }
    // ANCHOR_END: rpc

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async { vec!["RPC:add".to_string()] })
    }
}

// ─── main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = PvaServer::builder()
        .source("rpc-add", 10, Arc::new(RpcAddSource))
        .build();

    println!("RPC server running — channel 'RPC:add' available");
    println!("Send an RPC request with fields 'a' and 'b' to get their sum.");
    server.run().await
}
