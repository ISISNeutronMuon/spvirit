use argparse::{ArgumentParser, Store, StoreTrue};
use tokio::io::AsyncWriteExt;
use tokio::runtime::Runtime;

use spvirit_client::{pvget, pvinfo as high_level_pvinfo};
use spvirit_codec::epics_decode::{PvaPacket, PvaPacketCommand};
use spvirit_codec::spvd_decode::{StructureDesc, extract_subfield_desc, format_structure_tree};
use spvirit_codec::spvirit_encode::encode_header;
use spvirit_tools::spvirit_client::cli::CommonClientArgs;
use spvirit_tools::spvirit_client::client::{ChannelConn, establish_channel};
use spvirit_tools::spvirit_client::search::resolve_pv_server;
use spvirit_tools::spvirit_client::transport::read_until;
use spvirit_tools::spvirit_client::types::{PvGetError, PvGetOptions};

/// Legacy fallback: GET_FIELD without the field-name wire field, for servers
/// that crash when they receive an empty field-name string.
fn encode_get_field_request_without_field_name(cid: u32, version: u8, is_be: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        cid.to_be_bytes()
    } else {
        cid.to_le_bytes()
    });
    let mut out = encode_header(false, is_be, false, version, 17, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

fn should_retry_get_field_without_name(err: &PvGetError) -> bool {
    match err {
        PvGetError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
        ),
        PvGetError::Timeout("read_until") | PvGetError::Timeout("read header") => true,
        _ => false,
    }
}

fn should_fallback_to_pvget(err: &PvGetError) -> bool {
    should_retry_get_field_without_name(err)
        || matches!(err, PvGetError::Protocol(msg) if msg.contains("get_field") || msg.contains("GET_FIELD"))
}

/// Legacy fallback: GET_FIELD without field name string.
async fn pvinfo_no_field_name(opts: &PvGetOptions) -> Result<StructureDesc, PvGetError> {
    let (target, guid) = resolve_pv_server(opts).await?;
    let ChannelConn {
        mut stream,
        sid,
        version,
        is_be,
        mut reassembler,
        ..
    } = establish_channel(target, guid, opts).await?;

    let get_field = encode_get_field_request_without_field_name(sid, version, is_be);
    stream.write_all(&get_field).await?;

    let field_resp = read_until(&mut stream, opts.timeout, &mut reassembler, |cmd| {
        matches!(cmd, PvaPacketCommand::GetField(_))
    })
    .await?;
    let mut pkt = PvaPacket::new(&field_resp);
    let cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
        "get_field response decode failed".to_string(),
    ))?;
    match cmd {
        PvaPacketCommand::GetField(payload) => payload.introspection.ok_or_else(|| {
            let status_msg = payload
                .status
                .map(|s| format!("code={} message={}", s.code, s.message.unwrap_or_default()))
                .unwrap_or_else(|| "unknown error".to_string());
            PvGetError::Protocol(format!("get_field failed: {}", status_msg))
        }),
        _ => Err(PvGetError::Protocol(
            "unexpected get_field response".to_string(),
        )),
    }
}

async fn pvinfo_with_fallback(
    opts: &PvGetOptions,
    subfield: &str,
) -> Result<StructureDesc, PvGetError> {
    // Primary: high-level PvaClient::pvinfo
    let primary = high_level_pvinfo(opts).await;
    if let Ok(desc) = primary {
        return Ok(desc);
    }
    let primary_err = primary.unwrap_err();

    // Fallback 1: GET_FIELD without field name (legacy servers that crash on empty string)
    if subfield.is_empty() && should_retry_get_field_without_name(&primary_err) {
        match pvinfo_no_field_name(opts).await {
            Ok(desc) => return Ok(desc),
            Err(retry_err) => {
                if should_fallback_to_pvget(&retry_err) {
                    return pvget(opts)
                        .await
                        .map(|result| result.introspection)
                        .map_err(|get_err| {
                            PvGetError::Protocol(format!(
                                "pvinfo GET_FIELD failed ({}) and pvget fallback failed ({})",
                                retry_err, get_err
                            ))
                        });
                }
                return Err(retry_err);
            }
        }
    }

    // Fallback 2: pvget (for servers that don't support GET_FIELD at all)
    if should_fallback_to_pvget(&primary_err) {
        return pvget(opts)
            .await
            .map(|result| result.introspection)
            .map_err(|get_err| {
                PvGetError::Protocol(format!(
                    "pvinfo GET_FIELD failed ({}) and pvget fallback failed ({})",
                    primary_err, get_err
                ))
            });
    }

    Err(primary_err)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut pv_name = String::new();
    let mut subfield = String::new();
    let mut terse = false;
    let mut common = CommonClientArgs::new();

    {
        let mut ap = ArgumentParser::new();
        ap.set_description(
            "PVA pvinfo client - query PV type introspection (field names and types) \
             without fetching values. Uses the CMD_GET_FIELD (0x11) protocol command.",
        );
        ap.refer(&mut pv_name)
            .add_argument("pv", Store, "PV name to inspect");
        ap.refer(&mut subfield).add_option(
            &["--field", "-f"],
            Store,
            "Sub-field name to inspect (dot-separated path, e.g. 'value' or 'alarm.severity')",
        );
        common.add_to_parser(&mut ap);
        ap.refer(&mut terse).add_option(
            &["--terse", "-t"],
            StoreTrue,
            "Print compact one-line type summary instead of tree",
        );
        ap.parse_args_or_exit();
    }

    if pv_name.is_empty() {
        eprintln!("Error: PV name is required");
        std::process::exit(1);
    }

    common.init_tracing();
    let opts = common.into_pv_get_options(pv_name.clone())?;

    let rt = Runtime::new()?;
    let desc = rt.block_on(pvinfo_with_fallback(&opts, &subfield))?;

    // If a sub-field was requested, filter the result client-side
    let display_desc = if !subfield.is_empty() {
        match extract_subfield_desc(&desc, &subfield) {
            Some(sub) => sub,
            None => {
                // The server may have already filtered, so show what we got
                desc
            }
        }
    } else {
        desc
    };

    if terse {
        use spvirit_codec::spvd_decode::format_structure_desc;
        println!("{}: {}", pv_name, format_structure_desc(&display_desc));
    } else {
        println!("{}:", pv_name);
        println!("{}", format_structure_tree(&display_desc));
    }

    Ok(())
}
