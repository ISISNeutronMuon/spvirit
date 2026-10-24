//! Site 1, end to end: an unauthenticated peer declares a payload larger than
//! the server's inbound cap. The server must drop the connection promptly
//! instead of reserving the buffer and then waiting for bytes that will never
//! arrive.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use spvirit_server::PvaServer;
use spvirit_server::handler::{
    MAX_INBOUND_PAYLOAD_BYTES, PvListMode, ServerState, rand_guid, run_tcp_server,
};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::SourceRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PV: &str = "OVERSIZE:TARGET";
const IO_TIMEOUT: Duration = Duration::from_secs(5);

async fn spawn_server() -> SocketAddr {
    let server = PvaServer::builder().ao(PV, 1.0).build();
    let store = server.store().clone();

    let sources = Arc::new(SourceRegistry::new());
    sources.add("builtin", 0, store.clone()).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    let state = Arc::new(ServerState::new(
        sources,
        Arc::new(MonitorRegistry::new()),
        false,
        PvListMode::Off,
        0,
        None,
        rand_guid(),
        addr.port(),
        None,
        IpAddr::V4(Ipv4Addr::LOCALHOST),
    ));

    tokio::spawn(async move {
        let _ = run_tcp_server(state, listener, IO_TIMEOUT).await;
    });

    addr
}

#[tokio::test]
async fn a_frame_declaring_more_than_the_cap_closes_the_connection() {
    let addr = spawn_server().await;
    let mut stream = TcpStream::connect(addr).await.expect("connect");

    // An application frame (not control) declaring one byte more than the
    // cap, and nothing after the header. Pre-fix the server reserves the
    // buffer and blocks; post-fix it logs and drops the connection.
    let declared = (MAX_INBOUND_PAYLOAD_BYTES + 1) as u32;
    let mut frame = vec![0xCA, 0x02, 0x00, 0x00];
    frame.extend_from_slice(&declared.to_le_bytes());
    stream.write_all(&frame).await.expect("write header");
    stream.flush().await.expect("flush");

    // Drain until EOF. The server writes its own greeting first, so read in a
    // loop rather than expecting an immediate zero.
    let mut buf = [0u8; 4096];
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match stream.read(&mut buf).await {
                Ok(0) => return true,
                Ok(_) => continue,
                Err(_) => return true,
            }
        }
    })
    .await;

    assert_eq!(
        closed,
        Ok(true),
        "the server neither closed the connection nor errored: it is sitting \
         on a reserved buffer waiting for bytes that will never arrive"
    );
}
