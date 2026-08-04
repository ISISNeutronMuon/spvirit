//! Example: rejecting a client write.
//!
//! The builder's `.on_put(name, f)` takes `Fn(&str, &DecodedValue)` — it
//! returns `()` and cannot refuse a write. The typed-handle form takes
//! `Fn(&Pv<T>, T) -> Result<(), String>`, and `Err(msg)` rejects the PUT
//! on the wire. Use handles when you need validation.
//!
//! Try it:
//!   cargo run -p spvirit-server --example on_put_reject
//!
//! Then from another terminal:
//!   spput SIM:SETPOINT 30      # OK
//!   spput SIM:SETPOINT 500     # rejected — value stays 30
//!   spget SIM:SETPOINT

use spvirit_server::{Pv, PvaServer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: reject
    let setpoint = Pv::ao("SIM:SETPOINT", 25.0)
        .units("degC")
        .drive_limits(0.0, 100.0)
        .on_put(|pv, value: f64| {
            if !(0.0..=100.0).contains(&value) {
                // Err rejects the PUT; the client's put() fails.
                return Err(format!("{} outside 0..100: {value}", pv.name()));
            }
            println!("{} accepted {value}", pv.name());
            Ok(())
        });

    let server = PvaServer::serve([setpoint.clone()]).build().await;
    // ANCHOR_END: reject

    server.run().await
}
