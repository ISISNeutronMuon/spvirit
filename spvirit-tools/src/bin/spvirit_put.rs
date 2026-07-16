use std::time::Duration;

use argparse::{ArgumentParser, Store, StoreOption, StoreTrue};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::runtime::Runtime;

use spvirit_client::pvput as high_level_pvput;
use spvirit_client::pvput_fields as high_level_pvput_fields;
use spvirit_codec::epics_decode::{PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::encode_header;
use spvirit_tools::spvirit_client::cli::CommonClientArgs;
use spvirit_tools::spvirit_client::client::{
    ChannelConn, encode_get_request, encode_put_request, ensure_status_ok, establish_channel,
};
use spvirit_tools::spvirit_client::put_encode::encode_put_payload;
use spvirit_tools::spvirit_client::search::resolve_pv_server;
use spvirit_tools::spvirit_client::transport::read_until;
use spvirit_tools::spvirit_client::types::{PvGetError, PvGetOptions};

fn parse_cli_value(raw: &str) -> Value {
    let lowered = raw.trim().to_ascii_lowercase();
    if lowered == "true" {
        return Value::Bool(true);
    }
    if lowered == "false" {
        return Value::Bool(false);
    }
    if raw.contains('.') || raw.contains('e') || raw.contains('E') {
        if let Ok(f) = raw.parse::<f64>() {
            return Value::Number(serde_json::Number::from_f64(f).unwrap());
        }
    }
    if let Ok(i) = raw.parse::<i64>() {
        return Value::Number(serde_json::Number::from(i));
    }
    Value::String(raw.to_string())
}

fn encode_destroy_request(sid: u32, request_id: u32, version: u8, is_be: bool) -> Vec<u8> {
    // destroyRequest (0x0F): serverChannelID then requestID, both i32.
    let mut payload = Vec::with_capacity(8);
    for word in [sid, request_id] {
        payload.extend_from_slice(&if is_be {
            word.to_be_bytes()
        } else {
            word.to_le_bytes()
        });
    }
    let mut out = encode_header(false, is_be, false, version, 15, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

async fn run_get_cycle(
    stream: &mut tokio::net::TcpStream,
    timeout: Duration,
    sid: u32,
    ioid: u32,
    version: u8,
    is_be: bool,
) -> Result<(), PvGetError> {
    let get_init = encode_get_request(
        sid,
        ioid,
        0x08,
        &[0xFD, 0x02, 0x00, 0x80, 0x00, 0x00],
        version,
        is_be,
    );
    stream.write_all(&get_init).await?;
    let get_init_resp = read_until(stream, timeout, |cmd| {
        matches!(
            cmd,
            PvaPacketCommand::Op(op) if op.command == 10 && (op.subcmd & 0x08) != 0
        )
    })
    .await?;
    ensure_status_ok(&get_init_resp, is_be, "get init")?;

    let get_data = encode_get_request(sid, ioid, 0x00, &[], version, is_be);
    stream.write_all(&get_data).await?;
    let get_data_resp = read_until(
        stream,
        timeout,
        |cmd| matches!(cmd, PvaPacketCommand::Op(op) if op.command == 10 && op.subcmd == 0x00),
    )
    .await?;
    ensure_status_ok(&get_data_resp, is_be, "get data")?;

    let destroy = encode_destroy_request(sid, ioid, version, is_be);
    stream.write_all(&destroy).await?;
    Ok(())
}

async fn pvput_full_flow(opts: &PvGetOptions, input: &Value) -> Result<(), PvGetError> {
    let target = resolve_pv_server(opts).await?;

    let conn = establish_channel(target, opts).await?;
    let ChannelConn {
        mut stream,
        sid,
        version,
        is_be,
        ..
    } = conn;

    run_get_cycle(&mut stream, opts.timeout, sid, 1u32, version, is_be).await?;

    let put_ioid = 2u32;

    let put_init = encode_put_request(
        sid,
        put_ioid,
        0x08,
        &[0xfd, 0x02, 0x00, 0x80, 0x00, 0x00],
        version,
        is_be,
    );
    stream.write_all(&put_init).await?;

    let init_resp = read_until(&mut stream, opts.timeout, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && (op.subcmd & 0x08) != 0)
    })
    .await?;
    let mut pkt = PvaPacket::new(&init_resp);
    let cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
        "put init response decode failed".to_string(),
    ))?;

    let desc = match cmd {
        PvaPacketCommand::Op(op) => {
            if let Some(status) = op.status {
                return Err(PvGetError::Protocol(format!(
                    "put init error: code={} message={}",
                    status.code,
                    status.message.unwrap_or_default()
                )));
            }
            op.introspection
                .ok_or_else(|| PvGetError::Decode("missing pvPutStructureIF".to_string()))?
        }
        _ => {
            return Err(PvGetError::Protocol(
                "unexpected put init response".to_string(),
            ));
        }
    };

    let payload = encode_put_payload(&desc, input, is_be).map_err(|e| PvGetError::Protocol(e))?;

    // EPICS-base-style probe/readback step before writing value.
    let put_get_req = encode_put_request(sid, put_ioid, 0x40, &[], version, is_be);
    stream.write_all(&put_get_req).await?;
    let put_get_resp = read_until(&mut stream, opts.timeout, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && (op.subcmd & 0x40) != 0)
    })
    .await?;
    ensure_status_ok(&put_get_resp, is_be, "put get-put")?;

    let put_req = encode_put_request(sid, put_ioid, 0x00, &payload, version, is_be);
    stream.write_all(&put_req).await?;

    let put_resp = read_until(
        &mut stream,
        opts.timeout,
        |cmd| matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && op.subcmd == 0x00),
    )
    .await?;

    ensure_status_ok(&put_resp, is_be, "put data")?;

    // Explicitly retire request lifecycle to mirror EPICS base traces.
    let destroy = encode_destroy_request(sid, put_ioid, version, is_be);
    stream.write_all(&destroy).await?;

    run_get_cycle(&mut stream, opts.timeout, sid, 3u32, version, is_be).await?;

    Ok(())
}

async fn pvput(
    opts: &PvGetOptions,
    input: &Value,
    simple_flow: bool,
    no_flow_fallback: bool,
    fields: &[String],
) -> Result<(), PvGetError> {
    let do_high_level = |opts: &PvGetOptions, input: &Value| {
        let opts = opts.clone();
        let input = input.clone();
        let fields = fields.to_vec();
        async move {
            if fields.is_empty() {
                high_level_pvput(&opts, input).await
            } else {
                let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
                high_level_pvput_fields(&opts, input, &refs).await
            }
        }
    };

    if simple_flow {
        do_high_level(opts, input).await?;
        println!("{} OK", opts.pv_name);
        return Ok(());
    }

    match pvput_full_flow(opts, input).await {
        Ok(()) => {
            println!("{} OK", opts.pv_name);
            Ok(())
        }
        Err(primary_err) => {
            if no_flow_fallback {
                return Err(primary_err);
            }

            do_high_level(opts, input).await?;
            println!("{} OK", opts.pv_name);
            Ok(())
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut pv_name = String::new();
    let mut value_arg: Option<String> = None;
    let mut json_arg: Option<String> = None;
    let mut simple_flow = false;
    let mut no_flow_fallback = false;
    let mut common = CommonClientArgs::new();

    {
        let mut ap = ArgumentParser::new();
        ap.set_description(
            "PVA pvput client (full EPICS-base-style flow by default)\n\n\
             Usage: pvput PV=VALUE  or  pvput PV VALUE  or  pvput PV --json '{...}'\n\
             Use the PV=VALUE form to write negative numbers (e.g. pvput COUNTER=-1)",
        );
        ap.refer(&mut pv_name).add_argument(
            "pv",
            Store,
            "PV name, or PV=VALUE to set a value (supports negative numbers)",
        );
        ap.refer(&mut value_arg).add_argument(
            "value",
            StoreOption,
            "Scalar value to write (positional, cannot start with -)",
        );
        ap.refer(&mut json_arg)
            .add_option(&["--json"], StoreOption, "JSON payload to write");
        common.add_to_parser(&mut ap);
        ap.refer(&mut simple_flow).add_option(
            &["--simple-flow"],
            StoreTrue,
            "Use minimal flow (init + write only; skip pre/post GET, get-put probe, and DESTROY_REQUEST)",
        );
        ap.refer(&mut no_flow_fallback).add_option(
            &["--no-flow-fallback"],
            StoreTrue,
            "Disable automatic retry from full flow to simple flow when full flow fails",
        );
        ap.parse_args_or_exit();
    }

    common.init_tracing();

    // Support PV=VALUE syntax: split on the first '=' in the pv_name argument.
    // This allows negative numbers (e.g. pvput COUNTER=-1) that would otherwise
    // be mis-parsed as flags.
    if value_arg.is_none() && json_arg.is_none() {
        if let Some(eq_pos) = pv_name.find('=') {
            let val = pv_name[eq_pos + 1..].to_string();
            pv_name.truncate(eq_pos);
            if !val.is_empty() {
                value_arg = Some(val);
            }
        }
    }

    let input = match (json_arg, value_arg) {
        (Some(json), None) => serde_json::from_str(&json)?,
        (None, Some(value)) => parse_cli_value(&value),
        (Some(_), Some(_)) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "provide either a scalar value or --json, not both",
            )
            .into());
        }
        (None, None) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "provide a scalar value or --json payload",
            )
            .into());
        }
    };

    let fields = common.fields_list();
    let opts = common.into_pv_get_options(pv_name.clone())?;

    let rt = Runtime::new()?;
    let result =
        rt.block_on(
            async move { pvput(&opts, &input, simple_flow, no_flow_fallback, &fields).await },
        );
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("{} ERROR {}", pv_name, e);
            Err(e.into())
        }
    }
}
