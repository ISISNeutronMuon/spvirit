use argparse::{ArgumentParser, Store};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use spvirit_codec::epics_decode::{PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::encode_client_connection_validation;
use spvirit_tools::spvirit_client::client::{encode_create_channel_request, encode_get_request};

#[derive(Debug)]
struct Frame {
    dir: String,
    bytes: Vec<u8>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut dump_file = String::new();
    let mut raw_file = String::new();
    {
        let mut ap = ArgumentParser::new();
        ap.set_description("Compare captured PVA GET frames to local encoders");
        ap.refer(&mut dump_file)
            .add_option(&["--dump-file"], Store, "Hex dump file to parse");
        ap.refer(&mut raw_file).add_option(
            &["--dump-raw"],
            Store,
            "Raw dump file (len+bytes) to parse",
        );
        ap.parse_args_or_exit();
    }

    let frames = if !raw_file.is_empty() {
        parse_raw_dump(&raw_file)?
    } else if !dump_file.is_empty() {
        parse_hex_dump(&dump_file)?
    } else {
        eprintln!("Provide --dump-raw or --dump-file");
        std::process::exit(1);
    };

    let mut create_req: Option<Vec<u8>> = None;
    let mut get_init: Option<Vec<u8>> = None;
    let mut get_data: Option<Vec<u8>> = None;
    let mut validation_req: Option<Vec<u8>> = None;

    for f in frames.iter().filter(|f| f.dir == "C->S") {
        let mut pkt = PvaPacket::new(&f.bytes);
        let cmd = match pkt.decode_payload() {
            Some(c) => c,
            None => continue,
        };
        match cmd {
            PvaPacketCommand::ConnectionValidation(_) => {
                if validation_req.is_none() {
                    validation_req = Some(f.bytes.clone());
                }
            }
            PvaPacketCommand::CreateChannel(payload) => {
                if !payload.channels.is_empty() && create_req.is_none() {
                    create_req = Some(f.bytes.clone());
                }
            }
            PvaPacketCommand::Op(op) => {
                if op.command == 10 && op.subcmd == 0x08 && get_init.is_none() {
                    get_init = Some(f.bytes.clone());
                } else if op.command == 10 && op.subcmd == 0x00 && get_data.is_none() {
                    get_data = Some(f.bytes.clone());
                }
            }
            _ => {}
        }
    }

    let validation_req = validation_req.ok_or("missing VALIDATION")?;
    let create_req = create_req.ok_or("missing CREATE_CHANNEL")?;
    let get_init = get_init.ok_or("missing GET init")?;
    let get_data = get_data.ok_or("missing GET data")?;

    compare_validation(&validation_req)?;
    compare_create_channel(&create_req)?;
    compare_get_request(&get_init, 0x08)?;
    compare_get_request(&get_data, 0x00)?;

    Ok(())
}

fn compare_create_channel(actual: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut pkt = PvaPacket::new(actual);
    let cmd = pkt.decode_payload().ok_or("decode create")?;
    let (cid, pv) = match cmd {
        PvaPacketCommand::CreateChannel(payload) => {
            let (cid, pv) = payload.channels.first().ok_or("missing channel")?;
            (*cid, pv.clone())
        }
        _ => return Err("unexpected command for create".into()),
    };
    let expected =
        encode_create_channel_request(cid, &pv, pkt.header.version, pkt.header.flags.is_msb);
    compare_bytes("CREATE_CHANNEL", actual, &expected);
    Ok(())
}

fn compare_validation(actual: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut pkt = PvaPacket::new(actual);
    let cmd = pkt.decode_payload().ok_or("decode validation")?;
    let (authz, user, host) = match cmd {
        PvaPacketCommand::ConnectionValidation(_) => {
            let authz = "ca".to_string();
            let user = std::env::var("PVA_AUTHNZ_USER")
                .or_else(|_| std::env::var("USER"))
                .or_else(|_| std::env::var("LOGNAME"))
                .unwrap_or_else(|_| "unknown".to_string());
            let host = std::env::var("PVA_AUTHNZ_HOST")
                .or_else(|_| std::env::var("HOSTNAME"))
                .or_else(|_| std::env::var("HOST"))
                .unwrap_or_else(|_| "unknown".to_string());
            (authz, user, host)
        }
        _ => return Err("unexpected command for validation".into()),
    };

    let expected = encode_client_connection_validation(
        87_040,
        32_767,
        0,
        &authz,
        &user,
        &host,
        pkt.header.version,
        pkt.header.flags.is_msb,
    );
    compare_bytes("VALIDATION", actual, &expected);
    Ok(())
}

fn compare_get_request(actual: &[u8], subcmd: u8) -> Result<(), Box<dyn std::error::Error>> {
    let mut pkt = PvaPacket::new(actual);
    let cmd = pkt.decode_payload().ok_or("decode get")?;
    let (sid, ioid) = match cmd {
        PvaPacketCommand::Op(op) => (op.sid_or_cid, op.ioid),
        _ => return Err("unexpected command for get".into()),
    };
    let extra: &[u8] = if subcmd == 0x08 {
        &[0xfd, 0x02, 0x00, 0x80, 0x00, 0x00]
    } else {
        &[]
    };
    let expected = encode_get_request(
        sid,
        ioid,
        subcmd,
        extra,
        pkt.header.version,
        pkt.header.flags.is_msb,
    );
    compare_bytes("GET", actual, &expected);
    Ok(())
}

fn compare_bytes(label: &str, actual: &[u8], expected: &[u8]) {
    if actual == expected {
        println!("{}: OK (len={})", label, actual.len());
        return;
    }
    let min_len = std::cmp::min(actual.len(), expected.len());
    let mut idx = 0usize;
    while idx < min_len && actual[idx] == expected[idx] {
        idx += 1;
    }
    println!(
        "{}: MISMATCH at offset {} (actual len={}, expected len={})",
        label,
        idx,
        actual.len(),
        expected.len()
    );
    if idx < min_len {
        println!(
            "  actual:   {:02x}  expected: {:02x}",
            actual[idx], expected[idx]
        );
    }
}

fn parse_raw_dump(path: &str) -> Result<Vec<Frame>, Box<dyn std::error::Error>> {
    let mut f = File::open(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= buf.len() {
        let len = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if offset + len > buf.len() {
            break;
        }
        let bytes = buf[offset..offset + len].to_vec();
        offset += len;
        out.push(Frame {
            dir: String::new(),
            bytes,
        });
    }
    // Raw dump doesn't include direction. Infer by command flags (server bit).
    for f in &mut out {
        let pkt = PvaPacket::new(&f.bytes);
        let is_server = pkt.header.flags.is_server;
        f.dir = if is_server { "S->C" } else { "C->S" }.to_string();
    }
    Ok(out)
}

fn parse_hex_dump(path: &str) -> Result<Vec<Frame>, Box<dyn std::error::Error>> {
    let file = File::open(Path::new(path))?;
    let reader = BufReader::new(file);
    let mut frames = Vec::new();
    let mut current_dir = String::new();
    let mut current_bytes: Vec<u8> = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.starts_with("C->S") || line.starts_with("S->C") {
            if !current_bytes.is_empty() {
                frames.push(Frame {
                    dir: current_dir.clone(),
                    bytes: current_bytes.clone(),
                });
                current_bytes.clear();
            }
            current_dir = if line.starts_with("C->S") {
                "C->S"
            } else {
                "S->C"
            }
            .to_string();
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !current_bytes.is_empty() {
                frames.push(Frame {
                    dir: current_dir.clone(),
                    bytes: current_bytes.clone(),
                });
                current_bytes.clear();
            }
            continue;
        }
        let mut parts = trimmed.split_whitespace().collect::<Vec<_>>();
        if parts.is_empty() {
            continue;
        }
        // Drop offset token (eg "0000")
        if parts[0].len() == 4 {
            parts.remove(0);
        }
        for p in parts {
            if p.len() != 2 {
                continue;
            }
            if let Ok(b) = u8::from_str_radix(p, 16) {
                current_bytes.push(b);
            }
        }
    }
    if !current_bytes.is_empty() {
        frames.push(Frame {
            dir: current_dir,
            bytes: current_bytes,
        });
    }
    Ok(frames)
}
