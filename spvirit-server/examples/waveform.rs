use spvirit_server::PvaServer;
use spvirit_types::ScalarArrayValue;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: serve
    let server = PvaServer::builder()
        .waveform("SIM:SPECTRUM", ScalarArrayValue::F64(vec![0.0; 1024]))
        .build();
    // ANCHOR_END: serve

    let store = server.store().clone();

    // ANCHOR: update
    tokio::spawn(async move {
        const N: usize = 1024;
        let mut tick = 0u64;
        loop {
            let phase = (tick as f64) * 0.03;
            let samples = (0..N)
                .map(|i| {
                    let x = i as f64;
                    (phase + x * 0.02).sin() + 0.25 * (phase * 0.5 + x * 0.05).cos()
                })
                .collect::<Vec<_>>();
            store
                .set_array_value("SIM:SPECTRUM", ScalarArrayValue::F64(samples))
                .await;
            tick += 1;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });
    // ANCHOR_END: update

    server.run().await
}
