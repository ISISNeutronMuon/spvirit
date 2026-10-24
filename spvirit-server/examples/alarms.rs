//! Example: alarm severity.
//!
//! `.alarm_limits(lolo, low, high, hihi)` publishes the NTScalar
//! `valueAlarm` block and, with `.compute_alarms(true)`, the server derives
//! the severity from them on every write: MINOR past `low`/`high`, MAJOR
//! past `lolo`/`hihi`. See the Alarms chapter.
//!
//! For alarms that do not follow from the value, use `set_alarm`.
//!
//! Try it:
//!   cargo run -p spvirit-server --example alarms
//!
//! Then from another terminal:
//!   spget SIM:LINK        # INVALID, "device unreachable"
//!   spput SIM:PRESSURE 95 # MINOR, HIGH

use spvirit_server::{Pv, PvaServer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: limits
    let pressure = Pv::ao("SIM:PRESSURE", 50.0)
        .units("bar")
        .desc("Vessel pressure")
        // lolo, low, high, hihi — published, and evaluated on every write.
        .alarm_limits(5.0, 10.0, 90.0, 110.0);

    let link = Pv::ai("SIM:LINK", 0.0).desc("Device link health");

    let server = PvaServer::serve([pressure.clone(), link.clone()])
        .compute_alarms(true)
        .build()
        .await;
    // ANCHOR_END: limits

    // ANCHOR: manual
    // Alarms you decide yourself, independent of the value.
    // severity: 0=NONE 1=MINOR 2=MAJOR 3=INVALID
    link.set_alarm(3, 17, "device unreachable").await?;
    // ANCHOR_END: manual

    server.run().await
}
