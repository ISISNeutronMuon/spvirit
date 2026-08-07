//! PV listing — discover available PV names from a PVA server.
//!
//! Provides multiple discovery strategies (tried in order by
//! [`pvlist_with_fallback`]):
//!
//! 1. `__pvlist` GET (preferred — spvirit / EPICS7 servers)
//! 2. Connection-level GET_FIELD (legacy, opt-in via env var)
//! 3. Server RPC `op=channels`
//! 4. Server GET with heuristic parsing

use std::net::SocketAddr;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

use spvirit_codec::epics_decode::{DecodeMode, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvd_decode::{
    DecodedValue, FieldDesc, FieldType, PvdDecoder, StructureDesc, extract_nt_scalar_value,
};
use spvirit_codec::spvd_encode::{encode_string_pvd, encode_structure_desc};
use spvirit_codec::spvirit_encode::encode_rpc_request;

use crate::client::{
    ChannelConn, build_client_validation, encode_get_field_request, encode_get_request,
    establish_channel, pvget,
};
use crate::transport::{read_packet, read_until};
use crate::types::{PvGetError, PvOptions};

/// Which discovery strategy succeeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PvListSource {
    PvList,
    GetField,
    ServerRpc,
    ServerGet,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

const PV_REQUEST_EMPTY: [u8; 6] = [0xfd, 0x02, 0x00, 0x80, 0x00, 0x00];

/// Sort, deduplicate, and remove empty entries.
pub fn normalize_pv_names(mut names: Vec<String>) -> Vec<String> {
    names.retain(|name| !name.trim().is_empty());
    names.sort();
    names.dedup();
    names
}

/// Extract PV names from a decoded `__pvlist` value (NTScalarArray of strings).
pub fn parse_pvlist_value(value: &DecodedValue) -> Option<Vec<String>> {
    let root = extract_nt_scalar_value(value).unwrap_or(value);
    let DecodedValue::Array(items) = root else {
        return None;
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if let DecodedValue::String(name) = item {
            out.push(name.clone());
        } else {
            return None;
        }
    }
    Some(out)
}

fn candidate_server_addrs(opts: &PvOptions, server_addr: SocketAddr) -> Vec<SocketAddr> {
    let mut out = vec![server_addr];
    let default_addr = SocketAddr::new(server_addr.ip(), opts.tcp_port);
    if default_addr != server_addr {
        out.push(default_addr);
    }
    out
}

fn is_get_field_fallback_enabled() -> bool {
    match std::env::var("EPICS_PVA_ENABLE_GET_FIELD_FALLBACK") {
        Ok(v) => {
            let v = v.trim().to_ascii_uppercase();
            v == "YES" || v == "Y" || v == "1" || v == "TRUE"
        }
        Err(_) => false,
    }
}

fn collect_strings_from_decoded(value: &DecodedValue, out: &mut Vec<String>) {
    match value {
        DecodedValue::String(s) => out.push(s.clone()),
        DecodedValue::Array(items) => {
            for item in items {
                collect_strings_from_decoded(item, out);
            }
        }
        DecodedValue::Structure(fields) => {
            for (_, item) in fields {
                collect_strings_from_decoded(item, out);
            }
        }
        _ => {}
    }
}

fn looks_like_pv_name(candidate: &str) -> bool {
    if candidate.is_empty() || candidate.len() > 128 {
        return false;
    }
    if candidate.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let lower = candidate.to_ascii_lowercase();
    let deny = [
        "value",
        "alarm",
        "timestamp",
        "display",
        "control",
        "severity",
        "message",
        "seconds",
        "nanoseconds",
        "units",
    ];
    if deny.iter().any(|d| lower == *d) {
        return false;
    }
    if lower.starts_with("epics:") {
        return false;
    }
    true
}

fn extract_ascii_candidates(raw: &[u8], out: &mut Vec<String>) {
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i].is_ascii_alphanumeric() {
            let start = i;
            i += 1;
            while i < raw.len() {
                let b = raw[i];
                if b.is_ascii_alphanumeric()
                    || b == b':'
                    || b == b'.'
                    || b == b'_'
                    || b == b'-'
                    || b == b'/'
                {
                    i += 1;
                } else {
                    break;
                }
            }
            let len = i - start;
            if (3..=128).contains(&len) {
                if let Ok(s) = std::str::from_utf8(&raw[start..start + len]) {
                    out.push(s.to_string());
                }
            }
        } else {
            i += 1;
        }
    }
}

fn encode_server_rpc_channels_request(is_be: bool) -> Vec<u8> {
    let desc = StructureDesc {
        struct_id: Some("epics:nt/NTURI:1.0".to_string()),
        fields: vec![
            FieldDesc {
                name: "scheme".to_string(),
                field_type: FieldType::String,
            },
            FieldDesc {
                name: "path".to_string(),
                field_type: FieldType::String,
            },
            FieldDesc {
                name: "query".to_string(),
                field_type: FieldType::Structure(StructureDesc {
                    struct_id: None,
                    fields: vec![FieldDesc {
                        name: "op".to_string(),
                        field_type: FieldType::String,
                    }],
                }),
            },
        ],
    };

    let mut out = Vec::new();
    out.push(0x80);
    out.extend_from_slice(&encode_structure_desc(&desc, is_be));
    out.extend_from_slice(&encode_string_pvd("pva", is_be));
    out.extend_from_slice(&encode_string_pvd("server", is_be));
    out.extend_from_slice(&encode_string_pvd("channels", is_be));
    out
}

// ─── Listing strategies ──────────────────────────────────────────────────────

/// List PVs via the `__pvlist` channel (preferred).
async fn list_pvs_via_pvlist(
    opts: &PvOptions,
    server_addr: SocketAddr,
) -> Result<Vec<String>, PvGetError> {
    let mut get_opts = opts.clone();
    get_opts.pv_name = "__pvlist".to_string();
    get_opts.server_addr = Some(server_addr);
    let result = pvget(&get_opts).await?;
    let names = parse_pvlist_value(&result.value)
        .ok_or_else(|| PvGetError::Decode("failed to decode __pvlist value".to_string()))?;
    Ok(normalize_pv_names(names))
}

/// List PVs via connection-level GET_FIELD.
pub async fn list_pvs_via_get_field(
    opts: &PvOptions,
    server_addr: SocketAddr,
    field_pattern: Option<&str>,
) -> Result<Vec<String>, PvGetError> {
    let mut stream = timeout(opts.timeout, TcpStream::connect(server_addr))
        .await
        .map_err(|_| PvGetError::Timeout("connect"))??;

    let mut version = 2u8;
    let mut is_be = false;

    for _ in 0..2 {
        if let Ok(bytes) = read_packet(&mut stream, opts.timeout).await {
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

    let _ = read_until(&mut stream, opts.timeout, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidated(_))
    })
    .await?;

    let get_field = encode_get_field_request(0, 0, field_pattern, version, is_be);
    stream.write_all(&get_field).await?;

    let field_resp = read_until(
        &mut stream,
        opts.timeout,
        |cmd| matches!(cmd, PvaPacketCommand::GetField(payload) if payload.is_server),
    )
    .await?;
    let mut pkt = PvaPacket::new(&field_resp);
    let cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
        "get_field listing decode failed".to_string(),
    ))?;
    let PvaPacketCommand::GetField(payload) = cmd else {
        return Err(PvGetError::Protocol(
            "unexpected GET_FIELD response".to_string(),
        ));
    };

    if payload.status.as_ref().is_some_and(|s| s.is_error()) {
        let detail = payload
            .status
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        return Err(PvGetError::Protocol(format!(
            "get_field listing error: {}",
            detail
        )));
    }

    let desc = payload
        .introspection
        .ok_or_else(|| PvGetError::Decode("missing GET_FIELD introspection".to_string()))?;

    let names = desc.fields.into_iter().map(|f| f.name).collect::<Vec<_>>();
    Ok(normalize_pv_names(names))
}

/// List PVs via server RPC on a specific channel name.
async fn list_pvs_via_server_rpc_channel(
    opts: &PvOptions,
    server_addr: SocketAddr,
    rpc_channel: &str,
) -> Result<Vec<String>, PvGetError> {
    let mut rpc_opts = opts.clone();
    rpc_opts.pv_name = rpc_channel.to_string();
    let ChannelConn {
        mut stream,
        sid,
        version,
        is_be,
        ..
    } = establish_channel(server_addr, &rpc_opts).await?;

    let ioid = 1u32;
    let rpc_init = encode_rpc_request(sid, ioid, 0x08, &PV_REQUEST_EMPTY, version, is_be);
    stream.write_all(&rpc_init).await?;

    let init_resp = read_until(&mut stream, opts.timeout, |cmd| match cmd {
        PvaPacketCommand::Op(op) => op.command == 20 && op.ioid == ioid && (op.subcmd & 0x08) != 0,
        _ => false,
    })
    .await?;
    let mut pkt = PvaPacket::new(&init_resp);
    let init_cmd = pkt
        .decode_payload()
        .ok_or(PvGetError::Protocol("rpc init decode failed".to_string()))?;
    if let PvaPacketCommand::Op(op) = init_cmd {
        if op.status.as_ref().is_some_and(|s| s.is_error()) {
            let detail = op
                .status
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            return Err(PvGetError::Protocol(format!("rpc init failed: {}", detail)));
        }
    }

    let rpc_payload = encode_server_rpc_channels_request(is_be);
    let rpc_req = encode_rpc_request(sid, ioid, 0x00, &rpc_payload, version, is_be);
    stream.write_all(&rpc_req).await?;

    let rpc_resp = read_until(&mut stream, opts.timeout, |cmd| match cmd {
        PvaPacketCommand::Op(op) => op.command == 20 && op.ioid == ioid && op.subcmd == 0x00,
        _ => false,
    })
    .await?;
    let mut pkt = PvaPacket::new(&rpc_resp);
    let rpc_cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
        "rpc response decode failed".to_string(),
    ))?;
    let PvaPacketCommand::Op(op) = rpc_cmd else {
        return Err(PvGetError::Protocol("unexpected RPC response".to_string()));
    };
    if op.status.as_ref().is_some_and(|s| s.is_error()) {
        let detail = op
            .status
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        return Err(PvGetError::Protocol(format!(
            "rpc execute failed: {}",
            detail
        )));
    }

    if op.body.is_empty() {
        return Err(PvGetError::Decode("empty RPC response".to_string()));
    }

    let decoder = PvdDecoder::new(is_be);
    let (desc, consumed) = decoder
        .parse_introspection_with_len(&op.body)
        .map_err(|_| PvGetError::Decode("RPC missing introspection".to_string()))?;
    let value_raw = op
        .body
        .get(consumed..)
        .ok_or_else(|| PvGetError::Decode("RPC malformed payload".to_string()))?;
    let (decoded, _) = decoder
        .decode_structure(value_raw, &desc)
        .map_err(|_| PvGetError::Decode("RPC decode failed".to_string()))?;

    let mut strings = Vec::new();
    collect_strings_from_decoded(&decoded, &mut strings);
    if strings.is_empty() {
        return Err(PvGetError::Decode(
            "RPC list returned no strings".to_string(),
        ));
    }
    Ok(normalize_pv_names(strings))
}

/// List PVs via server RPC, trying `"server"` then `"__server"`.
pub async fn list_pvs_via_server_rpc(
    opts: &PvOptions,
    server_addr: SocketAddr,
) -> Result<Vec<String>, PvGetError> {
    let mut errs = Vec::new();
    for channel in ["server", "__server"] {
        match list_pvs_via_server_rpc_channel(opts, server_addr, channel).await {
            Ok(names) => return Ok(names),
            Err(err) => errs.push(format!("{}: {}", channel, err)),
        }
    }
    Err(PvGetError::Protocol(format!(
        "server RPC unavailable: {}",
        errs.join(" | ")
    )))
}

/// List PVs via server GET, trying `"server"` then `"__server"`.
pub async fn list_pvs_via_server_get(
    opts: &PvOptions,
    server_addr: SocketAddr,
) -> Result<Vec<String>, PvGetError> {
    async fn get_channel(
        opts: &PvOptions,
        server_addr: SocketAddr,
        channel: &str,
    ) -> Result<Vec<String>, PvGetError> {
        let mut get_opts = opts.clone();
        get_opts.pv_name = channel.to_string();
        let ChannelConn {
            mut stream,
            sid,
            version,
            is_be,
            ..
        } = establish_channel(server_addr, &get_opts).await?;

        let ioid = 1u32;
        let init_req = encode_get_request(sid, ioid, 0x08, &PV_REQUEST_EMPTY, version, is_be);
        stream.write_all(&init_req).await?;
        let init_resp = read_until(&mut stream, opts.timeout, |cmd| match cmd {
            PvaPacketCommand::Op(op) => {
                op.command == 10 && op.ioid == ioid && (op.subcmd & 0x08) != 0
            }
            _ => false,
        })
        .await?;
        let mut pkt = PvaPacket::new(&init_resp);
        let init_cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
            "server get init decode failed".to_string(),
        ))?;
        let init_desc = match init_cmd {
            PvaPacketCommand::Op(op) => {
                if op.status.as_ref().is_some_and(|s| s.is_error()) {
                    let detail = op
                        .status
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    return Err(PvGetError::Protocol(format!(
                        "server GET init failed: {}",
                        detail
                    )));
                }
                op.introspection
            }
            _ => None,
        };

        let data_req = encode_get_request(sid, ioid, 0x00, &[], version, is_be);
        stream.write_all(&data_req).await?;
        let data_resp = read_until(&mut stream, opts.timeout, |cmd| match cmd {
            PvaPacketCommand::Op(op) => op.command == 10 && op.ioid == ioid && op.subcmd == 0x00,
            _ => false,
        })
        .await?;
        let mut pkt = PvaPacket::new(&data_resp);
        let data_cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
            "server get data decode failed".to_string(),
        ))?;

        let PvaPacketCommand::Op(mut op) = data_cmd else {
            return Err(PvGetError::Protocol(
                "unexpected GET data response".to_string(),
            ));
        };
        if op.status.as_ref().is_some_and(|s| s.is_error()) {
            let detail = op
                .status
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            return Err(PvGetError::Protocol(format!(
                "server GET data failed: {}",
                detail
            )));
        }

        let mut names = Vec::new();
        names.extend(op.pv_names.clone());
        extract_ascii_candidates(&op.body, &mut names);
        if let Some(desc) = &init_desc {
            for field in &desc.fields {
                names.push(field.name.clone());
            }
            // A decode failure leaves decoded_value None, handled below.
            let _ = op.decode_with_field_desc(desc, is_be, DecodeMode::Strict);
            if let Some(decoded) = &op.decoded_value {
                collect_strings_from_decoded(decoded, &mut names);
            }
        }

        let mut names = normalize_pv_names(names);
        names.retain(|n| looks_like_pv_name(n));
        if names.is_empty() {
            return Err(PvGetError::Decode(
                "server GET returned no PV-like names".to_string(),
            ));
        }
        Ok(names)
    }

    let mut errs = Vec::new();
    for channel in ["server", "__server"] {
        match get_channel(opts, server_addr, channel).await {
            Ok(names) => return Ok(names),
            Err(err) => errs.push(format!("{}: {}", channel, err)),
        }
    }
    Err(PvGetError::Protocol(format!(
        "server GET unavailable: {}",
        errs.join(" | ")
    )))
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// List PV names from a server using `__pvlist` GET (preferred method).
pub async fn pvlist(opts: &PvOptions, server_addr: SocketAddr) -> Result<Vec<String>, PvGetError> {
    list_pvs_via_pvlist(opts, server_addr).await
}

/// List PV names with automatic fallback through all strategies.
///
/// Tries (in order): `__pvlist` → GET_FIELD (opt-in) → Server RPC → Server GET.
pub async fn pvlist_with_fallback(
    opts: &PvOptions,
    server_addr: SocketAddr,
) -> Result<(Vec<String>, PvListSource), PvGetError> {
    pvlist_with_fallback_progress(opts, server_addr, |_| {}).await
}

/// List PV names with fallback and progress callback.
pub async fn pvlist_with_fallback_progress<F>(
    opts: &PvOptions,
    server_addr: SocketAddr,
    mut on_progress: F,
) -> Result<(Vec<String>, PvListSource), PvGetError>
where
    F: FnMut(&str),
{
    let addrs = candidate_server_addrs(opts, server_addr);
    let mut attempts = Vec::new();
    let get_field_fallback = is_get_field_fallback_enabled();

    if addrs.len() > 1 {
        on_progress(&format!(
            "Trying {} candidate server endpoints...",
            addrs.len()
        ));
    }
    if !get_field_fallback {
        on_progress(
            "GET_FIELD fallback is disabled by default (set EPICS_PVA_ENABLE_GET_FIELD_FALLBACK=YES to enable)",
        );
    }

    for addr in addrs {
        on_progress(&format!("Trying __pvlist on {}", addr));
        let primary = list_pvs_via_pvlist(opts, addr).await;
        match primary {
            Ok(names) => return Ok((normalize_pv_names(names), PvListSource::PvList)),
            Err(primary_err) => {
                let get_field_result = if get_field_fallback {
                    on_progress(&format!(
                        "__pvlist unavailable on {}; trying GET_FIELD(*)",
                        addr
                    ));
                    let fallback_star = list_pvs_via_get_field(opts, addr, Some("*")).await;
                    match fallback_star {
                        Ok(names) => {
                            return Ok((normalize_pv_names(names), PvListSource::GetField));
                        }
                        Err(star_err) => {
                            on_progress(&format!(
                                "GET_FIELD(*) unavailable on {}; trying GET_FIELD(<empty>)",
                                addr
                            ));
                            let fallback_empty = list_pvs_via_get_field(opts, addr, None).await;
                            match fallback_empty {
                                Ok(names) => {
                                    return Ok((normalize_pv_names(names), PvListSource::GetField));
                                }
                                Err(empty_err) => Some(format!(
                                    "GET_FIELD(*): {}; GET_FIELD(<empty>): {}",
                                    star_err, empty_err
                                )),
                            }
                        }
                    }
                } else {
                    None
                };

                on_progress(&format!(
                    "__pvlist unavailable on {}; trying RPC(server)",
                    addr
                ));
                match list_pvs_via_server_rpc(opts, addr).await {
                    Ok(names) => return Ok((normalize_pv_names(names), PvListSource::ServerRpc)),
                    Err(rpc_err) => {
                        on_progress(&format!(
                            "RPC(server) unavailable on {}; trying GET(server)",
                            addr
                        ));
                        match list_pvs_via_server_get(opts, addr).await {
                            Ok(names) => {
                                return Ok((normalize_pv_names(names), PvListSource::ServerGet));
                            }
                            Err(get_err) => {
                                let get_field_msg = get_field_result
                                    .unwrap_or_else(|| "GET_FIELD: disabled".to_string());
                                attempts.push(format!(
                                    "{} => __pvlist: {}; {}; RPC(server): {}; GET(server): {}",
                                    addr, primary_err, get_field_msg, rpc_err, get_err
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    Err(PvGetError::Protocol(format!(
        "failed to list PVs from {}: {}",
        server_addr,
        attempts.join(" | ")
    )))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pvlist_value_extracts_ntscalararray_strings() {
        let value = DecodedValue::Structure(vec![
            (
                "value".to_string(),
                DecodedValue::Array(vec![
                    DecodedValue::String("SIM:AI".to_string()),
                    DecodedValue::String("SIM:AO".to_string()),
                ]),
            ),
            ("alarm".to_string(), DecodedValue::Structure(vec![])),
        ]);

        let parsed = parse_pvlist_value(&value).expect("parsed");
        assert_eq!(parsed, vec!["SIM:AI".to_string(), "SIM:AO".to_string()]);
    }

    #[test]
    fn normalize_pv_names_sorts_and_deduplicates() {
        let names = vec!["B".into(), "A".into(), "B".into(), " ".into()];
        let result = normalize_pv_names(names);
        assert_eq!(result, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn candidate_server_addrs_adds_default_tcp_port_fallback() {
        let mut opts = PvOptions::new(String::new());
        opts.tcp_port = 5075;
        let addr: SocketAddr = "10.0.0.2:6000".parse().unwrap();
        let addrs = candidate_server_addrs(&opts, addr);
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0], addr);
        assert_eq!(addrs[1], "10.0.0.2:5075".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn candidate_server_addrs_no_dup_when_same_port() {
        let mut opts = PvOptions::new(String::new());
        opts.tcp_port = 6000;
        let addr: SocketAddr = "10.0.0.2:6000".parse().unwrap();
        let addrs = candidate_server_addrs(&opts, addr);
        assert_eq!(addrs.len(), 1);
    }

    #[test]
    fn collect_strings_from_decoded_extracts_nested_strings() {
        let value = DecodedValue::Structure(vec![
            ("a".to_string(), DecodedValue::String("ONE".to_string())),
            (
                "b".to_string(),
                DecodedValue::Array(vec![DecodedValue::String("TWO".to_string())]),
            ),
        ]);
        let mut out = Vec::new();
        collect_strings_from_decoded(&value, &mut out);
        assert_eq!(out, vec!["ONE".to_string(), "TWO".to_string()]);
    }

    #[test]
    fn extract_ascii_candidates_finds_pv_like_tokens() {
        let raw = b"\x00SIM:AI\x00junk\x00IOC-01:PV1\x00";
        let mut out = Vec::new();
        extract_ascii_candidates(raw, &mut out);
        assert!(out.iter().any(|s| s == "SIM:AI"));
        assert!(out.iter().any(|s| s == "IOC-01:PV1"));
    }

    #[test]
    fn looks_like_pv_name_filters_metadata() {
        assert!(looks_like_pv_name("SIM:AI"));
        assert!(looks_like_pv_name("IOC-01:PV1"));
        assert!(!looks_like_pv_name("value"));
        assert!(!looks_like_pv_name("alarm"));
        assert!(!looks_like_pv_name("epics:nt/NTScalar:1.0"));
        assert!(!looks_like_pv_name(""));
        assert!(!looks_like_pv_name("has space"));
    }

    #[test]
    fn encode_server_rpc_channels_request_uses_nturi_channels() {
        let payload = encode_server_rpc_channels_request(false);
        assert_eq!(payload.first(), Some(&0x80));

        let decoder = PvdDecoder::new(false);
        let (desc, consumed) = decoder
            .parse_introspection_with_len(&payload)
            .expect("introspection");
        assert_eq!(desc.struct_id.as_deref(), Some("epics:nt/NTURI:1.0"));

        let (decoded, _) = decoder
            .decode_structure(&payload[consumed..], &desc)
            .expect("decode payload");
        let DecodedValue::Structure(fields) = decoded else {
            panic!("expected structure");
        };

        let mut scheme = None;
        let mut path = None;
        let mut op = None;
        for (name, value) in fields {
            match (name.as_str(), value) {
                ("scheme", DecodedValue::String(v)) => scheme = Some(v),
                ("path", DecodedValue::String(v)) => path = Some(v),
                ("query", DecodedValue::Structure(query_fields)) => {
                    for (qname, qvalue) in query_fields {
                        if qname == "op" {
                            if let DecodedValue::String(v) = qvalue {
                                op = Some(v);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        assert_eq!(scheme.as_deref(), Some("pva"));
        assert_eq!(path.as_deref(), Some("server"));
        assert_eq!(op.as_deref(), Some("channels"));
    }
}
