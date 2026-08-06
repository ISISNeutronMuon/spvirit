//! Example: the raw-NT level — building payloads by hand.
//!
//! Everywhere else in the book you set a *value* on a record and the server
//! fills in the rest. Here you build the whole `NtScalar` yourself — units,
//! display limits, precision, alarm severity, timestamp — and hand it to
//! `store.put_nt()`. That is the escape hatch for gateways, bridges, and any
//! PV whose metadata changes from update to update.
//!
//! Try it:
//!   cargo run -p spvirit-server --example nt_put_get
//!
//! Then from another terminal:
//!   spget SIM:TEMP
//!   spinfo SIM:TEMP
//!   spmonitor SIM:SPECTRUM

use std::time::{SystemTime, UNIX_EPOCH};

use spvirit_server::PvaServer;
use spvirit_types::{NtPayload, NtScalar, NtScalarArray, ScalarArrayValue, ScalarValue};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = PvaServer::builder()
        .ao("SIM:TEMP", 22.5)
        .waveform("SIM:SPECTRUM", ScalarArrayValue::F64(vec![0.0; 8]))
        .build();

    let store = server.store().clone();

    tokio::spawn(async move {
        let mut tick = 0u64;
        loop {
            let t = tick as f64;
            let temp = 22.0 + (t * 0.2).sin();

            // ANCHOR: putget
            // Lower-level NT write (scalar) with custom alarm logic.
            let temp_nt = make_temp_nt_with_custom_alarm(temp);
            store.put_nt("SIM:TEMP", NtPayload::Scalar(temp_nt)).await;

            // Lower-level NT write (array).
            let samples = (0..8)
                .map(|i| (t * 0.15 + i as f64 * 0.4).sin())
                .collect::<Vec<_>>();
            let array_nt = NtScalarArray::from_value(ScalarArrayValue::F64(samples));
            store
                .put_nt("SIM:SPECTRUM", NtPayload::ScalarArray(array_nt))
                .await;

            // Lower-level NT read: the whole payload, not just the value.
            if let Some(snapshot) = store.get_nt("SIM:TEMP").await {
                println!("SIM:TEMP => {snapshot:?}");
            }
            // ANCHOR_END: putget

            tick += 1;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });

    server.run().await
}

// ANCHOR: builder
/// Build a complete `NtScalar` from scratch: value, metadata, alarm, time.
fn make_temp_nt_with_custom_alarm(temp: f64) -> NtScalar {
    // Custom severity mapping. Nothing computes this for you at the raw-NT
    // level — a payload you build by hand is NO_ALARM until you say otherwise.
    // 0 = NO_ALARM, 1 = MINOR, 2 = MAJOR; status is example-only tagging.
    let (severity, status, message) = if temp >= 22.9 {
        (2, 3, "custom HIHI")
    } else if temp >= 22.7 {
        (1, 1, "custom HIGH")
    } else if temp <= 21.1 {
        (2, 5, "custom LOLO")
    } else if temp <= 21.3 {
        (1, 4, "custom LOW")
    } else {
        (0, 0, "custom OK")
    };

    // The builders are chained on the owned value and each returns `Self`.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut nt = NtScalar::from_value(ScalarValue::F64(temp))
        .with_units("degC".to_string())
        .with_description(format!("Simulated temperature ({temp:.2} degC)"))
        .with_precision(2)
        .with_limits(20.5, 23.5)
        // An explicit timestamp is honoured verbatim; leave it unset and the
        // encoder stamps at encode time instead.
        .with_timestamp(now.as_secs() as i64, now.subsec_nanos() as i32);

    // Anything without a builder is a plain public field.
    nt.alarm_severity = severity;
    nt.alarm_status = status;
    nt.alarm_message = message.to_string();
    nt
}
// ANCHOR_END: builder
