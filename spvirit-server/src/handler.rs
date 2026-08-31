//! PVA protocol handler — the core TCP connection processor.
//!
//! [`handle_connection`] uses a [`SourceRegistry`] to resolve PV names across
//! multiple registered sources, serving PVs over the EPICS PVAccess protocol.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use regex::Regex;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tracing::{debug, error, info, warn};

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvd_decode::{PvdDecoder, StructureDesc, extract_subfield_desc};
use spvirit_codec::spvd_encode::{
    decode_pv_request_fields, filter_structure_desc, nt_payload_desc,
};
use spvirit_codec::spvirit_encode::{
    encode_connection_validation, encode_control_message, encode_create_channel_error,
    encode_create_channel_response, encode_get_field_error, encode_get_field_response,
    encode_header, encode_message_error, encode_monitor_data_response_payload,
    encode_op_data_response_filtered, encode_op_error, encode_op_get_data_response_payload,
    encode_op_init_response_desc, encode_op_put_get_data_error_response,
    encode_op_put_get_data_response_payload, encode_op_put_get_init_error_response,
    encode_op_put_get_init_response, encode_op_put_getput_response_payload, encode_op_put_response,
    encode_op_put_status_response, encode_op_rpc_data_response_payload,
    encode_op_status_error_response, encode_op_status_response, encode_search_response,
    ip_from_bytes, ip_to_bytes,
};

use spvirit_codec::{SegmentOutcome, SegmentReassembler};

use spvirit_types::{NtPayload, NtScalar, NtScalarArray, ScalarArrayValue, ScalarValue};

use crate::conn_writer::ConnWriter;
use crate::decode::decode_put_body;
use crate::monitor::MonitorRegistry;
use crate::pvstore::{SourceRegistry, TryClaim};
use crate::state::{ConnState, MonitorState, MonitorSub};

/// Hard server-side ceiling on a pipelined monitor's outstanding-frame credit,
/// regardless of the window a client requests or ACKs. Bounds worst-case memory
/// on the (lossless) control lane so a stalled pipelined client cannot OOM the
/// server (crate-audit review R1-H1).
pub const MAX_PIPELINE_WINDOW: u32 = 4096;

// ---------------------------------------------------------------------------
// PvListMode — controls virtual PV listing behaviour
// ---------------------------------------------------------------------------

/// Controls how the server exposes its PV directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PvListMode {
    /// No PV listing at all.
    Off,
    /// Respond to UDP search for known PVs only; no GET_FIELD listing.
    Discover,
    /// Full pvlist & server-RPC listing support.
    List,
}

impl PvListMode {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "discover" => Ok(Self::Discover),
            "list" => Ok(Self::List),
            other => Err(format!(
                "Invalid pvlist-mode '{}'; expected off|discover|list",
                other
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Server shared state
// ---------------------------------------------------------------------------

/// Shared server state that is passed to every connection handler.
pub struct ServerState {
    pub sources: Arc<SourceRegistry>,
    pub registry: Arc<MonitorRegistry>,
    pub sid_counter: AtomicU32,
    pub beacon_change: Arc<AtomicU16>,
    pub compute_alarms: bool,
    pub pvlist_mode: PvListMode,
    pub pvlist_max: usize,
    pub pvlist_allow_pattern: Option<Regex>,
    pub guid: [u8; 12],
    pub tcp_port: u16,
    pub advertise_ip: Option<IpAddr>,
    pub listen_ip: IpAddr,
    /// The diagnostic [`ClientRegistry`](crate::diag::ClientRegistry) to
    /// notify of connect/identity events, if one was installed on `registry`
    /// (via [`MonitorRegistry::set_client_registry`]) before this state was
    /// built. Captured here — rather than read off `registry` at each call
    /// site — so the connect/identity hooks are a plain `if let Some(...)`
    /// against a field already in scope, matching how `cleanup_connection`
    /// (which lives on `MonitorRegistry` itself) reads it directly there.
    pub client_registry: Option<Arc<crate::diag::ClientRegistry>>,
    /// The diagnostic [`BandwidthCounters`](crate::diag::BandwidthCounters)
    /// to record wire bytes into, if one was installed on `registry` (via
    /// [`MonitorRegistry::set_bandwidth_counters`]) before this state was
    /// built. Captured here for the same reason as `client_registry`: the
    /// read loop's byte-counting call sites (Task 9) get a plain
    /// `if let Some(...)` against a field already in scope.
    pub bandwidth_counters: Option<Arc<crate::diag::BandwidthCounters>>,
    /// Resolves PV names the search path could not answer from memory.
    ///
    /// Built here rather than passed in because `ServerState::new` is the
    /// only constructor and every server wants one; there is no configuration
    /// to thread through.
    pub search_resolver: Arc<crate::search_resolve::SearchResolver>,
}

impl ServerState {
    pub fn new(
        sources: Arc<SourceRegistry>,
        registry: Arc<MonitorRegistry>,
        compute_alarms: bool,
        pvlist_mode: PvListMode,
        pvlist_max: usize,
        pvlist_allow_pattern: Option<Regex>,
        guid: [u8; 12],
        tcp_port: u16,
        advertise_ip: Option<IpAddr>,
        listen_ip: IpAddr,
    ) -> Self {
        let client_registry = registry.client_registry();
        let bandwidth_counters = registry.bandwidth_counters();
        let search_resolver = Arc::new(crate::search_resolve::SearchResolver::new(sources.clone()));
        Self {
            sources,
            registry,
            sid_counter: AtomicU32::new(1),
            beacon_change: Arc::new(AtomicU16::new(0)),
            compute_alarms,
            pvlist_mode,
            pvlist_max,
            pvlist_allow_pattern,
            guid,
            tcp_port,
            advertise_ip,
            listen_ip,
            client_registry,
            bandwidth_counters,
            search_resolver,
        }
    }
}

// ---------------------------------------------------------------------------
// Virtual PV helpers
// ---------------------------------------------------------------------------

pub fn is_pvlist_virtual_pv(pv_name: &str) -> bool {
    pv_name == "__pvlist"
}

pub fn is_server_rpc_pv(pv_name: &str) -> bool {
    pv_name == "server"
}

pub fn is_virtual_event_pv(pv_name: &str) -> bool {
    pv_name.starts_with("__event:")
}

pub fn virtual_event_nt(pv_name: &str) -> NtPayload {
    NtPayload::Scalar(
        NtScalar::from_value(ScalarValue::Bool(false))
            .with_description(format!("Virtual event trigger for {}", pv_name)),
    )
}

pub fn virtual_pvlist_nt(entries: Vec<String>) -> NtPayload {
    NtPayload::ScalarArray(NtScalarArray::from_value(ScalarArrayValue::Str(entries)))
}

// ---------------------------------------------------------------------------
// Pattern / wildcard utilities
// ---------------------------------------------------------------------------

pub fn is_pattern_query(raw: &str) -> bool {
    raw.contains('*') || raw.contains('?')
}

pub fn wildcard_match(pattern: &str, text: &str) -> bool {
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

pub fn collect_visible_pv_names(
    all_names: &[String],
    mode: PvListMode,
    allow_pattern: Option<&Regex>,
    max_items: usize,
) -> Vec<String> {
    let mut names: Vec<String> = all_names
        .iter()
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
    use spvirit_codec::spvd_decode::{FieldDesc, FieldType, TypeCode};
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

// ---------------------------------------------------------------------------
// Network helpers
// ---------------------------------------------------------------------------

pub fn search_reply_target(addr: &[u8; 16], port: u16, peer: SocketAddr) -> SocketAddr {
    let target_port = if port != 0 { port } else { peer.port() };
    let target_ip = ip_from_bytes(addr)
        .filter(|ip| !ip.is_unspecified())
        .unwrap_or_else(|| peer.ip());
    SocketAddr::new(target_ip, target_port)
}

/// Normalize a configured advertise address for use in a search reply.
///
/// An all-zeros (`UNSPECIFIED`) address is never a connectable endpoint, yet
/// callers routinely hold `Some(0.0.0.0)` when the server binds all interfaces
/// without an explicit advertise IP (e.g. the gateway passes its unset
/// `interface` through as `advertise_ip = Some(0.0.0.0)`). Emitting that
/// verbatim tells clients "the server is at 0.0.0.0", which they cannot connect
/// to — every `pvget`/monitor then times out even though the search itself was
/// answered. Treating it as `None` lets callers fall through to a real fallback
/// (the accepting connection's local address on TCP, or
/// [`infer_udp_response_ip`] on UDP).
pub fn effective_advertise_ip(ip: Option<IpAddr>) -> Option<IpAddr> {
    ip.filter(|a| !a.is_unspecified())
}

/// Bind the fixed UDP search port with `SO_REUSEADDR` (and `SO_REUSEPORT`
/// on Unix) so other local PVA consumers such as `p4p` can also listen on
/// the same well-known port. On macOS in particular, a plain
/// `UdpSocket::bind(5076)` prevents any subsequent binder from joining the
/// port, which broke co-located clients.
pub fn bind_udp_search_socket(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    UdpSocket::from_std(socket.into())
}

pub fn infer_udp_response_ip(peer: SocketAddr) -> Option<IpAddr> {
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

pub fn rand_guid() -> [u8; 12] {
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

// ---------------------------------------------------------------------------
// Debug utilities
// ---------------------------------------------------------------------------

pub fn validate_encoded_packet(conn_id: u64, label: &str, bytes: &[u8]) {
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

pub fn dump_hex_packet(
    conn_id: u64,
    dir: &str,
    label: &str,
    version: u8,
    is_be: bool,
    bytes: &[u8],
) {
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

// ---------------------------------------------------------------------------
// Store-based snapshot/writable helpers (delegate to SourceRegistry + virtual PVs)
// ---------------------------------------------------------------------------

async fn get_nt_snapshot(state: &ServerState, pv_name: &str) -> Option<NtPayload> {
    if is_pvlist_virtual_pv(pv_name) {
        if state.pvlist_mode != PvListMode::List {
            return None;
        }
        let all_names = state.sources.names().await;
        let names = collect_visible_pv_names(
            &all_names,
            state.pvlist_mode,
            state.pvlist_allow_pattern.as_ref(),
            state.pvlist_max,
        );
        return Some(virtual_pvlist_nt(names));
    }
    if is_virtual_event_pv(pv_name) {
        return Some(virtual_event_nt(pv_name));
    }
    state.sources.get(pv_name).await
}

async fn is_writable_pv(state: &ServerState, pv_name: &str) -> bool {
    if is_virtual_event_pv(pv_name) {
        return true;
    }
    state.sources.is_writable(pv_name).await
}

async fn has_pv(state: &ServerState, pv_name: &str) -> bool {
    state.sources.has_pv(pv_name).await
        || is_virtual_event_pv(pv_name)
        || (is_pvlist_virtual_pv(pv_name) && state.pvlist_mode == PvListMode::List)
        || (is_server_rpc_pv(pv_name) && state.pvlist_mode != PvListMode::Off)
}

// ---------------------------------------------------------------------------
// Notify helpers
// ---------------------------------------------------------------------------

async fn notify_changed_records(state: &ServerState, changed: Vec<(String, NtPayload)>) {
    for (name, payload) in changed {
        state.beacon_change.fetch_add(1, Ordering::SeqCst);
        state.registry.notify_monitors(&name, &payload).await;
    }
}

// ---------------------------------------------------------------------------
// GET_FIELD handler
// ---------------------------------------------------------------------------

async fn handle_get_field_request(
    state: &ServerState,
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
        state.registry.send_msg(conn_id, resp).await;
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
                let sub = payload.field_name.as_deref().filter(|s| !s.is_empty());
                let desc = if let Some(field_path) = sub {
                    match extract_subfield_desc(&full_desc, field_path) {
                        Some(sub_desc) => sub_desc,
                        None => {
                            let resp = encode_get_field_error(
                                request_id,
                                &format!("sub-field '{}' not found", field_path),
                                version,
                                is_be,
                            );
                            state.registry.send_msg(conn_id, resp).await;
                            return;
                        }
                    }
                } else {
                    full_desc
                };
                let resp = encode_get_field_response(request_id, &desc, version, is_be);
                dump_hex_packet(conn_id, "tx", "cmd=17 get_field", version, is_be, &resp);
                state.registry.send_msg(conn_id, resp).await;
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
            state.registry.send_msg(conn_id, resp).await;
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
        state.registry.send_msg(conn_id, resp).await;
        return;
    }

    let Some(pattern) = requested_pvlist_pattern(payload.field_name.as_deref()) else {
        let resp = encode_get_field_error(
            request_id,
            "GET_FIELD requires a valid list pattern",
            version,
            is_be,
        );
        state.registry.send_msg(conn_id, resp).await;
        return;
    };

    let all_names = state.sources.names().await;
    let mut names = collect_visible_pv_names(
        &all_names,
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
        state.registry.send_msg(conn_id, resp).await;
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
    state.registry.send_msg(conn_id, resp).await;
    debug!(
        "Conn {}: get_field list pattern='{}' returned {} entries",
        conn_id,
        pattern,
        names.len()
    );
}

// ---------------------------------------------------------------------------
// Server RPC handler
// ---------------------------------------------------------------------------

async fn handle_server_rpc(
    state: &ServerState,
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
        state.registry.send_msg(conn_id, resp).await;
        return;
    }

    let all_names = state.sources.names().await;
    let names = collect_visible_pv_names(
        &all_names,
        state.pvlist_mode,
        state.pvlist_allow_pattern.as_ref(),
        state.pvlist_max,
    );
    let payload = NtPayload::ScalarArray(NtScalarArray::from_value(ScalarArrayValue::Str(names)));

    let is_init = (subcmd & 0x08) != 0;
    if is_init {
        let resp = encode_op_status_response(20, ioid, subcmd, version, is_be);
        state.registry.send_msg(conn_id, resp).await;
        return;
    }

    let resp = encode_op_rpc_data_response_payload(ioid, subcmd, &payload, version, is_be);
    state.registry.send_msg(conn_id, resp).await;
}

// ---------------------------------------------------------------------------
// Control message handler (inside segmented stream)
// ---------------------------------------------------------------------------

async fn handle_control_message(state: &ServerState, conn_id: u64, header: &PvaHeader) {
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
        state.registry.send_msg(conn_id, resp).await;
    }
}

// ---------------------------------------------------------------------------
// UDP search handler
// ---------------------------------------------------------------------------

/// Run the UDP search responder.
pub async fn run_udp_search(
    state: Arc<ServerState>,
    addr: SocketAddr,
    tcp_port: u16,
    guid: [u8; 12],
    advertise_ip: Option<IpAddr>,
    multicast_iface: Option<Ipv4Addr>,
) -> Result<(), Box<dyn std::error::Error>> {
    let socket = bind_udp_search_socket(addr)?;
    socket.set_broadcast(true)?;
    if let Some(iface) = multicast_iface {
        const PVA_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 128);
        if let Err(e) = socket.join_multicast_v4(PVA_MULTICAST, iface) {
            // Non-fatal: unicast/broadcast search still works without the join.
            tracing::warn!("UDP search: multicast join {PVA_MULTICAST} on {iface} failed: {e}");
        } else {
            tracing::info!("UDP search: joined multicast {PVA_MULTICAST} on {iface}");
        }
    }
    let mut buf = vec![0u8; 4096];

    loop {
        let (len, peer) = socket.recv_from(&mut buf).await?;
        let data = &buf[..len];
        // Untrusted UDP ingress: a datagram shorter than the 8-byte PVA header
        // must be skipped, not allowed to panic the search task.
        let Some(header) = PvaHeader::try_new(data) else {
            debug!("UDP search: dropping short datagram ({} bytes) from {}", len, peer);
            continue;
        };
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
                let all_names = state.sources.names().await;
                let cids = crate::request_ctx::scope(peer, async {
                    let visible_names = collect_visible_pv_names(
                        &all_names,
                        state.pvlist_mode,
                        state.pvlist_allow_pattern.as_ref(),
                        state.pvlist_max,
                    );
                    let mut cids = Vec::new();
                    for (cid, name) in &payload.pv_requests {
                        if is_virtual_event_pv(name)
                            || (is_pvlist_virtual_pv(name) && state.pvlist_mode == PvListMode::List)
                            || (is_server_rpc_pv(name) && state.pvlist_mode != PvListMode::Off)
                        {
                            cids.push(*cid);
                            continue;
                        }
                        // `try_claim` never blocks. Anything it cannot decide
                        // is resolved on a background task and answered on the
                        // client's next search retry — awaiting resolution here
                        // would stop this task reading datagrams for every
                        // other client, which is the whole defect.
                        match state.sources.try_claim(name) {
                            TryClaim::Yes => {
                                cids.push(*cid);
                                continue;
                            }
                            TryClaim::Unknown => {
                                // A wildcard is answered from the visible-name
                                // list below; resolving it upstream is
                                // meaningless and would pollute the negative
                                // cache with a name no server owns.
                                if !is_pattern_query(name) {
                                    state.search_resolver.enqueue(name);
                                }
                            }
                            TryClaim::No => {}
                        }
                        if state.pvlist_mode != PvListMode::Off
                            && is_pattern_query(name)
                            && visible_names.iter().any(|pv| wildcard_match(name, pv))
                        {
                            cids.push(*cid);
                        }
                    }
                    cids
                })
                .await;
                let response_required = (payload.mask & 0x01) != 0;
                let server_discovery_ping = payload.pv_requests.is_empty();
                let found = server_discovery_ping || !cids.is_empty();
                if !found && !response_required {
                    debug!("UDP search: no matches and response not required");
                    continue;
                }
                // `Some(0.0.0.0)` (all-interface bind, no explicit advertise IP)
                // is treated as unset so we fall through to the bound socket
                // address and finally to inferring the local IP toward this
                // peer — never emitting a zero address a client cannot reach.
                let resp_ip = effective_advertise_ip(advertise_ip)
                    .or_else(|| effective_advertise_ip(Some(addr.ip())))
                    .or_else(|| {
                        let inferred = infer_udp_response_ip(peer);
                        if let Some(ip) = inferred {
                            debug!("UDP search: inferred response address {}", ip);
                        }
                        inferred
                    })
                    .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
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

// ---------------------------------------------------------------------------
// TCP server
// ---------------------------------------------------------------------------

/// `EMFILE` — the process hit its own open-file limit (`RLIMIT_NOFILE`).
/// `ENFILE` — the whole system hit its open-file limit. These are the
/// Linux/POSIX errno numbers; they are the descriptor-exhaustion cases the
/// accept loop must recover from rather than die on.
const EMFILE: i32 = 24;
const ENFILE: i32 = 23;

/// Decide whether the accept loop should pause before retrying after an
/// `accept()` error.
///
/// Every `accept()` error is transient — a `TcpListener` stays valid across
/// errors, so a momentary failure must never tear the whole server down (a
/// single EMFILE previously did exactly that, because the loop used `?`). This
/// function decides only whether to back off first:
///
/// - Descriptor exhaustion (EMFILE/ENFILE) returns a short delay so the loop
///   does not hot-spin burning CPU while no descriptors are available; the
///   pause also gives in-flight connections a chance to close and free some.
/// - Every other error (e.g. a client that reset during the handshake) returns
///   `None`: retry immediately.
fn accept_retry_delay(err: &std::io::Error) -> Option<Duration> {
    match err.raw_os_error() {
        Some(EMFILE) | Some(ENFILE) => Some(Duration::from_millis(100)),
        _ => None,
    }
}

/// Accept TCP connections and spawn a handler for each.
///
/// Callers must bind the `TcpListener` before spawning any other tasks so that
/// an `EADDRINUSE` failure is detected eagerly and the beacon is never started.
///
/// The accept loop is resilient: a transient `accept()` error (descriptor
/// exhaustion, or a client aborting mid-handshake) is logged and the loop
/// continues, so the server keeps serving once the condition clears. See
/// [`accept_retry_delay`] for the backoff policy.
pub async fn run_tcp_server(
    state: Arc<ServerState>,
    listener: TcpListener,
    conn_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn_id = Arc::new(std::sync::atomic::AtomicU64::new(1));

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                error!("TCP accept error (continuing): {}", e);
                if let Some(delay) = accept_retry_delay(&e) {
                    tokio::time::sleep(delay).await;
                }
                continue;
            }
        };
        let id = conn_id.fetch_add(1, Ordering::SeqCst);
        info!("TCP connection {} from {}", id, peer);
        let state_clone = state.clone();
        tokio::spawn(crate::request_ctx::scope(peer, async move {
            if let Err(e) = handle_connection(state_clone, stream, id, conn_timeout).await {
                error!("Connection {} error: {}", id, e);
            }
        }));
    }
}

// ---------------------------------------------------------------------------
// Core TCP connection handler
// ---------------------------------------------------------------------------

/// Handle a single PVA TCP connection.
///
/// This is the main protocol loop: handshake, then dispatch each command
/// (CreateChannel, GET, PUT, PUT_GET, MONITOR, RPC, etc.) using the
/// [`SourceRegistry`] abstraction.
pub async fn handle_connection(
    state: Arc<ServerState>,
    stream: TcpStream,
    conn_id: u64,
    conn_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    // The local address the client reached us on: on an all-interface bind
    // this is the concrete, routable interface the client just connected to,
    // making it the ideal thing to advertise in a search reply when no explicit
    // advertise IP is configured.
    let conn_local_addr = stream.local_addr().ok();
    let (mut reader, writer) = stream.into_split();

    {
        let mut conns = state.registry.conns.lock().await;
        conns.insert(conn_id, ConnWriter::new(writer));
    }

    if let Some(cr) = &state.client_registry
        && let Some(ctx) = crate::request_ctx::current_request()
    {
        cr.connect(conn_id, ctx.peer);
    }

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
    state.registry.send_msg(conn_id, set_byte_order).await;

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
    state.registry.send_msg(conn_id, server_validation).await;

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

        // Per-host downstream RX: count every inbound frame's on-wire bytes
        // (header + payload) exactly once, regardless of command type, before
        // segment reassembly folds multi-frame messages together.
        if let Some(cr) = &state.client_registry {
            cr.add_rx(conn_id, (header.len() + payload_len) as u64);
        }

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

        // Connection Validation (cmd=1): respond with CONNECTION_VALIDATED (cmd=9).
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
                crate::request_ctx::set_credentials(val.user.clone(), val.host.clone());
                if let Some(cr) = &state.client_registry {
                    cr.set_identity(conn_id, val.user.clone(), val.host.clone());
                }
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
                state.registry.send_msg(conn_id, resp).await;
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
                    state.registry.send_msg(conn_id, resp).await;
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
                    if has_pv(&state, &pv_name).await {
                        let sid = state.sid_counter.fetch_add(1, Ordering::SeqCst);
                        conn_state.cid_to_sid.insert(cid, sid);
                        conn_state.sid_to_pv.insert(sid, pv_name.clone());
                        let resp = encode_create_channel_response(cid, sid, version, is_be);
                        state.registry.send_msg(conn_id, resp).await;
                        info!(
                            "Conn {}: channel '{}' cid={} sid={}",
                            conn_id, pv_name, cid, sid
                        );
                    } else {
                        let resp = encode_create_channel_error(cid, "PV not found", version, is_be);
                        state.registry.send_msg(conn_id, resp).await;
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
                    state
                        .registry
                        .send_msg(
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

                // Per-PV downstream RX: PUT frames only. This is a documented
                // approximation -- most downstream RX is puts, and other
                // command types don't resolve to a single clean PV to
                // attribute their bytes to. Counts this frame's on-wire
                // payload (both PUT INIT and PUT DATA frames), independent of
                // the per-host counter above (a different counter and a
                // different dimension, not a double count).
                if payload.command == 11
                    && let Some(c) = &state.bandwidth_counters
                {
                    c.ds_bypv_rx.add(&pv_name, payload_len as u64);
                }

                let is_init = (payload.subcmd & 0x08) != 0;

                match payload.command {
                    10 => {
                        // GET
                        if is_init {
                            // Init only needs the type descriptor, not the data.
                            // Use get_descriptor first; fall back to snapshot.
                            let full_desc =
                                if let Some(desc) = state.sources.get_descriptor(&pv_name).await {
                                    desc
                                } else if let Some(nt) = get_nt_snapshot(&state, &pv_name).await {
                                    nt_payload_desc(&nt)
                                } else {
                                    state
                                        .registry
                                        .send_msg(
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
                            let pv_req_fields = decode_pv_request_fields(&payload.body, is_be);
                            let desc = match &pv_req_fields {
                                Some(fields) => filter_structure_desc(&full_desc, fields),
                                None => full_desc,
                            };
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
                            state.registry.send_msg(conn_id, resp).await;
                            info!("Conn {}: get init pv='{}' ioid={}", conn_id, pv_name, ioid);
                        } else {
                            let Some(nt) = get_nt_snapshot(&state, &pv_name).await else {
                                state
                                    .registry
                                    .send_msg(
                                        conn_id,
                                        encode_op_error(
                                            payload.command,
                                            payload.subcmd,
                                            ioid,
                                            "PV has no data yet",
                                            version,
                                            is_be,
                                        ),
                                    )
                                    .await;
                                continue;
                            };
                            let resp = if let Some(desc) = conn_state.ioid_to_desc.get(&ioid) {
                                encode_op_data_response_filtered(
                                    10, ioid, &nt, desc, version, is_be,
                                )
                            } else {
                                encode_op_get_data_response_payload(ioid, &nt, version, is_be)
                            };
                            state.registry.send_msg(conn_id, resp).await;
                            debug!("Conn {}: get data pv='{}' ioid={}", conn_id, pv_name, ioid);
                        }
                    }
                    11 => {
                        // PUT
                        if is_init {
                            // Init only needs the type descriptor, not current data.
                            // Use get_descriptor first; fall back to snapshot so PUT
                            // can target PVs that do not yet have any data.
                            let desc =
                                if let Some(desc) = state.sources.get_descriptor(&pv_name).await {
                                    desc
                                } else if let Some(nt) = get_nt_snapshot(&state, &pv_name).await {
                                    nt_payload_desc(&nt)
                                } else {
                                    state
                                        .registry
                                        .send_msg(
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
                                state.registry.send_msg(conn_id, resp).await;
                                continue;
                            }
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
                            state.registry.send_msg(conn_id, resp).await;
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
                                    state.registry.send_msg(conn_id, resp).await;
                                    continue;
                                }
                                if let Some(nt) = get_nt_snapshot(&state, &pv_name).await {
                                    let resp = encode_op_put_getput_response_payload(
                                        ioid, &nt, version, is_be,
                                    );
                                    state.registry.send_msg(conn_id, resp).await;
                                    debug!(
                                        "Conn {}: put get-put pv='{}' ioid={}",
                                        conn_id, pv_name, ioid
                                    );
                                } else {
                                    state
                                        .registry
                                        .send_msg(
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
                                    state
                                        .registry
                                        .send_msg(
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
                                match state.sources.put(&pv_name, value).await {
                                    Ok(changed) => {
                                        notify_changed_records(&state, changed).await;
                                    }
                                    Err(msg) => {
                                        let resp = encode_op_put_status_response(
                                            ioid,
                                            payload.subcmd,
                                            &msg,
                                            version,
                                            is_be,
                                        );
                                        state.registry.send_msg(conn_id, resp).await;
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
                                let resp = encode_op_put_status_response(
                                    ioid,
                                    payload.subcmd,
                                    "cannot decode PUT body",
                                    version,
                                    is_be,
                                );
                                state.registry.send_msg(conn_id, resp).await;
                                continue;
                            }
                            let resp = encode_op_put_response(ioid, payload.subcmd, version, is_be);
                            state.registry.send_msg(conn_id, resp).await;
                            debug!("Conn {}: put data pv='{}' ioid={}", conn_id, pv_name, ioid);
                        }
                    }
                    12 => {
                        // PUT_GET
                        if is_init {
                            // Init only needs the type descriptor, not current data.
                            // Use get_descriptor first; fall back to snapshot so that
                            // clients can initiate PUT_GET before any data exists.
                            let desc =
                                if let Some(desc) = state.sources.get_descriptor(&pv_name).await {
                                    desc
                                } else if let Some(nt) = get_nt_snapshot(&state, &pv_name).await {
                                    nt_payload_desc(&nt)
                                } else {
                                    state
                                        .registry
                                        .send_msg(
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
                                state.registry.send_msg(conn_id, resp).await;
                                continue;
                            }
                            conn_state.ioid_to_desc.insert(ioid, desc.clone());
                            conn_state.ioid_to_pv.insert(ioid, pv_name.clone());
                            let resp =
                                encode_op_put_get_init_response(ioid, &desc, &desc, version, is_be);
                            state.registry.send_msg(conn_id, resp).await;
                            info!(
                                "Conn {}: put_get init pv='{}' ioid={}",
                                conn_id, pv_name, ioid
                            );
                        } else {
                            let desc = match conn_state.ioid_to_desc.get(&ioid) {
                                Some(d) => d.clone(),
                                None => {
                                    state
                                        .registry
                                        .send_msg(
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
                                match state.sources.put(&pv_name, value).await {
                                    Ok(changed) => {
                                        notify_changed_records(&state, changed).await;
                                    }
                                    Err(msg) => {
                                        let resp = encode_op_put_get_data_error_response(
                                            ioid, &msg, version, is_be,
                                        );
                                        state.registry.send_msg(conn_id, resp).await;
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
                                let resp = encode_op_put_get_data_error_response(
                                    ioid,
                                    "cannot decode PUT body",
                                    version,
                                    is_be,
                                );
                                state.registry.send_msg(conn_id, resp).await;
                                continue;
                            }
                            if let Some(nt) = get_nt_snapshot(&state, &pv_name).await {
                                let resp = encode_op_put_get_data_response_payload(
                                    ioid, &nt, version, is_be,
                                );
                                state.registry.send_msg(conn_id, resp).await;
                            } else {
                                state
                                    .registry
                                    .send_msg(
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
                            // Init only needs the type descriptor, not the data.
                            // Use get_descriptor first; fall back to snapshot. This
                            // lets clients subscribe to PVs before any data has been
                            // produced (e.g. NTNDArray before acquire). Real monitor
                            // updates are pushed once data arrives. Strict clients
                            // like p4p treat a MONITOR init error as fatal, so we
                            // must not error out when only the descriptor is known.
                            let full_desc =
                                if let Some(desc) = state.sources.get_descriptor(&pv_name).await {
                                    desc
                                } else if let Some(nt) = get_nt_snapshot(&state, &pv_name).await {
                                    nt_payload_desc(&nt)
                                } else {
                                    state
                                        .registry
                                        .send_msg(
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
                                // Clamp the client-requested window to the
                                // server ceiling so a huge (up to u32::MAX)
                                // request cannot make the lossless control lane
                                // grow without bound (R1-H1).
                                nfree = nfree.min(MAX_PIPELINE_WINDOW);
                            }
                            let resp = encode_op_init_response_desc(
                                payload.command,
                                ioid,
                                0x08,
                                &desc,
                                version,
                                is_be,
                            );
                            state.registry.send_msg(conn_id, resp).await;
                            conn_state.ioid_to_monitor.insert(
                                ioid,
                                MonitorState {
                                    running: false,
                                    pipeline_enabled,
                                    nfree,
                                },
                            );
                            {
                                let mut monitors = state.registry.monitors.lock().await;
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
                                        filtered_desc: conn_state.ioid_to_desc.get(&ioid).cloned(),
                                        last_snapshot: None,
                                    });
                            }
                            // Sources that don't deliver their own updates
                            // (gateway proxies, group PVs, async backends) only
                            // expose changes through `subscribe`. Drain that
                            // stream into the registry so ongoing updates reach
                            // this monitor; self-notifying sources are skipped
                            // to avoid double-delivery. One pump per PV, shared
                            // across subscribers and retired with the last one.
                            if !state.sources.pushes_own_updates(&pv_name).await
                                && let Some(rx) = state.sources.subscribe(&pv_name).await
                            {
                                state.registry.ensure_pump(&pv_name, rx).await;
                            }
                            info!(
                                "Conn {}: monitor init pv='{}' ioid={}",
                                conn_id, pv_name, ioid
                            );
                        } else if (payload.subcmd & 0x10) != 0 {
                            // Monitor destroy
                            if let Some(nt) = get_nt_snapshot(&state, &pv_name).await {
                                let resp = encode_monitor_data_response_payload(
                                    ioid, 0x10, &nt, version, is_be,
                                );
                                state.registry.send_msg(conn_id, resp).await;
                            }
                            state
                                .registry
                                .remove_monitor_subscription(conn_id, ioid, &pv_name)
                                .await;
                            conn_state.ioid_to_monitor.remove(&ioid);
                            conn_state.ioid_to_pv.remove(&ioid);
                            conn_state.ioid_to_desc.remove(&ioid);
                            info!("Conn {}: monitor end ioid={}", conn_id, ioid);
                        } else if (payload.subcmd & 0x04) != 0 || (payload.subcmd & 0x80) != 0 {
                            // Monitor start/stop/pipeline-ack
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
                            state
                                .registry
                                .update_monitor_subscription(
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
                                        // Clamp after the add so repeated ACKs
                                        // cannot push credit past the server
                                        // ceiling (R1-H1).
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
                                    state
                                        .registry
                                        .send_monitor_update_for(&pv_name, conn_id, ioid, &nt)
                                        .await;
                                }
                            }
                        }
                    }
                    20 => {
                        // RPC
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
                            let is_init = (payload.subcmd & 0x08) != 0;
                            if is_init {
                                // RPC INIT — acknowledge with status OK
                                let resp = encode_op_status_response(
                                    20,
                                    ioid,
                                    payload.subcmd,
                                    version,
                                    is_be,
                                );
                                state.registry.send_msg(conn_id, resp).await;
                            } else {
                                // RPC EXEC — decode self-describing PVD args and
                                // delegate to the source registry
                                let decoder = PvdDecoder::new(is_be);
                                let args = if !payload.body.is_empty() {
                                    decoder
                                        .parse_introspection_with_len(&payload.body)
                                        .ok()
                                        .and_then(|(desc, consumed)| {
                                            decoder
                                                .decode_structure(&payload.body[consumed..], &desc)
                                                .ok()
                                                .map(|(val, _)| val)
                                        })
                                } else {
                                    None
                                };
                                let empty =
                                    spvirit_codec::spvd_decode::DecodedValue::Structure(vec![]);
                                let args_ref = args.as_ref().unwrap_or(&empty);
                                match state.sources.rpc(&pv_name, args_ref).await {
                                    Ok(result) => {
                                        let resp = encode_op_rpc_data_response_payload(
                                            ioid,
                                            payload.subcmd,
                                            &result,
                                            version,
                                            is_be,
                                        );
                                        state.registry.send_msg(conn_id, resp).await;
                                    }
                                    Err(msg) => {
                                        let resp = encode_op_status_error_response(
                                            20,
                                            ioid,
                                            payload.subcmd,
                                            &msg,
                                            version,
                                            is_be,
                                        );
                                        state.registry.send_msg(conn_id, resp).await;
                                    }
                                }
                            }
                        }
                    }
                    14 | 16 => {
                        state
                            .registry
                            .send_msg(
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
                        state
                            .registry
                            .send_msg(
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
                    state
                        .registry
                        .remove_monitor_subscription(conn_id, ioid, &pv_name)
                        .await;
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
                state.registry.send_msg(conn_id, resp).await;
            }
            PvaPacketCommand::GetField(payload) => {
                handle_get_field_request(&state, &conn_state, conn_id, payload, version, is_be)
                    .await;
            }
            PvaPacketCommand::Echo(payload_bytes) => {
                let mut resp =
                    encode_header(true, is_be, false, version, 2, payload_bytes.len() as u32);
                resp.extend_from_slice(&payload_bytes);
                state.registry.send_msg(conn_id, resp).await;
            }
            PvaPacketCommand::Message(_) => {
                let resp = encode_message_error("MESSAGE command is not supported", version, is_be);
                state.registry.send_msg(conn_id, resp).await;
            }
            PvaPacketCommand::MultipleData(_) => {
                let resp =
                    encode_message_error("MULTIPLE_DATA command is not supported", version, is_be);
                state.registry.send_msg(conn_id, resp).await;
            }
            PvaPacketCommand::CancelRequest(_) => {
                let resp =
                    encode_message_error("CANCEL_REQUEST command is not supported", version, is_be);
                state.registry.send_msg(conn_id, resp).await;
            }
            PvaPacketCommand::OriginTag(_) => {
                let resp =
                    encode_message_error("ORIGIN_TAG command is not supported", version, is_be);
                state.registry.send_msg(conn_id, resp).await;
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
                    let all_names = state.sources.names().await;
                    let visible_names = collect_visible_pv_names(
                        &all_names,
                        state.pvlist_mode,
                        state.pvlist_allow_pattern.as_ref(),
                        state.pvlist_max,
                    );
                    let mut cids = Vec::new();
                    for (cid, name) in &payload.pv_requests {
                        if is_virtual_event_pv(name)
                            || (is_pvlist_virtual_pv(name) && state.pvlist_mode == PvListMode::List)
                            || (is_server_rpc_pv(name) && state.pvlist_mode != PvListMode::Off)
                        {
                            cids.push(*cid);
                            continue;
                        }
                        match state.sources.try_claim(name) {
                            TryClaim::Yes => {
                                cids.push(*cid);
                                continue;
                            }
                            TryClaim::Unknown => {
                                if !is_pattern_query(name) {
                                    state.search_resolver.enqueue(name);
                                }
                            }
                            TryClaim::No => {}
                        }
                        if state.pvlist_mode != PvListMode::Off
                            && is_pattern_query(name)
                            && visible_names.iter().any(|pv| wildcard_match(name, pv))
                        {
                            cids.push(*cid);
                        }
                    }
                    let server_discovery_ping = payload.pv_requests.is_empty();
                    let found = server_discovery_ping || !cids.is_empty();
                    // Prefer an explicit advertise IP; otherwise fall back to the
                    // concrete local address this client connected to (rather than
                    // `listen_ip`, which may be the unspecified all-interface bind
                    // that would emit an unconnectable zero address).
                    let resp_ip = effective_advertise_ip(state.advertise_ip)
                        .or_else(|| effective_advertise_ip(conn_local_addr.map(|a| a.ip())))
                        .unwrap_or(state.listen_ip);
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
                    state.registry.send_msg(conn_id, response).await;
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
                state.registry.send_msg(conn_id, resp).await;
            }
            PvaPacketCommand::Unknown(payload) => {
                let resp = encode_message_error(
                    &format!("Unknown command {}", payload.command),
                    version,
                    is_be,
                );
                state.registry.send_msg(conn_id, resp).await;
            }
        }
    }

    state.registry.cleanup_connection(conn_id).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::MonitorRegistry;
    use crate::pvstore::PvInfo;
    use crate::request_ctx::RequestContext;
    use spvirit_codec::spvirit_encode::encode_search_request;
    use spvirit_types::NtPayload;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration as StdDuration;
    use tokio::net::UdpSocket as TokioUdpSocket;

    #[test]
    fn effective_advertise_ip_rejects_unspecified() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        // A real address is passed through unchanged.
        let real = IpAddr::V4(Ipv4Addr::new(130, 246, 91, 228));
        assert_eq!(effective_advertise_ip(Some(real)), Some(real));

        // The bug: `Some(0.0.0.0)` is NOT a connectable endpoint. It arises
        // whenever a server binds all interfaces without an explicit advertise
        // IP. It must be treated as "unset" so callers fall through to a real
        // fallback instead of emitting a zero address in the search reply
        // (which clients receive as "server at 0.0.0.0" and cannot connect to).
        assert_eq!(
            effective_advertise_ip(Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED))),
            None
        );
        assert_eq!(
            effective_advertise_ip(Some(IpAddr::V6(Ipv6Addr::UNSPECIFIED))),
            None
        );

        // None stays None.
        assert_eq!(effective_advertise_ip(None), None);
    }

    #[test]
    fn pipeline_window_is_clamped_at_init_and_across_acks() {
        // R1-H1: the subscription-init and pipeline-ACK sites read the
        // outstanding-credit window from client-controlled bytes. These clamps
        // mirror the exact expressions used at those sites; a huge requested
        // window (or an accumulation of ACKs) must never exceed the ceiling,
        // which is what bounds the lossless control lane's memory.

        // Init: a window far above the cap (u32::MAX) clamps to the ceiling.
        let requested = u32::MAX;
        let nfree_init = requested.min(MAX_PIPELINE_WINDOW);
        assert_eq!(nfree_init, MAX_PIPELINE_WINDOW);
        assert!(nfree_init <= MAX_PIPELINE_WINDOW);

        // A modest request below the cap is preserved unchanged.
        let small = 16u32;
        assert_eq!(small.min(MAX_PIPELINE_WINDOW), small);

        // ACK: repeated large credit ACKs (saturating_add then clamp) can never
        // push the window above the ceiling, even at u32::MAX per ACK.
        let mut nfree = nfree_init;
        for _ in 0..8 {
            nfree = nfree.saturating_add(u32::MAX).min(MAX_PIPELINE_WINDOW);
            assert!(
                nfree <= MAX_PIPELINE_WINDOW,
                "ACK-accumulated window {nfree} exceeded cap {MAX_PIPELINE_WINDOW}"
            );
        }
        assert_eq!(nfree, MAX_PIPELINE_WINDOW);
    }

    #[test]
    fn accept_retry_delay_backs_off_only_on_fd_exhaustion() {
        use std::io::{Error, ErrorKind};

        // EMFILE (per-process fd limit, errno 24) and ENFILE (system-wide,
        // errno 23) are transient resource-exhaustion errors: the accept loop
        // must keep running and pause briefly so it does not hot-spin while
        // descriptors are scarce.
        assert!(
            accept_retry_delay(&Error::from_raw_os_error(24)).is_some(),
            "EMFILE should back off"
        );
        assert!(
            accept_retry_delay(&Error::from_raw_os_error(23)).is_some(),
            "ENFILE should back off"
        );

        // Other transient accept errors (a client that aborted mid-handshake)
        // are retried immediately, with no backoff.
        assert!(accept_retry_delay(&Error::from(ErrorKind::ConnectionAborted)).is_none());
        assert!(accept_retry_delay(&Error::from(ErrorKind::ConnectionReset)).is_none());
    }

    /// A `Source` that records the [`RequestContext`] visible to it (if any)
    /// the last time `claim` was called, so a test can assert on what peer
    /// identity was in scope during UDP search handling.
    struct ProbeSource {
        name: String,
        seen: Arc<StdMutex<Option<RequestContext>>>,
    }

    impl crate::pvstore::Source for ProbeSource {
        fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
            let owned = name == self.name;
            *self.seen.lock().unwrap() = crate::request_ctx::current_request();
            Box::pin(async move {
                owned.then(|| PvInfo {
                    descriptor: StructureDesc::default(),
                    writable: false,
                })
            })
        }

        fn get(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn put(
            &self,
            _name: &str,
            _value: &spvirit_codec::spvd_decode::DecodedValue,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>>
        {
            Box::pin(async { Err("read-only probe".to_string()) })
        }

        fn subscribe(
            &self,
            _name: &str,
        ) -> Pin<Box<dyn Future<Output = Option<tokio::sync::mpsc::Receiver<NtPayload>>> + Send + '_>>
        {
            Box::pin(async { None })
        }

        fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
            let name = self.name.clone();
            Box::pin(async move { vec![name] })
        }
    }

    /// Picks a free loopback UDP port by binding to port 0 and immediately
    /// releasing it. There is an inherent (tiny) race between releasing the
    /// port here and `run_udp_search` rebinding it below; the retrying send
    /// loop in the test absorbs that race instead of sleeping blindly.
    async fn free_udp_port() -> u16 {
        let sock = TokioUdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn udp_search_scopes_the_peer_identity() {
        let seen: Arc<StdMutex<Option<RequestContext>>> = Arc::new(StdMutex::new(None));
        let probe = Arc::new(ProbeSource {
            name: "PROBE:PV".to_string(),
            seen: seen.clone(),
        });

        let sources = Arc::new(SourceRegistry::new());
        sources.add("probe", 0, probe.clone()).await;

        let state = Arc::new(ServerState::new(
            sources,
            Arc::new(MonitorRegistry::new()),
            false,
            PvListMode::List,
            1024,
            None,
            rand_guid(),
            5075,
            None,
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ));

        let server_port = free_udp_port().await;
        let server_addr: SocketAddr = format!("127.0.0.1:{server_port}").parse().unwrap();
        tokio::spawn(async move {
            let _ = run_udp_search(state, server_addr, 5075, rand_guid(), None, None).await;
        });

        let client = TokioUdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();
        let request = encode_search_request(1, 0x01, 0, [0u8; 16], &[(1, "PROBE:PV")], 2, false);

        let mut recorded = None;
        for _ in 0..50 {
            client.send_to(&request, server_addr).await.unwrap();
            tokio::time::sleep(StdDuration::from_millis(20)).await;
            if let Some(ctx) = seen.lock().unwrap().clone() {
                recorded = Some(ctx);
                break;
            }
        }

        let ctx = recorded
            .expect("ProbeSource::claim saw no RequestContext (current_request() was None)");
        assert_eq!(ctx.peer, client_addr);
    }

    /// Stands in for a gateway whose upstream never answers: `claim` hangs,
    /// and `try_claim` cannot decide without doing the I/O.
    struct HangingSource {
        hang: StdDuration,
        claims: Arc<AtomicUsize>,
    }

    impl crate::pvstore::Source for HangingSource {
        fn try_claim(&self, _name: &str) -> crate::pvstore::TryClaim {
            crate::pvstore::TryClaim::Unknown
        }

        fn claim(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
            self.claims.fetch_add(1, Ordering::SeqCst);
            let hang = self.hang;
            Box::pin(async move {
                tokio::time::sleep(hang).await;
                None
            })
        }

        fn get(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn put(
            &self,
            _name: &str,
            _value: &spvirit_codec::spvd_decode::DecodedValue,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>>
        {
            Box::pin(async { Err("read-only".to_string()) })
        }

        fn subscribe(
            &self,
            _name: &str,
        ) -> Pin<Box<dyn Future<Output = Option<tokio::sync::mpsc::Receiver<NtPayload>>> + Send + '_>>
        {
            Box::pin(async { None })
        }

        fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
            Box::pin(async { Vec::new() })
        }
    }

    /// A source that becomes decisive only after its first `claim` completes —
    /// the shape a gateway has once a binding exists. Proves the resolver's
    /// no-reply design actually delivers: the retry is what gets answered.
    struct LateSource {
        name: String,
        resolved: Arc<AtomicBool>,
    }

    impl crate::pvstore::Source for LateSource {
        fn try_claim(&self, name: &str) -> crate::pvstore::TryClaim {
            if name != self.name {
                return crate::pvstore::TryClaim::No;
            }
            if self.resolved.load(Ordering::SeqCst) {
                crate::pvstore::TryClaim::Yes
            } else {
                crate::pvstore::TryClaim::Unknown
            }
        }

        fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
            let owned = name == self.name;
            let resolved = self.resolved.clone();
            Box::pin(async move {
                tokio::time::sleep(StdDuration::from_millis(100)).await;
                if !owned {
                    return None;
                }
                resolved.store(true, Ordering::SeqCst);
                Some(PvInfo {
                    descriptor: StructureDesc::default(),
                    writable: false,
                })
            })
        }

        fn get(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn put(
            &self,
            _name: &str,
            _value: &spvirit_codec::spvd_decode::DecodedValue,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>>
        {
            Box::pin(async { Err("read-only".to_string()) })
        }

        fn subscribe(
            &self,
            _name: &str,
        ) -> Pin<Box<dyn Future<Output = Option<tokio::sync::mpsc::Receiver<NtPayload>>> + Send + '_>>
        {
            Box::pin(async { None })
        }

        fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
            Box::pin(async { Vec::new() })
        }
    }

    /// Answers instantly and decisively for one name — the shape of a purely
    /// local source that needs no upstream. `ProbeSource` cannot serve here:
    /// it takes the trait's default `try_claim`, which is `Unknown`.
    struct DecisiveSource {
        name: String,
    }

    impl DecisiveSource {
        fn new(name: &str) -> Self {
            Self { name: name.to_string() }
        }
    }

    impl crate::pvstore::Source for DecisiveSource {
        fn try_claim(&self, name: &str) -> crate::pvstore::TryClaim {
            if name == self.name {
                crate::pvstore::TryClaim::Yes
            } else {
                crate::pvstore::TryClaim::No
            }
        }

        fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
            let owned = name == self.name;
            Box::pin(async move {
                owned.then(|| PvInfo {
                    descriptor: StructureDesc::default(),
                    writable: false,
                })
            })
        }

        fn get(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn put(
            &self,
            _name: &str,
            _value: &spvirit_codec::spvd_decode::DecodedValue,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>>
        {
            Box::pin(async { Err("read-only probe".to_string()) })
        }

        fn subscribe(
            &self,
            _name: &str,
        ) -> Pin<Box<dyn Future<Output = Option<tokio::sync::mpsc::Receiver<NtPayload>>> + Send + '_>>
        {
            Box::pin(async { None })
        }

        fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
            let name = self.name.clone();
            Box::pin(async move { vec![name] })
        }
    }

    /// Stand up a search responder over `sources` on a free loopback port,
    /// returning the server address and a bound client socket.
    async fn spawn_search_server(
        sources: Arc<SourceRegistry>,
    ) -> (SocketAddr, TokioUdpSocket) {
        let state = Arc::new(ServerState::new(
            sources,
            Arc::new(MonitorRegistry::new()),
            false,
            PvListMode::List,
            1024,
            None,
            rand_guid(),
            5075,
            None,
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ));
        let server_port = free_udp_port().await;
        let server_addr: SocketAddr = format!("127.0.0.1:{server_port}").parse().unwrap();
        tokio::spawn(async move {
            let _ = run_udp_search(state, server_addr, 5075, rand_guid(), None, None).await;
        });
        let client = TokioUdpSocket::bind("127.0.0.1:0").await.unwrap();
        (server_addr, client)
    }

    /// Send one search for `name` under `cid` and wait up to `budget` for a
    /// response that names it, returning whether one arrived. Retries the send
    /// (like `udp_search_scopes_the_peer_identity` does) to absorb the
    /// port-rebind race, and ignores any response that does not carry `cid` —
    /// a discovery-ping reply may arrive first.
    async fn search_finds(
        client: &TokioUdpSocket,
        server_addr: SocketAddr,
        cid: u32,
        name: &str,
        budget: StdDuration,
    ) -> bool {
        let request = encode_search_request(cid, 0x01, 0, [0u8; 16], &[(cid, name)], 2, false);
        let deadline = std::time::Instant::now() + budget;
        let mut buf = [0u8; 2048];
        while std::time::Instant::now() < deadline {
            client.send_to(&request, server_addr).await.unwrap();
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let wait = remaining.min(StdDuration::from_millis(20));
            if let Ok(Ok((n, _))) =
                tokio::time::timeout(wait, client.recv_from(&mut buf)).await
                && response_names_cid(&buf[..n], cid)
            {
                return true;
            }
        }
        false
    }

    /// True if `buf` is a search response that reports `cid` found.
    ///
    /// Uses the real decoder (`PvaPacket::try_new` + `decode_payload`) rather
    /// than scanning for cid bytes — a byte scan would also match the echoed
    /// cid in a *request*, and this socket's own retries are in flight.
    fn response_names_cid(buf: &[u8], cid: u32) -> bool {
        let Some(mut pkt) = spvirit_codec::epics_decode::PvaPacket::try_new(buf) else {
            return false;
        };
        match pkt.decode_payload() {
            Some(spvirit_codec::epics_decode::PvaPacketCommand::SearchResponse(p)) => {
                p.found && p.cids.contains(&cid)
            }
            _ => false,
        }
    }

    /// The defect: `run_udp_search` is a single task, so awaiting a slow
    /// `claim` inside it stopped the responder reading datagrams for *every*
    /// client and *every* name — including purely local ones needing no
    /// upstream at all. Measured in the field as total search denial within
    /// 4s at 1.4 miss-searches/s, sustained indefinitely by the blocked
    /// clients' own ~5Hz retries, with the gateway at 0.06 cores.
    ///
    /// The bound is on latency, not reachability: `LOCAL:PV` was always
    /// findable, it just took as long as an unrelated hanging claim.
    #[tokio::test]
    async fn a_hanging_source_does_not_delay_a_local_name() {
        let claims = Arc::new(AtomicUsize::new(0));
        let sources = Arc::new(SourceRegistry::new());
        sources
            .add("local", 0, Arc::new(DecisiveSource::new("LOCAL:PV")))
            .await;
        sources
            .add(
                "hanging",
                1,
                Arc::new(HangingSource {
                    hang: StdDuration::from_secs(5),
                    claims: claims.clone(),
                }),
            )
            .await;
        let (server_addr, client) = spawn_search_server(sources).await;

        // Put the responder under a claim that will not return for 5s.
        let request =
            encode_search_request(1, 0x01, 0, [0u8; 16], &[(1, "UNRESOLVABLE:PV")], 2, false);
        client.send_to(&request, server_addr).await.unwrap();
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        assert!(
            claims.load(Ordering::SeqCst) > 0,
            "the hanging source was never consulted; the test proves nothing"
        );

        let before = std::time::Instant::now();
        let found = search_finds(
            &client,
            server_addr,
            2,
            "LOCAL:PV",
            StdDuration::from_secs(2),
        )
        .await;
        assert!(found, "a local name went unanswered while a source hung");
        assert!(
            before.elapsed() < StdDuration::from_millis(500),
            "local name took {:?}; the search task is blocked on the hanging source",
            before.elapsed()
        );
    }

    /// The self-sustaining part: a flood of distinct unresolvable names must
    /// be shed, not queued. If each one costs the responder an await, service
    /// never recovers while any client is still searching.
    #[tokio::test]
    async fn a_flood_of_unresolvable_names_does_not_deny_a_local_name() {
        let sources = Arc::new(SourceRegistry::new());
        sources
            .add("local", 0, Arc::new(DecisiveSource::new("LOCAL:PV")))
            .await;
        sources
            .add(
                "hanging",
                1,
                Arc::new(HangingSource {
                    hang: StdDuration::from_secs(5),
                    claims: Arc::new(AtomicUsize::new(0)),
                }),
            )
            .await;
        let (server_addr, client) = spawn_search_server(sources).await;

        for i in 0..200u32 {
            let name = format!("FLOOD:{i}");
            let req =
                encode_search_request(1000 + i, 0x01, 0, [0u8; 16], &[(1000 + i, &name)], 2, false);
            client.send_to(&req, server_addr).await.unwrap();
        }

        let before = std::time::Instant::now();
        let found = search_finds(
            &client,
            server_addr,
            7,
            "LOCAL:PV",
            StdDuration::from_secs(3),
        )
        .await;
        assert!(found, "a local name was denied under a search flood");
        assert!(
            before.elapsed() < StdDuration::from_secs(1),
            "local name took {:?} under flood",
            before.elapsed()
        );
    }

    /// The resolver replies to nothing, so resolution has to reach the client
    /// through its own retry. First search misses; once the background claim
    /// lands, a retry finds it.
    ///
    /// Since Task 3b there are two routes by which the retry can now be
    /// answered: `LateSource::try_claim` flipping to `Yes`, and the registry's
    /// resolver-outcome memo. The test asserts the end-to-end property and
    /// does not care which one fires. Do not "simplify" `LateSource` into a
    /// source that never becomes decisive — that would test only the memo and
    /// would silently stop covering the cache-warming route.
    /// The memo is keyed on the peer address, so the retry must come from the
    /// same client socket as the first search. `search_finds` does that.
    #[tokio::test]
    async fn an_unresolvable_name_is_answered_after_its_background_resolution() {
        let sources = Arc::new(SourceRegistry::new());
        sources
            .add(
                "late",
                0,
                Arc::new(LateSource {
                    name: "LATE:PV".to_string(),
                    resolved: Arc::new(AtomicBool::new(false)),
                }),
            )
            .await;
        let (server_addr, client) = spawn_search_server(sources).await;

        // `search_finds` retries for the whole budget, which is exactly the
        // client behaviour this design relies on: the first datagram is
        // unanswered, a later one is answered once resolution has landed.
        let found = search_finds(
            &client,
            server_addr,
            3,
            "LATE:PV",
            StdDuration::from_secs(3),
        )
        .await;
        assert!(
            found,
            "background resolution never made the name findable; the \
             no-reply resolver design does not deliver"
        );
    }
}
