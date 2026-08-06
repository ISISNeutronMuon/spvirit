use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use argparse::{ArgumentParser, Store, StoreTrue};
use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{Level, debug, error, info};

use spvirit_tools::spvirit_server::db::load_db;
use spvirit_tools::spvirit_server::state::{ConnState, MonitorState, MonitorSub};
use spvirit_tools::spvirit_server::types::{
    LinkExpr, NtPayload, NtScalar, OutputMode, RecordData, RecordInstance, ScalarArrayValue,
    ScalarValue, ScanMode,
};

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvd_decode::{DecodedValue, PvdDecoder};
use spvirit_codec::spvd_decode::{FieldDesc, FieldType, StructureDesc, TypeCode};
use spvirit_codec::spvd_encode::{
    decode_pv_request_fields, encode_size_pvd, filter_structure_desc, nt_payload_desc,
};
use spvirit_codec::spvirit_encode::encode_control_message;
use spvirit_codec::spvirit_encode::{
    encode_beacon, encode_connection_validation, encode_create_channel_error,
    encode_create_channel_response, encode_get_field_error, encode_get_field_response,
    encode_header, encode_message_error, encode_monitor_data_response_payload, encode_op_error,
    encode_op_get_data_response_payload, encode_op_init_response_desc,
    encode_op_put_get_data_error_response, encode_op_put_get_data_response_payload,
    encode_op_put_get_init_error_response, encode_op_put_get_init_response,
    encode_op_put_getput_response_payload, encode_op_put_response, encode_op_put_status_response,
    encode_op_rpc_data_response_payload, encode_op_status_error_response,
    encode_op_status_response, encode_search_response, ip_from_bytes, ip_to_bytes,
};
use spvirit_codec::spvirit_encode::{
    encode_monitor_data_response_delta, encode_monitor_data_response_filtered,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PvListMode {
    Off,
    Discover,
    List,
}

impl PvListMode {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "discover" => Ok(Self::Discover),
            "list" => Ok(Self::List),
            other => Err(format!(
                "Invalid --pvlist-mode '{}'; expected off|discover|list",
                other
            )),
        }
    }
}

#[derive(Debug)]
struct ServerState {
    pv_store: RwLock<HashMap<String, RecordInstance>>,
    /// Last (value, alarm severity) posted to monitors per PV — the MDEL
    /// monitor-deadband reference point (EPICS MLST semantics).
    last_posted: Mutex<HashMap<String, (f64, i32)>>,
    monitors: Mutex<HashMap<String, Vec<MonitorSub>>>,
    conns: Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>,
    sid_counter: AtomicU32,
    beacon_change: AtomicU16,
    compute_alarms: bool,
    event_tx: mpsc::Sender<PostedEvent>,
    pvlist_mode: PvListMode,
    pvlist_max: usize,
    pvlist_allow_pattern: Option<Regex>,
    guid: [u8; 12],
    tcp_port: u16,
    advertise_ip: Option<IpAddr>,
    listen_ip: IpAddr,
}

#[derive(Debug, Clone)]
enum PostedEvent {
    Event(String),
    IoEvent(String),
}

impl ServerState {
    fn new(
        pv_store: HashMap<String, RecordInstance>,
        compute_alarms: bool,
        event_tx: mpsc::Sender<PostedEvent>,
        pvlist_mode: PvListMode,
        pvlist_max: usize,
        pvlist_allow_pattern: Option<Regex>,
        guid: [u8; 12],
        tcp_port: u16,
        advertise_ip: Option<IpAddr>,
        listen_ip: IpAddr,
    ) -> Self {
        // EPICS initialises MLST from the record's starting value, so even
        // the first change is subject to the MDEL deadband.
        let last_posted = pv_store
            .iter()
            .filter_map(|(name, record)| match record.to_ntpayload() {
                NtPayload::Scalar(nt) => {
                    scalar_as_f64(&nt.value).map(|f| (name.clone(), (f, nt.alarm_severity)))
                }
                _ => None,
            })
            .collect();
        Self {
            pv_store: RwLock::new(pv_store),
            last_posted: Mutex::new(last_posted),
            monitors: Mutex::new(HashMap::new()),
            conns: Mutex::new(HashMap::new()),
            sid_counter: AtomicU32::new(1),
            beacon_change: AtomicU16::new(0),
            compute_alarms,
            event_tx,
            pvlist_mode,
            pvlist_max,
            pvlist_allow_pattern,
            guid,
            tcp_port,
            advertise_ip,
            listen_ip,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut db_file = String::new();

    let mut listen_addr = String::from("0.0.0.0");
    let mut tcp_port: u16 = 5075;
    let mut udp_port: u16 = 5076;
    let mut reload_interval: u64 = 2;
    let mut debug_mode = false;
    let mut advertise_addr = String::new();
    let mut beacon_period: u64 = 15;
    let mut beacon_addr = String::from("224.0.0.128:5076");
    let mut compute_alarms = false;
    let mut pvlist_mode_raw = String::from("list");
    let mut pvlist_max: usize = 1024;
    let mut pvlist_allow_pattern = String::new();
    let mut conn_timeout: u64 = std::env::var("EPICS_PVA_CONN_TMO")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(64000); // 20 hours default

    {
        let mut ap = ArgumentParser::new();
        ap.set_description("Basic PVA server (NTScalar)");
        ap.refer(&mut db_file)
            .add_option(&["--db-file"], Store, "Path to EPICS DB file");

        ap.refer(&mut listen_addr)
            .add_option(&["--listen-addr"], Store, "Listen address");
        ap.refer(&mut tcp_port)
            .add_option(&["--tcp-port"], Store, "TCP port");
        ap.refer(&mut udp_port)
            .add_option(&["--udp-port"], Store, "UDP port");
        ap.refer(&mut reload_interval).add_option(
            &["--reload-interval"],
            Store,
            "DB reload interval in seconds",
        );
        ap.refer(&mut debug_mode)
            .add_option(&["--debug"], StoreTrue, "Enable debug logging");
        ap.refer(&mut advertise_addr).add_option(
            &["--advertise-addr"],
            Store,
            "Advertise address in search response",
        );
        ap.refer(&mut beacon_period).add_option(
            &["--beacon-period"],
            Store,
            "Beacon period in seconds",
        );
        ap.refer(&mut beacon_addr).add_option(
            &["--beacon-addr"],
            Store,
            "Beacon target address (ip:port)",
        );
        ap.refer(&mut conn_timeout).add_option(
            &["--conn-timeout"],
            Store,
            "Idle connection timeout in seconds",
        );
        ap.refer(&mut compute_alarms).add_option(
            &["--compute-alarms"],
            StoreTrue,
            "Compute alarm status from limits",
        );
        ap.refer(&mut pvlist_mode_raw).add_option(
            &["--pvlist-mode"],
            Store,
            "PV list mode: off|discover|list (default list)",
        );
        ap.refer(&mut pvlist_max).add_option(
            &["--pvlist-max"],
            Store,
            "Maximum PV names exposed by pvlist responses",
        );
        ap.refer(&mut pvlist_allow_pattern).add_option(
            &["--pvlist-allow-pattern"],
            Store,
            "Optional regex filter for PV names exposed by pvlist responses",
        );
        ap.parse_args_or_exit();
    }

    if db_file.trim().is_empty() {
        eprintln!("--db-file is required");
        std::process::exit(1);
    }

    if debug_mode {
        tracing_subscriber::fmt::fmt()
            .with_max_level(Level::DEBUG)
            .init();
    } else {
        tracing_subscriber::fmt::fmt()
            .with_max_level(Level::INFO)
            .init();
    }

    let pvlist_mode = PvListMode::parse(&pvlist_mode_raw)?;
    let pvlist_allow_pattern = if pvlist_allow_pattern.trim().is_empty() {
        None
    } else {
        Some(
            Regex::new(pvlist_allow_pattern.trim())
                .map_err(|e| format!("Invalid --pvlist-allow-pattern: {}", e))?,
        )
    };
    if pvlist_max == 0 {
        return Err("--pvlist-max must be greater than 0".into());
    }

    let mut pv_store = load_db(&db_file).map_err(|e| format!("DB load error: {}", e))?;

    if compute_alarms {
        for record in pv_store.values_mut() {
            if let RecordData::Ai { .. }
            | RecordData::Ao { .. }
            | RecordData::Bi { .. }
            | RecordData::Bo { .. }
            | RecordData::StringIn { .. }
            | RecordData::StringOut { .. } = &record.data
            {
                record.nt_mut().update_alarm_from_value();
            }
        }
    }
    let (event_tx, event_rx) = mpsc::channel::<PostedEvent>(256);
    info!("Loaded DB file '{}' with {} PVs", db_file, pv_store.len());

    let listen_ip: IpAddr = listen_addr
        .parse()
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));

    // If the user did not override --beacon-addr and the listen address is IPv6,
    // switch to the PVA IPv6 multicast group [ff02::42:1]:5076.
    if listen_ip.is_ipv6() && beacon_addr == "224.0.0.128:5076" {
        beacon_addr = String::from("[ff02::42:1]:5076");
    }

    let advertise_ip: Option<IpAddr> = if advertise_addr.trim().is_empty() {
        None
    } else {
        Some(
            advertise_addr
                .parse()
                .map_err(|e| format!("Invalid --advertise-addr: {}", e))?,
        )
    };

    let guid = rand_guid();

    let state = Arc::new(ServerState::new(
        pv_store,
        compute_alarms,
        event_tx,
        pvlist_mode,
        pvlist_max,
        pvlist_allow_pattern,
        guid,
        tcp_port,
        advertise_ip,
        listen_ip,
    ));
    process_pini_records(&state).await;

    let beacon_target: SocketAddr = match beacon_addr.parse() {
        Ok(addr) => addr,
        Err(_) => {
            let ip: IpAddr = beacon_addr
                .parse()
                .map_err(|e| format!("Invalid --beacon-addr: {}", e))?;
            SocketAddr::new(ip, udp_port)
        }
    };
    let tcp_addr = SocketAddr::new(listen_ip, tcp_port);
    let udp_addr = SocketAddr::new(listen_ip, udp_port);

    // Bind TCP before spawning anything.  An EADDRINUSE error here exits
    // before the beacon task is created, preventing ghost beacons.
    let tcp_listener = TcpListener::bind(tcp_addr).await?;

    let udp_state = state.clone();
    info!(
        "Starting PVA server: udp={} tcp={} reload={}s pvlist_mode={:?} pvlist_max={} filter={}",
        udp_addr,
        tcp_addr,
        reload_interval,
        state.pvlist_mode,
        state.pvlist_max,
        state
            .pvlist_allow_pattern
            .as_ref()
            .map(|r| r.as_str())
            .unwrap_or("<none>")
    );

    let udp_task = tokio::spawn(async move {
        if let Err(e) = run_udp_search(udp_state, udp_addr, tcp_port, guid, advertise_ip).await {
            error!("UDP search server error: {}", e);
        }
    });

    let tcp_state = state.clone();
    let tcp_task = tokio::spawn(async move {
        if let Err(e) =
            run_tcp_server(tcp_state, tcp_listener, Duration::from_secs(conn_timeout)).await
        {
            error!("TCP server error: {}", e);
        }
    });

    let reload_state = state.clone();
    let db_file_clone = db_file.clone();
    let reload_task = tokio::spawn(async move {
        if let Err(e) = run_db_reload(reload_state, db_file_clone, reload_interval).await {
            error!("DB reload error: {}", e);
        }
    });

    let event_state = state.clone();
    let event_task = tokio::spawn(async move {
        if let Err(e) = run_event_worker(event_state, event_rx).await {
            error!("Event worker error: {}", e);
        }
    });

    let scan_state = state.clone();
    let scan_task = tokio::spawn(async move {
        if let Err(e) = run_scan_scheduler(scan_state).await {
            error!("Scan scheduler error: {}", e);
        }
    });

    let beacon_state = state.clone();
    let beacon_task = tokio::spawn(async move {
        if let Err(e) = run_beacon(
            beacon_state,
            beacon_target,
            guid,
            tcp_port,
            advertise_ip,
            listen_ip,
            beacon_period,
        )
        .await
        {
            error!("Beacon task error: {}", e);
        }
    });

    let _ = tokio::join!(
        udp_task,
        tcp_task,
        reload_task,
        beacon_task,
        event_task,
        scan_task
    );
    Ok(())
}

async fn run_udp_search(
    state: Arc<ServerState>,
    addr: SocketAddr,
    tcp_port: u16,
    guid: [u8; 12],
    advertise_ip: Option<IpAddr>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Bind with SO_REUSEADDR/SO_REUSEPORT so co-located PVA consumers
    // (e.g. p4p on macOS) can also listen on the fixed search port.
    let socket = spvirit_server::handler::bind_udp_search_socket(addr)?;
    socket.set_broadcast(true)?;
    let mut buf = vec![0u8; 4096];

    loop {
        let (len, peer) = socket.recv_from(&mut buf).await?;
        let data = &buf[..len];
        let header = PvaHeader::new(data);
        // UDP endpoint only handles SEARCH (cmd=3) data messages.
        // Ignore other traffic early to avoid noisy "unknown command" decode logs.
        if header.flags.is_control || header.command != 3 {
            continue;
        }
        let mut pkt = PvaPacket::new(data);
        let Some(cmd) = pkt.decode_payload() else {
            continue;
        };
        let version = pkt.header.version;
        let is_be = pkt.header.flags.is_msb;
        match cmd {
            PvaPacketCommand::Search(payload) => {
                debug!(
                    "UDP search from {}: pv_count={} mask=0x{:02x}",
                    peer,
                    payload.pv_requests.len(),
                    payload.mask
                );
                let accepts_tcp = payload.protocols.is_empty()
                    || payload
                        .protocols
                        .iter()
                        .any(|p| p.eq_ignore_ascii_case("tcp"));
                if !accepts_tcp {
                    debug!("UDP search: no compatible protocol (tcp not accepted)");
                    continue;
                }
                let pv_store = state.pv_store.read().await;
                let visible_names = collect_visible_pv_names_from_store(
                    &pv_store,
                    state.pvlist_mode,
                    state.pvlist_allow_pattern.as_ref(),
                    state.pvlist_max,
                );
                let mut cids = Vec::new();
                for (cid, name) in &payload.pv_requests {
                    if pv_store.contains_key(name)
                        || resolve_field_nt(&pv_store, name).is_some()
                        || is_virtual_event_pv(name)
                        || (is_pvlist_virtual_pv(name) && state.pvlist_mode == PvListMode::List)
                        || (is_server_rpc_pv(name) && state.pvlist_mode != PvListMode::Off)
                    {
                        cids.push(*cid);
                        continue;
                    }
                    if state.pvlist_mode != PvListMode::Off
                        && is_pattern_query(name)
                        && visible_names.iter().any(|pv| wildcard_match(name, pv))
                    {
                        cids.push(*cid);
                    }
                }
                let response_required = (payload.mask & 0x01) != 0;
                let server_discovery_ping = payload.pv_requests.is_empty();
                let found = server_discovery_ping || !cids.is_empty();
                if !found && !response_required {
                    debug!("UDP search: no matches and response not required");
                    continue;
                }
                let resp_ip = if let Some(ip) = advertise_ip {
                    ip
                } else if !addr.ip().is_unspecified() {
                    addr.ip()
                } else if let Some(ip) = infer_udp_response_ip(peer) {
                    debug!("UDP search: inferred response address {}", ip);
                    ip
                } else {
                    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
                };
                let addr_bytes = if resp_ip.is_unspecified() {
                    debug!("UDP search: responding with zero address (unspecified listen)");
                    [0u8; 16]
                } else {
                    ip_to_bytes(resp_ip)
                };
                let response = encode_search_response(
                    guid,
                    payload.seq,
                    addr_bytes,
                    tcp_port,
                    "tcp",
                    found,
                    &cids,
                    version,
                    is_be,
                );
                let reply_target = search_reply_target(&payload.addr, payload.port, peer);
                if let Err(e) = socket.send_to(&response, reply_target).await {
                    debug!(
                        "UDP search: failed sending {} matches to {}: {}",
                        cids.len(),
                        reply_target,
                        e
                    );
                    continue;
                }
                debug!(
                    "UDP search: responded found={} with {} matches to {}",
                    found,
                    cids.len(),
                    reply_target
                );
            }
            _ => {}
        }
    }
}

async fn run_tcp_server(
    state: Arc<ServerState>,
    listener: TcpListener,
    conn_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn_id = Arc::new(AtomicU64::new(1));

    loop {
        let (stream, peer) = listener.accept().await?;
        let id = conn_id.fetch_add(1, Ordering::SeqCst);
        info!("TCP connection {} from {}", id, peer);
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(state_clone, stream, id, conn_timeout).await {
                error!("Connection {} error: {}", id, e);
            }
        });
    }
}

async fn handle_connection(
    state: Arc<ServerState>,
    stream: TcpStream,
    conn_id: u64,
    conn_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(128);

    {
        let mut conns = state.conns.lock().await;
        conns.insert(conn_id, tx);
    }

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if writer.write_all(&msg).await.is_err() {
                break;
            }
        }
    });

    let mut conn_state = ConnState::default();
    // Per EPICS PVA protocol: send SET_BYTE_ORDER control message before validation.
    let set_byte_order = encode_control_message(true, false, 2, 2, 0);
    validate_encoded_packet(conn_id, "set_byte_order", &set_byte_order);
    dump_hex_packet(
        conn_id,
        "tx",
        "ctrl=2 set_byte_order",
        2,
        false,
        &set_byte_order,
    );
    send_msg(&state, conn_id, set_byte_order).await;

    // Server sends Connection Validation (cmd=1) next.
    let server_validation =
        encode_connection_validation(16_384, 512, &["anonymous", "ca"], 2, false);
    validate_encoded_packet(conn_id, "server_validation_init", &server_validation);
    dump_hex_packet(
        conn_id,
        "tx",
        "cmd=1 server_validation_init",
        2,
        false,
        &server_validation,
    );
    send_msg(&state, conn_id, server_validation).await;

    let mut last_activity = Instant::now();

    loop {
        let mut header = [0u8; 8];
        let elapsed = last_activity.elapsed();
        if elapsed >= conn_timeout {
            info!("Conn {} idle timeout", conn_id);
            break;
        }
        let remaining = conn_timeout - elapsed;
        let read_header = tokio::time::timeout(remaining, reader.read_exact(&mut header)).await;
        match read_header {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => {
                info!("Conn {} idle timeout", conn_id);
                break;
            }
        }
        let header_pkt = PvaPacket::new(&header);
        let payload_len = if header_pkt.header.flags.is_control {
            0usize
        } else {
            header_pkt.header.payload_length as usize
        };
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            let elapsed = last_activity.elapsed();
            if elapsed >= conn_timeout {
                info!("Conn {} idle timeout", conn_id);
                break;
            }
            let remaining = conn_timeout - elapsed;
            let read_payload =
                tokio::time::timeout(remaining, reader.read_exact(&mut payload)).await;
            match read_payload {
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    info!("Conn {} idle timeout", conn_id);
                    break;
                }
            }
        }
        last_activity = Instant::now();
        let mut full = header.to_vec();
        full.extend_from_slice(&payload);
        if header_pkt.header.flags.is_segmented != 0 && !header_pkt.header.flags.is_control {
            debug!(
                "Conn {}: segmented message cmd={} seg=0x{:02x}",
                conn_id, header_pkt.header.command, header_pkt.header.flags.is_segmented
            );
            let mut payloads = vec![payload];
            let mut seg_flags = header_pkt.header.flags;
            while !seg_flags.is_last_segment {
                let mut seg_header = [0u8; 8];
                let elapsed = last_activity.elapsed();
                if elapsed >= conn_timeout {
                    info!("Conn {} idle timeout", conn_id);
                    break;
                }
                let remaining = conn_timeout - elapsed;
                let read_header =
                    tokio::time::timeout(remaining, reader.read_exact(&mut seg_header)).await;
                match read_header {
                    Ok(Ok(_)) => {}
                    Ok(Err(_)) => break,
                    Err(_) => {
                        info!("Conn {} idle timeout", conn_id);
                        break;
                    }
                }

                let seg_header_pkt = PvaPacket::new(&seg_header);
                let seg_payload_len = if seg_header_pkt.header.flags.is_control {
                    0usize
                } else {
                    seg_header_pkt.header.payload_length as usize
                };
                let mut seg_payload = vec![0u8; seg_payload_len];
                if seg_payload_len > 0 {
                    let elapsed = last_activity.elapsed();
                    if elapsed >= conn_timeout {
                        info!("Conn {} idle timeout", conn_id);
                        break;
                    }
                    let remaining = conn_timeout - elapsed;
                    let read_payload =
                        tokio::time::timeout(remaining, reader.read_exact(&mut seg_payload)).await;
                    match read_payload {
                        Ok(Ok(_)) => {}
                        Ok(Err(_)) => break,
                        Err(_) => {
                            info!("Conn {} idle timeout", conn_id);
                            break;
                        }
                    }
                }
                last_activity = Instant::now();

                if seg_header_pkt.header.flags.is_control {
                    handle_control_message(&state, conn_id, &seg_header_pkt.header).await;
                    continue;
                }
                if seg_header_pkt.header.flags.is_segmented == 0 {
                    debug!(
                        "Conn {}: segmented message interrupted by non-segmented cmd={}",
                        conn_id, seg_header_pkt.header.command
                    );
                    break;
                }
                payloads.push(seg_payload);
                seg_flags = seg_header_pkt.header.flags;
            }
            full = assemble_segmented_message(header, payloads);
        }
        let mut pkt = PvaPacket::new(&full);
        let Some(cmd) = pkt.decode_payload() else {
            continue;
        };
        let version = pkt.header.version;
        let is_be = pkt.header.flags.is_msb;
        let cmd_code = pkt.header.command;
        let payload_slice = if full.len() >= 8 { &full[8..] } else { &[] };

        if cmd_code == 1 {
            dump_hex_packet(conn_id, "rx", "cmd=1 validation", version, is_be, &full);
            let validation = spvirit_codec::epics_decode::PvaConnectionValidationPayload::new(
                payload_slice,
                is_be,
                false,
            );
            if let Some(val) = validation {
                debug!(
                    "Conn {}: validation request (cmd=1) ver={} be={} buf={} qos={} authz={:?}",
                    conn_id, version, is_be, val.buffer_size, val.qos, val.authz
                );
                // Empirically required by pvAccess clients: send CONNECTION_VALIDATED (cmd=9) status OK.
                let resp = spvirit_codec::spvirit_encode::encode_connection_validated(
                    true, version, is_be,
                );
                validate_encoded_packet(conn_id, "conn_validated_resp", &resp);
                dump_hex_packet(
                    conn_id,
                    "tx",
                    "cmd=9 connection_validated",
                    version,
                    is_be,
                    &resp,
                );
                send_msg(&state, conn_id, resp).await;
                continue;
            }
        }
        if cmd_code == 17 {
            dump_hex_packet(conn_id, "rx", "cmd=17 get_field", version, is_be, &full);
        }
        match cmd {
            PvaPacketCommand::Control(payload) => {
                debug!("Conn {}: control {}", conn_id, payload);
                if payload.command == 3 {
                    let resp = encode_control_message(true, is_be, version, 4, payload.data);
                    send_msg(&state, conn_id, resp).await;
                }
                continue;
            }
            PvaPacketCommand::ConnectionValidation(_) => {
                debug!("Conn {}: validation request (decoded)", conn_id);
            }
            PvaPacketCommand::ConnectionValidated(_) => {
                debug!("Conn {}: validation confirmed (decoded)", conn_id);
            }
            PvaPacketCommand::CreateChannel(payload) => {
                debug!(
                    "Conn {}: create_channel count={}",
                    conn_id,
                    payload.channels.len()
                );
                for (cid, pv_name) in payload.channels {
                    let pv_store = state.pv_store.read().await;
                    if pv_store.contains_key(&pv_name)
                        || resolve_field_nt(&pv_store, &pv_name).is_some()
                        || is_virtual_event_pv(&pv_name)
                        || (is_pvlist_virtual_pv(&pv_name) && state.pvlist_mode == PvListMode::List)
                        || (is_server_rpc_pv(&pv_name) && state.pvlist_mode != PvListMode::Off)
                    {
                        let sid = state.sid_counter.fetch_add(1, Ordering::SeqCst);
                        conn_state.cid_to_sid.insert(cid, sid);
                        conn_state.sid_to_pv.insert(sid, pv_name.clone());
                        let resp = encode_create_channel_response(cid, sid, version, is_be);
                        send_msg(&state, conn_id, resp).await;
                        info!(
                            "Conn {}: channel '{}' cid={} sid={}",
                            conn_id, pv_name, cid, sid
                        );
                    } else {
                        let resp = encode_create_channel_error(cid, "PV not found", version, is_be);
                        send_msg(&state, conn_id, resp).await;
                        info!(
                            "Conn {}: channel '{}' not found (cid={})",
                            conn_id, pv_name, cid
                        );
                    }
                }
            }
            PvaPacketCommand::Op(payload) => {
                if payload.is_server {
                    continue;
                }
                let sid = payload.sid_or_cid;
                let ioid = payload.ioid;
                debug!(
                    "Conn {}: op cmd={} ioid={} sid={} sub=0x{:02x} body_len={}",
                    conn_id,
                    payload.command,
                    ioid,
                    sid,
                    payload.subcmd,
                    payload.body.len()
                );
                let Some(pv_name) = conn_state.sid_to_pv.get(&sid).cloned() else {
                    send_msg(
                        &state,
                        conn_id,
                        encode_op_error(
                            payload.command,
                            payload.subcmd,
                            ioid,
                            "Unknown SID",
                            version,
                            is_be,
                        ),
                    )
                    .await;
                    continue;
                };

                let is_init = (payload.subcmd & 0x08) != 0;

                match payload.command {
                    10 => {
                        // GET
                        let Some(nt) = get_nt_snapshot(&state, &pv_name).await else {
                            send_msg(
                                &state,
                                conn_id,
                                encode_op_error(
                                    payload.command,
                                    payload.subcmd,
                                    ioid,
                                    "PV not found",
                                    version,
                                    is_be,
                                ),
                            )
                            .await;
                            continue;
                        };
                        if is_init {
                            let desc = nt_payload_desc(&nt);
                            conn_state.ioid_to_desc.insert(ioid, desc.clone());
                            conn_state.ioid_to_pv.insert(ioid, pv_name.clone());
                            let resp = encode_op_init_response_desc(
                                payload.command,
                                ioid,
                                0x08,
                                &desc,
                                version,
                                is_be,
                            );
                            send_msg(&state, conn_id, resp).await;
                            info!("Conn {}: get init pv='{}' ioid={}", conn_id, pv_name, ioid);
                        } else {
                            let resp =
                                encode_op_get_data_response_payload(ioid, &nt, version, is_be);
                            send_msg(&state, conn_id, resp).await;
                            debug!("Conn {}: get data pv='{}' ioid={}", conn_id, pv_name, ioid);
                        }
                    }
                    11 => {
                        // PUT
                        if is_init {
                            let Some(nt) = get_nt_snapshot(&state, &pv_name).await else {
                                send_msg(
                                    &state,
                                    conn_id,
                                    encode_op_error(
                                        payload.command,
                                        payload.subcmd,
                                        ioid,
                                        "PV not found",
                                        version,
                                        is_be,
                                    ),
                                )
                                .await;
                                continue;
                            };
                            if !is_virtual_event_pv(&pv_name)
                                && !is_writable_pv(&state, &pv_name).await
                            {
                                let resp = encode_op_put_status_response(
                                    ioid,
                                    0x08,
                                    "Write access denied",
                                    version,
                                    is_be,
                                );
                                send_msg(&state, conn_id, resp).await;
                                continue;
                            }
                            let desc = nt_payload_desc(&nt);
                            conn_state.ioid_to_desc.insert(ioid, desc.clone());
                            conn_state.ioid_to_pv.insert(ioid, pv_name.clone());
                            let resp = encode_op_init_response_desc(
                                payload.command,
                                ioid,
                                0x08,
                                &desc,
                                version,
                                is_be,
                            );
                            send_msg(&state, conn_id, resp).await;
                            info!("Conn {}: put init pv='{}' ioid={}", conn_id, pv_name, ioid);
                        } else {
                            if (payload.subcmd & 0x40) != 0 {
                                if !is_virtual_event_pv(&pv_name)
                                    && !is_writable_pv(&state, &pv_name).await
                                {
                                    let resp = encode_op_put_status_response(
                                        ioid,
                                        0x40,
                                        "Write access denied",
                                        version,
                                        is_be,
                                    );
                                    send_msg(&state, conn_id, resp).await;
                                    continue;
                                }
                                if let Some(nt) = get_nt_snapshot(&state, &pv_name).await {
                                    let resp = encode_op_put_getput_response_payload(
                                        ioid, &nt, version, is_be,
                                    );
                                    send_msg(&state, conn_id, resp).await;
                                    debug!(
                                        "Conn {}: put get-put pv='{}' ioid={}",
                                        conn_id, pv_name, ioid
                                    );
                                } else {
                                    send_msg(
                                        &state,
                                        conn_id,
                                        encode_op_error(
                                            payload.command,
                                            payload.subcmd,
                                            ioid,
                                            "PV not found",
                                            version,
                                            is_be,
                                        ),
                                    )
                                    .await;
                                }
                                continue;
                            }
                            let desc = match conn_state.ioid_to_desc.get(&ioid) {
                                Some(d) => d.clone(),
                                None => {
                                    send_msg(
                                        &state,
                                        conn_id,
                                        encode_op_error(
                                            payload.command,
                                            payload.subcmd,
                                            ioid,
                                            "PUT without init",
                                            version,
                                            is_be,
                                        ),
                                    )
                                    .await;
                                    continue;
                                }
                            };
                            let decoded = decode_put_body(&payload.body, &desc, is_be);
                            if let Some(value) = decoded.as_ref() {
                                match apply_put_and_process(&state, &pv_name, value).await {
                                    Ok((changed, force)) => {
                                        notify_changed_records_forced(&state, changed, &force)
                                            .await
                                    }
                                    Err(msg) => {
                                        let resp = encode_op_put_status_response(
                                            ioid, 0x00, &msg, version, is_be,
                                        );
                                        send_msg(&state, conn_id, resp).await;
                                        continue;
                                    }
                                }
                            } else {
                                debug!(
                                    "Conn {}: put decode failed ioid={} body_len={}",
                                    conn_id,
                                    ioid,
                                    payload.body.len()
                                );
                            }
                            let resp = encode_op_put_response(ioid, payload.subcmd, version, is_be);
                            send_msg(&state, conn_id, resp).await;
                            debug!("Conn {}: put data pv='{}' ioid={}", conn_id, pv_name, ioid);
                        }
                    }
                    12 => {
                        // PUT_GET
                        if is_init {
                            let Some(nt) = get_nt_snapshot(&state, &pv_name).await else {
                                send_msg(
                                    &state,
                                    conn_id,
                                    encode_op_error(
                                        payload.command,
                                        payload.subcmd,
                                        ioid,
                                        "PV not found",
                                        version,
                                        is_be,
                                    ),
                                )
                                .await;
                                continue;
                            };
                            if !is_virtual_event_pv(&pv_name)
                                && !is_writable_pv(&state, &pv_name).await
                            {
                                let resp = encode_op_put_get_init_error_response(
                                    ioid,
                                    "Write access denied",
                                    version,
                                    is_be,
                                );
                                send_msg(&state, conn_id, resp).await;
                                continue;
                            }
                            let desc = nt_payload_desc(&nt);
                            conn_state.ioid_to_desc.insert(ioid, desc.clone());
                            conn_state.ioid_to_pv.insert(ioid, pv_name.clone());
                            let resp =
                                encode_op_put_get_init_response(ioid, &desc, &desc, version, is_be);
                            send_msg(&state, conn_id, resp).await;
                            info!(
                                "Conn {}: put_get init pv='{}' ioid={}",
                                conn_id, pv_name, ioid
                            );
                        } else {
                            let desc = match conn_state.ioid_to_desc.get(&ioid) {
                                Some(d) => d.clone(),
                                None => {
                                    send_msg(
                                        &state,
                                        conn_id,
                                        encode_op_error(
                                            payload.command,
                                            payload.subcmd,
                                            ioid,
                                            "PUT_GET without init",
                                            version,
                                            is_be,
                                        ),
                                    )
                                    .await;
                                    continue;
                                }
                            };
                            let decoded = decode_put_body(&payload.body, &desc, is_be);
                            if let Some(value) = decoded.as_ref() {
                                match apply_put_and_process(&state, &pv_name, value).await {
                                    Ok((changed, force)) => {
                                        notify_changed_records_forced(&state, changed, &force)
                                            .await
                                    }
                                    Err(msg) => {
                                        let resp = encode_op_put_get_data_error_response(
                                            ioid, &msg, version, is_be,
                                        );
                                        send_msg(&state, conn_id, resp).await;
                                        continue;
                                    }
                                }
                            } else {
                                debug!(
                                    "Conn {}: put_get decode failed ioid={} body_len={}",
                                    conn_id,
                                    ioid,
                                    payload.body.len()
                                );
                            }
                            if let Some(nt) = get_nt_snapshot(&state, &pv_name).await {
                                let resp = encode_op_put_get_data_response_payload(
                                    ioid, &nt, version, is_be,
                                );
                                send_msg(&state, conn_id, resp).await;
                            } else {
                                send_msg(
                                    &state,
                                    conn_id,
                                    encode_op_error(
                                        payload.command,
                                        payload.subcmd,
                                        ioid,
                                        "PV not found",
                                        version,
                                        is_be,
                                    ),
                                )
                                .await;
                            }
                            debug!(
                                "Conn {}: put_get data pv='{}' ioid={}",
                                conn_id, pv_name, ioid
                            );
                        }
                    }
                    13 => {
                        // MONITOR
                        if is_init {
                            let Some(nt) = get_nt_snapshot(&state, &pv_name).await else {
                                send_msg(
                                    &state,
                                    conn_id,
                                    encode_op_error(
                                        payload.command,
                                        payload.subcmd,
                                        ioid,
                                        "PV not found",
                                        version,
                                        is_be,
                                    ),
                                )
                                .await;
                                continue;
                            };
                            let full_desc = nt_payload_desc(&nt);
                            let pv_req_fields = decode_pv_request_fields(&payload.body, is_be);
                            let desc = match &pv_req_fields {
                                Some(fields) => filter_structure_desc(&full_desc, fields),
                                None => full_desc,
                            };
                            conn_state.ioid_to_desc.insert(ioid, desc.clone());
                            conn_state.ioid_to_pv.insert(ioid, pv_name.clone());
                            let pipeline_enabled = (payload.subcmd & 0x80) != 0;
                            let mut nfree = 0u32;
                            if pipeline_enabled && payload.body.len() >= 4 {
                                let start = payload.body.len() - 4;
                                nfree = if is_be {
                                    u32::from_be_bytes([
                                        payload.body[start],
                                        payload.body[start + 1],
                                        payload.body[start + 2],
                                        payload.body[start + 3],
                                    ])
                                } else {
                                    u32::from_le_bytes([
                                        payload.body[start],
                                        payload.body[start + 1],
                                        payload.body[start + 2],
                                        payload.body[start + 3],
                                    ])
                                };
                            }
                            let resp = encode_op_init_response_desc(
                                payload.command,
                                ioid,
                                0x08,
                                &desc,
                                version,
                                is_be,
                            );
                            send_msg(&state, conn_id, resp).await;
                            conn_state.ioid_to_monitor.insert(
                                ioid,
                                MonitorState {
                                    running: false,
                                    pipeline_enabled,
                                    nfree,
                                },
                            );
                            let mut monitors = state.monitors.lock().await;
                            monitors
                                .entry(pv_name.clone())
                                .or_default()
                                .push(MonitorSub {
                                    conn_id,
                                    ioid,
                                    version,
                                    is_be,
                                    running: false,
                                    pipeline_enabled,
                                    nfree,
                                    filtered_desc: Some(desc.clone()),
                                    last_snapshot: None,
                                });
                            info!(
                                "Conn {}: monitor init pv='{}' ioid={}",
                                conn_id, pv_name, ioid
                            );
                        } else if (payload.subcmd & 0x10) != 0 {
                            if let Some(nt) = get_nt_snapshot(&state, &pv_name).await {
                                let resp = encode_monitor_data_response_payload(
                                    ioid, 0x10, &nt, version, is_be,
                                );
                                send_msg(&state, conn_id, resp).await;
                            }
                            remove_monitor_subscription(&state, conn_id, ioid, &pv_name).await;
                            conn_state.ioid_to_monitor.remove(&ioid);
                            conn_state.ioid_to_pv.remove(&ioid);
                            conn_state.ioid_to_desc.remove(&ioid);
                            info!("Conn {}: monitor end ioid={}", conn_id, ioid);
                        } else if (payload.subcmd & 0x04) != 0 || (payload.subcmd & 0x80) != 0 {
                            let start = (payload.subcmd & 0x44) == 0x44;
                            let stop = (payload.subcmd & 0x44) == 0x04;
                            let pipeline_ack = (payload.subcmd & 0x80) != 0;
                            let mut nfree = None;
                            if pipeline_ack && payload.body.len() >= 4 {
                                let v = if is_be {
                                    u32::from_be_bytes([
                                        payload.body[0],
                                        payload.body[1],
                                        payload.body[2],
                                        payload.body[3],
                                    ])
                                } else {
                                    u32::from_le_bytes([
                                        payload.body[0],
                                        payload.body[1],
                                        payload.body[2],
                                        payload.body[3],
                                    ])
                                };
                                nfree = Some(v);
                            }
                            let running = if start {
                                true
                            } else if stop {
                                false
                            } else {
                                conn_state
                                    .ioid_to_monitor
                                    .get(&ioid)
                                    .map(|m| m.running)
                                    .unwrap_or(true)
                            };
                            update_monitor_subscription(
                                &state,
                                conn_id,
                                ioid,
                                &pv_name,
                                running,
                                nfree,
                                Some(pipeline_ack),
                            )
                            .await;
                            if let Some(mon) = conn_state.ioid_to_monitor.get_mut(&ioid) {
                                mon.running = running;
                                if pipeline_ack {
                                    mon.pipeline_enabled = true;
                                }
                                if let Some(v) = nfree {
                                    if pipeline_ack {
                                        mon.nfree = mon.nfree.saturating_add(v);
                                    } else {
                                        mon.nfree = v;
                                    }
                                }
                            }
                            info!(
                                "Conn {}: monitor {} ioid={} ack={} nfree={:?}",
                                conn_id,
                                if start {
                                    "start"
                                } else if stop {
                                    "stop"
                                } else {
                                    "ack"
                                },
                                ioid,
                                pipeline_ack,
                                nfree
                            );
                            if start {
                                if let Some(nt) = get_nt_snapshot(&state, &pv_name).await {
                                    send_monitor_update_for(&state, &pv_name, conn_id, ioid, &nt)
                                        .await;
                                }
                            }
                        }
                    }
                    20 => {
                        if is_server_rpc_pv(&pv_name) {
                            handle_server_rpc(
                                &state,
                                conn_id,
                                ioid,
                                payload.subcmd,
                                version,
                                is_be,
                            )
                            .await;
                        } else {
                            send_msg(
                                &state,
                                conn_id,
                                encode_op_error(
                                    payload.command,
                                    payload.subcmd,
                                    ioid,
                                    "Operation not supported",
                                    version,
                                    is_be,
                                ),
                            )
                            .await;
                        }
                    }
                    14 | 16 => {
                        send_msg(
                            &state,
                            conn_id,
                            encode_op_error(
                                payload.command,
                                payload.subcmd,
                                ioid,
                                "Operation not supported",
                                version,
                                is_be,
                            ),
                        )
                        .await;
                    }
                    _ => {
                        send_msg(
                            &state,
                            conn_id,
                            encode_op_error(
                                payload.command,
                                payload.subcmd,
                                ioid,
                                "Operation not supported",
                                version,
                                is_be,
                            ),
                        )
                        .await;
                    }
                }
            }
            PvaPacketCommand::DestroyChannel(payload) => {
                let sid = payload.sid;
                let cid = payload.cid;
                conn_state.cid_to_sid.remove(&cid);
                conn_state.sid_to_pv.remove(&sid);
                info!(
                    "Conn {}: channel destroyed sid={} cid={}",
                    conn_id, sid, cid
                );
            }
            PvaPacketCommand::DestroyRequest(payload) => {
                let ioid = payload.request_id;
                if let Some(pv_name) = conn_state.ioid_to_pv.remove(&ioid) {
                    remove_monitor_subscription(&state, conn_id, ioid, &pv_name).await;
                    conn_state.ioid_to_desc.remove(&ioid);
                    conn_state.ioid_to_monitor.remove(&ioid);
                    info!("Conn {}: monitor unsubscribed ioid={}", conn_id, ioid);
                }
            }
            PvaPacketCommand::AuthNZ(_) => {
                // Silently accept — pvxs and pvAccessCPP ignore AUTHNZ.
                debug!("Conn {}: ignoring AUTHNZ", conn_id);
            }
            PvaPacketCommand::AclChange(_) => {
                let resp =
                    encode_message_error("ACL_CHANGE command is not supported", version, is_be);
                send_msg(&state, conn_id, resp).await;
            }
            PvaPacketCommand::GetField(payload) => {
                handle_get_field_request(&state, &conn_state, conn_id, payload, version, is_be)
                    .await;
            }
            PvaPacketCommand::Echo(payload_bytes) => {
                // ECHO keepalive: echo back the same payload
                let mut resp =
                    encode_header(true, is_be, false, version, 2, payload_bytes.len() as u32);
                resp.extend_from_slice(&payload_bytes);
                send_msg(&state, conn_id, resp).await;
            }
            PvaPacketCommand::Message(_) => {
                let resp = encode_message_error("MESSAGE command is not supported", version, is_be);
                send_msg(&state, conn_id, resp).await;
            }
            PvaPacketCommand::MultipleData(_) => {
                let resp =
                    encode_message_error("MULTIPLE_DATA command is not supported", version, is_be);
                send_msg(&state, conn_id, resp).await;
            }
            PvaPacketCommand::CancelRequest(_) => {
                let resp =
                    encode_message_error("CANCEL_REQUEST command is not supported", version, is_be);
                send_msg(&state, conn_id, resp).await;
            }
            PvaPacketCommand::OriginTag(_) => {
                let resp =
                    encode_message_error("ORIGIN_TAG command is not supported", version, is_be);
                send_msg(&state, conn_id, resp).await;
            }
            PvaPacketCommand::Search(payload) => {
                debug!(
                    "Conn {}: TCP search: pv_count={} mask=0x{:02x}",
                    conn_id,
                    payload.pv_requests.len(),
                    payload.mask
                );
                let accepts_tcp = payload.protocols.is_empty()
                    || payload
                        .protocols
                        .iter()
                        .any(|p| p.eq_ignore_ascii_case("tcp"));
                if accepts_tcp {
                    let pv_store = state.pv_store.read().await;
                    let visible_names = collect_visible_pv_names_from_store(
                        &pv_store,
                        state.pvlist_mode,
                        state.pvlist_allow_pattern.as_ref(),
                        state.pvlist_max,
                    );
                    let mut cids = Vec::new();
                    for (cid, name) in &payload.pv_requests {
                        if pv_store.contains_key(name)
                            || resolve_field_nt(&pv_store, name).is_some()
                            || is_virtual_event_pv(name)
                            || (is_pvlist_virtual_pv(name) && state.pvlist_mode == PvListMode::List)
                            || (is_server_rpc_pv(name) && state.pvlist_mode != PvListMode::Off)
                        {
                            cids.push(*cid);
                            continue;
                        }
                        if state.pvlist_mode != PvListMode::Off
                            && is_pattern_query(name)
                            && visible_names.iter().any(|pv| wildcard_match(name, pv))
                        {
                            cids.push(*cid);
                        }
                    }
                    drop(pv_store);
                    let server_discovery_ping = payload.pv_requests.is_empty();
                    let found = server_discovery_ping || !cids.is_empty();
                    let resp_ip = state.advertise_ip.unwrap_or(state.listen_ip);
                    let addr_bytes = if resp_ip.is_unspecified() {
                        [0u8; 16]
                    } else {
                        ip_to_bytes(resp_ip)
                    };
                    let response = encode_search_response(
                        state.guid,
                        payload.seq,
                        addr_bytes,
                        state.tcp_port,
                        "tcp",
                        found,
                        &cids,
                        version,
                        is_be,
                    );
                    send_msg(&state, conn_id, response).await;
                    debug!(
                        "Conn {}: TCP search responded found={} matches={}",
                        conn_id,
                        found,
                        cids.len()
                    );
                } else {
                    debug!("Conn {}: TCP search: no compatible protocol", conn_id);
                }
            }
            PvaPacketCommand::SearchResponse(_) | PvaPacketCommand::Beacon(_) => {
                let resp =
                    encode_message_error("Unexpected command for server endpoint", version, is_be);
                send_msg(&state, conn_id, resp).await;
            }
            PvaPacketCommand::Unknown(payload) => {
                let resp = encode_message_error(
                    &format!("Unknown command {}", payload.command),
                    version,
                    is_be,
                );
                send_msg(&state, conn_id, resp).await;
            }
        }
    }

    cleanup_connection(&state, conn_id).await;
    let _ = writer_task.await;
    Ok(())
}

async fn cleanup_connection(state: &Arc<ServerState>, conn_id: u64) {
    let mut conns = state.conns.lock().await;
    conns.remove(&conn_id);

    let mut monitors = state.monitors.lock().await;
    for subs in monitors.values_mut() {
        subs.retain(|s| s.conn_id != conn_id);
    }
}

async fn run_db_reload(
    state: Arc<ServerState>,
    db_file: String,
    interval_secs: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut last_mtime = file_mtime(&db_file).unwrap_or(SystemTime::UNIX_EPOCH);
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

    loop {
        interval.tick().await;
        let current_mtime = match file_mtime(&db_file) {
            Some(t) => t,
            None => continue,
        };
        if current_mtime <= last_mtime {
            continue;
        }
        last_mtime = current_mtime;

        let mut new_store = match load_db(&db_file) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to reload DB: {}", e);
                continue;
            }
        };
        if state.compute_alarms {
            for record in new_store.values_mut() {
                if let RecordData::Ai { .. }
                | RecordData::Ao { .. }
                | RecordData::Bi { .. }
                | RecordData::Bo { .. }
                | RecordData::StringIn { .. }
                | RecordData::StringOut { .. } = &record.data
                {
                    record.nt_mut().update_alarm_from_value();
                }
            }
        }
        info!(
            "Reloaded DB file '{}' with {} PVs",
            db_file,
            new_store.len()
        );

        let mut changed: Vec<(String, NtPayload)> = Vec::new();
        {
            let mut store = state.pv_store.write().await;
            for (name, new_val) in &new_store {
                let old_val = store.get(name);
                let new_payload = new_val.to_ntpayload();
                if old_val.map(|v| v.to_ntpayload()) != Some(new_payload.clone()) {
                    changed.push((name.clone(), new_payload));
                }
            }
            *store = new_store;
        }

        for (name, payload) in changed {
            info!("PV changed: {}", name);
            state.beacon_change.fetch_add(1, Ordering::SeqCst);
            notify_monitors(&state, &name, &payload).await;
        }
    }
}

async fn run_beacon(
    state: Arc<ServerState>,
    target: SocketAddr,
    guid: [u8; 12],
    tcp_port: u16,
    advertise_ip: Option<IpAddr>,
    listen_ip: IpAddr,
    period_secs: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    if period_secs == 0 {
        return Ok(());
    }
    // Bind the beacon socket to the same address family as the target
    let bind_addr = if target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind_addr).await?;
    socket.set_broadcast(true)?;
    let mut interval = tokio::time::interval(Duration::from_secs(period_secs));
    let mut seq: u8 = 0;

    loop {
        interval.tick().await;
        let resp_ip = if let Some(ip) = advertise_ip {
            ip
        } else if !listen_ip.is_unspecified() {
            listen_ip
        } else {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        };
        let addr_bytes = if resp_ip.is_unspecified() {
            [0u8; 16]
        } else {
            ip_to_bytes(resp_ip)
        };
        let change_count = state.beacon_change.load(Ordering::SeqCst);
        let msg = encode_beacon(
            guid,
            seq,
            change_count,
            addr_bytes,
            tcp_port,
            "tcp",
            2,
            false,
        );
        let _ = socket.send_to(&msg, target).await;
        seq = seq.wrapping_add(1);
    }
}

async fn notify_monitors(state: &Arc<ServerState>, pv_name: &str, payload: &NtPayload) {
    let mut to_send: Vec<(u64, Vec<u8>)> = Vec::new();
    {
        let mut monitors = state.monitors.lock().await;
        if let Some(list) = monitors.get_mut(pv_name) {
            for sub in list.iter_mut() {
                if !sub.running {
                    continue;
                }
                if sub.pipeline_enabled && sub.nfree == 0 {
                    continue;
                }
                let Some(msg) = build_monitor_frame(sub, payload) else {
                    continue;
                };
                if sub.pipeline_enabled && sub.nfree > 0 {
                    sub.nfree -= 1;
                }
                sub.last_snapshot = Some(payload.clone());
                to_send.push((sub.conn_id, msg));
            }
        }
    }

    for (conn_id, msg) in to_send {
        send_msg(state, conn_id, msg).await;
        debug!("Monitor update pv='{}' conn={} ", pv_name, conn_id);
    }
}

fn build_monitor_frame(sub: &MonitorSub, payload: &NtPayload) -> Option<Vec<u8>> {
    let subcmd = 0x00;
    let Some(prev) = sub.last_snapshot.as_ref() else {
        let bytes = if let Some(ref desc) = sub.filtered_desc {
            encode_monitor_data_response_filtered(
                sub.ioid,
                subcmd,
                payload,
                desc,
                sub.version,
                sub.is_be,
            )
        } else {
            encode_monitor_data_response_payload(sub.ioid, subcmd, payload, sub.version, sub.is_be)
        };
        return Some(bytes);
    };
    if let Some(ref desc) = sub.filtered_desc {
        encode_monitor_data_response_delta(
            sub.ioid,
            subcmd,
            prev,
            payload,
            desc,
            sub.version,
            sub.is_be,
        )
    } else if prev == payload {
        None
    } else {
        Some(encode_monitor_data_response_payload(
            sub.ioid,
            subcmd,
            payload,
            sub.version,
            sub.is_be,
        ))
    }
}

async fn send_monitor_update_for(
    state: &Arc<ServerState>,
    pv_name: &str,
    conn_id: u64,
    ioid: u32,
    payload: &NtPayload,
) {
    let mut to_send: Option<(u64, Vec<u8>)> = None;
    {
        let mut monitors = state.monitors.lock().await;
        if let Some(list) = monitors.get_mut(pv_name) {
            if let Some(sub) = list
                .iter_mut()
                .find(|s| s.conn_id == conn_id && s.ioid == ioid)
            {
                if !sub.running {
                    return;
                }
                if sub.pipeline_enabled && sub.nfree == 0 {
                    return;
                }
                let Some(msg) = build_monitor_frame(sub, payload) else {
                    return;
                };
                if sub.pipeline_enabled && sub.nfree > 0 {
                    sub.nfree -= 1;
                }
                sub.last_snapshot = Some(payload.clone());
                to_send = Some((sub.conn_id, msg));
            }
        }
    }

    if let Some((conn_id, msg)) = to_send {
        send_msg(state, conn_id, msg).await;
    }
}

async fn update_monitor_subscription(
    state: &Arc<ServerState>,
    conn_id: u64,
    ioid: u32,
    pv_name: &str,
    running: bool,
    nfree: Option<u32>,
    pipeline_enabled: Option<bool>,
) -> bool {
    let mut monitors = state.monitors.lock().await;
    if let Some(list) = monitors.get_mut(pv_name) {
        if let Some(sub) = list
            .iter_mut()
            .find(|s| s.conn_id == conn_id && s.ioid == ioid)
        {
            sub.running = running;
            if let Some(v) = nfree {
                sub.nfree = v;
            }
            if let Some(enabled) = pipeline_enabled {
                if enabled {
                    sub.pipeline_enabled = true;
                }
            }
            return true;
        }
    }
    false
}

async fn remove_monitor_subscription(
    state: &Arc<ServerState>,
    conn_id: u64,
    ioid: u32,
    pv_name: &str,
) {
    let mut monitors = state.monitors.lock().await;
    if let Some(list) = monitors.get_mut(pv_name) {
        list.retain(|s| s.conn_id != conn_id || s.ioid != ioid);
    }
}

async fn send_msg(state: &Arc<ServerState>, conn_id: u64, msg: Vec<u8>) {
    let conns = state.conns.lock().await;
    if let Some(tx) = conns.get(&conn_id) {
        let _ = tx.send(msg).await;
    }
}

fn is_pvlist_virtual_pv(pv_name: &str) -> bool {
    pv_name == "__pvlist"
}

fn is_server_rpc_pv(pv_name: &str) -> bool {
    pv_name == "server"
}

fn is_virtual_event_pv(pv_name: &str) -> bool {
    pv_name.starts_with("__event:")
}

fn is_pattern_query(raw: &str) -> bool {
    raw.contains('*') || raw.contains('?')
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let mut i = 0usize;
    let mut j = 0usize;
    let mut star: Option<usize> = None;
    let mut match_j = 0usize;

    while j < t.len() {
        if i < p.len() && (p[i] == b'?' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == b'*' {
            star = Some(i);
            i += 1;
            match_j = j;
        } else if let Some(star_idx) = star {
            i = star_idx + 1;
            match_j += 1;
            j = match_j;
        } else {
            return false;
        }
    }

    while i < p.len() && p[i] == b'*' {
        i += 1;
    }
    i == p.len()
}

fn collect_visible_pv_names_from_store(
    pv_store: &HashMap<String, RecordInstance>,
    mode: PvListMode,
    allow_pattern: Option<&Regex>,
    max_items: usize,
) -> Vec<String> {
    let mut names: Vec<String> = pv_store
        .keys()
        .filter(|name| {
            allow_pattern
                .as_ref()
                .map(|re| re.is_match(name))
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    names.sort();
    if names.len() > max_items {
        names.truncate(max_items);
    }
    if mode == PvListMode::List && names.len() < max_items {
        names.push("__pvlist".to_string());
    }
    names
}

fn build_pvlist_structure(names: &[String]) -> StructureDesc {
    StructureDesc {
        struct_id: Some("epics:pva/pvlist:1.0".to_string()),
        fields: names
            .iter()
            .map(|name| FieldDesc {
                name: name.clone(),
                field_type: FieldType::Scalar(TypeCode::Boolean),
            })
            .collect(),
    }
}

fn requested_pvlist_pattern(field_name: Option<&str>) -> Option<&str> {
    let raw = field_name.map(str::trim).unwrap_or("");
    if raw.is_empty() || raw == "*" || raw == "__pvlist" || raw.eq_ignore_ascii_case("pvlist") {
        return Some("*");
    }
    if is_pattern_query(raw) {
        return Some(raw);
    }
    None
}

async fn handle_get_field_request(
    state: &Arc<ServerState>,
    conn_state: &ConnState,
    conn_id: u64,
    payload: spvirit_codec::epics_decode::PvaGetFieldPayload,
    version: u8,
    is_be: bool,
) {
    if payload.is_server {
        let resp = encode_get_field_error(
            payload.cid,
            "Unexpected server GET_FIELD payload",
            version,
            is_be,
        );
        send_msg(state, conn_id, resp).await;
        return;
    }

    let request_id = payload.ioid.unwrap_or(payload.cid);

    let sid = payload
        .sid
        .or_else(|| conn_state.cid_to_sid.get(&payload.cid).copied())
        .or_else(|| {
            conn_state
                .sid_to_pv
                .contains_key(&payload.cid)
                .then_some(payload.cid)
        })
        .or_else(|| {
            (payload.cid == 0 && conn_state.sid_to_pv.len() == 1)
                .then(|| conn_state.sid_to_pv.keys().copied().next())
                .flatten()
        });

    if let Some(sid) = sid {
        if let Some(pv_name) = conn_state.sid_to_pv.get(&sid) {
            if let Some(nt) = get_nt_snapshot(state, pv_name).await {
                let full_desc = nt_payload_desc(&nt);
                // If a sub-field name was requested, filter the introspection
                let sub = payload.field_name.as_deref().filter(|s| !s.is_empty());
                let desc = if let Some(field_path) = sub {
                    use spvirit_codec::spvd_decode::extract_subfield_desc;
                    match extract_subfield_desc(&full_desc, field_path) {
                        Some(sub_desc) => sub_desc,
                        None => {
                            let resp = encode_get_field_error(
                                request_id,
                                &format!("sub-field '{}' not found", field_path),
                                version,
                                is_be,
                            );
                            send_msg(state, conn_id, resp).await;
                            return;
                        }
                    }
                } else {
                    full_desc
                };
                let resp = encode_get_field_response(request_id, &desc, version, is_be);
                dump_hex_packet(conn_id, "tx", "cmd=17 get_field", version, is_be, &resp);
                send_msg(state, conn_id, resp).await;
                debug!(
                    "Conn {}: get_field cid={} sid={:?} ioid={:?} resolved_sid={} pv='{}' field={:?}",
                    conn_id,
                    payload.cid,
                    payload.sid,
                    payload.ioid,
                    sid,
                    pv_name,
                    payload.field_name
                );
                return;
            }
            let resp = encode_get_field_error(request_id, "PV not found", version, is_be);
            send_msg(state, conn_id, resp).await;
            return;
        }
    }

    if state.pvlist_mode != PvListMode::List {
        let resp = encode_get_field_error(
            request_id,
            "GET_FIELD listing is disabled (set --pvlist-mode=list)",
            version,
            is_be,
        );
        send_msg(state, conn_id, resp).await;
        return;
    }

    let Some(pattern) = requested_pvlist_pattern(payload.field_name.as_deref()) else {
        let resp = encode_get_field_error(
            request_id,
            "GET_FIELD requires a valid list pattern",
            version,
            is_be,
        );
        send_msg(state, conn_id, resp).await;
        return;
    };

    let pv_store = state.pv_store.read().await;
    let mut names = collect_visible_pv_names_from_store(
        &pv_store,
        state.pvlist_mode,
        state.pvlist_allow_pattern.as_ref(),
        state.pvlist_max,
    );
    if pattern != "*" {
        names.retain(|name| wildcard_match(pattern, name));
    }
    if names.is_empty() {
        let resp =
            encode_get_field_error(request_id, "No PVs matched list request", version, is_be);
        send_msg(state, conn_id, resp).await;
        return;
    }
    let desc = build_pvlist_structure(&names);
    let resp = encode_get_field_response(request_id, &desc, version, is_be);
    dump_hex_packet(
        conn_id,
        "tx",
        "cmd=17 get_field_list",
        version,
        is_be,
        &resp,
    );
    send_msg(state, conn_id, resp).await;
    debug!(
        "Conn {}: get_field list pattern='{}' returned {} entries",
        conn_id,
        pattern,
        names.len()
    );
}

async fn handle_server_rpc(
    state: &Arc<ServerState>,
    conn_id: u64,
    ioid: u32,
    subcmd: u8,
    version: u8,
    is_be: bool,
) {
    if state.pvlist_mode != PvListMode::List {
        let resp = encode_op_status_error_response(
            20,
            ioid,
            subcmd,
            "RPC list endpoint disabled (set --pvlist-mode=list)",
            version,
            is_be,
        );
        send_msg(state, conn_id, resp).await;
        return;
    }

    let names = {
        let store = state.pv_store.read().await;
        collect_visible_pv_names_from_store(
            &store,
            state.pvlist_mode,
            state.pvlist_allow_pattern.as_ref(),
            state.pvlist_max,
        )
    };
    let payload = NtPayload::ScalarArray(
        spvirit_tools::spvirit_server::types::NtScalarArray::from_value(ScalarArrayValue::Str(
            names,
        )),
    );

    let is_init = (subcmd & 0x08) != 0;
    if is_init {
        let resp = encode_op_status_response(20, ioid, subcmd, version, is_be);
        send_msg(state, conn_id, resp).await;
        return;
    }

    let resp = encode_op_rpc_data_response_payload(ioid, subcmd, &payload, version, is_be);
    send_msg(state, conn_id, resp).await;
}

fn parse_virtual_event(pv_name: &str) -> Option<PostedEvent> {
    let suffix = pv_name.strip_prefix("__event:")?;
    if let Some(io_name) = suffix.strip_prefix("io:") {
        Some(PostedEvent::IoEvent(io_name.to_string()))
    } else {
        Some(PostedEvent::Event(suffix.to_string()))
    }
}

fn virtual_event_nt(pv_name: &str) -> NtPayload {
    NtPayload::Scalar(
        NtScalar::from_value(ScalarValue::Bool(false))
            .with_description(format!("Virtual event trigger for {}", pv_name)),
    )
}

fn virtual_pvlist_nt(entries: Vec<String>) -> NtPayload {
    NtPayload::ScalarArray(
        spvirit_tools::spvirit_server::types::NtScalarArray::from_value(ScalarArrayValue::Str(
            entries,
        )),
    )
}

/// Resolve an IOC-style `<record>.<FIELD>[$]` reference against the store.
fn resolve_field_nt(store: &HashMap<String, RecordInstance>, pv_name: &str) -> Option<NtPayload> {
    use spvirit_tools::spvirit_server::record_fields::{parse_field_ref, payload_for};
    let field_ref = parse_field_ref(pv_name)?;
    let record = store.get(&field_ref.base)?;
    payload_for(record, &field_ref)
}

/// Numeric view of a scalar value, for MDEL deadband arithmetic.
fn scalar_as_f64(v: &ScalarValue) -> Option<f64> {
    Some(match v {
        ScalarValue::I8(x) => *x as f64,
        ScalarValue::I16(x) => *x as f64,
        ScalarValue::I32(x) => *x as f64,
        ScalarValue::I64(x) => *x as f64,
        ScalarValue::U8(x) => *x as f64,
        ScalarValue::U16(x) => *x as f64,
        ScalarValue::U32(x) => *x as f64,
        ScalarValue::U64(x) => *x as f64,
        ScalarValue::F32(x) => *x as f64,
        ScalarValue::F64(x) => *x,
        ScalarValue::Bool(_) | ScalarValue::Str(_) => return None,
    })
}

async fn get_nt_snapshot(state: &Arc<ServerState>, pv_name: &str) -> Option<NtPayload> {
    if is_pvlist_virtual_pv(pv_name) {
        if state.pvlist_mode != PvListMode::List {
            return None;
        }
        let store = state.pv_store.read().await;
        let names = collect_visible_pv_names_from_store(
            &store,
            state.pvlist_mode,
            state.pvlist_allow_pattern.as_ref(),
            state.pvlist_max,
        );
        return Some(virtual_pvlist_nt(names));
    }
    if is_virtual_event_pv(pv_name) {
        return Some(virtual_event_nt(pv_name));
    }
    let store = state.pv_store.read().await;
    store
        .get(pv_name)
        .map(|r| r.to_ntpayload())
        .or_else(|| resolve_field_nt(&store, pv_name))
}

async fn is_writable_pv(state: &Arc<ServerState>, pv_name: &str) -> bool {
    if is_virtual_event_pv(pv_name) {
        return true;
    }
    let store = state.pv_store.read().await;
    store.get(pv_name).map(|r| r.writable()).unwrap_or(false)
}

async fn process_pini_records(state: &Arc<ServerState>) {
    let mut startup: Vec<(i32, String)> = {
        let store = state.pv_store.read().await;
        store
            .iter()
            .filter_map(|(name, record)| {
                if record.common.pini {
                    Some((record.common.phas, name.clone()))
                } else {
                    None
                }
            })
            .collect()
    };
    startup.sort_by_key(|(phas, _)| *phas);

    for (_, name) in startup {
        let changed = process_single_record(state, &name).await;
        notify_changed_records(state, changed).await;
    }
}

async fn run_scan_scheduler(state: Arc<ServerState>) -> Result<(), Box<dyn std::error::Error>> {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    let mut next_due: HashMap<String, Instant> = HashMap::new();
    loop {
        interval.tick().await;
        let now = Instant::now();
        let periodic: Vec<(String, Duration)> = {
            let store = state.pv_store.read().await;
            store
                .iter()
                .filter_map(|(name, record)| match &record.common.scan {
                    ScanMode::Periodic(period) => Some((name.clone(), *period)),
                    _ => None,
                })
                .collect()
        };
        let mut due: Vec<String> = Vec::new();
        let periodic_names: HashSet<String> =
            periodic.iter().map(|(name, _)| name.clone()).collect();
        for (name, period) in periodic {
            let entry = next_due.entry(name.clone()).or_insert_with(|| now + period);
            if now >= *entry {
                due.push(name);
                *entry = now + period;
            }
        }
        next_due.retain(|name, _| periodic_names.contains(name));
        for name in due {
            let changed = process_single_record(&state, &name).await;
            notify_changed_records(&state, changed).await;
        }
    }
}

async fn run_event_worker(
    state: Arc<ServerState>,
    mut event_rx: mpsc::Receiver<PostedEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    while let Some(event) = event_rx.recv().await {
        let targets: Vec<String> = {
            let store = state.pv_store.read().await;
            store
                .iter()
                .filter_map(|(name, record)| match (&record.common.scan, &event) {
                    (ScanMode::Event(ev), PostedEvent::Event(trigger)) if ev == trigger => {
                        Some(name.clone())
                    }
                    (ScanMode::IoEvent(ev), PostedEvent::IoEvent(trigger)) if ev == trigger => {
                        Some(name.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        for target in targets {
            let changed = process_single_record(&state, &target).await;
            notify_changed_records(&state, changed).await;
        }
    }
    Ok(())
}

async fn post_event(state: &Arc<ServerState>, event: &str) {
    let _ = state
        .event_tx
        .send(PostedEvent::Event(event.to_string()))
        .await;
}

async fn post_io_event(state: &Arc<ServerState>, event: &str) {
    let _ = state
        .event_tx
        .send(PostedEvent::IoEvent(event.to_string()))
        .await;
}

async fn process_single_record(
    state: &Arc<ServerState>,
    pv_name: &str,
) -> Vec<(String, NtPayload)> {
    let mut store = state.pv_store.write().await;
    let mut active: HashSet<String> = HashSet::new();
    let mut changed_names: HashSet<String> = HashSet::new();
    process_record_by_name(
        &mut store,
        pv_name,
        state.compute_alarms,
        &mut active,
        &mut changed_names,
    );
    changed_names
        .into_iter()
        .filter_map(|name| store.get(&name).map(|record| (name, record.to_ntpayload())))
        .collect()
}

async fn apply_put_and_process(
    state: &Arc<ServerState>,
    pv_name: &str,
    value: &DecodedValue,
) -> Result<(Vec<(String, NtPayload)>, HashSet<String>), String> {
    if let Some(event) = parse_virtual_event(pv_name) {
        match event {
            PostedEvent::Event(name) => post_event(state, &name).await,
            PostedEvent::IoEvent(name) => post_io_event(state, &name).await,
        }
        return Ok((Vec::new(), HashSet::new()));
    }

    let mut store = state.pv_store.write().await;
    let Some(record) = store.get_mut(pv_name) else {
        return Err("PV not found".to_string());
    };
    if !record.writable() {
        return Err("Write access denied".to_string());
    }
    let mut changed_names: HashSet<String> = HashSet::new();
    let mut force_post: HashSet<String> = HashSet::new();
    let outcome = record.apply_put(value, state.compute_alarms);
    if outcome.value_changed {
        changed_names.insert(pv_name.to_string());
    }
    if outcome.client_stamped {
        // A client-supplied acquisition time is new information even when the
        // value is identical: post it past the MDEL gate.
        changed_names.insert(pv_name.to_string());
        force_post.insert(pv_name.to_string());
    }
    let mut active: HashSet<String> = HashSet::new();
    process_record_by_name(
        &mut store,
        pv_name,
        state.compute_alarms,
        &mut active,
        &mut changed_names,
    );
    let records = changed_names
        .into_iter()
        .filter_map(|name| store.get(&name).map(|record| (name, record.to_ntpayload())))
        .collect();
    Ok((records, force_post))
}

async fn notify_changed_records(state: &Arc<ServerState>, changed: Vec<(String, NtPayload)>) {
    notify_changed_records_forced(state, changed, &HashSet::new()).await;
}

/// As [`notify_changed_records`], but PVs named in `force` post even when the
/// MDEL deadband would suppress them — used for client-stamped PUTs, where the
/// value may be unchanged but the timestamp is new information.
async fn notify_changed_records_forced(
    state: &Arc<ServerState>,
    changed: Vec<(String, NtPayload)>,
    force: &HashSet<String>,
) {
    for (name, payload) in changed {
        if !force.contains(&name) && !should_post_update(state, &name, &payload).await {
            continue;
        }
        state.beacon_change.fetch_add(1, Ordering::SeqCst);
        notify_monitors(state, &name, &payload).await;
    }
}

/// MDEL monitor-deadband gate (EPICS semantics): a numeric-scalar update is
/// suppressed when MDEL > 0, the alarm severity did not change, and the value
/// moved less than MDEL from the last *posted* value. Posting records the new
/// reference point.
async fn should_post_update(state: &Arc<ServerState>, name: &str, payload: &NtPayload) -> bool {
    let NtPayload::Scalar(nt) = payload else {
        return true;
    };
    let Some(new_f) = scalar_as_f64(&nt.value) else {
        return true;
    };
    let mdel = {
        let store = state.pv_store.read().await;
        match store.get(name) {
            Some(record) => spvirit_tools::spvirit_server::record_fields::mdel_of(record),
            None => return true,
        }
    };
    let mut last_posted = state.last_posted.lock().await;
    let within_deadband = mdel > 0.0
        && last_posted.get(name).is_some_and(|(last_f, last_sevr)| {
            *last_sevr == nt.alarm_severity && (new_f - last_f).abs() < mdel
        });
    if within_deadband {
        return false;
    }
    last_posted.insert(name.to_string(), (new_f, nt.alarm_severity));
    true
}

fn process_record_by_name(
    store: &mut HashMap<String, RecordInstance>,
    pv_name: &str,
    compute_alarms: bool,
    active: &mut HashSet<String>,
    changed_names: &mut HashSet<String>,
) {
    if active.contains(pv_name) {
        return;
    }
    let Some(existing) = store.get(pv_name) else {
        return;
    };
    let before = existing.to_ntpayload();
    if existing.common.pact {
        return;
    }

    active.insert(pv_name.to_string());
    if let Some(record) = store.get_mut(pv_name) {
        record.common.pact = true;
    }

    let (disa, sdis, diss, flnk) = {
        let record = match store.get(pv_name) {
            Some(v) => v,
            None => return,
        };
        (
            record.common.disa,
            record.common.sdis.clone(),
            record.common.diss,
            record.common.flnk.clone(),
        )
    };

    let mut disabled = disa;
    if !disabled {
        if let Some(link) = sdis.as_ref() {
            if let Some(ScalarValue::Bool(true)) =
                read_link_value(store, link, compute_alarms, active, changed_names)
            {
                disabled = true;
            }
        }
    }
    if disabled {
        if let Some(record) = store.get_mut(pv_name) {
            let nt = record.nt_mut();
            nt.alarm_status = 16;
            nt.alarm_severity = diss;
        }
    } else {
        process_record_body(store, pv_name, compute_alarms, active, changed_names);
    }

    if let Some(record) = store.get_mut(pv_name) {
        record.common.pact = false;
    }
    let after = store
        .get(pv_name)
        .map(|record| record.to_ntpayload())
        .unwrap_or(before.clone());
    if before != after {
        changed_names.insert(pv_name.to_string());
    }
    if let Some(LinkExpr::DbLink { target, .. }) = flnk {
        let linked = normalize_link_target(&target);
        process_record_by_name(store, &linked, compute_alarms, active, changed_names);
    }
    active.remove(pv_name);
}

fn process_record_body(
    store: &mut HashMap<String, RecordInstance>,
    pv_name: &str,
    compute_alarms: bool,
    active: &mut HashSet<String>,
    changed_names: &mut HashSet<String>,
) {
    let Some(record_snapshot) = store.get(pv_name).cloned() else {
        return;
    };

    match record_snapshot.data {
        RecordData::Ai {
            inp,
            siml,
            siol,
            simm,
            ..
        } => {
            let chosen = if simm {
                siol.as_ref().or(inp.as_ref())
            } else {
                inp.as_ref()
            };
            if let Some(link) = chosen {
                if let Some(value) =
                    read_link_value(store, link, compute_alarms, active, changed_names)
                {
                    if let Some(v) = scalar_to_f64(&value) {
                        if let Some(record) = store.get_mut(pv_name) {
                            record.set_scalar_value(ScalarValue::F64(v), compute_alarms);
                        }
                    }
                }
            } else if simm {
                let _ = siml;
            }
        }
        RecordData::Ao {
            out,
            dol,
            omsl,
            drvl,
            drvh,
            oroc,
            siol,
            simm,
            ..
        } => {
            let mut next = store
                .get(pv_name)
                .and_then(|rec| scalar_to_f64(&rec.current_value()))
                .unwrap_or(0.0);
            if matches!(omsl, OutputMode::ClosedLoop) {
                if let Some(link) = dol.as_ref() {
                    if let Some(value) =
                        read_link_value(store, link, compute_alarms, active, changed_names)
                    {
                        if let Some(v) = scalar_to_f64(&value) {
                            next = v;
                        }
                    }
                }
            }
            if let Some(min) = drvl {
                if next < min {
                    next = min;
                }
            }
            if let Some(max) = drvh {
                if next > max {
                    next = max;
                }
            }
            let prev = store
                .get(pv_name)
                .and_then(|rec| scalar_to_f64(&rec.current_value()))
                .unwrap_or(next);
            if let Some(limit) = oroc {
                if limit > 0.0 {
                    let delta = next - prev;
                    if delta.abs() > limit {
                        next = prev + delta.signum() * limit;
                    }
                }
            }
            if let Some(record) = store.get_mut(pv_name) {
                record.set_scalar_value(ScalarValue::F64(next), compute_alarms);
            }
            let write_link = if simm {
                siol.as_ref().or(out.as_ref())
            } else {
                out.as_ref()
            };
            if let Some(link) = write_link {
                write_link_value(
                    store,
                    link,
                    ScalarValue::F64(next),
                    compute_alarms,
                    active,
                    changed_names,
                );
            }
        }
        RecordData::Bi {
            inp, siol, simm, ..
        } => {
            let chosen = if simm {
                siol.as_ref().or(inp.as_ref())
            } else {
                inp.as_ref()
            };
            if let Some(link) = chosen {
                if let Some(value) =
                    read_link_value(store, link, compute_alarms, active, changed_names)
                {
                    if let Some(v) = scalar_to_bool(&value) {
                        if let Some(record) = store.get_mut(pv_name) {
                            record.set_scalar_value(ScalarValue::Bool(v), compute_alarms);
                        }
                    }
                }
            }
        }
        RecordData::Bo {
            out,
            dol,
            omsl,
            siol,
            simm,
            ..
        } => {
            let mut next = store
                .get(pv_name)
                .and_then(|rec| scalar_to_bool(&rec.current_value()))
                .unwrap_or(false);
            if matches!(omsl, OutputMode::ClosedLoop) {
                if let Some(link) = dol.as_ref() {
                    if let Some(value) =
                        read_link_value(store, link, compute_alarms, active, changed_names)
                    {
                        if let Some(v) = scalar_to_bool(&value) {
                            next = v;
                        }
                    }
                }
            }
            if let Some(record) = store.get_mut(pv_name) {
                record.set_scalar_value(ScalarValue::Bool(next), compute_alarms);
            }
            let write_link = if simm {
                siol.as_ref().or(out.as_ref())
            } else {
                out.as_ref()
            };
            if let Some(link) = write_link {
                write_link_value(
                    store,
                    link,
                    ScalarValue::Bool(next),
                    compute_alarms,
                    active,
                    changed_names,
                );
            }
        }
        RecordData::StringIn {
            inp, siol, simm, ..
        } => {
            let chosen = if simm {
                siol.as_ref().or(inp.as_ref())
            } else {
                inp.as_ref()
            };
            if let Some(link) = chosen {
                if let Some(value) =
                    read_link_value(store, link, compute_alarms, active, changed_names)
                {
                    if let Some(v) = scalar_to_string(&value) {
                        if let Some(record) = store.get_mut(pv_name) {
                            record.set_scalar_value(ScalarValue::Str(v), compute_alarms);
                        }
                    }
                }
            }
        }
        RecordData::StringOut {
            out,
            dol,
            omsl,
            siol,
            simm,
            ..
        } => {
            let mut next = store
                .get(pv_name)
                .and_then(|rec| scalar_to_string(&rec.current_value()))
                .unwrap_or_default();
            if matches!(omsl, OutputMode::ClosedLoop) {
                if let Some(link) = dol.as_ref() {
                    if let Some(value) =
                        read_link_value(store, link, compute_alarms, active, changed_names)
                    {
                        if let Some(v) = scalar_to_string(&value) {
                            next = v;
                        }
                    }
                }
            }
            if let Some(record) = store.get_mut(pv_name) {
                record.set_scalar_value(ScalarValue::Str(next.clone()), compute_alarms);
            }
            let write_link = if simm {
                siol.as_ref().or(out.as_ref())
            } else {
                out.as_ref()
            };
            if let Some(link) = write_link {
                write_link_value(
                    store,
                    link,
                    ScalarValue::Str(next),
                    compute_alarms,
                    active,
                    changed_names,
                );
            }
        }
        RecordData::Waveform { inp, .. } | RecordData::Aai { inp, .. } => {
            if let Some(link) = inp.as_ref() {
                if let Some(arr) =
                    read_link_array_value(store, link, compute_alarms, active, changed_names)
                {
                    if let Some(record) = store.get_mut(pv_name) {
                        set_record_array_value(record, arr);
                    }
                }
            }
        }
        RecordData::Aao { out, dol, omsl, .. } => {
            let mut next = store.get(pv_name).and_then(record_array_value).unwrap_or(
                // Keep default deterministic in case parser left this empty.
                ScalarArrayValue::F64(Vec::new()),
            );
            if matches!(omsl, OutputMode::ClosedLoop) {
                if let Some(link) = dol.as_ref() {
                    if let Some(arr) =
                        read_link_array_value(store, link, compute_alarms, active, changed_names)
                    {
                        next = arr;
                    }
                }
            }
            if let Some(record) = store.get_mut(pv_name) {
                set_record_array_value(record, next.clone());
            }
            if let Some(link) = out.as_ref() {
                write_link_array_value(store, link, next, compute_alarms, active, changed_names);
            }
        }
        RecordData::SubArray {
            inp,
            indx,
            nelm,
            malm,
            ..
        } => {
            let source = if let Some(link) = inp.as_ref() {
                read_link_array_value(store, link, compute_alarms, active, changed_names)
            } else {
                store.get(pv_name).and_then(record_array_value)
            };
            if let Some(source) = source {
                let max_len = if malm > 0 { malm } else { nelm };
                let requested = if nelm > 0 { nelm } else { max_len };
                let sliced = slice_array_value(&source, indx, requested.min(max_len.max(1)));
                if let Some(record) = store.get_mut(pv_name) {
                    set_record_array_value(record, sliced);
                }
            }
        }
        RecordData::NtTable { inp, out, omsl, .. } => {
            if let Some(link) = inp.as_ref() {
                if let Some(arr) =
                    read_link_array_value(store, link, compute_alarms, active, changed_names)
                {
                    if let Some(record) = store.get_mut(pv_name) {
                        set_record_array_value(record, arr);
                    }
                }
            }
            if matches!(omsl, OutputMode::ClosedLoop) {
                // closed-loop NtTable: intentionally no-op for now (table DOL not supported)
            }
            if let Some(link) = out.as_ref() {
                if let Some(record) = store.get(pv_name) {
                    if let NtPayload::Table(nt) = record.data.payload() {
                        for col in &nt.columns {
                            write_link_array_value(
                                store,
                                link,
                                col.values.clone(),
                                compute_alarms,
                                active,
                                changed_names,
                            );
                        }
                    }
                }
            }
        }
        RecordData::NtNdArray { inp, out, omsl, .. } => {
            if let Some(link) = inp.as_ref() {
                if let Some(arr) =
                    read_link_array_value(store, link, compute_alarms, active, changed_names)
                {
                    if let Some(record) = store.get_mut(pv_name) {
                        set_record_array_value(record, arr);
                    }
                }
            }
            if matches!(omsl, OutputMode::ClosedLoop) {
                // closed-loop NtNdArray: intentionally no-op for now (ndarray DOL not supported)
            }
            if let Some(link) = out.as_ref() {
                if let Some(record) = store.get(pv_name) {
                    if let NtPayload::NdArray(nt) = record.data.payload() {
                        write_link_array_value(
                            store,
                            link,
                            nt.value.clone(),
                            compute_alarms,
                            active,
                            changed_names,
                        );
                    }
                }
            }
        }
        RecordData::NtEnum { .. } | RecordData::Generic { .. } => {
            // NtEnum and Generic records do not participate in link processing.
        }
    }
}

fn normalize_link_target(raw: &str) -> String {
    raw.split('.').next().unwrap_or(raw).trim().to_string()
}

fn read_link_value(
    store: &mut HashMap<String, RecordInstance>,
    link: &LinkExpr,
    compute_alarms: bool,
    active: &mut HashSet<String>,
    changed_names: &mut HashSet<String>,
) -> Option<ScalarValue> {
    match link {
        LinkExpr::Constant(value) => Some(value.clone()),
        LinkExpr::DbLink {
            target,
            process_passive,
            ..
        } => {
            let resolved = normalize_link_target(target);
            if *process_passive {
                process_record_by_name(store, &resolved, compute_alarms, active, changed_names);
            }
            store.get(&resolved).map(|record| record.current_value())
        }
    }
}

fn record_array_value(record: &RecordInstance) -> Option<ScalarArrayValue> {
    match record.to_ntpayload() {
        NtPayload::ScalarArray(nt) => Some(nt.value),
        NtPayload::NdArray(nt) => Some(nt.value),
        _ => None,
    }
}

fn set_record_array_value(record: &mut RecordInstance, value: ScalarArrayValue) {
    match &mut record.data {
        RecordData::Waveform { nt, nord, .. }
        | RecordData::Aai { nt, nord, .. }
        | RecordData::Aao { nt, nord, .. }
        | RecordData::SubArray { nt, nord, .. } => {
            *nord = value.len();
            nt.value = value;
        }
        RecordData::NtNdArray { nt, .. } => {
            nt.value = value;
        }
        _ => {}
    }
}

fn read_link_array_value(
    store: &mut HashMap<String, RecordInstance>,
    link: &LinkExpr,
    compute_alarms: bool,
    active: &mut HashSet<String>,
    changed_names: &mut HashSet<String>,
) -> Option<ScalarArrayValue> {
    match link {
        LinkExpr::Constant(_) => None,
        LinkExpr::DbLink {
            target,
            process_passive,
            ..
        } => {
            let resolved = normalize_link_target(target);
            if *process_passive {
                process_record_by_name(store, &resolved, compute_alarms, active, changed_names);
            }
            store.get(&resolved).and_then(record_array_value)
        }
    }
}

fn write_link_value(
    store: &mut HashMap<String, RecordInstance>,
    link: &LinkExpr,
    value: ScalarValue,
    compute_alarms: bool,
    active: &mut HashSet<String>,
    changed_names: &mut HashSet<String>,
) {
    if let LinkExpr::DbLink {
        target,
        process_passive,
        ..
    } = link
    {
        let resolved = normalize_link_target(target);
        if let Some(target_record) = store.get_mut(&resolved) {
            if target_record.set_scalar_value(value, compute_alarms) {
                changed_names.insert(resolved.clone());
            }
        }
        if *process_passive {
            process_record_by_name(store, &resolved, compute_alarms, active, changed_names);
        }
    }
}

fn write_link_array_value(
    store: &mut HashMap<String, RecordInstance>,
    link: &LinkExpr,
    value: ScalarArrayValue,
    compute_alarms: bool,
    active: &mut HashSet<String>,
    changed_names: &mut HashSet<String>,
) {
    if let LinkExpr::DbLink {
        target,
        process_passive,
        ..
    } = link
    {
        let resolved = normalize_link_target(target);
        if let Some(target_record) = store.get_mut(&resolved) {
            set_record_array_value(target_record, value);
            changed_names.insert(resolved.clone());
        }
        if *process_passive {
            process_record_by_name(store, &resolved, compute_alarms, active, changed_names);
        }
    }
}

fn slice_array_value(value: &ScalarArrayValue, start: usize, len: usize) -> ScalarArrayValue {
    let end = start.saturating_add(len);
    match value {
        ScalarArrayValue::Bool(v) => {
            ScalarArrayValue::Bool(v.get(start..end).unwrap_or(&[]).to_vec())
        }
        ScalarArrayValue::I8(v) => ScalarArrayValue::I8(v.get(start..end).unwrap_or(&[]).to_vec()),
        ScalarArrayValue::I16(v) => {
            ScalarArrayValue::I16(v.get(start..end).unwrap_or(&[]).to_vec())
        }
        ScalarArrayValue::I32(v) => {
            ScalarArrayValue::I32(v.get(start..end).unwrap_or(&[]).to_vec())
        }
        ScalarArrayValue::I64(v) => {
            ScalarArrayValue::I64(v.get(start..end).unwrap_or(&[]).to_vec())
        }
        ScalarArrayValue::U8(v) => ScalarArrayValue::U8(v.get(start..end).unwrap_or(&[]).to_vec()),
        ScalarArrayValue::U16(v) => {
            ScalarArrayValue::U16(v.get(start..end).unwrap_or(&[]).to_vec())
        }
        ScalarArrayValue::U32(v) => {
            ScalarArrayValue::U32(v.get(start..end).unwrap_or(&[]).to_vec())
        }
        ScalarArrayValue::U64(v) => {
            ScalarArrayValue::U64(v.get(start..end).unwrap_or(&[]).to_vec())
        }
        ScalarArrayValue::F32(v) => {
            ScalarArrayValue::F32(v.get(start..end).unwrap_or(&[]).to_vec())
        }
        ScalarArrayValue::F64(v) => {
            ScalarArrayValue::F64(v.get(start..end).unwrap_or(&[]).to_vec())
        }
        ScalarArrayValue::Str(v) => {
            ScalarArrayValue::Str(v.get(start..end).unwrap_or(&[]).to_vec())
        }
    }
}

fn scalar_to_bool(value: &ScalarValue) -> Option<bool> {
    match value {
        ScalarValue::Bool(v) => Some(*v),
        ScalarValue::I32(v) => Some(*v != 0),
        ScalarValue::F64(v) => Some(*v != 0.0),
        ScalarValue::Str(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        ScalarValue::I8(v) => Some(*v != 0),
        ScalarValue::I16(v) => Some(*v != 0),
        ScalarValue::I64(v) => Some(*v != 0),
        ScalarValue::U8(v) => Some(*v != 0),
        ScalarValue::U16(v) => Some(*v != 0),
        ScalarValue::U32(v) => Some(*v != 0),
        ScalarValue::U64(v) => Some(*v != 0),
        ScalarValue::F32(v) => Some(*v != 0.0),
    }
}

fn scalar_to_f64(value: &ScalarValue) -> Option<f64> {
    match value {
        ScalarValue::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
        ScalarValue::I8(v) => Some(*v as f64),
        ScalarValue::I16(v) => Some(*v as f64),
        ScalarValue::I32(v) => Some(*v as f64),
        ScalarValue::I64(v) => Some(*v as f64),
        ScalarValue::U8(v) => Some(*v as f64),
        ScalarValue::U16(v) => Some(*v as f64),
        ScalarValue::U32(v) => Some(*v as f64),
        ScalarValue::U64(v) => Some(*v as f64),
        ScalarValue::F32(v) => Some(*v as f64),
        ScalarValue::F64(v) => Some(*v),
        ScalarValue::Str(v) => v.parse::<f64>().ok(),
    }
}

fn scalar_to_string(value: &ScalarValue) -> Option<String> {
    match value {
        ScalarValue::Bool(v) => Some(if *v { "1".to_string() } else { "0".to_string() }),
        ScalarValue::I8(v) => Some(v.to_string()),
        ScalarValue::I16(v) => Some(v.to_string()),
        ScalarValue::I32(v) => Some(v.to_string()),
        ScalarValue::I64(v) => Some(v.to_string()),
        ScalarValue::U8(v) => Some(v.to_string()),
        ScalarValue::U16(v) => Some(v.to_string()),
        ScalarValue::U32(v) => Some(v.to_string()),
        ScalarValue::U64(v) => Some(v.to_string()),
        ScalarValue::F32(v) => Some(v.to_string()),
        ScalarValue::F64(v) => Some(v.to_string()),
        ScalarValue::Str(v) => Some(v.clone()),
    }
}

fn decode_put_body(
    body: &[u8],
    desc: &spvirit_codec::spvd_decode::StructureDesc,
    is_be: bool,
) -> Option<DecodedValue> {
    let decoder = PvdDecoder::new(is_be);
    if let Some((value, _)) = decoder.decode_structure_with_bitset(body, desc) {
        if !decoded_is_empty(&value) {
            return Some(value);
        }
    }
    if !body.is_empty() && body[0] == 0xFF {
        if let Some((value, _)) = decoder.decode_structure_with_bitset(&body[1..], desc) {
            if !decoded_is_empty(&value) {
                return Some(value);
            }
        }
    }
    if let Some(value) = decode_put_body_shifted_bitset(body, desc, is_be) {
        return Some(value);
    }
    if let Some(value) = decode_put_body_value_only(body, desc, is_be) {
        return Some(value);
    }
    None
}

fn decoded_is_empty(value: &DecodedValue) -> bool {
    matches!(value, DecodedValue::Structure(fields) if fields.is_empty())
}

fn decode_put_body_shifted_bitset(
    body: &[u8],
    desc: &spvirit_codec::spvd_decode::StructureDesc,
    is_be: bool,
) -> Option<DecodedValue> {
    let decoder = PvdDecoder::new(is_be);
    let (size, consumed) = decoder.decode_size(body)?;
    if size == 0 || body.len() < consumed + size {
        return None;
    }
    let bitset = &body[consumed..consumed + size];
    let data = &body[consumed + size..];
    let shifted = shift_bitset_left(bitset, 1);
    let mut shifted_body = Vec::new();
    shifted_body.extend_from_slice(&encode_size_pvd(shifted.len(), is_be));
    shifted_body.extend_from_slice(&shifted);
    shifted_body.extend_from_slice(data);
    decoder
        .decode_structure_with_bitset(&shifted_body, desc)
        .map(|(value, _)| value)
        .filter(|value| !decoded_is_empty(value))
}

fn decode_put_body_value_only(
    body: &[u8],
    desc: &spvirit_codec::spvd_decode::StructureDesc,
    is_be: bool,
) -> Option<DecodedValue> {
    let decoder = PvdDecoder::new(is_be);
    if let Some((size, consumed)) = decoder.decode_size(body) {
        if consumed + size <= body.len() {
            let data = &body[consumed + size..];
            if let Some(value) = decode_value_only_from_data(data, desc, &decoder) {
                return Some(value);
            }
        }
    }
    decode_value_only_from_data(body, desc, &decoder)
}

fn decode_value_only_from_data(
    data: &[u8],
    desc: &spvirit_codec::spvd_decode::StructureDesc,
    decoder: &PvdDecoder,
) -> Option<DecodedValue> {
    let value_field = desc.fields.iter().find(|f| f.name == "value")?;
    decoder
        .decode_value(data, &value_field.field_type)
        .map(|(value, _)| DecodedValue::Structure(vec![("value".to_string(), value)]))
}

fn shift_bitset_left(bitset: &[u8], shift: usize) -> Vec<u8> {
    if shift == 0 {
        return bitset.to_vec();
    }
    let total_bits = bitset.len() * 8;
    let new_bits = total_bits + shift;
    let mut out = vec![0u8; (new_bits + 7) / 8];
    for bit in 0..total_bits {
        if (bitset[bit / 8] & (1 << (bit % 8))) != 0 {
            let new_bit = bit + shift;
            out[new_bit / 8] |= 1 << (new_bit % 8);
        }
    }
    out
}

async fn handle_control_message(state: &Arc<ServerState>, conn_id: u64, header: &PvaHeader) {
    debug!(
        "Conn {}: control (segmented) cmd={} data={}",
        conn_id, header.command, header.payload_length
    );
    if header.command == 3 {
        let resp = encode_control_message(
            true,
            header.flags.is_msb,
            header.version,
            4,
            header.payload_length,
        );
        send_msg(state, conn_id, resp).await;
    }
}

fn assemble_segmented_message(first_header: [u8; 8], payloads: Vec<Vec<u8>>) -> Vec<u8> {
    let mut header = first_header;
    let is_be = (header[2] & 0x80) != 0;
    header[2] &= !0x30;
    let total_len: usize = payloads.iter().map(|p| p.len()).sum();
    let len_bytes = if is_be {
        (total_len as u32).to_be_bytes()
    } else {
        (total_len as u32).to_le_bytes()
    };
    header[4..8].copy_from_slice(&len_bytes);
    let mut out = header.to_vec();
    for payload in payloads {
        out.extend_from_slice(&payload);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_codec::spvd_encode::{encode_nt_scalar_bitset_parts, nt_scalar_desc};

    #[test]
    fn decode_put_body_accepts_status_prefix() {
        let nt = NtScalar::from_value(spvirit_tools::spvirit_server::types::ScalarValue::F64(2.5));
        let desc = nt_scalar_desc(&nt.value);
        let (bitset, values) = encode_nt_scalar_bitset_parts(&nt, false);
        let mut body = Vec::new();
        body.push(0xFF);
        body.extend_from_slice(&bitset);
        body.extend_from_slice(&values);

        let decoded = decode_put_body(&body, &desc, false);
        assert!(decoded.is_some());
    }

    #[test]
    fn decode_put_body_accepts_bitset_without_struct_bit() {
        let nt = NtScalar::from_value(spvirit_tools::spvirit_server::types::ScalarValue::F64(0.0));
        let desc = nt_scalar_desc(&nt.value);
        let mut body = Vec::new();
        body.extend_from_slice(&encode_size_pvd(1, false));
        body.push(0x01); // bit0 set (value) with no struct bit
        body.extend_from_slice(&30.0f64.to_le_bytes());

        let decoded = decode_put_body(&body, &desc, false).expect("decoded");
        if let DecodedValue::Structure(fields) = decoded {
            let value = fields
                .iter()
                .find(|(name, _)| name == "value")
                .expect("value field");
            assert!(matches!(value.1, DecodedValue::Float64(v) if (v - 30.0).abs() < 1e-6));
        } else {
            panic!("expected structure");
        }
    }

    #[test]
    fn decode_put_body_accepts_value_only_payload() {
        let nt = NtScalar::from_value(spvirit_tools::spvirit_server::types::ScalarValue::F64(0.0));
        let desc = nt_scalar_desc(&nt.value);
        let body = 12.5f64.to_le_bytes();

        let decoded = decode_put_body(&body, &desc, false).expect("decoded");
        if let DecodedValue::Structure(fields) = decoded {
            let value = fields
                .iter()
                .find(|(name, _)| name == "value")
                .expect("value field");
            assert!(matches!(value.1, DecodedValue::Float64(v) if (v - 12.5).abs() < 1e-6));
        } else {
            panic!("expected structure");
        }
    }

    #[test]
    fn assemble_segmented_message_updates_length_and_clears_flags() {
        let mut header = [0u8; 8];
        header[0] = 0xCA;
        header[1] = 0x02;
        header[2] = 0x10; // first segment
        header[3] = 11; // PUT
        header[4..8].copy_from_slice(&3u32.to_le_bytes());

        let payloads = vec![b"abc".to_vec(), b"def".to_vec()];
        let full = assemble_segmented_message(header, payloads);

        assert_eq!(full[0], 0xCA);
        assert_eq!(full[1], 0x02);
        assert_eq!(full[2] & 0x30, 0x00);
        assert_eq!(u32::from_le_bytes(full[4..8].try_into().unwrap()), 6);
        assert_eq!(&full[8..], b"abcdef");
    }

    #[test]
    fn search_reply_target_prefers_payload_addr_and_port() {
        let mut addr = [0u8; 16];
        addr[10] = 0xFF;
        addr[11] = 0xFF;
        addr[12..16].copy_from_slice(&[130, 246, 90, 69]);
        let peer: SocketAddr = "130.246.90.69:5076".parse().expect("peer");

        let target = search_reply_target(&addr, 60292, peer);
        assert_eq!(target, "130.246.90.69:60292".parse().unwrap());
    }

    #[test]
    fn search_reply_target_falls_back_to_peer_when_payload_unspecified() {
        let addr = [0u8; 16];
        let peer: SocketAddr = "130.246.90.69:5076".parse().expect("peer");

        let target = search_reply_target(&addr, 0, peer);
        assert_eq!(target, peer);
    }

    #[test]
    fn search_reply_target_falls_back_to_peer_ip_for_ipv4_any() {
        let mut addr = [0u8; 16];
        addr[10] = 0xFF;
        addr[11] = 0xFF;
        let peer: SocketAddr = "130.246.90.69:5076".parse().expect("peer");

        let target = search_reply_target(&addr, 60292, peer);
        assert_eq!(target, "130.246.90.69:60292".parse().unwrap());
    }
}

fn file_mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn search_reply_target(addr: &[u8; 16], port: u16, peer: SocketAddr) -> SocketAddr {
    let target_port = if port != 0 { port } else { peer.port() };
    let target_ip = ip_from_bytes(addr)
        .filter(|ip| !ip.is_unspecified())
        .unwrap_or_else(|| peer.ip());
    SocketAddr::new(target_ip, target_port)
}

fn infer_udp_response_ip(peer: SocketAddr) -> Option<IpAddr> {
    let bind_addr = if peer.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let sock = std::net::UdpSocket::bind(bind_addr).ok()?;
    sock.connect(peer).ok()?;
    let local = sock.local_addr().ok()?;
    if local.ip().is_unspecified() {
        None
    } else {
        Some(local.ip())
    }
}

fn rand_guid() -> [u8; 12] {
    let pid = std::process::id().to_le_bytes();
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_le_bytes();
    let mut guid = [0u8; 12];
    guid[..4].copy_from_slice(&pid);
    guid[4..12].copy_from_slice(&nanos[..8]);
    guid
}

fn validate_encoded_packet(conn_id: u64, label: &str, bytes: &[u8]) {
    let mut pkt = PvaPacket::new(bytes);
    let decoded = pkt.decode_payload();
    match decoded {
        Some(PvaPacketCommand::ConnectionValidation(payload)) => {
            debug!(
                "Conn {}: {} decoded as cmd=1 buffer_size={} qos={} authz={:?}",
                conn_id, label, payload.buffer_size, payload.qos, payload.authz
            );
        }
        Some(PvaPacketCommand::ConnectionValidated(_)) => {
            debug!("Conn {}: {} decoded as cmd=9", conn_id, label);
        }
        Some(other) => {
            debug!("Conn {}: {} decoded as {:?}", conn_id, label, other);
        }
        None => {
            debug!("Conn {}: {} failed to decode", conn_id, label);
        }
    }
}

fn dump_hex_packet(conn_id: u64, dir: &str, label: &str, version: u8, is_be: bool, bytes: &[u8]) {
    debug!(
        "Conn {}: {} {} ver={} be={} len={}",
        conn_id,
        dir,
        label,
        version,
        is_be,
        bytes.len()
    );
    let mut offset = 0usize;
    while offset < bytes.len() {
        let end = usize::min(offset + 16, bytes.len());
        let chunk = &bytes[offset..end];
        let mut line = String::new();
        for (i, b) in chunk.iter().enumerate() {
            if i > 0 {
                line.push(' ');
            }
            line.push_str(&format!("{:02x}", b));
        }
        debug!("Conn {}: {:04x} {}", conn_id, offset, line);
        offset += 16;
    }
}
