//! Example: a complete soft IOC.
//!
//! Pulls together everything in Part III — metadata, deadbands, validation,
//! periodic scanning, computed PVs, arrays, and explicit alarms — into one
//! server that behaves like a small piece of real beamline equipment.
//!
//! Try it:
//!   cargo run -p spvirit-server --example complete_ioc
//!
//! Then from another terminal:
//!   splist
//!   spget VAC:PRESSURE
//!   spput VAC:SETPOINT 5e-7      # accepted
//!   spput VAC:SETPOINT 1.0       # rejected - outside range
//!   spmonitor VAC:PRESSURE

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use spvirit_server::{Pv, PvArray, PvaServer};
use spvirit_types::ScalarArrayValue;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: build
    // --- Readback: scanned, with units and a monitor deadband -----------
    let tick = Arc::new(AtomicU64::new(0));
    let t = tick.clone();

    let pressure = Pv::ai("VAC:PRESSURE", 1.0e-6)
        .units("mbar")
        .prec(3)
        .desc("Chamber pressure")
        .mdel(1.0e-8) // suppress sub-nanobar jitter
        .scan(Duration::from_millis(500), move |_pv| {
            let n = t.fetch_add(1, Ordering::Relaxed) as f64;
            // A decaying pump-down curve with a little noise.
            1.0e-6 * (-n / 40.0).exp() + 1.0e-9 * (n * 1.7).sin()
        });

    // --- Setpoint: validated on write -----------------------------------
    let setpoint = Pv::ao("VAC:SETPOINT", 1.0e-6)
        .units("mbar")
        .prec(3)
        .desc("Target pressure")
        .on_put(|pv, value: f64| {
            // Drive limits are advisory, so enforce the range here.
            if !(1.0e-9..=1.0e-3).contains(&value) {
                return Err(format!("{}: {value} outside 1e-9..1e-3", pv.name()));
            }
            println!("{} -> {value:e}", pv.name());
            Ok(())
        });

    // --- Derived: recomputed whenever an input moves ---------------------
    let error = Pv::calc("VAC:ERROR", &[&pressure, &setpoint], |inputs: &[f64]| {
        inputs[0] - inputs[1]
    })
    .units("mbar")
    .desc("Readback minus setpoint");

    // --- Array: a spectrum a client can read but not write ---------------
    let spectrum = PvArray::aai("VAC:RGA", ScalarArrayValue::F64(vec![0.0; 64]));

    // --- Status: severity we set ourselves -------------------------------
    let status = Pv::ai("VAC:LINK", 0.0).desc("Gauge controller link");

    let server = PvaServer::serve([
        pressure.clone(),
        setpoint.clone(),
        error.clone(),
        status.clone(),
    ])
    .pvs([spectrum.clone()])
    .build()
    .await;
    // ANCHOR_END: build

    // ANCHOR: drive
    // Everything above is declarative. Anything else you want the IOC to do
    // is an ordinary task driving the handles.
    let spec = spectrum.clone();
    tokio::spawn(async move {
        let mut frame = 0u64;
        loop {
            let data: Vec<f64> = (0..64)
                .map(|i| ((i as f64) * 0.2 + frame as f64 * 0.1).sin().abs())
                .collect();
            let _ = spec.set(ScalarArrayValue::F64(data)).await;
            frame += 1;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    });

    // The gauge controller is reachable, so clear the alarm explicitly.
    status.set_alarm(0, 0, "").await?;
    // ANCHOR_END: drive

    println!("complete_ioc running - try `splist`");
    server.run().await
}
