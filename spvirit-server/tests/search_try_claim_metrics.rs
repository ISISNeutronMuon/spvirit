//! V2-2: the `search_try_claim_*` counters must actually be fed.
//!
//! `note_try_claim` is called from exactly two places — the UDP search loop
//! and the TCP name-server search block. Deleting *both* calls left every test
//! in `spvirit-server` and `spvirit-gateway` green, so the three counters the
//! incident post-mortem asked for ("the signal that was entirely invisible
//! during the observed outage") were one deletion away from reading a flat
//! zero forever.
//!
//! The counters are process-wide, so this is deliberately the **only** test in
//! its binary: nothing else here drives a search, and the two phases below
//! take their own baselines so each call site is asserted independently — the
//! TCP phase cannot be satisfied by the UDP call site, or vice versa.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::{encode_client_connection_validation, encode_search_request};
use spvirit_server::PvaServer;
use spvirit_server::handler::{PvListMode, ServerState, rand_guid, run_tcp_server, run_udp_search};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::SourceRegistry;
use spvirit_server::search_resolve::global_stats;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

const PV: &str = "METRICS:TARGET";
const ABSENT: &str = "METRICS:NOSUCHPV";
const VERSION: u8 = 2;
const IS_BE: bool = false;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

async fn build_sources() -> Arc<SourceRegistry> {
    let server = PvaServer::builder().ai(PV, 1.0).build();
    let sources = Arc::new(SourceRegistry::new());
    sources.add("builtin", 0, server.store().clone()).await;
    sources
}

fn state_for(sources: Arc<SourceRegistry>, port: u16) -> Arc<ServerState> {
    Arc::new(ServerState::new(
        sources,
        Arc::new(MonitorRegistry::new()),
        false,
        PvListMode::Off,
        1024,
        None,
        rand_guid(),
        port,
        None,
        IpAddr::V4(Ipv4Addr::LOCALHOST),
    ))
}

// --- TCP harness (modelled on tests/tcp_name_server_search.rs) -------------

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

async fn tcp_search(stream: &mut TcpStream, seq: u32, cid: u32, name: &str) {
    let req = encode_search_request(seq, 0x81, 0, [0u8; 16], &[(cid, name)], VERSION, IS_BE);
    stream.write_all(&req).await.expect("write search");
    read_until(stream, |cmd| {
        matches!(cmd, PvaPacketCommand::SearchResponse(p) if p.seq == seq)
    })
    .await;
}

// --- UDP harness -----------------------------------------------------------

/// Drive one UDP search and wait (briefly) for any reply, retrying to absorb
/// the bind race. Returns once a response arrives or the budget expires — the
/// counters, not the reply, are what this test asserts on.
async fn udp_search(client: &UdpSocket, server: SocketAddr, cid: u32, name: &str) {
    let req = encode_search_request(cid, 0x01, 0, [0u8; 16], &[(cid, name)], VERSION, IS_BE);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 2048];
    while std::time::Instant::now() < deadline {
        client.send_to(&req, server).await.unwrap();
        if tokio::time::timeout(Duration::from_millis(20), client.recv_from(&mut buf))
            .await
            .is_ok()
        {
            return;
        }
    }
    panic!("no UDP search response for {name} within the budget");
}

#[tokio::test]
async fn both_search_paths_record_their_try_claim_outcomes() {
    // --- TCP call site -----------------------------------------------------
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind tcp");
    let tcp_addr = listener.local_addr().unwrap();
    let tcp_state = state_for(build_sources().await, tcp_addr.port());
    tokio::spawn(async move {
        let _ = run_tcp_server(tcp_state, listener, IO_TIMEOUT).await;
    });

    let before_tcp = global_stats();
    let mut stream = handshake(tcp_addr).await;
    tcp_search(&mut stream, 1, 11, PV).await;
    tcp_search(&mut stream, 2, 12, ABSENT).await;
    let after_tcp = global_stats();

    assert!(
        after_tcp.try_claim_yes > before_tcp.try_claim_yes,
        "the TCP search path recorded no `try_claim` Yes \
         ({} -> {}); search_try_claim_yes can silently read zero",
        before_tcp.try_claim_yes,
        after_tcp.try_claim_yes
    );
    assert!(
        after_tcp.try_claim_no > before_tcp.try_claim_no,
        "the TCP search path recorded no `try_claim` No ({} -> {})",
        before_tcp.try_claim_no,
        after_tcp.try_claim_no
    );

    // --- UDP call site -----------------------------------------------------
    // Baseline taken *after* the TCP phase, so the TCP call site cannot
    // satisfy these assertions.
    let udp_port = {
        let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        probe.local_addr().unwrap().port()
    };
    let udp_addr: SocketAddr = format!("127.0.0.1:{udp_port}").parse().unwrap();
    let udp_state = state_for(build_sources().await, udp_port);
    tokio::spawn(async move {
        if let Err(e) = run_udp_search(udp_state, udp_addr, 5075, rand_guid(), None, None).await {
            eprintln!("udp responder exited early: {e}");
        }
    });
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let before_udp = global_stats();
    udp_search(&client, udp_addr, 21, PV).await;
    udp_search(&client, udp_addr, 22, ABSENT).await;
    let after_udp = global_stats();

    assert!(
        after_udp.try_claim_yes > before_udp.try_claim_yes,
        "the UDP search path recorded no `try_claim` Yes ({} -> {})",
        before_udp.try_claim_yes,
        after_udp.try_claim_yes
    );
    assert!(
        after_udp.try_claim_no > before_udp.try_claim_no,
        "the UDP search path recorded no `try_claim` No ({} -> {})",
        before_udp.try_claim_no,
        after_udp.try_claim_no
    );
}
