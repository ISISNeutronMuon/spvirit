use std::time::Duration;

use spvirit_server::PvaServer;
use spvirit_types::ScalarValue;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: records
    let server = PvaServer::builder()
        .ai("SIM:TEMPERATURE", 22.5)
        .ao("SIM:SETPOINT", 25.0)
        .bo("SIM:ENABLE", false)
        .build();
    // ANCHOR_END: records

    // First-order smoothing: SIM:TEMPERATURE tracks SIM:SETPOINT when
    // SIM:ENABLE is true.  Each tick moves 10 % of the way toward the
    // setpoint, imitating a real thermal system.
    let store = server.store().clone();
    tokio::spawn(async move {
        let alpha = 0.10; // smoothing factor
        let mut interval = tokio::time::interval(Duration::from_millis(200));
        loop {
            interval.tick().await;

            let enabled = matches!(
                store.get_value("SIM:ENABLE").await,
                Some(ScalarValue::Bool(true))
            );
            if !enabled {
                continue;
            }

            let sp = match store.get_value("SIM:SETPOINT").await {
                Some(ScalarValue::F64(v)) => v,
                _ => continue,
            };
            let temp = match store.get_value("SIM:TEMPERATURE").await {
                Some(ScalarValue::F64(v)) => v,
                _ => continue,
            };

            // Exponential first-order filter: T_new = T + α·(SP − T)
            let new_temp = temp + alpha * (sp - temp);
            store
                .set_value("SIM:TEMPERATURE", ScalarValue::F64(new_temp))
                .await;
        }
    });

    server.run().await
}
