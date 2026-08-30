//! Task 4: `ClientRegistry` wired into the connection lifecycle.
//!
//! A downstream TCP client that completes `ConnectionValidation` with a `ca`
//! user must show up in the injected `ClientRegistry`'s snapshot (peer +
//! user), and must disappear from it once the connection drops.
//!
//! Modelled on `tests/segmented_put.rs`'s harness, but drives the accept loop
//! through the public `run_tcp_server` (rather than hand-rolling
//! `handle_connection` per accepted socket) so the connection runs inside the
//! same `request_ctx::scope(peer, ...)` wrapping production traffic gets —
//! that's how the connect hook recovers the peer address.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::{
    encode_client_connection_validation, encode_create_channel_request,
};
use spvirit_server::PvaServer;
use spvirit_server::diag::ClientRegistry;
use spvirit_server::handler::{PvListMode, ServerState, rand_guid, run_tcp_server};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::SourceRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PV: &str = "CLIENTREG:TARGET";
const VERSION: u8 = 2;
const IS_BE: bool = false;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Bind a listener on an ephemeral port and serve `PV`, with `client_registry`
/// installed on the `MonitorRegistry` before `ServerState` is built (mirrors
/// how `PvaServer::resolved_monitor_registry` installs it in production).
async fn spawn_server(client_registry: Arc<ClientRegistry>) -> SocketAddr {
    let server = PvaServer::builder().ai(PV, 1.0).build();
    let store = server.store().clone();

    let sources = Arc::new(SourceRegistry::new());
    sources.add("builtin", 0, store.clone()).await;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");

    let mon_registry = Arc::new(MonitorRegistry::new());
    mon_registry.set_client_registry(client_registry);

    let state = Arc::new(ServerState::new(
        sources,
        mon_registry,
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

async fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 8];
    tokio::time::timeout(IO_TIMEOUT, stream.read_exact(&mut header))
        .await
        .expect("timeout reading header")
        .expect("read header");

    let parsed = PvaHeader::new(&header);
    let payload_len = if parsed.flags.is_control {
        0
    } else {
        parsed.payload_length as usize
    };

    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        tokio::time::timeout(IO_TIMEOUT, stream.read_exact(&mut payload))
            .await
            .expect("timeout reading payload")
            .expect("read payload");
    }

    let mut full = header.to_vec();
    full.extend_from_slice(&payload);
    full
}

async fn read_until<F>(stream: &mut TcpStream, mut accept: F) -> PvaPacketCommand
where
    F: FnMut(&PvaPacketCommand) -> bool,
{
    for _ in 0..32 {
        let raw = read_frame(stream).await;
        let mut pkt = PvaPacket::new(&raw);
        if let Some(cmd) = pkt.decode_payload()
            && accept(&cmd)
        {
            return cmd;
        }
    }
    panic!("expected command never arrived");
}

/// Complete the handshake with a `ca` user/host, then create a channel (so
/// the connection stays open past validation and the server has done real
/// work on it) and return the open socket plus the address the client used
/// to connect (matched against the registry's recorded peer).
async fn handshake_as(addr: SocketAddr, user: &str, host: &str) -> (TcpStream, SocketAddr) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let local_addr = stream.local_addr().expect("local addr");

    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidation(_))
    })
    .await;

    let validation =
        encode_client_connection_validation(16_384, 512, 0, "ca", user, host, VERSION, IS_BE);
    stream
        .write_all(&validation)
        .await
        .expect("write validation");
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidated(_))
    })
    .await;

    let cid = 1u32;
    stream
        .write_all(&encode_create_channel_request(cid, PV, VERSION, IS_BE))
        .await
        .expect("write create channel");
    read_until(
        &mut stream,
        |cmd| matches!(cmd, PvaPacketCommand::CreateChannel(p) if p.is_server && p.cid == cid),
    )
    .await;

    (stream, local_addr)
}

#[tokio::test]
async fn connect_identity_and_disconnect_are_tracked_in_the_registry() {
    let registry = Arc::new(ClientRegistry::new());
    let addr = spawn_server(registry.clone()).await;

    let (stream, client_addr) = handshake_as(addr, "alice", "ws-42").await;

    // Give the connect/identity hooks a moment to land (they run inline in
    // the handler task, but the test's own accept happens on a separate
    // task).
    let mut snap = Vec::new();
    for _ in 0..50 {
        snap = registry.snapshot();
        if !snap.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(snap.len(), 1, "expected exactly one tracked connection");
    let entry = &snap[0];
    assert_eq!(entry.peer, client_addr, "tracked peer must match the client's address");
    assert_eq!(entry.user.as_deref(), Some("alice"));
    assert_eq!(entry.host.as_deref(), Some("ws-42"));

    // Drop the client; the server's read loop should notice the closed
    // socket and clean up.
    drop(stream);

    let mut cleaned = false;
    for _ in 0..50 {
        if registry.snapshot().is_empty() {
            cleaned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(cleaned, "registry entry must be removed after disconnect");
}
