use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::auth::{resolved_authnz_host, resolved_authnz_user};
use crate::search::resolve_pv_server;
use crate::transport::{read_packet, read_until};
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
    codec_decode_op_response_status(raw, is_be).map_err(PvGetError::Protocol)
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

pub async fn establish_channel(
    target: std::net::SocketAddr,
    guid: [u8; 12],
    opts: &PvGetOptions,
) -> Result<ChannelConn, PvGetError> {
    let mut stream = timeout(opts.timeout, TcpStream::connect(target))
        .await
        .map_err(|_| PvGetError::Timeout("connect"))??;

    // One reassembler for the lifetime of this connection; it is handed to
    // the caller in `ChannelConn` so pending segments survive the handshake.
    let mut reassembler = SegmentReassembler::new();

    let mut version = 2u8;
    let mut is_be = false;

    for _ in 0..2 {
        if let Ok(bytes) = read_packet(&mut stream, opts.timeout, &mut reassembler).await {
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

    let _ = read_until(&mut stream, opts.timeout, &mut reassembler, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidated(_))
    })
    .await?;

    let cid = 1u32;
    let create = encode_create_channel_request(cid, &opts.pv_name, version, is_be);
    stream.write_all(&create).await?;

    let create_resp = read_until(&mut stream, opts.timeout, &mut reassembler, |cmd| {
        matches!(cmd, PvaPacketCommand::CreateChannel(_))
    })
    .await?;
    let mut pkt = PvaPacket::new(&create_resp);
    let cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
        "create_channel decode failed".to_string(),
    ))?;
    let sid = match cmd {
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
            payload.sid
        }
        _ => {
            return Err(PvGetError::Protocol(
                "unexpected create_channel response".to_string(),
            ));
        }
    };

    Ok(ChannelConn {
        stream,
        sid,
        version,
        is_be,
        server_addr: target,
        guid,
        reassembler,
    })
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

    let conn = establish_channel(target, guid, opts).await?;
    let ChannelConn {
        mut stream,
        sid,
        version,
        is_be,
        mut reassembler,
        ..
    } = conn;

    let ioid = 1u32;
    let pv_request = if fields.is_empty() {
        // Empty pvRequest — request all fields
        vec![0xfd, 0x02, 0x00, 0x80, 0x00, 0x00]
    } else {
        encode_pv_request(fields, is_be)
    };
    let get_init_req = encode_get_request(sid, ioid, 0x08, &pv_request, version, is_be);
    stream.write_all(&get_init_req).await?;

    let init_resp = read_until(
        &mut stream,
        opts.timeout,
        &mut reassembler,
        |cmd| matches!(cmd, PvaPacketCommand::Op(op) if (op.subcmd & 0x08) != 0),
    )
    .await?;
    let mut pkt = PvaPacket::new(&init_resp);
    let cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
        "get init response decode failed".to_string(),
    ))?;

    let desc = match cmd {
        PvaPacketCommand::Op(op) => op
            .introspection
            .ok_or_else(|| PvGetError::Decode("missing introspection".to_string()))?,
        _ => {
            return Err(PvGetError::Protocol(
                "unexpected get init response".to_string(),
            ));
        }
    };

    let get_data_req = encode_get_request(sid, ioid, 0x00, &[], version, is_be);
    stream.write_all(&get_data_req).await?;

    let data_resp = read_until(
        &mut stream,
        opts.timeout,
        &mut reassembler,
        |cmd| matches!(cmd, PvaPacketCommand::Op(op) if op.subcmd == 0x00),
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
                    pv_name: opts.pv_name.clone(),
                    value,
                    raw_pva: data_resp,
                    raw_pvd: op.body,
                    introspection: desc,
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
