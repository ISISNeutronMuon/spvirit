//! Nagle's algorithm plus the peer's delayed ACK puts a ~40 ms (Linux) to
//! ~200 ms (Windows) floor under every sub-MSS response. PVA frames are tens
//! of bytes, so that floor is the round-trip latency. Every accepted
//! connection must have TCP_NODELAY set.

use std::path::Path;
use std::time::Duration;

use spvirit_server::handler::disable_nagle;
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn disable_nagle_sets_the_option_on_the_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let client = tokio::spawn(async move { TcpStream::connect(addr).await });

    let (accepted, _peer) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("accept did not time out")
        .expect("accept");

    assert!(
        !accepted.nodelay().expect("read nodelay"),
        "precondition: a freshly accepted socket has Nagle enabled, so the \
         assertion below is proving the helper did something"
    );

    disable_nagle(&accepted, 1);

    assert!(
        accepted.nodelay().expect("read nodelay"),
        "disable_nagle must set TCP_NODELAY on the accepted socket"
    );

    let _ = client.await.expect("client task").expect("connect");
}

/// A helper nobody calls fixes nothing. `handle_connection` is the single place
/// that owns an accepted `TcpStream` (it consumes it with `into_split`), so the
/// call must appear there, before the split.
#[test]
fn handle_connection_disables_nagle_before_splitting_the_stream() {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/handler.rs"),
    )
    .expect("handler.rs must be readable");

    let call = src
        .find("disable_nagle(&stream")
        .expect("handle_connection must call disable_nagle(&stream, conn_id)");
    let split = src
        .find("stream.into_split()")
        .expect("handle_connection must still split the stream");

    assert!(
        call < split,
        "disable_nagle must be called while the whole TcpStream is still in \
         hand — after into_split() the socket is no longer reachable as a \
         TcpStream"
    );
}
