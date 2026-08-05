//! Example: a fully described scalar PV.
//!
//! Engineering units, precision, description, alarm limits, drive limits
//! and a monitor deadband — all of it set through typed `Pv<T>` handles,
//! which is the only way to set them in code. The `PvaServer::builder()`
//! record methods take a name and an initial value and nothing else.
//!
//! Also demonstrates picking the NTScalar wire type explicitly via
//! `Pv::<ScalarValue>::scalar_in`/`scalar_out`, for the eight types
//! (`byte`/`short`/`ubyte`/`ushort`/`uint`/`ulong`, plus explicit
//! `float`/`bool`/`long`/`string`) the fixed-type constructors
//! (`Pv::ai`/`ao`/`bi`/`bo`/`longin`/`longout`/`string_in`/`string_out`)
//! don't reach.
//!
//! Try it:
//!   cargo run -p spvirit-server --example scalar_metadata
//!
//! Then from another terminal:
//!   spget SIM:TEMPERATURE      # value, units and precision
//!   spget SIM:TEMPERATURE.MDEL # 0.5 — MDEL lands in the record's fields
//!   spput SIM:SETPOINT 30      # accepted
//!   spput SIM:SETPOINT 500     # ALSO accepted — drive limits are
//!                              # advertised to clients, not enforced by
//!                              # the server. Reject out-of-range writes
//!                              # yourself with `.on_put(...)`.
//!   spinfo SIM:GAIN            # wire type: ushort
//!   spinfo SIM:STATUS          # wire type: byte

use spvirit_server::{AnyPv, Pv, PvaServer};
use spvirit_types::ScalarValue;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: meta
    let temperature = Pv::ai("SIM:TEMPERATURE", 22.5)
        .units("degC")
        .prec(2)
        .desc("Sample block temperature")
        // lolo, low, high, hihi — MAJOR outside the outer pair,
        // MINOR outside the inner pair.
        .alarm_limits(0.0, 15.0, 30.0, 40.0)
        // Monitors stay quiet for changes smaller than this.
        .mdel(0.5);

    let setpoint = Pv::ao("SIM:SETPOINT", 25.0)
        .units("degC")
        .prec(1)
        .desc("Demanded temperature")
        .drive_limits(0.0, 100.0);

    // ANCHOR: types
    // `scalar_out`/`scalar_in` pick the wire type from the `ScalarValue`
    // variant of `initial` — not from Rust's `u16`/`u8`, which have no
    // native `Pv<T>` handle of their own. This is the route to the eight
    // NTScalar types `ai`/`ao`/`bi`/`bo`/`longin`/`longout`/`string_in`/
    // `string_out` don't cover.
    let gain = Pv::<ScalarValue>::scalar_out("SIM:GAIN", ScalarValue::U16(1));
    let status = Pv::<ScalarValue>::scalar_in("SIM:STATUS", ScalarValue::U8(0));
    // ANCHOR_END: types

    let server = PvaServer::serve([
        AnyPv::from(temperature.clone()),
        AnyPv::from(setpoint.clone()),
        AnyPv::from(gain.clone()),
        AnyPv::from(status.clone()),
    ])
    .build()
    .await;
    // ANCHOR_END: meta

    server.run().await
}
