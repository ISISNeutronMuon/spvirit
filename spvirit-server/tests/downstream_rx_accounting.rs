//! Task 9: downstream (client -> server) RX byte accounting.
//!
//! Every inbound frame's on-wire byte count (8-byte header + payload) must
//! land in the injected `ClientRegistry`'s per-connection `rx` counter
//! (`ClientRegistry::add_rx`, already implemented by Task 3 -- this task only
//! adds the call site). Additionally, PUT frames (and only PUT frames, as a
//! documented approximation) must also add their payload bytes to
//! `BandwidthCounters::ds_bypv_rx` under the target PV's name.
//!
//! Modelled on `tests/client_registry_lifecycle.rs` (registry injection via
//! `MonitorRegistry`, driven through the public `run_tcp_server`) and
//! `tests/segmented_put.rs` (the PUT init/data handshake helpers).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::{
    encode_client_connection_validation, encode_create_channel_request, encode_put_request,
};
use spvirit_server::PvaServer;
use spvirit_server::diag::{BandwidthCounters, ClientRegistry};
use spvirit_server::handler::{PvListMode, ServerState, rand_guid, run_tcp_server};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::SourceRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PV: &str = "RXACCT:TARGET";
const VERSION: u8 = 2;
const IS_BE: bool = false;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Bind a listener on an ephemeral port and serve `PV`, with both the
/// `ClientRegistry` and `BandwidthCounters` installed on the
/// `MonitorRegistry` before `ServerState` is built (mirrors how
/// `PvaServer::resolved_monitor_registry` installs them in production).
async fn spawn_server(
    client_registry: Arc<ClientRegistry>,
    bandwidth_counters: Arc<BandwidthCounters>,
) -> SocketAddr {
    let server = PvaServer::builder().ao(PV, 1.0).build();
    let store = server.store().clone();

    let sources = Arc::new(SourceRegistry::new());
    sources.add("builtin", 0, store.clone()).await;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");

    let mon_registry = Arc::new(MonitorRegistry::new());
    mon_registry.set_client_registry(client_registry);
    mon_registry.set_bandwidth_counters(bandwidth_counters);

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

/// Handshake, create a channel, and PUT INIT. Returns the open socket, the
/// client-observed peer address, the server-assigned sid/ioid, and the exact
/// bytes of every frame the *client* wrote (validation response, create
/// channel request, put init) -- used to compute the expected registry `rx`
/// total independently of the production code under test.
async fn open_put_channel(addr: SocketAddr) -> (TcpStream, SocketAddr, u32, u32, Vec<Vec<u8>>) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let local_addr = stream.local_addr().expect("local addr");
    let mut sent_frames = Vec::new();

    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidation(_))
    })
    .await;

    let validation = encode_client_connection_validation(
        16_384, 512, 0, "anonymous", "tester", "localhost", VERSION, IS_BE,
    );
    stream
        .write_all(&validation)
        .await
        .expect("write validation");
    sent_frames.push(validation);
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidated(_))
    })
    .await;

    let cid = 1u32;
    let create_channel = encode_create_channel_request(cid, PV, VERSION, IS_BE);
    stream
        .write_all(&create_channel)
        .await
        .expect("write create channel");
    sent_frames.push(create_channel);
    let sid = match read_until(
        &mut stream,
        |cmd| matches!(cmd, PvaPacketCommand::CreateChannel(p) if p.is_server && p.cid == cid),
    )
    .await
    {
        PvaPacketCommand::CreateChannel(p) => {
            assert!(p.status.is_none(), "create channel failed: {:?}", p.status);
            p.sid
        }
        other => panic!("unexpected command: {other:?}"),
    };

    let ioid = 1u32;
    let pv_request = vec![0xfd, 0x02, 0x00, 0x80, 0x00, 0x00];
    let put_init = encode_put_request(sid, ioid, 0x08, &pv_request, VERSION, IS_BE);
    stream
        .write_all(&put_init)
        .await
        .expect("write put init");
    sent_frames.push(put_init);
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && (op.subcmd & 0x08) != 0)
    })
    .await;

    (stream, local_addr, sid, ioid, sent_frames)
}

/// Poll `f` until it returns `Some`, or panic after a short timeout. Used
/// because the connection handler's byte-counter updates run inline on the
/// server's own task, asynchronously with respect to the test's assertions.
async fn poll_until<T, F: Fn() -> Option<T>>(f: F, msg: &str) -> T {
    for _ in 0..50 {
        if let Some(v) = f() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{msg}");
}

#[tokio::test]
async fn downstream_frames_and_puts_are_counted_by_host_and_by_pv() {
    let registry = Arc::new(ClientRegistry::new());
    let counters = Arc::new(BandwidthCounters::new());
    let addr = spawn_server(registry.clone(), counters.clone()).await;

    let (mut stream, client_addr, sid, ioid, sent_frames) = open_put_channel(addr).await;

    // Standard PUT body: 1-byte bitset with bit 1 ("value") set, then the f64
    // payload.
    let mut body = vec![0x01, 0x02];
    body.extend_from_slice(&2.5f64.to_le_bytes());
    let put_data = encode_put_request(sid, ioid, 0x00, &body, VERSION, IS_BE);
    let put_data_payload_len = put_data.len() - 8;
    stream
        .write_all(&put_data)
        .await
        .expect("write put data");
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && (op.subcmd & 0x08) == 0)
    })
    .await;

    // Per-host RX: every client-written frame (validation, create channel,
    // put init, put data) must be counted at 8 (header) + payload_len each --
    // which is exactly that frame's total wire length, since the encoders
    // above already produce header+payload.
    let expected_rx: u64 = sent_frames.iter().map(|f| f.len() as u64).sum::<u64>()
        + put_data.len() as u64;
    let rx = poll_until(
        || {
            registry
                .snapshot()
                .into_iter()
                .find(|e| e.peer == client_addr)
                .map(|e| e.rx)
        },
        "expected a tracked connection with nonzero rx",
    )
    .await;
    assert_eq!(
        rx, expected_rx,
        "per-host rx must equal the sum of every inbound frame's on-wire bytes"
    );

    // Per-PV RX (puts only): PUT INIT (subcmd 0x08) and PUT DATA (subcmd
    // 0x00) both dispatch through the PUT command path once `sid` resolves
    // to `PV`, so both contribute -- this is the documented "puts only"
    // approximation, not a double count of the per-host counter above (a
    // different counter, a different dimension).
    let put_init_payload_len = sent_frames[2].len() - 8;
    let expected_bypv_rx = (put_init_payload_len + put_data_payload_len) as u64;
    let bypv = poll_until(
        || {
            counters
                .ds_bypv_rx
                .snapshot()
                .into_iter()
                .find(|(k, _)| k == PV)
                .map(|(_, n)| n)
        },
        "expected ds_bypv_rx to have an entry for the target PV",
    )
    .await;
    assert_eq!(
        bypv, expected_bypv_rx,
        "ds_bypv_rx must equal the sum of PUT frame payload bytes for this PV"
    );

    drop(stream);
}
