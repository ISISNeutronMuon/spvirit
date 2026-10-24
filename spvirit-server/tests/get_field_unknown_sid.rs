//! GET_FIELD that names a channel this connection does not have must be
//! answered with an error, not with the server's PV listing.
//!
//! The listing is reached by naming *no* channel: spvirit's own `pvlist`
//! (`spvirit-client/src/pvlist.rs`) sends `encode_get_field_request(0, 0, ..)`,
//! which decodes with an explicit `sid = Some(0)`. Sids start at 1, so 0 is
//! unambiguous and must keep reaching the listing.
//!
//! Harness modelled on `tests/channel_tables_registration.rs`, with
//! `PvListMode::List` so the listing branch is live.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::{
    encode_client_connection_validation, encode_create_channel_request, encode_get_field_request,
};
use spvirit_server::PvaServer;
use spvirit_server::handler::{PvListMode, ServerState, rand_guid, run_tcp_server};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::SourceRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PV: &str = "GETFIELD:TARGET";
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
        PvListMode::List,
        100, // pvlist_max: 0 would truncate the listing to nothing
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

/// Connect, validate, and create one channel for `PV`; return the stream and
/// the channel's sid.
async fn connect_with_channel(addr: SocketAddr) -> (TcpStream, u32) {
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

    let cid = 1u32;
    stream
        .write_all(&encode_create_channel_request(cid, PV, VERSION, IS_BE))
        .await
        .expect("write create channel");
    let PvaPacketCommand::CreateChannel(p) = read_until(
        &mut stream,
        |cmd| matches!(cmd, PvaPacketCommand::CreateChannel(p) if p.is_server && p.cid == cid),
    )
    .await
    else {
        unreachable!()
    };
    (stream, p.sid)
}

async fn get_field(
    stream: &mut TcpStream,
    sid: u32,
    ioid: u32,
    field: Option<&str>,
) -> PvaPacketCommand {
    stream
        .write_all(&encode_get_field_request(sid, ioid, field, VERSION, IS_BE))
        .await
        .expect("write get_field");
    read_until(
        stream,
        |cmd| matches!(cmd, PvaPacketCommand::GetField(p) if p.is_server && p.cid == ioid),
    )
    .await
}

#[tokio::test]
async fn get_field_on_an_unknown_sid_is_an_error_not_the_pv_list() {
    let addr = spawn_server().await;
    let (mut stream, sid) = connect_with_channel(addr).await;

    // A sid this connection never had (sids are server-global and start at 1).
    let unknown = sid + 1000;
    let PvaPacketCommand::GetField(p) = get_field(&mut stream, unknown, 7, None).await else {
        unreachable!()
    };
    let status = p.status.expect("an error reply carries a status");
    assert!(
        status.is_error(),
        "an unknown channel must be refused, got introspection {:?}",
        p.introspection.map(|d| d.struct_id)
    );
    assert_eq!(status.message.as_deref(), Some("Unknown SID"));
    assert!(p.introspection.is_none());

    // The connection is still usable, and the real channel still answers.
    let PvaPacketCommand::GetField(p) = get_field(&mut stream, sid, 8, None).await else {
        unreachable!()
    };
    assert!(!p.status.as_ref().is_some_and(|s| s.is_error()));
    let desc = p.introspection.expect("the PV's own introspection");
    assert_ne!(desc.struct_id.as_deref(), Some("epics:pva/pvlist:1.0"));
}

/// The listing request spvirit's `pvlist` sends — sid 0, ioid 0 — still lists,
/// even on a connection that has a channel open.
#[tokio::test]
async fn get_field_naming_sid_zero_still_returns_the_pv_list() {
    let addr = spawn_server().await;
    let (mut stream, _sid) = connect_with_channel(addr).await;

    // Mirrors `pvlist.rs`: `encode_get_field_request(0, 0, pattern, ..)`.
    let PvaPacketCommand::GetField(p) = get_field(&mut stream, 0, 0, Some("*")).await else {
        unreachable!()
    };
    assert!(
        !p.status.as_ref().is_some_and(|s| s.is_error()),
        "{:?}",
        p.status
    );
    let desc = p.introspection.expect("the listing");
    assert_eq!(desc.struct_id.as_deref(), Some("epics:pva/pvlist:1.0"));
    assert!(desc.fields.iter().any(|f| f.name == PV));
}
