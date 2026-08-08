use std::ops::ControlFlow;
use std::time::Duration;

use argparse::{ArgumentParser, List, Store, StoreTrue};
use hex::encode as hex_encode;
use tokio::io::AsyncWriteExt;
use tokio::runtime::Runtime;
use tokio::time::interval;

use spvirit_client::{MonitorOptions, client_from_opts};
use spvirit_codec::epics_decode::{DecodeMode, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvd_encode::{encode_pv_request, encode_pv_request_with_options};
use spvirit_codec::spvirit_encode::encode_control_message;
use spvirit_tools::spvirit_client::cli::CommonClientArgs;
use spvirit_tools::spvirit_client::client::{
    ChannelConn, encode_monitor_request, establish_channel,
};
use spvirit_tools::spvirit_client::format::{OutputFormat, RenderOptions, format_output};
use spvirit_tools::spvirit_client::search::resolve_pv_server;
use spvirit_tools::spvirit_client::transport::{read_packet, read_until};
use spvirit_tools::spvirit_client::types::{PvGetError, PvGetOptions};

/// High-level monitor path (no raw hex output).
async fn pvmonitor_high_level(
    opts: PvGetOptions,
    json: bool,
    fields: Vec<String>,
    pipeline: Option<u32>,
) -> Result<(), PvGetError> {
    let client = client_from_opts(&opts);

    let pv_name = opts.pv_name.clone();
    let mut render_opts = RenderOptions::default();
    if json {
        render_opts.format = OutputFormat::Json;
    }

    let cb = |update: &spvirit_client::MonitorUpdate| {
        println!("{}", format_output(&pv_name, &update.value, &render_opts));
        // Overruns go to stderr: stdout is compared byte-for-byte by
        // docs_verify and the interop tests.
        if update.has_overrun() {
            eprintln!(
                "{}: overrun on {}",
                pv_name,
                update.overrun_paths().join(", ")
            );
        }
        ControlFlow::Continue(())
    };
    let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
    if let Some(q) = pipeline {
        client
            .pvmonitor_with_options(&opts.pv_name, &refs, MonitorOptions::pipelined(q), cb)
            .await
    } else if fields.is_empty() {
        client.pvmonitor(&opts.pv_name, cb).await
    } else {
        client.pvmonitor_fields(&opts.pv_name, &refs, cb).await
    }
}

/// Low-level monitor path with raw hex output support.
async fn pvmonitor_raw(
    opts: PvGetOptions,
    json: bool,
    fields: Vec<String>,
    pipeline: Option<u32>,
) -> Result<(), PvGetError> {
    let target = resolve_pv_server(&opts).await?;

    let conn = establish_channel(target, &opts).await?;
    let ChannelConn {
        mut stream,
        sid,
        version,
        is_be,
        mut reassembler,
        ..
    } = conn;

    let ioid = 1u32;
    let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
    let (pv_request, init_subcmd, start_subcmd) = if let Some(q) = pipeline {
        let qs_str = q.to_string();
        let mut body = encode_pv_request_with_options(
            &refs,
            &[("pipeline", "true"), ("queueSize", qs_str.as_str())],
            is_be,
        );
        let qs_bytes = if is_be {
            q.to_be_bytes()
        } else {
            q.to_le_bytes()
        };
        body.extend_from_slice(&qs_bytes);
        (body, 0x08u8 | 0x80, 0x44u8 | 0x80)
    } else if fields.is_empty() {
        (vec![0xfd, 0x02, 0x00, 0x80, 0x00, 0x00], 0x08u8, 0x44u8)
    } else {
        (encode_pv_request(&refs, is_be), 0x08u8, 0x44u8)
    };
    let mon_init = encode_monitor_request(sid, ioid, init_subcmd, &pv_request, version, is_be);
    stream.write_all(&mon_init).await?;

    let init_resp = read_until(&mut stream, opts.timeout, &mut reassembler, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 13 && (op.subcmd & 0x08) != 0)
    })
    .await?;
    let mut pkt = PvaPacket::new(&init_resp);
    let cmd = pkt.decode_payload().ok_or(PvGetError::Protocol(
        "monitor init response decode failed".to_string(),
    ))?;

    let desc = match cmd {
        PvaPacketCommand::Op(op) => op
            .introspection
            .ok_or_else(|| PvGetError::Decode("missing introspection".to_string()))?,
        _ => {
            return Err(PvGetError::Protocol(
                "unexpected monitor init response".to_string(),
            ));
        }
    };

    let mon_start = encode_monitor_request(sid, ioid, start_subcmd, &[], version, is_be);
    stream.write_all(&mon_start).await?;

    let mut echo_interval = interval(Duration::from_secs(10));
    let mut echo_token: u32 = 1;
    let mut consumed_since_ack: u32 = 0;
    let ack_threshold: u32 = pipeline.map(|q| (q / 2).max(1)).unwrap_or(0);

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
                let mut pkt = PvaPacket::new(&bytes);
                let cmd = match pkt.decode_payload() {
                    Some(c) => c,
                    None => continue,
                };
                match cmd {
                    PvaPacketCommand::Op(mut op) => {
                        if op.command != 13 || (op.subcmd != 0x00 && op.subcmd != 0x10) {
                            continue;
                        }
                        // A decode failure leaves decoded_value None; skip the update.
                        let _ = op.decode_with_field_desc(&desc, is_be, DecodeMode::Strict);
                        if let Some(full) = op.decoded_value {
                            let mut render_opts = RenderOptions::default();
                            if json {
                                render_opts.format = OutputFormat::Json;
                            }
                            println!("{}", format_output(&opts.pv_name, &full, &render_opts));
                            println!("raw_pva: {}", hex_encode(&bytes));
                            println!("raw_pvd: {}", hex_encode(&op.body));
                        }
                        if pipeline.is_some() && op.subcmd == 0x00 {
                            consumed_since_ack = consumed_since_ack.saturating_add(1);
                            if consumed_since_ack >= ack_threshold {
                                let ack_bytes = if is_be {
                                    consumed_since_ack.to_be_bytes()
                                } else {
                                    consumed_since_ack.to_le_bytes()
                                };
                                let ack = encode_monitor_request(
                                    sid, ioid, 0x80, &ack_bytes, version, is_be,
                                );
                                let _ = stream.write_all(&ack).await;
                                consumed_since_ack = 0;
                            }
                        }
                        if op.subcmd == 0x10 {
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn pvmonitor(
    opts: PvGetOptions,
    raw: bool,
    json: bool,
    fields: Vec<String>,
    pipeline: Option<u32>,
) -> Result<(), PvGetError> {
    if raw {
        pvmonitor_raw(opts, json, fields, pipeline).await
    } else {
        pvmonitor_high_level(opts, json, fields, pipeline).await
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut pv_names: Vec<String> = Vec::new();
    let mut raw = false;
    let mut json = false;
    let mut pipeline_queue: u32 = 0;
    let mut common = CommonClientArgs::new();

    {
        let mut ap = ArgumentParser::new();
        ap.set_description("Minimal PVA pvmonitor client");
        ap.refer(&mut pv_names)
            .add_argument("pv", List, "PV name(s) to monitor");
        common.add_to_parser(&mut ap);
        ap.refer(&mut raw)
            .add_option(&["--raw"], StoreTrue, "Print raw hex payload");
        ap.refer(&mut json)
            .add_option(&["--json"], StoreTrue, "Print JSON output");
        ap.refer(&mut pipeline_queue).add_option(
            &["--pipeline"],
            Store,
            "Enable monitor pipelining with the given queueSize (0 = off)",
        );
        ap.parse_args_or_exit();
    }
    let pipeline: Option<u32> = if pipeline_queue > 0 {
        Some(pipeline_queue)
    } else {
        None
    };

    common.init_tracing();

    if pv_names.is_empty() {
        eprintln!("At least one PV name is required");
        std::process::exit(1);
    }

    let fields = common.fields_list();
    let base_opts = common.into_pv_get_options(String::new())?;

    let rt = Runtime::new()?;
    rt.block_on(async move {
        let mut set = tokio::task::JoinSet::new();
        for pv in pv_names {
            let mut opts = base_opts.clone();
            opts.pv_name = pv.clone();

            let pv_label = pv;
            let fields = fields.clone();
            set.spawn(async move {
                let res = pvmonitor(opts, raw, json, fields, pipeline).await;
                (pv_label, res)
            });
        }

        let mut had_error = false;
        while let Some(result) = set.join_next().await {
            match result {
                Ok((pv, Ok(()))) => {
                    eprintln!("pvmonitor {}: stopped", pv);
                }
                Ok((pv, Err(err))) => {
                    had_error = true;
                    eprintln!("pvmonitor {}: {}", pv, err);
                }
                Err(err) => {
                    had_error = true;
                    eprintln!("pvmonitor task error: {}", err);
                }
            }
        }

        if had_error {
            Err(PvGetError::Protocol(
                "one or more monitors failed".to_string(),
            ))
        } else {
            Ok(())
        }
    })?;
    Ok(())
}
