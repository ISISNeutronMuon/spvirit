//! Task 6: each accepted connection's `ChannelTables` is registered with the
//! `MonitorRegistry` and unregistered when the connection ends.
//!
//! The unit tests in `monitor.rs` cover the registry's half of this (the map,
//! the accessors, the removal in `cleanup_connection`). This one covers the
//! *wiring*: that `handle_connection` actually hands the registry the very
//! handle the connection task mutates, so a channel created downstream is
//! visible through `MonitorRegistry::channel_tables` — which is how Task 7's
//! upstream-death teardown will find the channel it has to destroy.
//!
//! Harness modelled on `tests/client_registry_lifecycle.rs`: it drives the
//! public `run_tcp_server`, whose connection ids start at 1, so the single
//! connection this test makes is deterministically conn_id 1.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::{
    encode_client_connection_validation, encode_create_channel_request,
};
use spvirit_server::PvaServer;
use spvirit_server::handler::{PvListMode, ServerState, rand_guid, run_tcp_server};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::SourceRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PV: &str = "CHANTABLES:TARGET";
const VERSION: u8 = 2;
const IS_BE: bool = false;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Bind on an ephemeral port, serve `PV`, and return the address plus the
/// `MonitorRegistry` the server is using (so the test can inspect it).
async fn spawn_server() -> (SocketAddr, Arc<MonitorRegistry>) {
    let server = PvaServer::builder().ai(PV, 1.0).build();
    let store = server.store().clone();

    let sources = Arc::new(SourceRegistry::new());
    sources.add("builtin", 0, store.clone()).await;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");

    let mon_registry = Arc::new(MonitorRegistry::new());

    let state = Arc::new(ServerState::new(
        sources,
        mon_registry.clone(),
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

    (addr, mon_registry)
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

/// The tables must be registered for the *whole* life of the connection, from
/// before the first frame — not merely by the time a channel exists.
///
/// Task 7 fires teardown from the registry against whatever connections are
/// live at that moment, including ones still mid-handshake; if registration
/// were deferred until after validation, such a connection would be invisible
/// to it. This test connects, reads the server's `ConnectionValidation`, and
/// then deliberately **never replies** — so the handshake is provably
/// incomplete — yet the tables must already be reachable.
#[tokio::test]
async fn tables_are_registered_before_the_handshake_completes() {
    let (addr, registry) = spawn_server().await;

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidation(_))
    })
    .await;
    // No client ConnectionValidation is sent: the connection is unvalidated and
    // has no channels, and will stay that way for the rest of the test.

    let mut registered = false;
    for _ in 0..50 {
        if registry.channel_tables(1).await.is_some() {
            registered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        registered,
        "a connection's tables must be registered before the handshake completes, \
         not only once a channel has been created"
    );

    // Guard against the assertion above passing for the wrong reason: confirm
    // the connection really is still unvalidated and channel-less.
    let tables = registry.channel_tables(1).await.expect("registered");
    let t = tables.lock().unwrap();
    assert!(
        t.cid_to_sid.is_empty() && t.sid_to_pv.is_empty(),
        "the handshake was never completed, so no channel can exist yet"
    );
    drop(t);

    drop(stream);
}

#[tokio::test]
async fn a_created_channel_is_visible_through_the_registrys_channel_tables() {
    let (addr, registry) = spawn_server().await;

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidation(_))
    })
    .await;
    let validation =
        encode_client_connection_validation(16_384, 512, 0, "ca", "bob", "ws-6", VERSION, IS_BE);
    stream
        .write_all(&validation)
        .await
        .expect("write validation");
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidated(_))
    })
    .await;

    let cid = 7u32;
    stream
        .write_all(&encode_create_channel_request(cid, PV, VERSION, IS_BE))
        .await
        .expect("write create channel");
    read_until(
        &mut stream,
        |cmd| matches!(cmd, PvaPacketCommand::CreateChannel(p) if p.is_server && p.cid == cid),
    )
    .await;

    // The registry must be able to reach this connection's tables, and they
    // must be the *live* ones the connection task just wrote the channel into
    // — not an empty copy taken at accept time.
    let mut seen_pv = None;
    for _ in 0..50 {
        if let Some(tables) = registry.channel_tables(1).await {
            let t = tables.lock().unwrap();
            if let Some(sid) = t.cid_to_sid.get(&cid).copied() {
                seen_pv = t.sid_to_pv.get(&sid).cloned();
                if seen_pv.is_some() {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        seen_pv.as_deref(),
        Some(PV),
        "the registered handle must be the connection's live tables"
    );

    // And the registry must let go when the connection ends, or every
    // connection the server ever accepted leaks its tables.
    drop(stream);
    let mut cleaned = false;
    for _ in 0..50 {
        if registry.channel_tables(1).await.is_none() {
            cleaned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        cleaned,
        "the connection's tables must be unregistered on disconnect"
    );
}
