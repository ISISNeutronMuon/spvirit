//! B-2: coverage for the *TCP* name-server search path.
//!
//! `handle_connection` answers `PvaPacketCommand::Search` over an established
//! TCP connection — the `EPICS_PVA_NAME_SERVERS` route — using the same
//! non-blocking `Source::try_claim` the UDP datagram path uses. That block had
//! no test at all: deleting it wholesale, or making its `TryClaim::Yes` arm
//! stop pushing the cid, left `cargo test --all-targets` green.
//!
//! These tests drive a real socket through the real handshake and assert on
//! the wire-level `SearchResponse`, so they fail under either mutation.
//!
//! Harness modelled on `tests/client_registry_lifecycle.rs`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::{encode_client_connection_validation, encode_search_request};
use spvirit_server::PvaServer;
use spvirit_server::handler::{PvListMode, ServerState, rand_guid, run_tcp_server};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::SourceRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PV: &str = "TCPSEARCH:TARGET";
const VERSION: u8 = 2;
const IS_BE: bool = false;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

async fn spawn_server() -> SocketAddr {
    let server = PvaServer::builder().ai(PV, 1.0).build();
    let store = server.store().clone();

    let sources = Arc::new(SourceRegistry::new());
    sources.add("builtin", 0, store.clone()).await;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");

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

async fn handshake(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidation(_))
    })
    .await;
    let validation =
        encode_client_connection_validation(16_384, 512, 0, "ca", "tester", "host", VERSION, IS_BE);
    stream
        .write_all(&validation)
        .await
        .expect("write validation");
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidated(_))
    })
    .await;
    stream
}

/// Send a TCP search for `name` with client id `cid` and return the decoded
/// response payload.
async fn tcp_search(
    stream: &mut TcpStream,
    seq: u32,
    cid: u32,
    name: &str,
) -> spvirit_codec::epics_decode::PvaSearchResponsePayload {
    let req = encode_search_request(seq, 0x81, 0, [0u8; 16], &[(cid, name)], VERSION, IS_BE);
    stream.write_all(&req).await.expect("write search");
    match read_until(stream, |cmd| {
        matches!(cmd, PvaPacketCommand::SearchResponse(p) if p.seq == seq)
    })
    .await
    {
        PvaPacketCommand::SearchResponse(p) => p,
        other => panic!("expected SearchResponse, got {other:?}"),
    }
}

/// A name the registry claims decisively must come back `found`, with the
/// client's cid echoed. Fails if the `try_claim` block is deleted (nothing
/// else on this path can push the cid when pvlist is `Off`), and fails if the
/// `TryClaim::Yes` arm stops pushing.
#[tokio::test]
async fn a_tcp_search_for_a_served_pv_answers_with_the_cid() {
    let addr = spawn_server().await;
    let mut stream = handshake(addr).await;

    let resp = tcp_search(&mut stream, 7, 42, PV).await;

    assert!(resp.found, "a served PV must be found over the TCP path");
    assert_eq!(resp.cids, vec![42], "the searched cid must be echoed back");
    assert_eq!(resp.protocol, "tcp");
    assert_eq!(resp.port, addr.port(), "must advertise the serving port");
}

/// The other half: a name nothing serves must *not* be answered. Without this,
/// a mutant that pushes every cid unconditionally would still pass the test
/// above.
#[tokio::test]
async fn a_tcp_search_for_an_unknown_pv_is_not_answered() {
    let addr = spawn_server().await;
    let mut stream = handshake(addr).await;

    let resp = tcp_search(&mut stream, 8, 43, "TCPSEARCH:NOSUCHPV").await;

    assert!(!resp.found, "an unserved PV must not be claimed");
    assert!(
        resp.cids.is_empty(),
        "no cid may be echoed for an unserved PV, got {:?}",
        resp.cids
    );
}
