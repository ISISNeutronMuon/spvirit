use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::{interval, timeout};

use crate::spvirit_client::client::{
    build_client_validation, encode_create_channel_request, encode_monitor_request, pvget,
};
use crate::spvirit_client::transport::{read_packet, read_until};
use crate::spvirit_client::types::{PvGetError, PvGetOptions, PvGetResult};
use spvirit_codec::epics_decode::{DecodeMode, PvaPacketCommand};
use spvirit_codec::spvirit_encode::encode_control_message;

pub use spvirit_client::pvlist::PvListSource;

const PV_REQUEST_EMPTY: [u8; 6] = [0xfd, 0x02, 0x00, 0x80, 0x00, 0x00];

fn candidate_server_addrs(opts: &PvGetOptions, server_addr: SocketAddr) -> Vec<SocketAddr> {
    let mut out = vec![server_addr];
    let default_addr = SocketAddr::new(server_addr.ip(), opts.tcp_port);
    if default_addr != server_addr {
        out.push(default_addr);
    }
    out
}

pub async fn list_pvs_with_fallback(
    opts: &PvGetOptions,
    server_addr: SocketAddr,
) -> Result<(Vec<String>, PvListSource), PvGetError> {
    spvirit_client::pvlist::pvlist_with_fallback(opts, server_addr).await
}

pub async fn list_pvs_with_fallback_progress<F>(
    opts: &PvGetOptions,
    server_addr: SocketAddr,
    on_progress: F,
) -> Result<(Vec<String>, PvListSource), PvGetError>
where
    F: FnMut(&str),
{
    spvirit_client::pvlist::pvlist_with_fallback_progress(opts, server_addr, on_progress).await
}

pub async fn fetch_snapshot_from_server(
    opts: &PvGetOptions,
    server_addr: SocketAddr,
    pv_name: &str,
) -> Result<PvGetResult, PvGetError> {
    let addrs = candidate_server_addrs(opts, server_addr);
    let mut errs = Vec::new();

    for addr in addrs {
        let mut get_opts = opts.clone();
        get_opts.server_addr = Some(addr);
        get_opts.pv_name = pv_name.to_string();
        match pvget(&get_opts).await {
            Ok(result) => return Ok(result),
            Err(err) => errs.push(format!("{} => {}", addr, err)),
        }
    }

    Err(PvGetError::Protocol(format!(
        "failed to fetch '{}' from {}: {}",
        pv_name,
        server_addr,
        errs.join(" | ")
    )))
}

async fn monitor_pv_at_addr<F, P>(
    opts: &PvGetOptions,
    server_addr: SocketAddr,
    pv_name: &str,
    on_update: &mut F,
    on_progress: &mut P,
) -> Result<(), PvGetError>
where
    F: FnMut(PvGetResult),
    P: FnMut(&str),
{
    on_progress(&format!("Connecting monitor to {}...", server_addr));
    let mut stream = timeout(opts.timeout, TcpStream::connect(server_addr))
        .await
        .map_err(|_| PvGetError::Timeout("connect"))??;

    // One reassembler for this connection, shared by every read below.
    let mut reassembler = spvirit_client::SegmentReassembler::new();

    let mut version = 2u8;
    let mut is_be = false;

    // Read initial server messages: SET_BYTE_ORDER and server validation.
    for _ in 0..2 {
        if let Ok(bytes) = read_packet(&mut stream, opts.timeout, &mut reassembler).await {
            let mut pkt = spvirit_codec::epics_decode::PvaPacket::new(&bytes);
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
    let create = encode_create_channel_request(cid, pv_name, version, is_be);
    stream.write_all(&create).await?;
    let create_cmd = read_until(&mut stream, opts.timeout, &mut reassembler, |cmd| {
        matches!(cmd, PvaPacketCommand::CreateChannel(_))
    })
    .await?;
    let mut pkt = spvirit_codec::epics_decode::PvaPacket::new(&create_cmd);
    let decoded = pkt.decode_payload().ok_or(PvGetError::Protocol(
        "monitor create decode failed".to_string(),
    ))?;
    let sid = match decoded {
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

    let ioid = 1u32;
    let mon_init = encode_monitor_request(sid, ioid, 0x08, &PV_REQUEST_EMPTY, version, is_be);
    stream.write_all(&mon_init).await?;
    let init_resp = read_until(
        &mut stream,
        opts.timeout,
        &mut reassembler,
        |cmd| match cmd {
            PvaPacketCommand::Op(op) => {
                op.command == 13 && op.ioid == ioid && (op.subcmd & 0x08) != 0
            }
            _ => false,
        },
    )
    .await?;
    let mut pkt = spvirit_codec::epics_decode::PvaPacket::new(&init_resp);
    let init_cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
        "monitor init decode failed".to_string(),
    ))?;
    let desc = match init_cmd {
        PvaPacketCommand::Op(op) => {
            if op.status.as_ref().is_some_and(|s| s.is_error()) {
                let detail = op
                    .status
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                return Err(PvGetError::Protocol(format!(
                    "monitor init failed: {}",
                    detail
                )));
            }
            op.introspection
                .ok_or_else(|| PvGetError::Decode("missing monitor introspection".to_string()))?
        }
        _ => {
            return Err(PvGetError::Protocol(
                "unexpected monitor init response".to_string(),
            ));
        }
    };

    let mon_start = encode_monitor_request(sid, ioid, 0x44, &[], version, is_be);
    stream.write_all(&mon_start).await?;
    on_progress(&format!(
        "Monitoring {} on {} (streaming updates)...",
        pv_name, server_addr
    ));

    let mut echo_interval = interval(Duration::from_secs(10));
    let mut echo_token: u32 = 1;

    loop {
        tokio::select! {
            _ = echo_interval.tick() => {
                let msg = encode_control_message(false, is_be, version, 3, echo_token);
                echo_token = echo_token.wrapping_add(1);
                let _ = stream.write_all(&msg).await;
            }
            res = read_packet(&mut stream, opts.timeout, &mut reassembler) => {
                let bytes = match res {
                    Ok(b) => b,
                    Err(PvGetError::Timeout(_)) => continue,
                    Err(e) => return Err(e),
                };
                let mut pkt = spvirit_codec::epics_decode::PvaPacket::new(&bytes);
                let Some(cmd) = pkt.decode_payload() else {
                    continue;
                };
                let PvaPacketCommand::Op(mut op) = cmd else {
                    continue;
                };
                if op.command != 13 || op.ioid != ioid || (op.subcmd != 0x00 && op.subcmd != 0x10) {
                    continue;
                }

                // A decode failure leaves decoded_value None; skip the update.
                let _ = op.decode_with_field_desc(&desc, is_be, DecodeMode::Strict);
                if let Some(value) = op.decoded_value {
                    on_update(PvGetResult {
                        pv_name: pv_name.to_string(),
                        value,
                        raw_pva: bytes.clone(),
                        raw_pvd: op.body,
                        introspection: desc.clone(),
                        server_host: server_addr.to_string(),
                    });
                }
                if op.subcmd == 0x10 {
                    return Ok(());
                }
            }
        }
    }
}

pub async fn monitor_pv_from_server<F, P>(
    opts: &PvGetOptions,
    server_addr: SocketAddr,
    pv_name: &str,
    mut on_update: F,
    mut on_progress: P,
) -> Result<(), PvGetError>
where
    F: FnMut(PvGetResult),
    P: FnMut(&str),
{
    let addrs = candidate_server_addrs(opts, server_addr);
    let mut errs = Vec::new();

    for addr in addrs {
        match monitor_pv_at_addr(opts, addr, pv_name, &mut on_update, &mut on_progress).await {
            Ok(()) => return Ok(()),
            Err(err) => errs.push(format!("{} => {}", addr, err)),
        }
    }

    Err(PvGetError::Protocol(format!(
        "failed to monitor '{}' from {}: {}",
        pv_name,
        server_addr,
        errs.join(" | ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_codec::epics_decode::PvaStatus;

    #[test]
    fn candidate_server_addrs_adds_default_tcp_port_fallback() {
        let mut opts = PvGetOptions::new(String::new());
        opts.tcp_port = 5075;
        let addr: SocketAddr = "10.0.0.2:6000".parse().unwrap();
        let addrs = candidate_server_addrs(&opts, addr);
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0], addr);
        assert_eq!(addrs[1], "10.0.0.2:5075".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn pva_status_code_zero_is_not_an_error() {
        let ok = PvaStatus {
            code: 0,
            message: None,
            stack: None,
        };
        let err = PvaStatus {
            code: 2,
            message: Some("bad".to_string()),
            stack: None,
        };
        assert!(!None::<&PvaStatus>.is_some_and(|s| s.is_error()));
        assert!(!Some(&ok).is_some_and(|s| s.is_error()));
        assert!(Some(&err).is_some_and(|s| s.is_error()));
    }
}
