use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::auth::{resolved_authnz_host, resolved_authnz_user};
use crate::search::resolve_pv_server;
use crate::transport::{FrameBuf, read_frame_resumable, read_until};
use crate::types::{PvGetError, PvGetOptions, PvGetResult};
use spvirit_codec::SegmentReassembler;
use spvirit_codec::epics_decode::{
    DecodeMode, PvaPacket, PvaPacketCommand,
    decode_op_response_status as codec_decode_op_response_status,
};
use spvirit_codec::spvd_encode::encode_pv_request;
use spvirit_codec::spvirit_encode::encode_client_connection_validation;
pub use spvirit_codec::spvirit_encode::{
    encode_create_channel_request, encode_get_field_request, encode_get_request,
    encode_monitor_request, encode_put_request,
};

pub fn build_client_validation(
    opts: &crate::types::PvGetOptions,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let user = resolved_authnz_user(opts);
    let host = resolved_authnz_host(opts);
    encode_client_connection_validation(87_040, 32_767, 0, "ca", &user, &host, version, is_be)
}

pub fn op_response_status(
    raw: &[u8],
    is_be: bool,
) -> Result<Option<spvirit_codec::epics_decode::PvaStatus>, PvGetError> {
    codec_decode_op_response_status(raw, is_be).map_err(|e| PvGetError::Protocol(e.to_string()))
}

pub fn ensure_status_ok(raw: &[u8], is_be: bool, step: &str) -> Result<(), PvGetError> {
    match op_response_status(raw, is_be)? {
        None => Ok(()),
        Some(st) if st.code == 0 => Ok(()),
        Some(st) => Err(PvGetError::Protocol(format!(
            "{} failed: {}",
            step,
            st.message.unwrap_or_else(|| format!("code={}", st.code))
        ))),
    }
}

pub struct ChannelConn {
    pub stream: TcpStream,
    pub sid: u32,
    pub version: u8,
    pub is_be: bool,
    pub server_addr: std::net::SocketAddr,
    /// The responder's GUID, as reported by the search response that
    /// resolved `server_addr` (all-zero when resolved via the unicast
    /// shortcut, which has no search response to read a guid from).
    pub guid: [u8; 12],
    /// Segment reassembly state for this connection.
    ///
    /// It lives on the connection rather than on each read call because a
    /// message's segments may be separated by control frames — and by the
    /// boundary between [`establish_channel`] and whatever the caller does
    /// next on the same socket.
    pub reassembler: SegmentReassembler,
}

/// Perform the PVA connection-validation handshake on an already-connected
/// TCP stream.
///
/// Reads the server's opening frames (SET_BYTE_ORDER control + the server's
/// `ConnectionValidation`) to learn the protocol version and byte order, sends
/// this client's `ConnectionValidation`, then waits for `ConnectionValidated`.
/// Returns the negotiated `(version, is_be)`.
///
/// `reassembler` must be the one owned by this connection so pending segments
/// survive the handshake into the caller's subsequent reads.
///
/// Shared by [`establish_channel`] and `pvlist::list_pvs_via_get_field`, which
/// previously carried byte-identical copies of this exchange.
pub async fn pva_handshake(
    stream: &mut TcpStream,
    opts: &PvGetOptions,
    reassembler: &mut SegmentReassembler,
) -> Result<(u8, bool), PvGetError> {
    let mut version = 2u8;
    let mut is_be = false;

    // A persistent buffer, so committed reads can be deadline-bounded
    // (`read_frame_resumable` sets `bound_committed`) without ever losing a
    // partially-read frame. `read_packet` deliberately does NOT bound
    // committed reads -- the tools' fresh-buffer `Err(Timeout) => continue`
    // loops depend on that -- so the handshake needs its own buffer rather
    // than a change to the shared helper.
    let mut fb = FrameBuf::new();

    for _ in 0..2 {
        if let Ok(bytes) = read_frame_resumable(stream, opts.timeout, reassembler, &mut fb).await {
            let mut pkt = PvaPacket::new(&bytes);
            if let Some(cmd) = pkt.decode_payload() {
                match cmd {
                    PvaPacketCommand::Control(payload) => {
                        if payload.command == 2 {
                            is_be = pkt.header.flags.is_msb;
                        }
                    }
                    PvaPacketCommand::ConnectionValidation(_) => {
                        version = pkt.header.version;
                        is_be = pkt.header.flags.is_msb;
                    }
                    _ => {}
                }
            }
        }
    }

    let validation = build_client_validation(opts, version, is_be);
    stream.write_all(&validation).await?;

    let _ = read_until(stream, opts.timeout, reassembler, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidated(_))
    })
    .await?;

    Ok((version, is_be))
}

/// A TCP connection that has completed the PVA connection-validation
/// handshake but carries no channel yet.
///
/// This is the unit the connection pool stores: everything here is
/// per-connection, nothing is per-operation.
pub struct ValidatedConn {
    pub stream: TcpStream,
    pub version: u8,
    pub is_be: bool,
    pub server_addr: std::net::SocketAddr,
    /// The responder's GUID, as reported by the search response that
    /// resolved `server_addr` (all-zero when resolved via the unicast
    /// shortcut).
    pub guid: [u8; 12],
    /// Segment reassembly state for this connection. It lives here because a
    /// message's segments may be separated by control frames and by the
    /// boundary between one operation and the next on the same socket.
    pub reassembler: SegmentReassembler,
}

impl ValidatedConn {
    /// Pair this connection with a server-side channel id, producing the
    /// single-channel [`ChannelConn`] the existing API hands out.
    pub fn into_channel(self, sid: u32) -> ChannelConn {
        ChannelConn {
            stream: self.stream,
            sid,
            version: self.version,
            is_be: self.is_be,
            server_addr: self.server_addr,
            guid: self.guid,
            reassembler: self.reassembler,
        }
    }
}

/// Connect to `target` and complete the PVA connection-validation handshake.
pub async fn connect_and_validate(
    target: std::net::SocketAddr,
    guid: [u8; 12],
    opts: &PvGetOptions,
) -> Result<ValidatedConn, PvGetError> {
    let mut stream = timeout(opts.timeout, TcpStream::connect(target))
        .await
        .map_err(|_| PvGetError::Timeout("connect"))??;

    let mut reassembler = SegmentReassembler::new();
    let (version, is_be) = pva_handshake(&mut stream, opts, &mut reassembler).await?;

    Ok(ValidatedConn {
        stream,
        version,
        is_be,
        server_addr: target,
        guid,
        reassembler,
    })
}

/// Create one channel for `pv_name` on an already-validated connection and
/// return the server-side sid.
///
/// `cid` must be unique for the lifetime of `conn`; the caller owns that
/// allocation (see `cache::PooledConn::next_cid`).
///
/// The response predicate pins `cid` because a pooled connection may already
/// carry other channels: both the success and the error CREATE_CHANNEL server
/// responses echo the cid (`spvirit-codec/src/epics_decode.rs:1174-1229`,
/// `spvirit_encode::encode_create_channel_error`), so this cannot mistake a
/// neighbouring channel's reply for its own.
pub async fn create_channel_on(
    conn: &mut ValidatedConn,
    pv_name: &str,
    cid: u32,
    timeout_dur: Duration,
) -> Result<u32, PvGetError> {
    let create = encode_create_channel_request(cid, pv_name, conn.version, conn.is_be);
    conn.stream.write_all(&create).await?;

    let create_resp = read_until(
        &mut conn.stream,
        timeout_dur,
        &mut conn.reassembler,
        |cmd| matches!(cmd, PvaPacketCommand::CreateChannel(p) if p.is_server && p.cid == cid),
    )
    .await?;

    let mut pkt = PvaPacket::new(&create_resp);
    let cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
        "create_channel decode failed".to_string(),
    ))?;
    match cmd {
        PvaPacketCommand::CreateChannel(payload) => {
            if payload.status.as_ref().is_some_and(|s| s.is_error()) {
                let detail = payload
                    .status
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                return Err(PvGetError::Protocol(format!(
                    "create_channel error: {}",
                    detail
                )));
            }
            Ok(payload.sid)
        }
        _ => Err(PvGetError::Protocol(
            "unexpected create_channel response".to_string(),
        )),
    }
}

/// Connect, validate, and create a single channel for `opts.pv_name`.
///
/// Signature-compatible with the pre-split version; spvirit-py imports this.
pub async fn establish_channel(
    target: std::net::SocketAddr,
    guid: [u8; 12],
    opts: &PvGetOptions,
) -> Result<ChannelConn, PvGetError> {
    let mut conn = connect_and_validate(target, guid, opts).await?;
    let sid = create_channel_on(&mut conn, &opts.pv_name, 1, opts.timeout).await?;
    Ok(conn.into_channel(sid))
}

/// Convenience wrapper: GET with no field filtering.
pub async fn pvget(opts: &PvGetOptions) -> Result<PvGetResult, PvGetError> {
    pvget_fields(opts, &[]).await
}

/// GET with optional field filtering.
///
/// If `fields` is empty, requests all fields (equivalent to `-r ""`).
/// Otherwise, encodes a pvRequest like `field(value,alarm,timeStamp)`.
pub async fn pvget_fields(opts: &PvGetOptions, fields: &[&str]) -> Result<PvGetResult, PvGetError> {
    let (target, guid) = resolve_pv_server(opts).await?;
    let mut conn = connect_and_validate(target, guid, opts).await?;
    let sid = create_channel_on(&mut conn, &opts.pv_name, 1, opts.timeout).await?;
    run_get_on(&mut conn, sid, &opts.pv_name, fields, 1, opts.timeout).await
}

/// Run one GET (INIT then data) for `pv_name` on an already-created channel.
///
/// Split out of [`pvget_fields`] so `PvaClient` can run a GET over a pooled
/// connection. `ioid` must be unique among the operations that have run on
/// `conn` — a pooled connection is checked out exclusively, but a *stale*
/// response from a previous operation on the same socket must not be mistaken
/// for this one's, so callers pass a fresh `alloc_ioid()` rather than a
/// constant **and** both predicates below pin `op.command` and `op.ioid`.
/// Matching on `subcmd` alone (as the pre-pool code did) would accept a late
/// PUT reply here.
pub async fn run_get_on(
    conn: &mut ValidatedConn,
    sid: u32,
    pv_name: &str,
    fields: &[&str],
    ioid: u32,
    timeout_dur: Duration,
) -> Result<PvGetResult, PvGetError> {
    let is_be = conn.is_be;
    let version = conn.version;

    let pv_request = if fields.is_empty() {
        // Empty pvRequest — request all fields
        vec![0xfd, 0x02, 0x00, 0x80, 0x00, 0x00]
    } else {
        encode_pv_request(fields, is_be)
    };
    let get_init_req = encode_get_request(sid, ioid, 0x08, &pv_request, version, is_be);
    conn.stream.write_all(&get_init_req).await?;

    let init_resp = read_until(
        &mut conn.stream,
        timeout_dur,
        &mut conn.reassembler,
        |cmd| {
            matches!(cmd, PvaPacketCommand::Op(op)
            if op.command == 10 && op.ioid == ioid && (op.subcmd & 0x08) != 0)
        },
    )
    .await?;
    let mut pkt = PvaPacket::new(&init_resp);
    let cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
        "get init response decode failed".to_string(),
    ))?;
    let desc = match cmd {
        PvaPacketCommand::Op(op) => {
            // Name a server refusal at INIT ("PV not found", "Unknown SID",
            // ...) instead of the bare "missing introspection" it used to fall
            // through to; the pooled path classifies retries on this message.
            // Still `Decode`, the variant this path has always returned, so
            // spvirit-py's exception class for it is unchanged.
            if let Some(st) = op.status.as_ref().filter(|st| st.is_error()) {
                let msg = st
                    .message
                    .clone()
                    .unwrap_or_else(|| format!("code={}", st.code));
                return Err(PvGetError::Decode(format!(
                    "missing introspection: GET init error: {msg}"
                )));
            }
            op.introspection
                .ok_or_else(|| PvGetError::Decode("missing introspection".to_string()))?
        }
        _ => {
            return Err(PvGetError::Protocol(
                "unexpected get init response".to_string(),
            ));
        }
    };

    let get_data_req = encode_get_request(sid, ioid, 0x00, &[], version, is_be);
    conn.stream.write_all(&get_data_req).await?;

    let data_resp = read_until(
        &mut conn.stream,
        timeout_dur,
        &mut conn.reassembler,
        |cmd| {
            matches!(cmd, PvaPacketCommand::Op(op)
            if op.command == 10 && op.ioid == ioid && op.subcmd == 0x00)
        },
    )
    .await?;
    let mut pkt = PvaPacket::new(&data_resp);
    let cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
        "get data response decode failed".to_string(),
    ))?;

    match cmd {
        PvaPacketCommand::Op(mut op) => {
            // A decode failure leaves decoded_value None, handled below.
            let _ = op.decode_with_field_desc(&desc, is_be, DecodeMode::Strict);
            if let Some(value) = op.decoded_value {
                return Ok(PvGetResult {
                    pv_name: pv_name.to_string(),
                    value,
                    raw_pva: data_resp,
                    raw_pvd: op.body,
                    introspection: desc,
                    server_host: conn.server_addr.to_string(),
                });
            }
            Err(PvGetError::Decode("no decoded value".to_string()))
        }
        _ => Err(PvGetError::Protocol(
            "unexpected get data response".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_codec::epics_decode::{PvaPacket, PvaPacketCommand, PvaStatus};

    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Duration;

    /// Two different PVs created on ONE validated connection get two
    /// different server-side sids. This is the property the connection pool
    /// depends on: a pooled connection keyed only by server address must be
    /// able to serve any PV that server hosts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_connection_can_host_two_channels_with_distinct_sids() {
        use spvirit_server::pva_server::PvaServer;

        // Port 0: the server binds and reports its own ports, so a
        // parallel test cannot take the port between probe and bind.
        let running = PvaServer::builder()
            .ao("MULTI:A", 1.0)
            .ao("MULTI:B", 2.0)
            .port(0)
            .udp_port(0)
            .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .build()
            .start()
            .await
            .expect("server start");
        let (target, _udp): (SocketAddr, SocketAddr) =
            running.local_addrs().await.expect("server bind");
        let mut opts = PvGetOptions::new("MULTI:A".to_string());
        opts.timeout = Duration::from_secs(5);
        opts.server_addr = Some(target);

        let mut conn = connect_and_validate(target, [0u8; 12], &opts)
            .await
            .expect("connect_and_validate");

        let sid_a = create_channel_on(&mut conn, "MULTI:A", 1, opts.timeout)
            .await
            .expect("create MULTI:A");
        let sid_b = create_channel_on(&mut conn, "MULTI:B", 2, opts.timeout)
            .await
            .expect("create MULTI:B");

        assert_ne!(sid_a, sid_b, "each channel must get its own sid");
    }

    /// `establish_channel` keeps working exactly as before the split — this
    /// is the signature spvirit-py imports.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn establish_channel_still_returns_a_usable_channel_conn() {
        use spvirit_server::pva_server::PvaServer;

        // Port 0: the server binds and reports its own ports, so a
        // parallel test cannot take the port between probe and bind.
        let running = PvaServer::builder()
            .ao("SPLIT:PV", 1.0)
            .port(0)
            .udp_port(0)
            .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .build()
            .start()
            .await
            .expect("server start");
        let (target, _udp): (SocketAddr, SocketAddr) =
            running.local_addrs().await.expect("server bind");
        let mut opts = PvGetOptions::new("SPLIT:PV".to_string());
        opts.timeout = Duration::from_secs(5);
        opts.server_addr = Some(target);

        let conn = establish_channel(target, [0u8; 12], &opts)
            .await
            .expect("establish_channel");
        assert_eq!(conn.server_addr, target);
        assert_eq!(conn.version, 2);
    }

    #[test]
    fn encode_decode_monitor_request_roundtrip() {
        let msg =
            encode_monitor_request(1, 2, 0x08, &[0xfd, 0x02, 0x00, 0x80, 0x00, 0x00], 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::Op(op) => {
                assert_eq!(op.command, 13);
                assert_eq!(op.subcmd, 0x08);
                assert_eq!(op.sid_or_cid, 1);
                assert_eq!(op.ioid, 2);
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_put_init_roundtrip() {
        let msg = encode_put_request(5, 6, 0x08, &[0xfd, 0x02, 0x00, 0x80, 0x00, 0x00], 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::Op(op) => {
                assert_eq!(op.command, 11);
                assert_eq!(op.subcmd, 0x08);
                assert_eq!(op.sid_or_cid, 5);
                assert_eq!(op.ioid, 6);
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_get_field_request_roundtrip() {
        let msg = encode_get_field_request(7, 1, Some("*"), 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::GetField(payload) => {
                assert!(!payload.is_server);
                assert_eq!(payload.sid, Some(7));
                assert_eq!(payload.ioid, Some(1));
                assert_eq!(payload.field_name.as_deref(), Some("*"));
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_get_field_request_empty_field_roundtrip() {
        let msg = encode_get_field_request(7, 1, None, 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::GetField(payload) => {
                assert!(!payload.is_server);
                assert_eq!(payload.sid, Some(7));
                assert_eq!(payload.ioid, Some(1));
                assert_eq!(payload.field_name.as_deref(), Some(""));
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn pva_status_code_zero_is_not_an_error() {
        let ok = PvaStatus {
            code: 0,
            message: None,
            stack: None,
        };
        let err = PvaStatus {
            code: 1,
            message: Some("bad".to_string()),
            stack: None,
        };
        assert!(!None::<&PvaStatus>.is_some_and(|s| s.is_error()));
        assert!(!Some(&ok).is_some_and(|s| s.is_error()));
        assert!(Some(&err).is_some_and(|s| s.is_error()));
    }
}
