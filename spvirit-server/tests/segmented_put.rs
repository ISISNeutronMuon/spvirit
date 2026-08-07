//! Characterisation test for server-side segment reassembly.
//!
//! A PUT whose payload arrives split across two PVA segments must land the
//! exact same record state as the identical PUT sent as one unsegmented
//! message. This pins the behaviour of the connection handler's reassembly
//! path across the refactor onto `spvirit_codec::SegmentReassembler`.
//!
//! The workspace does not *emit* segmented messages (encoder-side
//! segmentation is out of scope), so the test re-frames an already-encoded
//! message into two segments by hand.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::{
    encode_client_connection_validation, encode_create_channel_request, encode_put_request,
};
use spvirit_server::PvaServer;
use spvirit_server::handler::{PvListMode, ServerState, handle_connection, rand_guid};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::SourceRegistry;
use spvirit_server::simple_store::SimplePvStore;
use spvirit_types::ScalarValue;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PV: &str = "SEG:TARGET";
const VERSION: u8 = 2;
/// Every frame in this test is little-endian, matching what the server sends
/// for its own handshake messages.
const IS_BE: bool = false;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Split an encoded PVA message into two segment frames at `at` payload bytes.
fn split_into_segments(msg: &[u8], at: usize) -> (Vec<u8>, Vec<u8>) {
    let (header, payload) = msg.split_at(8);
    let (a, b) = payload.split_at(at);

    let mut first = header.to_vec();
    first[2] = (first[2] & !0x30) | 0x10; // first segment
    first[4..8].copy_from_slice(&(a.len() as u32).to_le_bytes());
    first.extend_from_slice(a);

    let mut last = header.to_vec();
    last[2] = (last[2] & !0x30) | 0x20; // last segment
    last[4..8].copy_from_slice(&(b.len() as u32).to_le_bytes());
    last.extend_from_slice(b);

    (first, last)
}

/// Bind a listener on an ephemeral port and serve `PV` from an in-process
/// store. Returns the store so the test can read the record back directly.
async fn spawn_server(initial: f64) -> (Arc<SimplePvStore>, SocketAddr) {
    let server = PvaServer::builder().ao(PV, initial).build();
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
        let mut conn_id = 1u64;
        while let Ok((stream, _peer)) = listener.accept().await {
            let state = state.clone();
            let id = conn_id;
            conn_id += 1;
            tokio::spawn(async move {
                let _ = handle_connection(state, stream, id, IO_TIMEOUT).await;
            });
        }
    });

    (store, addr)
}

/// Read one whole PVA frame (header plus payload) from the socket.
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

/// Read frames until one decodes to a command the predicate accepts.
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

/// Handshake, create a channel and send PUT INIT. Returns the open socket and
/// the server-assigned sid.
async fn open_put_channel(addr: SocketAddr) -> (TcpStream, u32, u32) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");

    // SET_BYTE_ORDER control frame, then the server's CONNECTION_VALIDATION.
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidation(_))
    })
    .await;

    let validation = encode_client_connection_validation(
        16_384,
        512,
        0,
        "anonymous",
        "tester",
        "localhost",
        VERSION,
        IS_BE,
    );
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

    // PUT INIT with the canonical "all fields" pvRequest.
    let ioid = 1u32;
    let pv_request = vec![0xfd, 0x02, 0x00, 0x80, 0x00, 0x00];
    stream
        .write_all(&encode_put_request(
            sid,
            ioid,
            0x08,
            &pv_request,
            VERSION,
            IS_BE,
        ))
        .await
        .expect("write put init");
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && (op.subcmd & 0x08) != 0)
    })
    .await;

    (stream, sid, ioid)
}

/// PUT `value` into `PV` and return the resulting stored value.
///
/// When `segmented` is set the encoded PUT message is re-framed as two
/// segments before it goes on the wire.
async fn put_value(value: f64, segmented: bool) -> Option<ScalarValue> {
    let (store, addr) = spawn_server(1.0).await;
    let (mut stream, sid, ioid) = open_put_channel(addr).await;

    // Standard PUT body: 1-byte bitset with bit 1 ("value") set, then the
    // f64 payload.
    let mut body = vec![0x01, 0x02];
    body.extend_from_slice(&value.to_le_bytes());
    let msg = encode_put_request(sid, ioid, 0x00, &body, VERSION, IS_BE);

    if segmented {
        // Split part-way through the f64 so the boundary falls inside a
        // single wire field, not on a convenient field edge.
        let payload_len = msg.len() - 8;
        let split_at = payload_len - 6;
        assert!(split_at > 0 && split_at < payload_len, "interior split");
        let (first, last) = split_into_segments(&msg, split_at);
        stream.write_all(&first).await.expect("write first segment");
        stream.write_all(&last).await.expect("write last segment");
    } else {
        stream.write_all(&msg).await.expect("write put data");
    }

    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && (op.subcmd & 0x08) == 0)
    })
    .await;

    store.get_value(PV).await
}

#[tokio::test]
async fn segmented_put_matches_unsegmented_put() {
    let segmented = put_value(2.5, true).await;
    let unsegmented = put_value(2.5, false).await;

    assert_eq!(
        segmented,
        Some(ScalarValue::F64(2.5)),
        "segmented PUT did not land the written value"
    );
    assert_eq!(
        segmented, unsegmented,
        "segmented and unsegmented PUT produced different record state"
    );
}
