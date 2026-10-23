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
use tracing::{Level, debug, error, info, warn};

use spvirit_tools::spvirit_server::db::load_db;
use spvirit_tools::spvirit_server::state::{ConnState, MonitorState, MonitorSub};
use spvirit_tools::spvirit_server::types::{
    LinkExpr, NtPayload, OutputMode, RecordData, RecordInstance, ScalarArrayValue,
    ScalarValue, ScanMode,
};

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_codec::spvd_encode::{
    decode_pv_request_fields, filter_structure_desc, nt_payload_desc,
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
use spvirit_codec::{SegmentOutcome, SegmentReassembler};

use spvirit_server::decode::decode_put_body;
use spvirit_server::handler::{
    PvListMode, build_pvlist_structure, collect_visible_pv_names_iter, dump_hex_packet,
    infer_udp_response_ip, is_pattern_query, is_pvlist_virtual_pv, is_server_rpc_pv,
    is_virtual_event_pv, rand_guid, requested_pvlist_pattern, search_reply_target,
    validate_encoded_packet, virtual_event_nt, virtual_pvlist_nt, wildcard_match,
};
use spvirit_server::monitor::MonitorRegistry;

/// Parse `--pvlist-mode` using the library's parser, but keep this binary's
/// error wording. The library says "Invalid pvlist-mode"; the CLI has always
/// said "Invalid --pvlist-mode" (naming the flag the user actually typed) and
/// that text is a user-visible compatibility surface.
fn parse_pvlist_mode_arg(raw: &str) -> Result<PvListMode, String> {
    PvListMode::parse(raw).map_err(|_| {
        // The deleted copy interpolated the already-trimmed, already-lowercased
        // token (`other` in its `match`), not the raw argument. Reproduce that
        // exactly so `--pvlist-mode NOPE` still reports `'nope'`.
        format!(
            "Invalid --pvlist-mode '{}'; expected off|discover|list",
            raw.trim().to_ascii_lowercase()
        )
    })
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
    /// Bumped whenever the whole store is replaced (DB reload). Long-lived
    /// derived indexes — the scan scheduler's periodic-record list — cache
    /// against this instead of rebuilding themselves every tick.
    db_generation: AtomicU64,
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
            db_generation: AtomicU64::new(0),
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

    fn db_generation(&self) -> u64 {
        self.db_generation.load(Ordering::Relaxed)
    }

    fn bump_db_generation(&self) {
        self.db_generation.fetch_add(1, Ordering::Relaxed);
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

    let pvlist_mode = parse_pvlist_mode_arg(&pvlist_mode_raw)?;
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
        let Some(header) = PvaHeader::try_new(data) else {
            continue;
        };
        // UDP endpoint only handles SEARCH (cmd=3) data messages.
        // Ignore other traffic early to avoid noisy "unknown command" decode logs.
        if header.flags.is_control || header.command != 3 {
            continue;
        }
        let Some(mut pkt) = PvaPacket::try_new(data) else {
            continue;
        };
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
                let cids = resolve_search_cids(
                    &payload.pv_requests,
                    state.pvlist_mode,
                    |name| {
                        pv_store.contains_key(name)
                            || resolve_field_nt(&pv_store, name).is_some()
                            || is_virtual_event_pv(name)
                            || (is_pvlist_virtual_pv(name) && state.pvlist_mode == PvListMode::List)
                            || (is_server_rpc_pv(name) && state.pvlist_mode != PvListMode::Off)
                    },
                    || {
                        collect_visible_pv_names_iter(
                            pv_store.keys().map(String::as_str),
                            state.pvlist_mode,
                            state.pvlist_allow_pattern.as_ref(),
                            state.pvlist_max,
                        )
                    },
                );
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
        // A transient accept() error (EMFILE/ENFILE descriptor exhaustion, or a
        // client that aborted mid-handshake) must not kill the server: the
        // listener stays valid, so log and keep accepting. Back off briefly on
        // descriptor exhaustion (errno 24/23) so the loop does not hot-spin
        // while no descriptors are available.
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                error!("TCP accept error (continuing): {}", e);
                if matches!(e.raw_os_error(), Some(24) | Some(23)) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                continue;
            }
        };
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
    // One reassembler for the whole connection: the segments of a message may
    // be separated by control frames, which are handled in between.
    let mut reassembler = SegmentReassembler::new();

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

        // Segment reassembly is delegated to the shared codec state machine.
        // Control frames come back verbatim and are answered here; they never
        // reach command dispatch below.
        let full = match reassembler.push(header, payload) {
            Ok(SegmentOutcome::Complete(msg)) => msg,
            Ok(SegmentOutcome::Pending) => continue,
            Ok(SegmentOutcome::Control(msg)) => {
                let ctrl = PvaPacket::new(&msg);
                handle_control_message(&state, conn_id, &ctrl.header).await;
                continue;
            }
            Err(e) => {
                warn!("Conn {}: segmentation error: {}", conn_id, e);
                break;
            }
        };

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
                        conn_state
                            .channels
                            .lock()
                            .unwrap()
                            .insert_channel(cid, sid, &pv_name);
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
                let pv_lookup = conn_state
                    .channels
                    .lock()
                    .unwrap()
                    .sid_to_pv
                    .get(&sid)
                    .cloned();
                let Some(pv_name) = pv_lookup else {
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
                            // Cap client-supplied pipeline window (see MAX_PIPELINE_WINDOW).
                            let nfree = nfree.min(MAX_PIPELINE_WINDOW);
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
                                nfree = Some(v.min(MAX_PIPELINE_WINDOW));
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
                                        mon.nfree =
                                            mon.nfree.saturating_add(v).min(MAX_PIPELINE_WINDOW);
                                    } else {
                                        mon.nfree = v.min(MAX_PIPELINE_WINDOW);
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
                conn_state
                    .channels
                    .lock()
                    .unwrap()
                    .remove_channel(cid, sid);
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
                    let cids = resolve_search_cids(
                        &payload.pv_requests,
                        state.pvlist_mode,
                        |name| {
                            pv_store.contains_key(name)
                                || resolve_field_nt(&pv_store, name).is_some()
                                || is_virtual_event_pv(name)
                                || (is_pvlist_virtual_pv(name) && state.pvlist_mode == PvListMode::List)
                                || (is_server_rpc_pv(name) && state.pvlist_mode != PvListMode::Off)
                        },
                        || {
                            collect_visible_pv_names_iter(
                                pv_store.keys().map(String::as_str),
                                state.pvlist_mode,
                                state.pvlist_allow_pattern.as_ref(),
                                state.pvlist_max,
                            )
                        },
                    );
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
            state.bump_db_generation();
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
    // Phase 1: build frames under the monitors lock, but DO NOT commit per-sub
    // state (`nfree` / `last_snapshot`) yet. A frame that gets dropped by a full
    // client queue in phase 2 must not consume a pipeline credit (would leak →
    // permanent stall) nor advance the delta baseline (would latch a filtered
    // client on a value it never received). Commit happens in phase 3, only for
    // frames actually enqueued (crate-audit review R2-H1).
    let mut to_send: Vec<(u64, u32, Vec<u8>)> = Vec::new();
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
                let Some(msg) = MonitorRegistry::build_monitor_frame(sub, payload) else {
                    continue;
                };
                to_send.push((sub.conn_id, sub.ioid, msg));
            }
        }
    }

    // Phase 2: enqueue outside the lock; remember which subs actually took delivery.
    // Set, not Vec: phase 3 walks every sub and asks "was this one delivered?",
    // so a linear scan per sub makes the commit phase O(delivered × subs).
    let mut delivered: HashSet<(u64, u32)> = HashSet::new();
    for (conn_id, ioid, msg) in to_send {
        if send_msg(state, conn_id, msg).await {
            delivered.insert((conn_id, ioid));
            debug!("Monitor update pv='{}' conn={} ", pv_name, conn_id);
        }
    }

    // Phase 3: commit delta baseline + pipeline credit ONLY for delivered frames.
    if !delivered.is_empty() {
        let mut monitors = state.monitors.lock().await;
        if let Some(list) = monitors.get_mut(pv_name) {
            for sub in list.iter_mut() {
                if delivered.contains(&(sub.conn_id, sub.ioid)) {
                    if sub.pipeline_enabled && sub.nfree > 0 {
                        sub.nfree -= 1;
                    }
                    sub.last_snapshot = Some(payload.clone());
                }
            }
        }
    }
}

async fn send_monitor_update_for(
    state: &Arc<ServerState>,
    pv_name: &str,
    conn_id: u64,
    ioid: u32,
    payload: &NtPayload,
) {
    // Build under the lock WITHOUT committing `nfree` / `last_snapshot` — see
    // notify_monitors: a dropped frame must not spend credit or advance the
    // delta baseline (crate-audit review R2-H1).
    let msg = {
        let mut monitors = state.monitors.lock().await;
        let Some(list) = monitors.get_mut(pv_name) else {
            return;
        };
        let Some(sub) = list
            .iter_mut()
            .find(|s| s.conn_id == conn_id && s.ioid == ioid)
        else {
            return;
        };
        if !sub.running {
            return;
        }
        if sub.pipeline_enabled && sub.nfree == 0 {
            return;
        }
        match MonitorRegistry::build_monitor_frame(sub, payload) {
            Some(m) => m,
            None => return,
        }
    };

    // Enqueue outside the lock; commit baseline + credit only on delivery.
    if send_msg(state, conn_id, msg).await {
        let mut monitors = state.monitors.lock().await;
        if let Some(list) = monitors.get_mut(pv_name) {
            if let Some(sub) = list
                .iter_mut()
                .find(|s| s.conn_id == conn_id && s.ioid == ioid)
            {
                if sub.pipeline_enabled && sub.nfree > 0 {
                    sub.nfree -= 1;
                }
                sub.last_snapshot = Some(payload.clone());
            }
        }
    }
}

/// Ceiling on a client-supplied pipeline window (`nfree`), mirroring the
/// library's `spvirit_server::handler::MAX_PIPELINE_WINDOW`. A client could
/// otherwise request an unbounded window and force the server to buffer an
/// arbitrary number of monitor frames. Applied at every point where `nfree`
/// is set from client input: subscription init, ACK refill, and here.
const MAX_PIPELINE_WINDOW: u32 = 4096;

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
                sub.nfree = v.min(MAX_PIPELINE_WINDOW);
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

/// Enqueue one already-built frame for a connection. Returns `true` iff the
/// frame was actually accepted into the client's queue.
///
/// Non-blocking send: if this client's bounded queue is full, drop this update
/// for THIS client only rather than stalling monitor delivery to every later
/// client and the global DB-reload / PUT paths that drive the fan-out loop.
/// Delivery stays lossless & in-order per client whose queue is keeping up; a
/// slow client just loses its own updates.
///
/// The returned delivered/dropped signal is load-bearing: monitor callers must
/// commit pipeline credit (`nfree`) and the delta baseline (`last_snapshot`)
/// ONLY when this returns `true`. Committing on a dropped frame would (a) leak a
/// pipeline credit → permanent stall of a pipelined client, and (b) advance the
/// delta baseline past a value the client never received → a filtered client
/// latched on stale data (crate-audit review R2-H1).
async fn send_msg(state: &Arc<ServerState>, conn_id: u64, msg: Vec<u8>) -> bool {
    let conns = state.conns.lock().await;
    if let Some(tx) = conns.get(&conn_id) {
        match tx.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(
                    "Dropping monitor update for conn={}: send queue full (slow client)",
                    conn_id
                );
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    } else {
        false
    }
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

    let (sid, resolved_pv) = {
        let tables = conn_state.channels.lock().unwrap();
        let sid = payload
            .sid
            .or_else(|| tables.cid_to_sid.get(&payload.cid).copied())
            .or_else(|| {
                tables
                    .sid_to_pv
                    .contains_key(&payload.cid)
                    .then_some(payload.cid)
            })
            .or_else(|| {
                (payload.cid == 0 && tables.sid_to_pv.len() == 1)
                    .then(|| tables.sid_to_pv.keys().copied().next())
                    .flatten()
            });
        let pv = sid.and_then(|s| tables.sid_to_pv.get(&s).cloned());
        (sid, pv)
    };

    if let Some(sid) = sid {
        if let Some(pv_name) = resolved_pv.as_ref() {
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
    let mut names = collect_visible_pv_names_iter(
        pv_store.keys().map(String::as_str),
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
        collect_visible_pv_names_iter(
            store.keys().map(String::as_str),
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

/// Decide which of a SEARCH packet's CIDs this server can answer.
///
/// `visible` is a *thunk*: it is invoked at most once per packet, and only if
/// a pattern query is actually present. Materialising the visible-PV list means
/// filtering, sorting and cloning every name in the store - far too expensive
/// to pay on every SEARCH broadcast, when the common packet is a handful of
/// exact names that never consults the list at all.
fn resolve_search_cids<D, V>(
    requests: &[(u32, String)],
    mode: PvListMode,
    mut direct_hit: D,
    mut visible: V,
) -> Vec<u32>
where
    D: FnMut(&str) -> bool,
    V: FnMut() -> Vec<String>,
{
    let mut cids = Vec::new();
    let mut visible_names: Option<Vec<String>> = None;
    for (cid, name) in requests {
        if direct_hit(name) {
            cids.push(*cid);
            continue;
        }
        if mode != PvListMode::Off && is_pattern_query(name) {
            let names = visible_names.get_or_insert_with(&mut visible);
            if names.iter().any(|pv| wildcard_match(name, pv)) {
                cids.push(*cid);
            }
        }
    }
    cids
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
        let names = collect_visible_pv_names_iter(
            store.keys().map(String::as_str),
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

/// True when a cached periodic-record index must be rebuilt.
fn periodic_index_is_stale(
    cache: Option<&(u64, Vec<(String, Duration)>)>,
    generation: u64,
) -> bool {
    match cache {
        Some((cached_generation, _)) => *cached_generation != generation,
        None => true,
    }
}

async fn run_scan_scheduler(state: Arc<ServerState>) -> Result<(), Box<dyn std::error::Error>> {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    let mut next_due: HashMap<String, Instant> = HashMap::new();
    // (db_generation, periodic records) — rebuilt only when the store is
    // replaced, not on every 100 ms tick. `record.common.scan` is immutable at
    // runtime; a whole-store swap in `run_db_reload` is the only thing that can
    // change this set, and it bumps the generation.
    let mut periodic_cache: Option<(u64, Vec<(String, Duration)>)> = None;
    loop {
        interval.tick().await;
        let now = Instant::now();
        let generation = state.db_generation();
        if periodic_index_is_stale(periodic_cache.as_ref(), generation) {
            let store = state.pv_store.read().await;
            let records: Vec<(String, Duration)> = store
                .iter()
                .filter_map(|(name, record)| match &record.common.scan {
                    ScanMode::Periodic(period) => Some((name.clone(), *period)),
                    _ => None,
                })
                .collect();
            drop(store);
            next_due.retain(|name, _| records.iter().any(|(n, _)| n == name));
            periodic_cache = Some((generation, records));
        }
        let periodic = &periodic_cache
            .as_ref()
            .expect("periodic_cache populated above")
            .1;

        let mut due: Vec<String> = Vec::new();
        for (name, period) in periodic {
            let entry = next_due
                .entry(name.clone())
                .or_insert_with(|| now + *period);
            if now >= *entry {
                due.push(name.clone());
                *entry = now + *period;
            }
        }

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
    // (db_generation, event-tag index, io-event-tag index). Rebuilt only when
    // the store is replaced, not on every posted event. `record.common.scan` is
    // immutable at runtime (its only two readers in the whole workspace are
    // this function and `run_scan_scheduler`), and a whole-store swap in
    // `run_db_reload` is the only thing that can change these sets — it bumps
    // the generation. `Event` and `IoEvent` get separate maps because the
    // original `filter_map` matched the (scan, event) pair: a soft event must
    // never fire an `I/O Intr` record that happens to share its tag.
    type EventIndex = (u64, HashMap<String, Vec<String>>, HashMap<String, Vec<String>>);
    let mut event_cache: Option<EventIndex> = None;
    while let Some(event) = event_rx.recv().await {
        let generation = state.db_generation();
        if event_cache.as_ref().map(|(g, _, _)| *g) != Some(generation) {
            let store = state.pv_store.read().await;
            let mut soft: HashMap<String, Vec<String>> = HashMap::new();
            let mut io: HashMap<String, Vec<String>> = HashMap::new();
            for (name, record) in store.iter() {
                match &record.common.scan {
                    ScanMode::Event(tag) => soft.entry(tag.clone()).or_default().push(name.clone()),
                    ScanMode::IoEvent(tag) => io.entry(tag.clone()).or_default().push(name.clone()),
                    _ => {}
                }
            }
            drop(store);
            event_cache = Some((generation, soft, io));
        }
        let (_, soft, io) = event_cache.as_ref().expect("event_cache populated above");
        let targets: &[String] = match &event {
            PostedEvent::Event(trigger) => soft.get(trigger).map(Vec::as_slice).unwrap_or(&[]),
            PostedEvent::IoEvent(trigger) => io.get(trigger).map(Vec::as_slice).unwrap_or(&[]),
        };
        // `process_single_record` takes `&Arc<ServerState>` and awaits, which
        // would hold the `event_cache` borrow across the await points. Copy the
        // (small) target list out first so the borrow ends here.
        let targets: Vec<String> = targets.to_vec();
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
        // A value change is always judged by the deadband (should_post_update
        // must run so the MDEL reference point stays current); it is never
        // force-posted just because the PUT also carried a client stamp.
        changed_names.insert(pv_name.to_string());
    } else if outcome.client_stamped {
        // A client-supplied acquisition time is new information even when the
        // value is identical: post it past the MDEL gate. should_post_update
        // is skipped here, matching SimplePvStore::put — an unchanged value
        // must not touch the MDEL reference point.
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
/// MDEL deadband would suppress them — used only for client-stamped PUTs
/// whose value did *not* change, where the timestamp alone is new
/// information. A PUT that changed the value is never in `force`: it must
/// always be judged by `should_post_update` so the MDEL reference point
/// stays current, matching `SimplePvStore::put`.
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

/// The subset of a record that `process_record_body` actually reads.
///
/// `process_record_body` used to start with `store.get(pv_name).cloned()`,
/// deep-copying the whole `RecordInstance` — `raw_fields`, and the entire value
/// payload (`NtScalarArray`, `NtTable`, `NtNdArray`, `Generic::fields`) — on
/// every PUT and every scan tick. None of it is used: every match arm binds
/// only link expressions, a `bool`, an `OutputMode` and a few numeric limits.
/// `RecordPlan` mirrors `RecordData`'s variant and field *names* exactly, so
/// the arm bodies below are unchanged; only the arm heads say `RecordPlan::`.
#[derive(Debug, Clone)]
enum RecordPlan {
    Ai {
        inp: Option<LinkExpr>,
        siml: Option<LinkExpr>,
        siol: Option<LinkExpr>,
        simm: bool,
    },
    Ao {
        out: Option<LinkExpr>,
        dol: Option<LinkExpr>,
        omsl: OutputMode,
        drvl: Option<f64>,
        drvh: Option<f64>,
        oroc: Option<f64>,
        siol: Option<LinkExpr>,
        simm: bool,
    },
    Bi {
        inp: Option<LinkExpr>,
        siol: Option<LinkExpr>,
        simm: bool,
    },
    Bo {
        out: Option<LinkExpr>,
        dol: Option<LinkExpr>,
        omsl: OutputMode,
        siol: Option<LinkExpr>,
        simm: bool,
    },
    StringIn {
        inp: Option<LinkExpr>,
        siol: Option<LinkExpr>,
        simm: bool,
    },
    StringOut {
        out: Option<LinkExpr>,
        dol: Option<LinkExpr>,
        omsl: OutputMode,
        siol: Option<LinkExpr>,
        simm: bool,
    },
    Waveform {
        inp: Option<LinkExpr>,
    },
    Aao {
        out: Option<LinkExpr>,
        dol: Option<LinkExpr>,
        omsl: OutputMode,
    },
    SubArray {
        inp: Option<LinkExpr>,
        indx: usize,
        nelm: usize,
        malm: usize,
    },
    NtTable {
        inp: Option<LinkExpr>,
        out: Option<LinkExpr>,
        omsl: OutputMode,
    },
    NtNdArray {
        inp: Option<LinkExpr>,
        out: Option<LinkExpr>,
        omsl: OutputMode,
    },
    /// `NtEnum` and `Generic` have no processing behaviour.
    Inert,
}

fn record_plan(data: &RecordData) -> RecordPlan {
    match data {
        RecordData::Ai {
            inp,
            siml,
            siol,
            simm,
            ..
        } => RecordPlan::Ai {
            inp: inp.clone(),
            siml: siml.clone(),
            siol: siol.clone(),
            simm: *simm,
        },
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
        } => RecordPlan::Ao {
            out: out.clone(),
            dol: dol.clone(),
            omsl: omsl.clone(),
            drvl: *drvl,
            drvh: *drvh,
            oroc: *oroc,
            siol: siol.clone(),
            simm: *simm,
        },
        RecordData::Bi {
            inp, siol, simm, ..
        } => RecordPlan::Bi {
            inp: inp.clone(),
            siol: siol.clone(),
            simm: *simm,
        },
        RecordData::Bo {
            out,
            dol,
            omsl,
            siol,
            simm,
            ..
        } => RecordPlan::Bo {
            out: out.clone(),
            dol: dol.clone(),
            omsl: omsl.clone(),
            siol: siol.clone(),
            simm: *simm,
        },
        RecordData::StringIn {
            inp, siol, simm, ..
        } => RecordPlan::StringIn {
            inp: inp.clone(),
            siol: siol.clone(),
            simm: *simm,
        },
        RecordData::StringOut {
            out,
            dol,
            omsl,
            siol,
            simm,
            ..
        } => RecordPlan::StringOut {
            out: out.clone(),
            dol: dol.clone(),
            omsl: omsl.clone(),
            siol: siol.clone(),
            simm: *simm,
        },
        RecordData::Waveform { inp, .. } | RecordData::Aai { inp, .. } => {
            RecordPlan::Waveform { inp: inp.clone() }
        }
        RecordData::Aao { out, dol, omsl, .. } => RecordPlan::Aao {
            out: out.clone(),
            dol: dol.clone(),
            omsl: omsl.clone(),
        },
        RecordData::SubArray {
            inp,
            indx,
            nelm,
            malm,
            ..
        } => RecordPlan::SubArray {
            inp: inp.clone(),
            indx: *indx,
            nelm: *nelm,
            malm: *malm,
        },
        RecordData::NtTable { inp, out, omsl, .. } => RecordPlan::NtTable {
            inp: inp.clone(),
            out: out.clone(),
            omsl: omsl.clone(),
        },
        RecordData::NtNdArray { inp, out, omsl, .. } => RecordPlan::NtNdArray {
            inp: inp.clone(),
            out: out.clone(),
            omsl: omsl.clone(),
        },
        RecordData::NtEnum { .. } | RecordData::Generic { .. } => RecordPlan::Inert,
    }
}

fn process_record_body(
    store: &mut HashMap<String, RecordInstance>,
    pv_name: &str,
    compute_alarms: bool,
    active: &mut HashSet<String>,
    changed_names: &mut HashSet<String>,
) {
    // Extract only the fields the arms below read, under a short immutable
    // borrow, then release it — the arms need `&mut store` for link reads and
    // writes. See `RecordPlan` for why this is not `store.get(..).cloned()`.
    let Some(plan) = store.get(pv_name).map(|record| record_plan(&record.data)) else {
        return;
    };

    match plan {
        RecordPlan::Ai {
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
        RecordPlan::Ao {
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
        RecordPlan::Bi {
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
        RecordPlan::Bo {
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
        RecordPlan::StringIn {
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
        RecordPlan::StringOut {
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
        RecordPlan::Waveform { inp } => {
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
        RecordPlan::Aao { out, dol, omsl } => {
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
        RecordPlan::SubArray {
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
        RecordPlan::NtTable { inp, out, omsl } => {
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
        RecordPlan::NtNdArray { inp, out, omsl } => {
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
        RecordPlan::Inert => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_tools::spvirit_server::types::NtScalar;

    #[test]
    fn record_plan_carries_every_field_the_processing_arms_read() {
        let ai = RecordData::Ai {
            nt: NtScalar::from_value(ScalarValue::F64(1.0)),
            inp: None,
            siml: None,
            siol: None,
            simm: true,
        };
        assert!(matches!(
            record_plan(&ai),
            RecordPlan::Ai { simm: true, .. }
        ));

        let ao = RecordData::Ao {
            nt: NtScalar::from_value(ScalarValue::F64(0.0)),
            out: None,
            dol: None,
            omsl: OutputMode::ClosedLoop,
            drvl: Some(-1.0),
            drvh: Some(1.0),
            oroc: Some(0.5),
            siml: None,
            siol: None,
            simm: false,
        };
        match record_plan(&ao) {
            RecordPlan::Ao {
                omsl,
                drvl,
                drvh,
                oroc,
                simm,
                ..
            } => {
                assert!(matches!(omsl, OutputMode::ClosedLoop));
                assert_eq!(drvl, Some(-1.0));
                assert_eq!(drvh, Some(1.0));
                assert_eq!(oroc, Some(0.5));
                assert!(!simm);
            }
            other => panic!("expected RecordPlan::Ao, got {:?}", other),
        }

        // `NtScalarArray` is NOT in the bin's top-level `use` list, so `use
        // super::*` does not bring it into the test module. Spell it out, as
        // `virtual_pvlist_nt` does.
        use spvirit_tools::spvirit_server::types::NtScalarArray;

        let subarray = RecordData::SubArray {
            nt: NtScalarArray::from_value(ScalarArrayValue::F64(vec![1.0, 2.0, 3.0])),
            inp: None,
            ftvl: "DOUBLE".to_string(),
            malm: 8,
            nelm: 3,
            nord: 3,
            indx: 1,
        };
        assert!(matches!(
            record_plan(&subarray),
            RecordPlan::SubArray {
                indx: 1,
                nelm: 3,
                malm: 8,
                ..
            }
        ));

        let generic = RecordData::Generic {
            struct_id: "s".to_string(),
            fields: Vec::new(),
            inp: None,
            out: None,
            omsl: OutputMode::Supervisory,
        };
        assert!(matches!(record_plan(&generic), RecordPlan::Inert));
    }

    /// Every link slot gets a DISTINCT constant, so a constructor in
    /// `record_plan` that swapped two same-typed fields (`out`/`dol`,
    /// `inp`/`siol`, ...) fails here; the existing test used `None` for
    /// every link and could not see a swap.
    #[test]
    fn record_plan_routes_each_link_to_its_own_slot() {
        use spvirit_tools::spvirit_server::types::{LinkExpr, NtScalarArray};
        let k = |n: i32| Some(LinkExpr::Constant(ScalarValue::I32(n)));
        let sc = || NtScalar::from_value(ScalarValue::F64(0.0));
        let arr = || NtScalarArray::from_value(ScalarArrayValue::F64(vec![0.0]));

        match record_plan(&RecordData::Ai { nt: sc(), inp: k(1), siml: k(2), siol: k(3), simm: false }) {
            RecordPlan::Ai { inp, siml, siol, .. } => {
                assert_eq!((inp, siml, siol), (k(1), k(2), k(3)));
            }
            other => panic!("Ai -> {other:?}"),
        }
        match record_plan(&RecordData::Ao {
            nt: sc(), out: k(1), dol: k(2), omsl: OutputMode::Supervisory,
            drvl: None, drvh: None, oroc: None, siml: k(4), siol: k(3), simm: false,
        }) {
            RecordPlan::Ao { out, dol, siol, .. } => {
                assert_eq!((out, dol, siol), (k(1), k(2), k(3)));
            }
            other => panic!("Ao -> {other:?}"),
        }
        match record_plan(&RecordData::Bi {
            nt: sc(), inp: k(1), znam: String::new(), onam: String::new(),
            siml: k(4), siol: k(3), simm: true,
        }) {
            RecordPlan::Bi { inp, siol, simm } => {
                assert_eq!((inp, siol, simm), (k(1), k(3), true));
            }
            other => panic!("Bi -> {other:?}"),
        }
        match record_plan(&RecordData::Bo {
            nt: sc(), out: k(1), dol: k(2), omsl: OutputMode::ClosedLoop,
            znam: String::new(), onam: String::new(), siml: k(4), siol: k(3), simm: true,
        }) {
            RecordPlan::Bo { out, dol, omsl, siol, simm } => {
                assert_eq!((out, dol, siol, simm), (k(1), k(2), k(3), true));
                assert_eq!(omsl, OutputMode::ClosedLoop);
            }
            other => panic!("Bo -> {other:?}"),
        }
        match record_plan(&RecordData::StringIn { nt: sc(), inp: k(1), siml: k(4), siol: k(3), simm: true }) {
            RecordPlan::StringIn { inp, siol, simm } => {
                assert_eq!((inp, siol, simm), (k(1), k(3), true));
            }
            other => panic!("StringIn -> {other:?}"),
        }
        match record_plan(&RecordData::StringOut {
            nt: sc(), out: k(1), dol: k(2), omsl: OutputMode::ClosedLoop,
            siml: k(4), siol: k(3), simm: true,
        }) {
            RecordPlan::StringOut { out, dol, omsl, siol, simm } => {
                assert_eq!((out, dol, siol, simm), (k(1), k(2), k(3), true));
                assert_eq!(omsl, OutputMode::ClosedLoop);
            }
            other => panic!("StringOut -> {other:?}"),
        }
        for data in [
            RecordData::Waveform { nt: arr(), inp: k(1), ftvl: String::new(), nelm: 1, nord: 1 },
            RecordData::Aai { nt: arr(), inp: k(1), ftvl: String::new(), nelm: 1, nord: 1 },
        ] {
            match record_plan(&data) {
                RecordPlan::Waveform { inp } => assert_eq!(inp, k(1)),
                other => panic!("Waveform/Aai -> {other:?}"),
            }
        }
        match record_plan(&RecordData::Aao {
            nt: arr(), out: k(1), dol: k(2), omsl: OutputMode::ClosedLoop,
            ftvl: String::new(), nelm: 1, nord: 1,
        }) {
            RecordPlan::Aao { out, dol, omsl } => {
                assert_eq!((out, dol), (k(1), k(2)));
                assert_eq!(omsl, OutputMode::ClosedLoop);
            }
            other => panic!("Aao -> {other:?}"),
        }
        match record_plan(&RecordData::SubArray {
            nt: arr(), inp: k(1), ftvl: String::new(), malm: 9, nelm: 5, nord: 1, indx: 2,
        }) {
            RecordPlan::SubArray { inp, indx, nelm, malm } => {
                assert_eq!(inp, k(1));
                assert_eq!((indx, nelm, malm), (2, 5, 9));
            }
            other => panic!("SubArray -> {other:?}"),
        }
    }

    #[test]
    fn packet_validation_and_hex_dump_are_total_functions() {
        // Both are pure diagnostics: they return `()` and only emit `debug!`
        // lines. The property worth pinning is therefore totality: neither may
        // panic or index out of bounds on a header that lies about its payload
        // length, which is exactly the input a hostile or buggy peer sends.
        let good = encode_header(false, false, false, 2, 1, 0);
        validate_encoded_packet(1, "well-formed", &good);
        dump_hex_packet(1, "->", "well-formed", 2, false, &good);

        let mut truncated = encode_header(false, false, false, 2, 1, 16);
        truncated.truncate(8);
        validate_encoded_packet(1, "truncated", &truncated);
        dump_hex_packet(1, "->", "truncated", 2, false, &truncated);

        // A payload that is not a multiple of the 16-byte dump line width.
        dump_hex_packet(1, "<-", "ragged", 2, true, &[0u8; 37]);
        validate_encoded_packet(1, "empty", &[]);
        dump_hex_packet(1, "<-", "empty", 2, false, &[]);
    }

    #[test]
    fn virtual_pv_helpers_come_from_the_library() {
        // Same names, same answers: proving the local copies are gone and the
        // library's are wired in. `virtual_pvlist_nt` is the one with a body
        // worth pinning: it must produce a string ScalarArray, not a scalar.
        assert!(is_pvlist_virtual_pv("__pvlist"));
        assert!(!is_pvlist_virtual_pv("SOME:PV"));
        assert!(is_server_rpc_pv("server"));
        assert!(!is_server_rpc_pv("__server"));
        assert!(is_virtual_event_pv("__event:MY_EVT"));
        assert!(is_virtual_event_pv("__event:io:ADC0"));
        assert!(!is_virtual_event_pv("SOME:PV"));
        assert!(matches!(
            virtual_pvlist_nt(vec!["A".to_string(), "B".to_string()]),
            NtPayload::ScalarArray(_)
        ));
        assert!(matches!(virtual_event_nt("__event:MY_EVT"), NtPayload::Scalar(_)));
    }

    #[test]
    fn pvlist_mode_comes_from_the_library_and_keeps_the_cli_error_text() {
        assert_eq!(PvListMode::parse("list"), Ok(PvListMode::List));
        assert_eq!(PvListMode::parse(" DISCOVER "), Ok(PvListMode::Discover));
        assert_eq!(
            parse_pvlist_mode_arg("nope"),
            Err("Invalid --pvlist-mode 'nope'; expected off|discover|list".to_string())
        );
    }

    #[test]
    fn search_does_not_enumerate_the_store_for_exact_names() {
        let mut enumerations = 0usize;
        let requests = vec![
            (1u32, "A:PV".to_string()),
            (2u32, "MISSING:PV".to_string()),
        ];
        let cids = resolve_search_cids(
            &requests,
            PvListMode::Discover,
            |name| name == "A:PV",
            || {
                enumerations += 1;
                vec!["A:PV".to_string(), "B:PV".to_string()]
            },
        );
        assert_eq!(cids, vec![1]);
        assert_eq!(enumerations, 0, "no pattern query, so no enumeration");
    }

    #[test]
    fn search_enumerates_at_most_once_for_many_pattern_queries() {
        let mut enumerations = 0usize;
        let requests = vec![
            (1u32, "A:*".to_string()),
            (2u32, "B:*".to_string()),
            (3u32, "Z:*".to_string()),
        ];
        let cids = resolve_search_cids(
            &requests,
            PvListMode::Discover,
            |_| false,
            || {
                enumerations += 1;
                vec!["A:PV".to_string(), "B:PV".to_string()]
            },
        );
        assert_eq!(cids, vec![1, 2]);
        assert_eq!(enumerations, 1, "the list is built once and reused");
    }

    #[test]
    fn search_skips_pattern_matching_entirely_when_pvlist_is_off() {
        let mut enumerations = 0usize;
        let requests = vec![(1u32, "A:*".to_string())];
        let cids = resolve_search_cids(
            &requests,
            PvListMode::Off,
            |_| false,
            || {
                enumerations += 1;
                vec!["A:PV".to_string()]
            },
        );
        assert!(cids.is_empty());
        assert_eq!(enumerations, 0);
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

    fn test_state(record: RecordInstance) -> Arc<ServerState> {
        let mut store = HashMap::new();
        store.insert(record.name.clone(), record);
        let (event_tx, _event_rx) = mpsc::channel(1);
        Arc::new(ServerState::new(
            store,
            false,
            event_tx,
            PvListMode::Off,
            1024,
            None,
            [0u8; 12],
            5075,
            None,
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        ))
    }

    fn mdel_ai_record(name: &str, val: f64, mdel: &str) -> RecordInstance {
        let nt = NtScalar::from_value(spvirit_tools::spvirit_server::types::ScalarValue::F64(val));
        let mut raw_fields = HashMap::new();
        raw_fields.insert("MDEL".to_string(), mdel.to_string());
        RecordInstance {
            name: name.to_string(),
            record_type: spvirit_tools::spvirit_server::types::RecordType::Ai,
            common: spvirit_tools::spvirit_server::types::DbCommonState::default(),
            data: RecordData::Ai {
                nt,
                inp: None,
                siml: None,
                siol: None,
                simm: true, // Ai is only writable() in simulation mode.
            },
            raw_fields,
        }
    }

    #[test]
    fn periodic_index_is_stale_only_when_the_generation_moves() {
        let cached: (u64, Vec<(String, Duration)>) =
            (7, vec![("A:PV".to_string(), Duration::from_secs(1))]);
        assert!(periodic_index_is_stale(None, 7), "an empty cache is always stale");
        assert!(!periodic_index_is_stale(Some(&cached), 7));
        assert!(periodic_index_is_stale(Some(&cached), 8));
    }

    #[tokio::test]
    async fn db_generation_starts_at_zero_and_bumps() {
        let state = test_state(mdel_ai_record("test_pv", 1.0, "0"));
        assert_eq!(state.db_generation(), 0);
        state.bump_db_generation();
        state.bump_db_generation();
        assert_eq!(state.db_generation(), 2);
    }

    const SCAN_SRC_DB: &str = r#"
record(ai, "SRC") {
    field(VAL, "0")
}
"#;

    fn scan_db(p_scan: &str) -> String {
        format!(
            r#"{SCAN_SRC_DB}
record(ai, "P") {{
    field(SCAN, "{p_scan}")
    field(INP, "SRC")
}}
"#
        )
    }

    /// A `ServerState` built from a real .db text, with a live event channel.
    fn scan_test_state(db: &str) -> (Arc<ServerState>, mpsc::Receiver<PostedEvent>) {
        let store = spvirit_tools::spvirit_server::db::parse_db(db).expect("test db parses");
        let (event_tx, event_rx) = mpsc::channel(16);
        let state = Arc::new(ServerState::new(
            store,
            false,
            event_tx,
            PvListMode::Off,
            1024,
            None,
            [0u8; 12],
            5075,
            None,
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        ));
        (state, event_rx)
    }

    async fn scan_value(state: &Arc<ServerState>, name: &str) -> f64 {
        let store = state.pv_store.read().await;
        match store.get(name).expect("record exists").to_ntpayload() {
            NtPayload::Scalar(nt) => scalar_as_f64(&nt.value).expect("numeric value"),
            _ => panic!("{name} is not a scalar"),
        }
    }

    async fn set_src(state: &Arc<ServerState>, v: f64) {
        let mut store = state.pv_store.write().await;
        store
            .get_mut("SRC")
            .expect("SRC exists")
            .set_scalar_value(ScalarValue::F64(v), false);
    }

    /// Poll every 20 ms until `name == want`, failing after 3 s.
    async fn wait_for_value(state: &Arc<ServerState>, name: &str, want: f64) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let got = scan_value(state, name).await;
            if got == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{name} never reached {want} within 3 s (last seen {got})"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The store swap + generation bump from `run_db_reload` (which itself
    /// polls a file's mtime, so it is not driven directly here).
    async fn simulate_reload(state: &Arc<ServerState>, db: &str) {
        let new_store = spvirit_tools::spvirit_server::db::parse_db(db).expect("reload db parses");
        let mut store = state.pv_store.write().await;
        *store = new_store;
        state.bump_db_generation();
    }

    // Real-time test (the bin's tests do not use a paused clock): positive
    // checks poll with a 3 s deadline; negative windows can only false-pass.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn periodic_scheduler_scans_and_follows_db_reload() {
        let periodic = scan_db("0.1 second");
        let passive = scan_db("Passive");
        let (state, _event_rx) = scan_test_state(&periodic);
        let sched_state = state.clone();
        let handle = tokio::spawn(async move {
            let _ = run_scan_scheduler(sched_state).await.is_ok();
        });

        for i in 1..=3 {
            set_src(&state, i as f64).await;
            wait_for_value(&state, "P", i as f64).await;
        }

        // Reload to a DB where P is Passive: it must stop scanning. The
        // reloaded P starts from its parsed VAL, so read that baseline.
        simulate_reload(&state, &passive).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let before = scan_value(&state, "P").await;
        set_src(&state, 42.0).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(
            scan_value(&state, "P").await,
            before,
            "a record made Passive by a reload must stop scanning"
        );

        // Reload back to the periodic DB: P must start scanning again.
        simulate_reload(&state, &periodic).await;
        set_src(&state, 7.0).await;
        wait_for_value(&state, "P", 7.0).await;

        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn event_worker_keeps_soft_and_io_events_apart() {
        let db = format!(
            r#"{SCAN_SRC_DB}
record(ai, "E") {{
    field(SCAN, "Event")
    field(EVNT, "T")
    field(INP, "SRC")
}}
record(ai, "I") {{
    field(SCAN, "I/O Intr")
    field(IOSCAN, "T")
    field(INP, "SRC")
}}
"#
        );
        let (state, event_rx) = scan_test_state(&db);
        set_src(&state, 5.0).await;
        state
            .event_tx
            .send(PostedEvent::Event("T".to_string()))
            .await
            .unwrap();
        state
            .event_tx
            .send(PostedEvent::Event("nope".to_string()))
            .await
            .unwrap();
        let worker_state = state.clone();
        let handle = tokio::spawn(async move {
            let _ = run_event_worker(worker_state, event_rx).await.is_ok();
        });

        wait_for_value(&state, "E", 5.0).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            scan_value(&state, "I").await,
            0.0,
            "a soft event must not fire an I/O Intr record sharing its tag"
        );

        set_src(&state, 9.0).await;
        state
            .event_tx
            .send(PostedEvent::IoEvent("T".to_string()))
            .await
            .unwrap();
        wait_for_value(&state, "I", 9.0).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            scan_value(&state, "E").await,
            5.0,
            "an I/O event must not fire a soft-event record sharing its tag"
        );

        handle.abort();
    }

    // Regression test for finding tools-T1 (cross-client head-of-line
    // blocking): a slow client whose bounded 128-deep queue is full must NOT
    // stall monitor fan-out to other, healthy clients. The fan-out loop sends
    // to conns in registration order, so the full client (conn 1) is visited
    // before the healthy one (conn 2). With a blocking `tx.send().await` this
    // hangs forever on conn 1 and conn 2 never receives its update; with the
    // non-blocking `try_send` the frame for conn 1 is dropped and conn 2 still
    // gets its frame.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_client_does_not_block_fanout_to_healthy_client() {
        let record = mdel_ai_record("test_pv", 1.0, "0");
        let payload = record.to_ntpayload();
        let state = test_state(record);

        // conn 1: a slow client. Fill its 128-deep queue to capacity so any
        // further send would block. Keep _rx1 alive so the channel stays open
        // (Full, not Closed).
        let (tx1, _rx1) = mpsc::channel::<Vec<u8>>(128);
        for _ in 0..128 {
            tx1.try_send(Vec::new()).expect("prefill slow client queue");
        }
        // conn 2: a healthy client with an empty queue.
        let (tx2, mut rx2) = mpsc::channel::<Vec<u8>>(128);

        {
            let mut conns = state.conns.lock().await;
            conns.insert(1, tx1);
            conns.insert(2, tx2);
        }
        {
            let mut monitors = state.monitors.lock().await;
            monitors.insert(
                "test_pv".to_string(),
                vec![
                    MonitorSub {
                        conn_id: 1,
                        ioid: 10,
                        version: 2,
                        is_be: false,
                        running: true,
                        pipeline_enabled: false,
                        nfree: 0,
                        filtered_desc: None,
                        last_snapshot: None,
                    },
                    MonitorSub {
                        conn_id: 2,
                        ioid: 20,
                        version: 2,
                        is_be: false,
                        running: true,
                        pipeline_enabled: false,
                        nfree: 0,
                        filtered_desc: None,
                        last_snapshot: None,
                    },
                ],
            );
        }

        // Must complete promptly. With the blocking send this future never
        // resolves (conn 1's queue is full), so the timeout fires and the test
        // fails.
        tokio::time::timeout(
            Duration::from_secs(2),
            notify_monitors(&state, "test_pv", &payload),
        )
        .await
        .expect("notify_monitors must not block on a full slow-client queue");

        // The healthy client must have received its monitor frame.
        let got = rx2
            .try_recv()
            .expect("healthy client should receive its monitor update");
        assert!(!got.is_empty(), "healthy client frame should be non-empty");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_delivered_sub_commits_exactly_once_across_many_subs() {
        let record = mdel_ai_record("test_pv", 1.0, "0");
        let payload = record.to_ntpayload();
        let state = test_state(record);

        // Keep the receivers alive for the whole test so every channel stays
        // open (Ok, not Closed) and none of the 8-deep queues fills.
        let mut rxs = Vec::new();
        for conn_id in 0..64u64 {
            let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
            state.conns.lock().await.insert(conn_id, tx);
            rxs.push(rx);
        }
        {
            let mut monitors = state.monitors.lock().await;
            let list = monitors.entry("test_pv".to_string()).or_default();
            for conn_id in 0..64u64 {
                list.push(MonitorSub {
                    conn_id,
                    ioid: 100 + conn_id as u32,
                    version: 2,
                    is_be: false,
                    running: true,
                    pipeline_enabled: true,
                    nfree: 4,
                    filtered_desc: None,
                    last_snapshot: None,
                });
            }
        }

        notify_monitors(&state, "test_pv", &payload).await;

        let monitors = state.monitors.lock().await;
        let list = monitors.get("test_pv").expect("subs");
        assert_eq!(list.len(), 64);
        for sub in list {
            assert_eq!(
                sub.nfree, 3,
                "conn {} must spend exactly one credit",
                sub.conn_id
            );
            assert!(sub.last_snapshot.is_some());
        }
    }

    // Regression test for finding R2-H1 (premature commit): when a pipelined
    // subscriber's frame is DROPPED because its bounded queue is full, the
    // fan-out must NOT consume a pipeline credit and must NOT advance the delta
    // baseline. The old code committed both under the monitors lock *before* the
    // lossy `try_send`, so a dropped frame leaked a credit (→ permanent stall,
    // since the client never ACKs a frame it never got) and latched the delta
    // baseline on an un-delivered value. This test uses a *pipelined* sub with a
    // full queue — the exact configuration the tools-T1 test could not exercise.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_pipelined_frame_does_not_consume_credit_or_advance_baseline() {
        let record = mdel_ai_record("test_pv", 1.0, "0");
        let payload = record.to_ntpayload();
        let state = test_state(record);

        // A slow pipelined client: fill its 128-deep queue so any send drops.
        let (tx1, _rx1) = mpsc::channel::<Vec<u8>>(128);
        for _ in 0..128 {
            tx1.try_send(Vec::new()).expect("prefill slow client queue");
        }
        {
            let mut conns = state.conns.lock().await;
            conns.insert(1, tx1);
        }
        {
            let mut monitors = state.monitors.lock().await;
            monitors.insert(
                "test_pv".to_string(),
                vec![MonitorSub {
                    conn_id: 1,
                    ioid: 10,
                    version: 2,
                    is_be: false,
                    running: true,
                    pipeline_enabled: true,
                    nfree: 1,
                    filtered_desc: None,
                    last_snapshot: None,
                }],
            );
        }

        notify_monitors(&state, "test_pv", &payload).await;

        let monitors = state.monitors.lock().await;
        let sub = &monitors.get("test_pv").unwrap()[0];
        assert_eq!(
            sub.nfree, 1,
            "credit must NOT be consumed for a dropped frame (would leak → stall)"
        );
        assert!(
            sub.last_snapshot.is_none(),
            "delta baseline must NOT advance past a value the client never received"
        );
    }

    // Positive counterpart: a healthy pipelined client whose frame is actually
    // enqueued MUST have its credit decremented and its delta baseline advanced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delivered_pipelined_frame_consumes_credit_and_advances_baseline() {
        let record = mdel_ai_record("test_pv", 1.0, "0");
        let payload = record.to_ntpayload();
        let state = test_state(record);

        let (tx1, mut rx1) = mpsc::channel::<Vec<u8>>(128);
        {
            let mut conns = state.conns.lock().await;
            conns.insert(1, tx1);
        }
        {
            let mut monitors = state.monitors.lock().await;
            monitors.insert(
                "test_pv".to_string(),
                vec![MonitorSub {
                    conn_id: 1,
                    ioid: 10,
                    version: 2,
                    is_be: false,
                    running: true,
                    pipeline_enabled: true,
                    nfree: 1,
                    filtered_desc: None,
                    last_snapshot: None,
                }],
            );
        }

        notify_monitors(&state, "test_pv", &payload).await;

        assert!(
            rx1.try_recv().is_ok(),
            "healthy client should receive its frame"
        );
        let monitors = state.monitors.lock().await;
        let sub = &monitors.get("test_pv").unwrap()[0];
        assert_eq!(sub.nfree, 0, "credit must be consumed for a delivered frame");
        assert!(
            sub.last_snapshot.is_some(),
            "delta baseline must advance after a delivered frame"
        );
    }

    // A client must not be able to request an unbounded pipeline window: every
    // path that sets `nfree` from client input clamps it to MAX_PIPELINE_WINDOW.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_pipeline_window_is_clamped_to_ceiling() {
        let record = mdel_ai_record("test_pv", 1.0, "0");
        let state = test_state(record);
        {
            let mut monitors = state.monitors.lock().await;
            monitors.insert(
                "test_pv".to_string(),
                vec![MonitorSub {
                    conn_id: 1,
                    ioid: 10,
                    version: 2,
                    is_be: false,
                    running: true,
                    pipeline_enabled: true,
                    nfree: 0,
                    filtered_desc: None,
                    last_snapshot: None,
                }],
            );
        }

        // Client asks for a wildly oversized window.
        let ok = update_monitor_subscription(
            &state,
            1,
            10,
            "test_pv",
            true,
            Some(u32::MAX),
            Some(true),
        )
        .await;
        assert!(ok, "subscription should be found and updated");

        let monitors = state.monitors.lock().await;
        let sub = &monitors.get("test_pv").unwrap()[0];
        assert_eq!(
            sub.nfree, MAX_PIPELINE_WINDOW,
            "client-supplied nfree must be capped at MAX_PIPELINE_WINDOW"
        );
    }

    fn ts_field(seconds: i64, nanos: i32) -> DecodedValue {
        DecodedValue::Structure(vec![
            (
                "secondsPastEpoch".to_string(),
                DecodedValue::Int64(seconds),
            ),
            ("nanoseconds".to_string(), DecodedValue::Int32(nanos)),
            ("userTag".to_string(), DecodedValue::Int32(0)),
        ])
    }

    // Regression test for the Task 6 review finding: a client-stamped PUT
    // that *also* changes the value must be judged by should_post_update
    // (never force-posted), and posting must refresh the MDEL reference
    // point in `last_posted` — matching SimplePvStore::put exactly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_stamped_value_change_consults_deadband_and_updates_last_posted() {
        let state = test_state(mdel_ai_record("test_pv", 1.0, "0.5"));

        // Sanity: ServerState::new seeded the MDEL reference from the
        // starting value.
        {
            let last_posted = state.last_posted.lock().await;
            assert_eq!(last_posted.get("test_pv"), Some(&(1.0, 0)));
        }

        // A PUT that changes the value (delta 1.0 >= MDEL 0.5, so it clears
        // the deadband) and also carries a non-default client timestamp.
        let body = DecodedValue::Structure(vec![
            ("value".to_string(), DecodedValue::Float64(2.0)),
            ("timeStamp".to_string(), ts_field(5_000, 0)),
        ]);
        let (changed, force) = apply_put_and_process(&state, "test_pv", &body)
            .await
            .expect("put accepted");

        // The value change must not be force-posted: it goes through the
        // ordinary deadband check like any other value change.
        assert!(
            force.is_empty(),
            "a value-changing PUT must never be in the force set, even when client-stamped"
        );

        notify_changed_records_forced(&state, changed, &force).await;

        // Posting a value change must refresh the MDEL reference point.
        let last_posted = state.last_posted.lock().await;
        assert_eq!(
            last_posted.get("test_pv"),
            Some(&(2.0, 0)),
            "should_post_update must run on a value change so last_posted stays current"
        );
    }
}

fn file_mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}
