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
    /// Permits for [`spawn_pattern_query_reply`] — the concurrency bound on
    /// pattern-query enumerations running off the search task.
    pub pattern_enum_permits: Arc<tokio::sync::Semaphore>,
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
            pattern_enum_permits: Arc::new(tokio::sync::Semaphore::new(PATTERN_ENUM_CONCURRENCY)),
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
    // Sort and truncate *references*, and clone only what survives. The
    // caller's `max_items` is typically a few hundred while `all_names` is
    // whatever every registered source put together — on a large gateway,
    // tens of thousands of strings — so cloning first and discarding after
    // meant a heap allocation per name for names that were then thrown away.
    // Reference-sized elements also make the sort itself cheaper to move.
    let mut refs: Vec<&String> = all_names
        .iter()
        .filter(|name| {
            allow_pattern
                .as_ref()
                .map(|re| re.is_match(name))
                .unwrap_or(true)
        })
        .collect();
    refs.sort();
    if refs.len() > max_items {
        refs.truncate(max_items);
    }
    let mut names: Vec<String> = refs.into_iter().cloned().collect();
    if mode == PvListMode::List && names.len() < max_items {
        names.push("__pvlist".to_string());
    }
    names
}

/// Maximum simultaneous pattern-query enumerations, per server.
///
/// Deliberately its own budget rather than a share of
/// [`RESOLVE_CONCURRENCY`](crate::search_resolve::RESOLVE_CONCURRENCY): the
/// two are independent failure domains. A wildcard flood must not be able to
/// consume the permits that exact-name resolution needs, and eight upstreams
/// hung in `claim` must not stop pattern queries being answered. Four is
/// ample — a pattern query is a rare, non-latency-critical operator action,
/// and every one of these may be unbounded third-party work in `names()`.
pub const PATTERN_ENUM_CONCURRENCY: usize = 4;

/// How long one pattern-query enumeration may run before it is abandoned.
///
/// Without this, a single source whose `names()` never returns holds its
/// permit forever. Four such sources — or four datagrams naming `"*"` while
/// one hung source is registered — retire the whole budget permanently, and
/// from then on *every* pattern query is shed. The permit is only ever
/// released by the spawned task finishing, so an enumeration that cannot
/// finish must be made to.
///
/// Thirty seconds is chosen to be far longer than any legitimate enumeration
/// and far shorter than "forever". `SourceRegistry::names()` is a fan-out over
/// registered sources: an in-process store answers in microseconds, and even a
/// Python-backed source walking a large listing is a sub-second operation. The
/// slowest realistic case is a proxying source waiting on a network peer,
/// which is bounded by that peer's own timeouts — an EPICS client's default
/// search/connect budget is a few seconds. Thirty seconds clears all of that
/// by an order of magnitude, so a trip of this timeout is not a slow source,
/// it is a stuck one. It also stays well under the interval at which a real
/// operator would retry, so the permit is back before the retry needs it.
///
/// A timed-out enumeration is treated as a shed, not as "matched nothing": it
/// sends no reply and increments the same counter (see
/// [`PatternDispatch::withholds_negative`] for why silence beats a confident
/// `found=false`).
pub const PATTERN_ENUM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What a search loop must do about the negative half of its reply, after
/// handing this datagram's pattern queries to [`spawn_pattern_query_reply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternDispatch {
    /// The datagram carried no pattern queries; this loop answers as usual.
    None,
    /// A spawned task owns the pattern half of the reply, including the duty
    /// to answer a `response_required` datagram this loop found nothing for.
    Deferred,
    /// The enumeration cap was reached and the query was dropped.
    /// **Nothing** answers the pattern half.
    Shed,
}

impl PatternDispatch {
    /// Whether this loop must withhold a `found=false` response.
    ///
    /// `Deferred` withholds it because the spawned task will send the real
    /// answer. `Shed` withholds it because the query is *undecided*: the
    /// server may well serve a name matching that pattern, it simply declined
    /// to look. Answering `found=false` there would be an authoritative "I do
    /// not serve that" for a name the server does serve, and — unlike
    /// silence — a retry cannot correct it, because every retry gets the same
    /// confident lie while the cap stays saturated. Staying silent is exactly
    /// what [`TryClaim::Unknown`](crate::pvstore::TryClaim::Unknown) does on
    /// the exact-name path, and search is retry-driven precisely so that an
    /// undecided query costs a round trip rather than a wrong answer.
    ///
    /// Exact names on the same datagram are unaffected: they are answered
    /// inline, and only the negative-because-nothing-matched response is
    /// suppressed.
    fn withholds_negative(self) -> bool {
        matches!(self, Self::Deferred | Self::Shed)
    }
}

/// Answer a datagram's pattern queries on a *separate* task, and tell the
/// caller whether that task now owns the reply.
///
/// `SourceRegistry::names()` awaits *every* registered source's `names()`,
/// which for a proxying or Python-backed source is unbounded third-party
/// work, and both search loops run on a single task shared by every client.
/// Doing it inline reinstates exactly the head-of-line stall that
/// [`Source::try_claim`](crate::pvstore::Source::try_claim) exists to remove,
/// and — because `is_pattern_query` tests bytes the remote peer chooses — it
/// is reachable from one unauthenticated datagram naming `"*"`. No predicate
/// over attacker-controlled input can gate it safely, so the enumeration and
/// the reply it feeds both move off the search task entirely; the loop
/// continues to the next datagram immediately.
///
/// A permit is taken *before* spawning and the work is shed rather than
/// queued if none is free — the same ruling, for the same reason, as
/// [`SearchResolver::enqueue`](crate::search_resolve::SearchResolver::enqueue):
/// under a flood, delaying the work only converts a CPU problem into an
/// unbounded-task problem, and search is retry-driven anyway.
///
/// The spawned enumeration is bounded by [`PATTERN_ENUM_TIMEOUT`] so that the
/// permit is returned even when a source's `names()` never is. A timed-out
/// enumeration is a shed: no reply goes out, and the shed counter moves. So is
/// a *panicking* one — see [`ShedUnlessAnswered`].
///
/// `ctx` is the caller's [`RequestContext`](crate::request_ctx::RequestContext),
/// captured on the task that still holds the request's task-local. A spawned
/// task does **not** inherit task-locals, and `SourceRegistry::names()` is an
/// access-aware call: the gateway's status source filters its own listing with
/// `decide_local(Op::Get, name, &current_identity())`, so an enumeration that
/// runs with no identity silently stops matching every host-qualified pvlist
/// rule — `DENY … FROM host` leaks names it should hide, and `ALLOW … FROM
/// host` hides names from a legitimate operator. Reinstalled below with
/// `scope_with`, exactly as
/// [`SearchResolver::enqueue`](crate::search_resolve::SearchResolver::enqueue)
/// does for the same reason.
///
/// See [`PatternDispatch`] for what the return value obliges the caller to do.
/// Counts a shed unless the enumeration reached an answer.
///
/// Every way a spawned enumeration can end without replying must move
/// `pattern_enum_shed`, because shedding is silent on the wire and the counter
/// is an operator's only trace of a query that went unanswered. The timeout
/// arm is one such way; a source whose `names()` **panics** is the other, and
/// it used to be invisible: the task unwinds, the permit comes back by RAII,
/// but neither arm of the `match` on `timeout` runs, so nothing is counted and
/// the client's wildcard is never answered. That is the failure mode that
/// produces the most confusing silence, so it is the last one that should be
/// missing from the counter.
///
/// A drop guard rather than `catch_unwind` because the future is not
/// `UnwindSafe` and we do not want to swallow the panic — it should still
/// reach tokio's task-panic reporting. This only makes the panic *countable*.
struct ShedUnlessAnswered {
    armed: bool,
}

impl ShedUnlessAnswered {
    fn new() -> Self {
        Self { armed: true }
    }

    /// The enumeration produced cids; whatever the reply does from here is not
    /// a shed.
    fn answered(&mut self) {
        self.armed = false;
    }
}

impl Drop for ShedUnlessAnswered {
    fn drop(&mut self) {
        if self.armed {
            crate::search_resolve::note_pattern_enum_shed();
        }
    }
}

fn spawn_pattern_query_reply<R, Fut>(
    state: &Arc<ServerState>,
    ctx: Option<crate::request_ctx::RequestContext>,
    pattern_requests: Vec<(u32, String)>,
    reply: R,
) -> PatternDispatch
where
    R: FnOnce(Vec<u32>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    if pattern_requests.is_empty() {
        return PatternDispatch::None;
    }
    let Ok(permit) = state.pattern_enum_permits.clone().try_acquire_owned() else {
        debug!(
            "search: shedding {} pattern quer(ies); enumeration cap reached",
            pattern_requests.len()
        );
        crate::search_resolve::note_pattern_enum_shed();
        return PatternDispatch::Shed;
    };
    let state = state.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let enumerate = async move {
            let all_names = state.sources.names().await;
            let visible = collect_visible_pv_names(
                &all_names,
                state.pvlist_mode,
                state.pvlist_allow_pattern.as_ref(),
                state.pvlist_max,
            );
            pattern_requests
                .iter()
                .filter(|(_, name)| visible.iter().any(|pv| wildcard_match(name, pv)))
                .map(|(cid, _)| *cid)
                .collect::<Vec<u32>>()
        };
        // Only the enumeration is under the timeout; sending the reply is not,
        // so a slow socket can never turn a completed enumeration into a
        // silent one.
        let enumerate = async move {
            match ctx {
                Some(ctx) => crate::request_ctx::scope_with(ctx, enumerate).await,
                None => enumerate.await,
            }
        };
        // Armed across the enumeration so that *every* way of ending without
        // an answer — the timeout below, and a panic out of a source's
        // `names()` — is counted. Disarmed the moment cids exist.
        let mut shed = ShedUnlessAnswered::new();
        match tokio::time::timeout(PATTERN_ENUM_TIMEOUT, enumerate).await {
            Ok(cids) => {
                shed.answered();
                reply(cids).await
            }
            Err(_) => {
                debug!(
                    "search: abandoning pattern enumeration after {:?}; a source's names() is \
                     not returning",
                    PATTERN_ENUM_TIMEOUT
                );
                // Left armed: the guard counts this shed on the way out.
            }
        }
    });
    PatternDispatch::Deferred
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
    // `Arc` so a pattern-query reply can be sent from the task that computed
    // it, without that computation ever running on this loop.
    let socket = Arc::new(bind_udp_search_socket(addr)?);
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
                let (cids, pattern_requests, req_ctx) = crate::request_ctx::scope(peer, async {
                    let mut cids = Vec::new();
                    let mut pattern_requests: Vec<(u32, String)> = Vec::new();
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
                        let outcome = state.sources.try_claim(name);
                        crate::search_resolve::note_try_claim(outcome);
                        match outcome {
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
                        // Answered off this task: matching needs the full
                        // enumeration, which is unbounded third-party work.
                        if state.pvlist_mode != PvListMode::Off && is_pattern_query(name) {
                            pattern_requests.push((*cid, name.clone()));
                        }
                    }
                    // Captured *inside* the scope, on the task that holds the
                    // request's task-local: the pattern enumeration is
                    // spawned below, after this scope has already ended.
                    (cids, pattern_requests, crate::request_ctx::current_request())
                })
                .await;
                let response_required = (payload.mask & 0x01) != 0;
                let server_discovery_ping = payload.pv_requests.is_empty();
                let found = server_discovery_ping || !cids.is_empty();
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
                // Hand the pattern queries to their own task. If it takes
                // them, it also inherits the duty to answer a
                // `response_required` datagram this loop found nothing for —
                // so a wildcard-only search still gets exactly one reply,
                // carrying exactly the cids it used to carry.
                let seq = payload.seq;
                let answer_negative = response_required && !found;
                let dispatch = {
                    let socket = socket.clone();
                    spawn_pattern_query_reply(&state, req_ctx, pattern_requests, move |pattern_cids| async move {
                        if pattern_cids.is_empty() && !answer_negative {
                            return;
                        }
                        let response = encode_search_response(
                            guid,
                            seq,
                            addr_bytes,
                            tcp_port,
                            "tcp",
                            !pattern_cids.is_empty(),
                            &pattern_cids,
                            version,
                            is_be,
                        );
                        if let Err(e) = socket.send_to(&response, reply_target).await {
                            debug!("UDP search: failed sending pattern reply to {reply_target}: {e}");
                        }
                    })
                };
                if !found && (dispatch.withholds_negative() || !response_required) {
                    debug!("UDP search: no immediate matches (pattern dispatch={dispatch:?})");
                    continue;
                }
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
                    let mut cids = Vec::new();
                    let mut pattern_requests: Vec<(u32, String)> = Vec::new();
                    for (cid, name) in &payload.pv_requests {
                        if is_virtual_event_pv(name)
                            || (is_pvlist_virtual_pv(name) && state.pvlist_mode == PvListMode::List)
                            || (is_server_rpc_pv(name) && state.pvlist_mode != PvListMode::Off)
                        {
                            cids.push(*cid);
                            continue;
                        }
                        let outcome = state.sources.try_claim(name);
                        crate::search_resolve::note_try_claim(outcome);
                        match outcome {
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
                        // Same ruling as the UDP loop: the enumeration a
                        // pattern query needs is unbounded third-party work
                        // and must not run on a task that serves other names.
                        // This one serves a whole name-server connection —
                        // the `EPICS_PVA_NAME_SERVERS` route the gateway is
                        // deployed on.
                        if state.pvlist_mode != PvListMode::Off && is_pattern_query(name) {
                            pattern_requests.push((*cid, name.clone()));
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
                    // As on UDP: the spawned enumeration owns the reply for
                    // the pattern half, and inherits the duty to answer at
                    // all when this loop found nothing — so a wildcard-only
                    // search over a name-server connection still gets exactly
                    // one response with exactly the cids it used to carry.
                    let seq = payload.seq;
                    let answer_negative = !found;
                    let dispatch = {
                        let replier = state.clone();
                        // `handle_connection` runs inside `request_ctx::scope`
                        // and `set_credentials` has already installed the
                        // validated `ca` user, so this is the full identity the
                        // inline enumeration used to see.
                        let req_ctx = crate::request_ctx::current_request();
                        spawn_pattern_query_reply(
                            &state,
                            req_ctx,
                            pattern_requests,
                            move |pattern_cids| async move {
                                let state = replier;
                                if pattern_cids.is_empty() && !answer_negative {
                                    return;
                                }
                                let response = encode_search_response(
                                    state.guid,
                                    seq,
                                    addr_bytes,
                                    state.tcp_port,
                                    "tcp",
                                    !pattern_cids.is_empty(),
                                    &pattern_cids,
                                    version,
                                    is_be,
                                );
                                state.registry.send_msg(conn_id, response).await;
                            },
                        )
                    };
                    if found || !dispatch.withholds_negative() {
                        state.registry.send_msg(conn_id, response).await;
                        debug!(
                            "Conn {}: TCP search responded found={} matches={}",
                            conn_id,
                            found,
                            cids.len()
                        );
                    }
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

    /// V6 LOW-1. The bound's *magnitude* was unpinned: `from_secs(30)` →
    /// `from_millis(30)`, the classic units slip, survived the whole suite.
    /// The paused-clock test asserts only that *some* bound exists, and it
    /// sleeps `PATTERN_ENUM_TIMEOUT + 1s`, so it stays green whatever the
    /// constant says.
    ///
    /// The constant is load-bearing in both directions, which is why a range
    /// rather than an equality: too small and a legitimately slow enumeration
    /// — the proxying source the doc comment names, bounded by an EPICS
    /// client's few-second search budget — is abandoned and counted as a stuck
    /// source, turning a slow gateway into one that answers no wildcards at
    /// all. Too large and a hung source holds its permit long enough that
    /// `PATTERN_ENUM_CONCURRENCY` of them disable pattern queries for what an
    /// operator experiences as forever, which is the defect the bound exists
    /// to remove.
    #[test]
    fn the_pattern_enumeration_bound_is_seconds_not_milliseconds() {
        assert!(
            PATTERN_ENUM_TIMEOUT >= StdDuration::from_secs(5),
            "PATTERN_ENUM_TIMEOUT is {PATTERN_ENUM_TIMEOUT:?}: shorter than the \
             few-second search/connect budget of the slowest legitimate \
             enumeration, so an ordinary slow source would be abandoned and \
             counted as a stuck one. A `from_secs`/`from_millis` slip lands \
             exactly here."
        );
        assert!(
            PATTERN_ENUM_TIMEOUT <= StdDuration::from_secs(120),
            "PATTERN_ENUM_TIMEOUT is {PATTERN_ENUM_TIMEOUT:?}: long enough that \
             {PATTERN_ENUM_CONCURRENCY} hung sources disable pattern queries \
             for what an operator experiences as forever, which is the defect \
             the bound exists to remove."
        );
        // The documented value, so a deliberate retune is a visible edit here
        // and not a silent one.
        assert_eq!(PATTERN_ENUM_TIMEOUT, StdDuration::from_secs(30));
    }

    /// V6 MEDIUM-2. `collect_visible_pv_names` sorts and *then* truncates, so
    /// `pvlist_max` discloses the alphabetically-first names. Swapping the two
    /// steps discloses a different set entirely — the names that happened to
    /// come first in source order — and nothing observed that.
    ///
    /// The integration tests cannot see it: `SourceRegistry::names()` already
    /// sorts its fan-out, so by the time the spawned enumeration calls this
    /// function the input order and the sorted order agree. The unsorted input
    /// below is the whole point; `Z:ZED` must be the name that is dropped.
    #[test]
    fn collect_visible_pv_names_sorts_before_it_truncates() {
        let unsorted = [
            "Z:ZED".to_string(),
            "A:ALPHA".to_string(),
            "M:MID".to_string(),
        ];

        // Anti-vacuity: with no cap in play, all three come back, sorted.
        assert_eq!(
            collect_visible_pv_names(&unsorted, PvListMode::Discover, None, 100),
            vec!["A:ALPHA", "M:MID", "Z:ZED"],
            "the function is not sorting at all"
        );

        // The load-bearing case. Truncating first would keep the source-order
        // prefix `["Z:ZED", "A:ALPHA"]` and sort it to `["A:ALPHA", "Z:ZED"]`.
        assert_eq!(
            collect_visible_pv_names(&unsorted, PvListMode::Discover, None, 2),
            vec!["A:ALPHA", "M:MID"],
            "`pvlist_max` disclosed the source-order prefix rather than the \
             alphabetically-first names: the truncation is happening before \
             the sort, so which names a wildcard reveals depends on source \
             registration order"
        );

        // A cap of one is the sharpest form of the same contract.
        assert_eq!(
            collect_visible_pv_names(&unsorted, PvListMode::Discover, None, 1),
            vec!["A:ALPHA"],
            "the single visible name must be the first in sort order"
        );
    }

    /// The rest of the ordering contract: filtering happens before the sort
    /// and before the cap, and the `__pvlist` entry is appended *after* the
    /// truncation and only when there is room for it under `pvlist_max`.
    #[test]
    fn collect_visible_pv_names_filters_then_sorts_then_caps_then_appends_pvlist() {
        let names = [
            "Z:ZED".to_string(),
            "A:ALPHA".to_string(),
            "X:SKIP".to_string(),
            "M:MID".to_string(),
        ];
        let allow = Regex::new("^[AMZ]:").unwrap();

        // The filter removes `X:SKIP` before the cap is applied, so a cap of 2
        // sees three candidates and keeps the two smallest of *those*.
        assert_eq!(
            collect_visible_pv_names(&names, PvListMode::Discover, Some(&allow), 2),
            vec!["A:ALPHA", "M:MID"],
            "the allow-pattern must be applied before the sort and the cap"
        );

        // `List` mode appends `__pvlist`, but only while the truncated list is
        // still short of `max_items` — the cap bounds the whole reply.
        assert_eq!(
            collect_visible_pv_names(&names, PvListMode::List, None, 2),
            vec!["A:ALPHA", "M:MID"],
            "`__pvlist` was appended past `pvlist_max`"
        );
        assert_eq!(
            collect_visible_pv_names(&names, PvListMode::List, None, 10),
            vec!["A:ALPHA", "M:MID", "X:SKIP", "Z:ZED", "__pvlist"],
            "`__pvlist` must be appended, after the sort, when there is room"
        );

        // Empty input is not a special case.
        assert_eq!(
            collect_visible_pv_names(&[], PvListMode::Discover, None, 5),
            Vec::<String>::new()
        );
    }

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
    ///
    /// `names()` hangs for the same duration, and deliberately so. An upstream
    /// that cannot answer `claim` cannot answer an enumeration either, and a
    /// fixture whose `names()` returned instantly made both head-of-line tests
    /// blind to the `sources.names().await` that the search loops used to run
    /// unconditionally per datagram, three lines above the `try_claim` they
    /// were certifying.
    struct HangingSource {
        hang: StdDuration,
        claims: Arc<AtomicUsize>,
        /// Bumped by `names()`, so a test can prove the enumeration was (or
        /// was not) reached.
        name_calls: Arc<AtomicUsize>,
    }

    impl HangingSource {
        fn new(hang: StdDuration, claims: Arc<AtomicUsize>) -> Self {
            Self {
                hang,
                claims,
                name_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
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
            self.name_calls.fetch_add(1, Ordering::SeqCst);
            let hang = self.hang;
            Box::pin(async move {
                tokio::time::sleep(hang).await;
                Vec::new()
            })
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
            let name = self.name.clone();
            Box::pin(async move { vec![name] })
        }
    }

    /// Stand up a search responder over `sources` on a free loopback port,
    /// returning the server address, a bound client socket, and the shared
    /// `ServerState` (so a test can inspect `search_resolver.stats()` after
    /// driving traffic through it). A bind or I/O failure inside
    /// `run_udp_search` is logged rather than silently dropped, so a dead
    /// responder shows up as a diagnosable log line instead of a `found`
    /// assertion that misleadingly points at the head-of-line defect.
    async fn spawn_search_server(
        sources: Arc<SourceRegistry>,
    ) -> (SocketAddr, TokioUdpSocket, Arc<ServerState>) {
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
        let state_for_task = state.clone();
        tokio::spawn(async move {
            if let Err(e) =
                run_udp_search(state_for_task, server_addr, 5075, rand_guid(), None, None).await
            {
                eprintln!("test search responder on {server_addr} exited early: {e}");
            }
        });
        let client = TokioUdpSocket::bind("127.0.0.1:0").await.unwrap();
        (server_addr, client, state)
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
    /// Against the pre-fix loop this is total unreachability, not merely
    /// slow reachability: with a 5s hang and a 2s search budget, `found`
    /// itself fails (the budget expires before the wedged responder ever
    /// answers) — the latency assertion below never even gets reached in
    /// that case. It still earns its place for a *partial* wedge (a
    /// sub-budget hang that would satisfy `found` but blow the 500ms bound).
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
                Arc::new(HangingSource::new(
                    StdDuration::from_secs(5),
                    claims.clone(),
                )),
            )
            .await;
        let (server_addr, client, _state) = spawn_search_server(sources).await;

        // Put the responder under a claim that will not return for 5s. Poll
        // (send + short sleep) rather than one-shot-send-then-sleep, so this
        // absorbs the same free_udp_port rebind race that free_udp_port's own
        // doc comment warns about instead of risking the first datagram
        // landing on a not-yet-bound port.
        let request =
            encode_search_request(1, 0x01, 0, [0u8; 16], &[(1, "UNRESOLVABLE:PV")], 2, false);
        for _ in 0..50 {
            client.send_to(&request, server_addr).await.unwrap();
            tokio::time::sleep(StdDuration::from_millis(20)).await;
            if claims.load(Ordering::SeqCst) > 0 {
                break;
            }
        }
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

    /// The same head-of-line defect one level up: `run_udp_search` used to
    /// `await state.sources.names()` unconditionally on every datagram,
    /// *before* the first `try_claim`. A source that hangs in `names()` then
    /// wedges the responder exactly as a hanging `claim` used to, and neither
    /// of the two tests above could see it, because the fixture's `names()`
    /// returned instantly (reviewer B's probe M50).
    ///
    /// The enumeration is only ever read to answer a pattern query under
    /// pvlist, so this datagram — an exact name — must not pay for it at all.
    #[tokio::test]
    async fn a_source_that_hangs_in_names_does_not_delay_a_local_name() {
        let claims = Arc::new(AtomicUsize::new(0));
        let hanging = Arc::new(HangingSource::new(
            StdDuration::from_secs(5),
            claims.clone(),
        ));
        let name_calls = hanging.name_calls.clone();
        let sources = Arc::new(SourceRegistry::new());
        sources
            .add("local", 0, Arc::new(DecisiveSource::new("LOCAL:PV")))
            .await;
        sources.add("hanging", 1, hanging).await;
        let (server_addr, client, _state) = spawn_search_server(sources).await;

        // Anti-vacuity, and it absorbs the free_udp_port rebind race: prove
        // the hanging source is really registered and really on the search
        // path before timing anything.
        let request =
            encode_search_request(1, 0x01, 0, [0u8; 16], &[(1, "UNRESOLVABLE:PV")], 2, false);
        for _ in 0..50 {
            client.send_to(&request, server_addr).await.unwrap();
            tokio::time::sleep(StdDuration::from_millis(20)).await;
            if claims.load(Ordering::SeqCst) > 0 {
                break;
            }
        }
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
        assert!(
            found,
            "a local name went unanswered while a source hung in names()"
        );
        assert!(
            before.elapsed() < StdDuration::from_millis(500),
            "local name took {:?}; the search task is blocked enumerating names",
            before.elapsed()
        );
        assert_eq!(
            name_calls.load(Ordering::SeqCst),
            0,
            "the search loop enumerated every source's names for a datagram \
             that carries no pattern query and could never read the result"
        );
    }

    /// Send one search and wait for a response that reports `cid` *not*
    /// found. Retries like [`search_finds`]; every retry is decided the same
    /// way, so this does not perturb the resolver counters a caller asserts
    /// on afterwards.
    async fn search_answers_absent(
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
            if let Ok(Ok((n, _))) = tokio::time::timeout(wait, client.recv_from(&mut buf)).await {
                let Some(mut pkt) = spvirit_codec::epics_decode::PvaPacket::try_new(&buf[..n])
                else {
                    continue;
                };
                if let Some(spvirit_codec::epics_decode::PvaPacketCommand::SearchResponse(p)) =
                    pkt.decode_payload()
                    && p.seq == cid
                    && !p.found
                {
                    return true;
                }
            }
        }
        false
    }

    /// V2-4: the deliverable of the decisive-`try_claim` work, not merely its
    /// self-consistency.
    ///
    /// The three tests shipped with that change all assert `try_claim` agrees
    /// with `claim` — a property an all-`Unknown` implementation satisfies
    /// just as well, which is exactly what the code did *before* the fix and
    /// exactly the defect. This stands the search loop up over the production
    /// composition instead (a `SimplePvStore` plus the `RecordFieldSource`
    /// that every `PvaServer` registers at order 10) and asserts the outcome:
    /// both a served name and an absent one are settled on the **first**
    /// datagram, with the background resolver never started.
    ///
    /// `started == 0` is the load-bearing assertion. While `RecordFieldSource`
    /// answered `Unknown`, its single vote made the registry's aggregate
    /// `Unknown` for *every* name on *every* server, so an absent name cost a
    /// resolver round trip and a client retry before anyone could say no.
    #[tokio::test]
    async fn the_production_composition_settles_names_without_the_resolver() {
        let server = crate::PvaServer::builder().ai("LOCAL:PV", 1.0).build();
        let store = server.store().clone();
        let sources = Arc::new(SourceRegistry::new());
        sources.add("builtin", 0, store.clone()).await;
        sources
            .add(
                "record-fields",
                10,
                Arc::new(crate::record_fields::RecordFieldSource::new(store.clone())),
            )
            .await;
        let (server_addr, client, state) = spawn_search_server(sources).await;

        let found = search_finds(
            &client,
            server_addr,
            31,
            "LOCAL:PV",
            StdDuration::from_secs(2),
        )
        .await;
        assert!(found, "a served local PV was not answered");
        assert_eq!(
            state.search_resolver.stats().started,
            0,
            "a served local PV was sent to the background resolver; it should \
             have been decided from memory on the first datagram"
        );

        let answered = search_answers_absent(
            &client,
            server_addr,
            32,
            "LOCAL:ABSENT",
            StdDuration::from_secs(2),
        )
        .await;
        assert!(
            answered,
            "the absent-name search was never answered; the test proves nothing"
        );
        assert_eq!(
            state.search_resolver.stats().started,
            0,
            "an absent plain name started a background resolution: the \
             registry's aggregate is not decisive over the production \
             composition, so every miss costs a round trip and a client retry"
        );
    }

    /// V1's HIGH-1, inverted into an acceptance test.
    ///
    /// Making the enumeration lazy narrowed the head-of-line vector to
    /// "any datagram carrying a pattern query", but `is_pattern_query` tests
    /// bytes the *remote peer* chooses and `PvListMode::List` is every
    /// server's default — so one unauthenticated datagram naming `"*"` still
    /// wedged the shared search task for the full duration of a hanging
    /// `names()`, denying search to every other client and every other name.
    /// V1 measured exactly that: `LOCAL:PV` unanswered for a whole 2s budget.
    ///
    /// The fix is not a narrower predicate — any predicate over attacker
    /// input just moves the trigger — but moving the enumeration, and the
    /// reply it feeds, onto their own task.
    #[tokio::test]
    async fn a_wildcard_datagram_does_not_delay_a_local_name() {
        let claims = Arc::new(AtomicUsize::new(0));
        let hanging = Arc::new(HangingSource::new(
            StdDuration::from_secs(5),
            claims.clone(),
        ));
        let name_calls = hanging.name_calls.clone();
        let sources = Arc::new(SourceRegistry::new());
        sources
            .add("local", 0, Arc::new(DecisiveSource::new("LOCAL:PV")))
            .await;
        sources.add("hanging", 1, hanging).await;
        let (server_addr, client, _state) = spawn_search_server(sources).await;

        // One wildcard datagram is enough to trigger the enumeration; the
        // loop absorbs the free_udp_port rebind race and gives the spawned
        // task time to actually enter the hanging `names()`.
        let wildcard = encode_search_request(1, 0x01, 0, [0u8; 16], &[(1, "*")], 2, false);
        for _ in 0..50 {
            client.send_to(&wildcard, server_addr).await.unwrap();
            tokio::time::sleep(StdDuration::from_millis(20)).await;
            if name_calls.load(Ordering::SeqCst) > 0 {
                break;
            }
        }
        assert!(
            name_calls.load(Ordering::SeqCst) > 0,
            "the wildcard never reached the enumeration; the test proves nothing"
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
        assert!(
            found,
            "a local name went unanswered while a wildcard query enumerated a \
             hanging source — one attacker-chosen '*' still denies search"
        );
        assert!(
            before.elapsed() < StdDuration::from_millis(500),
            "local name took {:?}; the search task is blocked enumerating names \
             for someone else's wildcard",
            before.elapsed()
        );
    }

    /// The other half of the laziness contract: a datagram that *does* carry
    /// a pattern query, with pvlist enabled, must still get the enumerated
    /// name list. Without this, "compute it only when needed" could be
    /// satisfied by never computing it at all.
    #[tokio::test]
    async fn a_pattern_query_still_matches_against_the_enumerated_names() {
        let sources = Arc::new(SourceRegistry::new());
        sources
            .add("local", 0, Arc::new(DecisiveSource::new("LOCAL:PV")))
            .await;
        let (server_addr, client, _state) = spawn_search_server(sources).await;

        let found = search_finds(
            &client,
            server_addr,
            11,
            "LOCAL:*",
            StdDuration::from_secs(2),
        )
        .await;
        assert!(
            found,
            "a wildcard search matched nothing; the visible-name list was not \
             computed for a datagram that reads it"
        );
    }

    /// A source whose `names()` output depends on `request_identity()`, the
    /// shape `spvirit-gateway`'s `GatewayStatusSource::names` has (it filters
    /// with `decide_local(Op::Get, name, &current_identity())`).
    struct IdentityFilteredNames;

    impl crate::pvstore::Source for IdentityFilteredNames {
        fn try_claim(&self, _name: &str) -> crate::pvstore::TryClaim {
            // Decisive "no": the only route to a `found` here is the pattern
            // path's enumeration.
            crate::pvstore::TryClaim::No
        }

        fn claim(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
            Box::pin(async { None })
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
            Box::pin(async {
                let (peer_ip, _user) = crate::request_ctx::request_identity();
                if peer_ip.is_some() {
                    vec!["IDENT:WITHPEER".to_string()]
                } else {
                    vec!["IDENT:ANONYMOUS".to_string()]
                }
            })
        }
    }

    /// V4 HIGH-1, UDP half. The enumeration is spawned *after* the datagram's
    /// `request_ctx::scope` has ended, and a spawned task inherits no
    /// task-local — so without an explicit `scope_with` the access-aware
    /// `names()` sees no peer at all and every host-qualified pvlist rule
    /// stops matching. Both directions are asserted: the peer-visible name
    /// must be answered, and the anonymous-only name must not leak.
    #[tokio::test]
    async fn the_udp_pattern_enumeration_sees_the_requesting_peer() {
        let sources = Arc::new(SourceRegistry::new());
        sources.add("identity", 0, Arc::new(IdentityFilteredNames)).await;
        let (server_addr, client, _state) = spawn_search_server(sources).await;

        let found = search_finds(
            &client,
            server_addr,
            61,
            "IDENT:WITHPEER*",
            StdDuration::from_secs(2),
        )
        .await;
        assert!(
            found,
            "the spawned enumeration ran with no peer identity, so an \
             `ALLOW … FROM <host>` rule would stop matching and hide names \
             from a legitimate operator"
        );

        let leaked = search_finds(
            &client,
            server_addr,
            62,
            "IDENT:ANONYMOUS*",
            StdDuration::from_secs(1),
        )
        .await;
        assert!(
            !leaked,
            "a name only the identity-less branch produces was answered: the \
             enumeration lost the request context, so `DENY … FROM <host>` \
             stops matching and hidden names are disclosed"
        );
    }

    /// A source whose `names()` never returns — the case
    /// [`PATTERN_ENUM_TIMEOUT`] exists for.
    struct NeverReturnsNames;

    impl crate::pvstore::Source for NeverReturnsNames {
        fn try_claim(&self, _name: &str) -> crate::pvstore::TryClaim {
            crate::pvstore::TryClaim::No
        }

        fn claim(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
            Box::pin(async { None })
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
            Box::pin(std::future::pending())
        }
    }

    fn bare_state(sources: Arc<SourceRegistry>) -> Arc<ServerState> {
        Arc::new(ServerState::new(
            sources,
            Arc::new(MonitorRegistry::new()),
            false,
            PvListMode::Discover,
            100,
            None,
            [0u8; 12],
            5075,
            None,
            "127.0.0.1".parse().unwrap(),
        ))
    }

    /// V4 LOW-2. A source stuck in `names()` used to hold its permit for the
    /// life of the process; four of those retired the whole budget and every
    /// later pattern query was shed forever. The enumeration is now bounded,
    /// and a timed-out one is a shed: the permit comes back, no reply is sent,
    /// and the counter moves.
    ///
    /// Runs on a paused clock, so the 30s bound costs no wall time — and the
    /// assertion is therefore about the timeout firing, not about the test
    /// being slow. No sockets are involved, so nothing else can be woken by
    /// the auto-advance.
    #[tokio::test(start_paused = true)]
    async fn an_enumeration_that_never_finishes_is_shed_and_returns_its_permit() {
        let sources = Arc::new(SourceRegistry::new());
        sources.add("hung", 0, Arc::new(NeverReturnsNames)).await;
        let state = bare_state(sources);

        let before = crate::search_resolve::global_stats().pattern_enum_shed;
        let replied = Arc::new(AtomicUsize::new(0));
        let seen = replied.clone();
        let dispatch = spawn_pattern_query_reply(
            &state,
            None,
            vec![(7, "HUNG:*".to_string())],
            move |_cids| async move {
                seen.fetch_add(1, Ordering::SeqCst);
            },
        );
        assert_eq!(
            dispatch,
            PatternDispatch::Deferred,
            "a permit was free, so the query must have been spawned"
        );

        // The permit is held while the enumeration runs...
        tokio::task::yield_now().await;
        assert_eq!(
            state.pattern_enum_permits.available_permits(),
            PATTERN_ENUM_CONCURRENCY - 1,
            "the spawned enumeration is not holding its permit"
        );

        // ...and must be released once the bound expires. The paused clock
        // auto-advances only when every task is idle, which is exactly the
        // situation a hung `names()` creates.
        tokio::time::sleep(PATTERN_ENUM_TIMEOUT + StdDuration::from_secs(1)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            state.pattern_enum_permits.available_permits(),
            PATTERN_ENUM_CONCURRENCY,
            "the permit was never returned: a source hung in names() still \
             retires one permit permanently, and {PATTERN_ENUM_CONCURRENCY} of \
             them disable pattern queries for the life of the process"
        );
        assert_eq!(
            replied.load(Ordering::SeqCst),
            0,
            "a timed-out enumeration answered anyway; it knows nothing about \
             what the server serves, so any answer it sends is a guess"
        );
        assert!(
            crate::search_resolve::global_stats().pattern_enum_shed > before,
            "the abandoned enumeration was not counted as a shed, so the one \
             failure mode that is silent on the wire is also invisible to an \
             operator"
        );
    }

    /// The self-sustaining part: a flood of distinct unresolvable names must
    /// be shed, not queued. If each one costs the responder an await, service
    /// never recovers while any client is still searching.
    ///
    /// The 200-name flood size and the 1s bound below are coupled: the
    /// responder answers `found = false` to every flood datagram (mask 0x01
    /// requests a reply), so `search_finds` must drain ~200 queued negative
    /// responses before it sees the `LOCAL:PV` reply, at one datagram (and
    /// one fresh retry) per loop iteration. Raising the flood size without
    /// raising the bound will make this fail on a perfectly healthy
    /// responder — that is a cost-of-the-harness effect, not evidence of a
    /// regression.
    #[tokio::test]
    async fn a_flood_of_unresolvable_names_does_not_deny_a_local_name() {
        let claims = Arc::new(AtomicUsize::new(0));
        let sources = Arc::new(SourceRegistry::new());
        sources
            .add("local", 0, Arc::new(DecisiveSource::new("LOCAL:PV")))
            .await;
        sources
            .add(
                "hanging",
                1,
                Arc::new(HangingSource::new(
                    StdDuration::from_secs(5),
                    claims.clone(),
                )),
            )
            .await;
        let (server_addr, client, state) = spawn_search_server(sources).await;

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

        // Anti-vacuity: prove the burst actually reached the responder and
        // was shed rather than queued. UDP gives no delivery guarantee, so
        // without this a fully dropped burst would pass this test against an
        // effectively idle server and certify nothing. `claims > 0` shows at
        // least one flood name was dispatched to a source; `dropped_full > 0`
        // shows the RESOLVE_CONCURRENCY=8 permit cap actually shed some of
        // the other ~192 rather than queuing them (which is the property the
        // test's name promises). Both are asserted directionally, not to an
        // exact count, so this does not flake on scheduling.
        assert!(
            claims.load(Ordering::SeqCst) > 0,
            "the flood never reached the responder; the test proves nothing"
        );
        let stats = state.search_resolver.stats();
        assert!(
            stats.dropped_full > 0,
            "no flood names were shed (dropped_full={}); the permit cap was \
             never exercised, so this test does not prove shedding happened",
            stats.dropped_full
        );
    }

    /// The resolver replies to nothing, so resolution has to reach the client
    /// through its own retry. First search misses; once the background claim
    /// lands, a retry finds it.
    ///
    /// This is not a head-of-line regression test — it exercises the
    /// Task-3b no-reply resolver design, not the Task-3 defect. It would
    /// (correctly) still pass against the pre-fix loop: `has_pv` there
    /// awaits `LateSource::claim` inline and answers the very first search
    /// once it resolves 100ms later, no retry needed. It was not run through
    /// Step 5's revert for that reason.
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
        let (server_addr, client, _state) = spawn_search_server(sources).await;

        // Negative first: an immediate search (well inside the 100ms claim
        // delay) must miss. Without this, a future change that made
        // `try_claim` resolve inline again — i.e. reintroduced the
        // head-of-line defect at this call site — would leave this test
        // green, since it only checks that *some* search eventually
        // succeeds.
        let immediate = search_finds(
            &client,
            server_addr,
            2,
            "LATE:PV",
            StdDuration::from_millis(30),
        )
        .await;
        assert!(
            !immediate,
            "LATE:PV was found before its background claim (100ms) could \
             have completed; the miss/retry path this test exists to prove \
             is not being exercised"
        );

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
