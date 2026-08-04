//! Example: serving an EPICS `.db` file.
//!
//! The same file `spserver --db` takes can be loaded into a `PvaServer`,
//! so a database written for a real IOC serves unchanged.
//!
//! Try it:
//!   cargo run -p spvirit-server --example db_file
//!
//! Then from another terminal:
//!   splist
//!   spget DEMO:TEMP
//!   spput DEMO:TEMP 46      # >= HIHI -> MAJOR
//!   spget DEMO:TEMP

use spvirit_server::PvaServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: load
    let server = PvaServer::builder()
        .db_file("spvirit-server/examples/example.db")
        // .db LOW/HIGH/LOLO/HIHI are only evaluated when this is on.
        .compute_alarms(true)
        .build();
    // ANCHOR_END: load

    server.run().await
}
