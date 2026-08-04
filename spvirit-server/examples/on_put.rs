use spvirit_server::PvaServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: callback
    let server = PvaServer::builder()
        .ao("SIM:SETPOINT", 25.0)
        .on_put("SIM:SETPOINT", |pv, val| {
            println!("{pv} was set to {val:?}");
        })
        .build();
    // ANCHOR_END: callback

    server.run().await
}
