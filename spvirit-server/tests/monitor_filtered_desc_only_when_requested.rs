//! A monitor that asked for no field filter must not be given a
//! `filtered_desc`. Storing one forces `build_monitor_frame` down the
//! filtered branch, which encodes a throwaway delta as a change detector and
//! then a full filtered frame: two full encodes per subscriber per update,
//! for a projection that is the identity.
//!
//! Both branches must produce identical bytes for an unfiltered subscriber,
//! because the unfiltered branch has been dead code until now.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvd_encode::encode_pv_request;
use spvirit_codec::spvirit_encode::{
    encode_client_connection_validation, encode_create_channel_request, encode_monitor_request,
};
use spvirit_server::PvaServer;
use spvirit_server::handler::{PvListMode, ServerState, rand_guid, run_tcp_server};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::SourceRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PV: &str = "FILTDESC:TARGET";
const VERSION: u8 = 2;
const IS_BE: bool = false;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// A pvRequest with an **empty** `field(...)` selector: the shape a client that
/// wants the whole structure sends. `decode_pv_request_fields`
/// (`spvirit-codec/src/spvd_encode.rs:1394-1416`) returns `None` for it because
/// the inner `field` structure has no members — so the server takes the
/// no-filter path. Built with the codec's own encoder rather than a
/// hand-written byte literal so it cannot drift from what the decoder accepts.
fn no_field_filter() -> Vec<u8> {
    encode_pv_request(&[], IS_BE)
}

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

/// Connect, validate, create a channel on `PV`, send a MONITOR INIT carrying
/// `pv_request`, and wait for the INIT response. Returns the live stream.
async fn monitor_with(addr: SocketAddr, pv_request: &[u8]) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidation(_))
    })
    .await;
    stream
        .write_all(&encode_client_connection_validation(
            16_384, 512, 0, "ca", "bob", "ws-6", VERSION, IS_BE,
        ))
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
    let sid = match read_until(
        &mut stream,
        |cmd| matches!(cmd, PvaPacketCommand::CreateChannel(p) if p.is_server && p.cid == cid),
    )
    .await
    {
        PvaPacketCommand::CreateChannel(p) => p.sid,
        other => panic!("unexpected command: {other:?}"),
    };

    stream
        .write_all(&encode_monitor_request(
            sid, 1u32, 0x08, pv_request, VERSION, IS_BE,
        ))
        .await
        .expect("write monitor init");
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 13 && (op.subcmd & 0x08) != 0)
    })
    .await;

    stream
}

/// Poll the registry until `PV` has exactly one subscriber, then return a copy
/// of it. The connection task registers the sub asynchronously with respect to
/// the INIT response the test just read.
async fn only_sub(registry: &MonitorRegistry) -> spvirit_server::state::MonitorSub {
    for _ in 0..50 {
        {
            let monitors = registry.monitors.lock().await;
            if let Some(list) = monitors.get(PV)
                && list.len() == 1
            {
                return list[0].clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no subscriber registered for {PV}");
}

#[tokio::test]
async fn an_unfiltered_monitor_gets_no_filtered_desc() {
    let (addr, registry) = spawn_server().await;
    let stream = monitor_with(addr, &no_field_filter()).await;

    let sub = only_sub(&registry).await;
    assert!(
        sub.filtered_desc.is_none(),
        "a monitor that requested no field filter must carry filtered_desc: None, \
         so build_monitor_frame takes the single-encode unfiltered branch instead \
         of encoding a throwaway delta plus a full filtered frame; got {:?}",
        sub.filtered_desc
    );

    drop(stream);
}

#[tokio::test]
async fn a_field_filtered_monitor_still_gets_a_filtered_desc() {
    let (addr, registry) = spawn_server().await;
    let stream = monitor_with(addr, &encode_pv_request(&["value"], IS_BE)).await;

    let sub = only_sub(&registry).await;
    let desc = sub
        .filtered_desc
        .expect("a monitor that requested field(value) must keep its filtered desc");
    let names: Vec<&str> = desc.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["value"],
        "the stored desc must be the projection the client asked for"
    );

    drop(stream);
}
