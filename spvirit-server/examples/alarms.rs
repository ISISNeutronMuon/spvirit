//! Example: alarm severity.
//!
//! `.alarm_limits(lolo, low, high, hihi)` fills in — and publishes — the
//! NTScalar `valueAlarm` block, but the server does *not* evaluate values
//! against it, even with `.compute_alarms(true)`. Only limits loaded from a
//! `.db` file (LOW/HIGH/LOLO/HIHI) are evaluated. See the Alarms chapter.
//!
//! For handle-built PVs, set severity yourself with `set_alarm`.
//!
//! Try it:
//!   cargo run -p spvirit-server --example alarms
//!
//! Then from another terminal:
//!   spget SIM:LINK        # INVALID, "device unreachable"
//!   spget SIM:PRESSURE    # limits published, severity 0

use spvirit_server::{Pv, PvaServer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: limits
    let pressure = Pv::ao("SIM:PRESSURE", 50.0)
        .units("bar")
        .desc("Vessel pressure")
        // lolo, low, high, hihi — published to clients, not evaluated.
        .alarm_limits(5.0, 10.0, 90.0, 110.0);

    let link = Pv::ai("SIM:LINK", 0.0).desc("Device link health");

    let server = PvaServer::serve([pressure.clone(), link.clone()])
        .compute_alarms(true)
        .build()
        .await;
    // ANCHOR_END: limits

    // ANCHOR: manual
    // Alarms you decide yourself, independent of the value. This is the
    // route that works for handle-built PVs.
    // severity: 0=NONE 1=MINOR 2=MAJOR 3=INVALID
    link.set_alarm(3, 17, "device unreachable").await?;
    // ANCHOR_END: manual

    server.run().await
}
