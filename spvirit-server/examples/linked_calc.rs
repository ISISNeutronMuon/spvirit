//! Example: linked PVs with automatic recomputation.
//!
//! Demonstrates `.link()` — whenever an input PV changes, the output PV
//! is recomputed automatically. Monitor clients on the output PV see
//! updates without any extra code.
//!
//! PVs:
//!   CALC:A      (ao) — writable input A
//!   CALC:B      (ao) — writable input B
//!   CALC:SUM    (ai) — A + B  (auto-computed)
//!   CALC:PROD   (ai) — A × B  (auto-computed)
//!   CALC:MEAN   (ai) — (A + B) / 2  (auto-computed)
//!
//! Try it:
//!   cargo run -p spvirit-server --example linked_calc
//!
//! Then from another terminal:
//!   pvput CALC:A 10
//!   pvput CALC:B 3
//!   pvget CALC:SUM      # → 13
//!   pvget CALC:PROD     # → 30
//!   pvget CALC:MEAN     # → 6.5
//!   pvmonitor CALC:SUM  # live updates whenever A or B changes

use spvirit_server::PvaServer;
use spvirit_types::ScalarValue;

fn f64_of(v: &ScalarValue) -> f64 {
    match v {
        ScalarValue::F64(x) => *x,
        _ => 0.0,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: links
    let server = PvaServer::builder()
        // Writable inputs
        .ao("CALC:A", 0.0)
        .ao("CALC:B", 0.0)
        // Computed outputs (read-only)
        .ai("CALC:SUM", 0.0)
        .ai("CALC:PROD", 0.0)
        .ai("CALC:MEAN", 0.0)
        // Links: recomputed whenever CALC:A or CALC:B changes
        .link("CALC:SUM", &["CALC:A", "CALC:B"], |v| {
            ScalarValue::F64(f64_of(&v[0]) + f64_of(&v[1]))
        })
        .link("CALC:PROD", &["CALC:A", "CALC:B"], |v| {
            ScalarValue::F64(f64_of(&v[0]) * f64_of(&v[1]))
        })
        .link("CALC:MEAN", &["CALC:A", "CALC:B"], |v| {
            ScalarValue::F64((f64_of(&v[0]) + f64_of(&v[1])) / 2.0)
        })
        .build();
    // ANCHOR_END: links

    server.run().await
}
