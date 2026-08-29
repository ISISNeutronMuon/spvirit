use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use dns_lookup::lookup_host;
use get_if_addrs::{IfAddr, get_if_addrs};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;
use tracing::debug;

use crate::auth::{default_authnz_host, default_authnz_user};
use crate::transport::read_packet;
use crate::types::{PvGetError, PvGetOptions};
use spvirit_codec::SegmentReassembler;
use spvirit_codec::epics_decode::{PvaPacket, PvaPacketCommand, PvaSearchResponsePayload};
use spvirit_codec::spvirit_encode::{
    encode_client_connection_validation, encode_search_request, ip_to_bytes,
    socket_addr_from_pva_bytes,
};

#[derive(Clone, Copy, Debug)]
pub struct SearchTarget {
    pub target: IpAddr,
    pub bind: IpAddr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiscoveredServer {
    pub guid: [u8; 12],
    pub tcp_addr: SocketAddr,
}

pub fn parse_addr_list(env: &str) -> Vec<IpAddr> {
    env.split(|c| c == ',' || c == ' ' || c == '\t')
        .filter(|s| !s.trim().is_empty())
        .filter_map(|s| parse_search_target_ip(s.trim()))
        .collect()
}

fn parse_search_target_ip(token: &str) -> Option<IpAddr> {
    if token.is_empty() {
        return None;
    }

    if let Ok(ip) = token.parse::<IpAddr>() {
        return Some(ip);
    }
    if let Ok(sock) = token.parse::<SocketAddr>() {
        return Some(sock.ip());
    }

    // Accept host:port where host may be a name or an IP literal.
    // For IPv6 bracket notation [::1]:port, SocketAddr::parse above already handles it.
    if let Some((host, port_str)) = token.rsplit_once(':') {
        if !host.is_empty()
            && !port_str.is_empty()
            && port_str.chars().all(|c| c.is_ascii_digit())
            && !host.contains(']')
        {
            if let Ok(ip) = host.parse::<IpAddr>() {
                return Some(ip);
            }
            if let Ok(addrs) = lookup_host(host) {
                // Prefer IPv4 for backward compat, fall back to first IPv6
                let addrs: Vec<IpAddr> = addrs.collect();
                if let Some(ip) = addrs
                    .iter()
                    .find(|ip| ip.is_ipv4())
                    .copied()
                    .or_else(|| addrs.into_iter().next())
                {
                    return Some(ip);
                }
            }
        }
    }

    if let Ok(addrs) = lookup_host(token) {
        // Prefer IPv4, fall back to first IPv6
        let addrs: Vec<IpAddr> = addrs.collect();
        if let Some(ip) = addrs
            .iter()
            .find(|ip| ip.is_ipv4())
            .copied()
            .or_else(|| addrs.into_iter().next())
        {
            return Some(ip);
        }
    }

    None
}

/// Return a default unspecified bind address matching the target's address family.
fn unspecified_for(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

pub fn build_search_targets(
    search_addr: Option<IpAddr>,
    bind_addr: Option<IpAddr>,
) -> Vec<SearchTarget> {
    // Explicit --search-addr overrides everything (single target).
    if let Some(ip) = search_addr {
        return vec![SearchTarget {
            target: ip,
            bind: bind_addr.unwrap_or_else(|| unspecified_for(ip)),
        }];
    }

    let mut targets = Vec::new();
    let mut seen = HashSet::new();

    // Addresses from EPICS_PVA_ADDR_LIST.
    if let Ok(env) = std::env::var("EPICS_PVA_ADDR_LIST") {
        for ip in parse_addr_list(&env) {
            if seen.insert(ip) {
                targets.push(SearchTarget {
                    target: ip,
                    bind: bind_addr.unwrap_or_else(|| unspecified_for(ip)),
                });
            }
        }
    }

    // Merge auto-discovered broadcast addresses unless explicitly disabled.
    // This matches EPICS Base behaviour: ADDR_LIST + auto-broadcast combined.
    if is_auto_addr_list_enabled() {
        for t in build_auto_broadcast_targets() {
            if seen.insert(t.target) {
                targets.push(SearchTarget {
                    target: t.target,
                    bind: bind_addr.unwrap_or(t.bind),
                });
            }
        }
    }

    targets
}

pub fn is_auto_addr_list_enabled() -> bool {
    match std::env::var("EPICS_PVA_AUTO_ADDR_LIST") {
        Ok(v) => {
            let v = v.trim().to_ascii_uppercase();
            v == "YES" || v == "Y" || v == "1" || v == "TRUE"
        }
        Err(_) => true,
    }
}

fn ipv4_is_link_local(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 169 && octets[1] == 254
}

fn choose_default_bind_v4() -> Option<Ipv4Addr> {
    let ifaces = get_if_addrs().ok()?;
    for iface in ifaces {
        if let IfAddr::V4(v4) = iface.addr {
            let ip = v4.ip;
            if ip.is_loopback() || ipv4_is_link_local(ip) {
                continue;
            }
            return Some(ip);
        }
    }
    None
}

fn choose_default_bind_v6() -> Option<Ipv6Addr> {
    let ifaces = get_if_addrs().ok()?;
    for iface in ifaces {
        if let IfAddr::V6(v6) = iface.addr {
            let ip = v6.ip;
            if ip.is_loopback() {
                continue;
            }
            // Skip link-local (fe80::/10) — not routable without scope id
            let segs = ip.segments();
            if segs[0] & 0xffc0 == 0xfe80 {
                continue;
            }
            return Some(ip);
        }
    }
    None
}

fn broadcast_for(ip: Ipv4Addr, netmask: Ipv4Addr) -> Ipv4Addr {
    let ip_u = u32::from(ip);
    let mask_u = u32::from(netmask);
    Ipv4Addr::from(ip_u | !mask_u)
}

fn discovery_target_for(ip: Ipv4Addr, netmask: Ipv4Addr) -> Ipv4Addr {
    let limited_broadcast = Ipv4Addr::new(255, 255, 255, 255);
    if netmask == Ipv4Addr::new(255, 255, 255, 255) || netmask.is_unspecified() {
        return limited_broadcast;
    }
    let directed = broadcast_for(ip, netmask);
    if directed == ip {
        limited_broadcast
    } else {
        directed
    }
}

pub fn build_auto_broadcast_targets() -> Vec<SearchTarget> {
    let mut targets = Vec::new();
    let mut fallback_targets = Vec::new();
    let mut fallback_seen = HashSet::new();
    let mut added_v4_multicast = false;
    let mut added_v6_multicast = false;
    let ifaces = match get_if_addrs() {
        Ok(v) => v,
        Err(_) => return targets,
    };
    for iface in &ifaces {
        if let IfAddr::V4(v4) = &iface.addr {
            let ip = v4.ip;
            if ip.is_loopback() || ipv4_is_link_local(ip) {
                continue;
            }
            let bcast = discovery_target_for(ip, v4.netmask);
            targets.push(SearchTarget {
                target: IpAddr::V4(bcast),
                bind: IpAddr::V4(ip),
            });
            // Also send to IPv4 multicast group (matching PVXS behaviour).
            // Docker overlay networks may block broadcast but allow multicast.
            targets.push(SearchTarget {
                target: IpAddr::V4(PVA_MULTICAST_V4),
                bind: IpAddr::V4(ip),
            });
            if fallback_seen.insert(IpAddr::V4(bcast)) {
                fallback_targets.push(SearchTarget {
                    target: IpAddr::V4(bcast),
                    bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                });
            }
            if !added_v4_multicast {
                added_v4_multicast = true;
                fallback_targets.push(SearchTarget {
                    target: IpAddr::V4(PVA_MULTICAST_V4),
                    bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                });
            }
        }
    }
    // Add IPv6 multicast targets for each non-loopback, non-link-local v6 iface.
    for iface in &ifaces {
        if let IfAddr::V6(v6) = &iface.addr {
            let ip = v6.ip;
            if ip.is_loopback() {
                continue;
            }
            let segs = ip.segments();
            if segs[0] & 0xffc0 == 0xfe80 {
                continue; // skip link-local
            }
            let multicast_target = IpAddr::V6(PVA_MULTICAST_V6);
            targets.push(SearchTarget {
                target: multicast_target,
                bind: IpAddr::V6(ip),
            });
            if !added_v6_multicast {
                added_v6_multicast = true;
                fallback_targets.push(SearchTarget {
                    target: multicast_target,
                    bind: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                });
            }
        }
    }
    targets.extend(fallback_targets);
    targets
}

/// PVA multicast group (IPv4).
const PVA_MULTICAST_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 128);

/// PVA multicast group (IPv6 link-local, ff02::42:1).
const PVA_MULTICAST_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0x42, 1);

/// Best-effort join the PVA multicast group appropriate for the bind address.
fn join_multicast_any(socket: &std::net::UdpSocket, bind: IpAddr) {
    match bind {
        IpAddr::V4(iface) => {
            let _ = socket.join_multicast_v4(&PVA_MULTICAST_V4, &iface);
        }
        IpAddr::V6(_) => {
            // interface index 0 = OS picks the default interface
            let _ = socket.join_multicast_v6(&PVA_MULTICAST_V6, 0);
        }
    }
}

fn decode_search_response_addr(addr: [u8; 16], port: u16, src: SocketAddr) -> SocketAddr {
    socket_addr_from_pva_bytes(addr, port)
        .filter(|a| !a.ip().is_unspecified())
        .unwrap_or_else(|| SocketAddr::new(src.ip(), port))
}

fn normalize_discovered_servers(items: Vec<DiscoveredServer>) -> Vec<DiscoveredServer> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if seen.insert((item.guid, item.tcp_addr)) {
            out.push(item);
        }
    }
    out.sort_by(|a, b| a.tcp_addr.to_string().cmp(&b.tcp_addr.to_string()));
    out
}

/// Create a UDP socket with SO_REUSEADDR set (matching PVXS behaviour),
/// allowing multiple processes to share the search port.
///
/// On Windows SO_REUSEADDR has different (unsafe) semantics — it allows
/// a second socket to steal an actively-used port — so we only enable it
/// on Unix where it merely permits rebinding during TIME_WAIT.
fn bind_udp_reuse(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    #[cfg(unix)]
    sock.set_reuse_address(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

/// UDP sockets opened for one search/discovery operation, plus the shared
/// channel their receiver tasks forward inbound packets into.
///
/// `recv_tasks` MUST be held for the whole receive loop and dropped only when
/// the operation ends: dropping this struct aborts the receiver tasks, which
/// releases their `Arc<UdpSocket>` clones and closes the ephemeral sockets.
/// This is the fd-leak fix — a receiver parked in `recv_from().await` on a
/// quiet target otherwise lives forever holding the socket open (one leaked fd
/// per call, exhausting the fd table under churn). See the
/// `search_pv_does_not_leak_udp_sockets` regression test.
struct SearchSockets {
    /// Per bind group: (socket, encoded request, destinations) for retransmit.
    socket_info: Vec<(Arc<UdpSocket>, Vec<u8>, Vec<SocketAddr>)>,
    /// Receiver tasks; held only for their `Drop` — dropping the set aborts
    /// the tasks and closes the sockets (the fd-leak fix), so the field is
    /// never read directly.
    #[allow(dead_code)]
    recv_tasks: tokio::task::JoinSet<()>,
    /// Inbound (packet, source) stream merged from every socket.
    rx: tokio::sync::mpsc::Receiver<(Vec<u8>, SocketAddr)>,
}

/// Shared UDP setup for [`search_pv`] and [`discover_servers`]: group targets
/// by bind address, open one ephemeral socket per bind, send the request built
/// by `build_msg(reply_port, reply_addr)` to every target, and spawn a
/// receiver task per socket forwarding packets into a shared channel.
///
/// Returns `Err(last_io_error)` when no socket could be opened, so the caller
/// can pick the error kind appropriate to its operation (`None` = no I/O error
/// occurred, the caller had no viable targets/binds).
async fn open_search_sockets<F>(
    udp_port: u16,
    targets: &[SearchTarget],
    debug_enabled: bool,
    op: &str,
    mut build_msg: F,
) -> Result<SearchSockets, Option<std::io::Error>>
where
    F: FnMut(u16, [u8; 16]) -> Vec<u8>,
{
    let mut last_io_error: Option<std::io::Error> = None;

    // Group targets by bind address so we can share a socket per bind.
    let mut bind_groups: Vec<(IpAddr, Vec<IpAddr>)> = Vec::new();
    for t in targets {
        if let Some(group) = bind_groups.iter_mut().find(|(b, _)| *b == t.bind) {
            group.1.push(t.target);
        } else {
            bind_groups.push((t.bind, vec![t.target]));
        }
    }

    // Open sockets and send to all targets first, then collect responses.
    // Store (socket, message, destinations) for retransmission.
    let mut socket_info: Vec<(Arc<UdpSocket>, Vec<u8>, Vec<SocketAddr>)> = Vec::new();

    for (bind_ip, group_targets) in &bind_groups {
        // Always use an ephemeral port for the client socket. We only receive
        // unicast replies, so sharing the server's search port is unnecessary
        // — and on Linux with SO_REUSEPORT the kernel would route our own
        // outbound packet back to us instead of the server.
        let bind_addr = SocketAddr::new(*bind_ip, 0);
        let (std_sock, actual_bind_addr) = match bind_udp_reuse(bind_addr) {
            Ok(sock) => {
                let actual = sock.local_addr().unwrap_or(bind_addr);
                (sock, actual)
            }
            Err(err) => {
                if debug_enabled {
                    debug!(
                        "pva {op} skipping bind={} step=bind kind={:?} err={}",
                        bind_addr,
                        err.kind(),
                        err
                    );
                }
                last_io_error = Some(err);
                continue;
            }
        };
        if let Err(err) = std_sock.set_broadcast(true) {
            if debug_enabled {
                debug!(
                    "pva {op} skipping bind={} step=set_broadcast kind={:?} err={}",
                    bind_addr,
                    err.kind(),
                    err
                );
            }
            last_io_error = Some(err);
            continue;
        }

        join_multicast_any(&std_sock, *bind_ip);

        let reply_addr = ip_to_bytes(*bind_ip);
        let reply_port = match std_sock.local_addr() {
            Ok(addr) => addr.port(),
            Err(err) => {
                if debug_enabled {
                    debug!(
                        "pva {op} skipping bind={} step=local_addr kind={:?} err={}",
                        bind_addr,
                        err.kind(),
                        err
                    );
                }
                last_io_error = Some(err);
                continue;
            }
        };
        let msg = build_msg(reply_port, reply_addr);

        let socket = match UdpSocket::from_std(std_sock) {
            Ok(socket) => socket,
            Err(err) => {
                if debug_enabled {
                    debug!(
                        "pva {op} skipping bind={} step=from_std kind={:?} err={}",
                        bind_addr,
                        err.kind(),
                        err
                    );
                }
                last_io_error = Some(err);
                continue;
            }
        };

        let dests: Vec<SocketAddr> = group_targets
            .iter()
            .map(|ip| SocketAddr::new(*ip, udp_port))
            .collect();

        // Send to every target in this bind group immediately.
        for dest in &dests {
            if debug_enabled {
                debug!(
                    "pva {op} bind={} target={} server_port={} reply_port={}",
                    actual_bind_addr,
                    dest.ip(),
                    udp_port,
                    reply_port
                );
                debug!("pva {op} send {} bytes to {}", msg.len(), dest);
            }
            if let Err(err) = socket.send_to(&msg, dest).await {
                if debug_enabled {
                    debug!(
                        "pva {op} send_to target={} kind={:?} err={}",
                        dest,
                        err.kind(),
                        err
                    );
                }
                last_io_error = Some(err);
            }
        }

        socket_info.push((Arc::new(socket), msg, dests));
    }

    if socket_info.is_empty() {
        return Err(last_io_error);
    }

    // Spawn a receiver task per socket that forwards packets into a shared
    // channel. The tasks are held in a JoinSet so that when the operation ends
    // — on success, timeout, or error — the JoinSet is dropped and every
    // receiver task is aborted, closing the ephemeral socket instead of
    // leaking it (see the `SearchSockets` doc and the fd-leak regression test).
    let (tx, rx) = tokio::sync::mpsc::channel::<(Vec<u8>, SocketAddr)>(64);
    let mut recv_tasks = tokio::task::JoinSet::new();
    for (sock, _, _) in &socket_info {
        let sock = Arc::clone(sock);
        let tx = tx.clone();
        recv_tasks.spawn(async move {
            loop {
                let mut buf = vec![0u8; 2048];
                match sock.recv_from(&mut buf).await {
                    Ok((len, src)) => {
                        buf.truncate(len);
                        if tx.send((buf, src)).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    drop(tx); // Only spawned tasks hold senders; channel closes when they exit.

    Ok(SearchSockets {
        socket_info,
        recv_tasks,
        rx,
    })
}

/// Shared receive loop for [`search_pv`] and [`discover_servers`].
///
/// Drives the retransmit schedule against `deadline`, decodes each inbound
/// packet, applies the filtering common to both callers (matching `seq`; TCP
/// protocol only), and hands every accepted [`PvaSearchResponsePayload`] to
/// `on_response`. The handler returns [`ControlFlow::Break`] to stop early and
/// return its value (first-match, as `search_pv` does) or
/// [`ControlFlow::Continue`] to keep collecting (as `discover_servers` does).
///
/// `fail_on_decode_error` selects the decode-failure policy: `true` returns a
/// [`PvGetError::Search`] (search_pv), `false` skips the packet (discover).
/// Returns `Ok(None)` when the deadline elapses or every socket closed without
/// an early return.
async fn run_search_recv_loop<H, T>(
    sockets: &mut SearchSockets,
    seq: u32,
    deadline: tokio::time::Instant,
    debug_enabled: bool,
    op: &str,
    fail_on_decode_error: bool,
    mut on_response: H,
) -> Result<Option<T>, PvGetError>
where
    H: FnMut(PvaSearchResponsePayload, SocketAddr) -> ControlFlow<T>,
{
    // Bind the two accessed fields as disjoint borrows up front so the
    // `select!` below cannot be read as borrowing all of `*sockets`.
    let rx = &mut sockets.rx;
    let socket_info = &sockets.socket_info;

    // Retransmit schedule: exponential backoff from start.
    let retransmit_offsets = [100u64, 500, 1000, 2000];
    let start = tokio::time::Instant::now();
    let mut next_retransmit = 0usize;

    loop {
        // Compute the next wake-up: either the next retransmit or the deadline.
        let next_retransmit_at = if next_retransmit < retransmit_offsets.len() {
            start + Duration::from_millis(retransmit_offsets[next_retransmit])
        } else {
            deadline
        };
        let wake_at = next_retransmit_at.min(deadline);

        tokio::select! {
            recv = rx.recv() => {
                let Some((buf, src)) = recv else { break };
                let mut pkt = PvaPacket::new(&buf);
                let cmd = match pkt.decode_payload() {
                    Some(cmd) => cmd,
                    None => {
                        if fail_on_decode_error {
                            return Err(PvGetError::Search("failed to decode search response"));
                        }
                        continue;
                    }
                };
                if let PvaPacketCommand::SearchResponse(payload) = cmd {
                    if debug_enabled {
                        debug!(
                            "pva {op} response found={} cids={:?} addr={:?} port={}",
                            payload.found, payload.cids, payload.addr, payload.port
                        );
                    }
                    if payload.seq != seq {
                        continue;
                    }
                    if !payload.protocol.is_empty() && !payload.protocol.eq_ignore_ascii_case("tcp") {
                        continue;
                    }
                    if let ControlFlow::Break(value) = on_response(payload, src) {
                        return Ok(Some(value));
                    }
                }
            }
            _ = tokio::time::sleep_until(wake_at) => {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                // Retransmit to all targets on all sockets.
                if next_retransmit < retransmit_offsets.len() {
                    if debug_enabled {
                        debug!("pva {op} retransmit round {}", next_retransmit + 1);
                    }
                    for (sock, msg, dests) in socket_info {
                        for dest in dests {
                            let _ = sock.send_to(msg, dest).await;
                        }
                    }
                    next_retransmit += 1;
                }
            }
        }
    }

    Ok(None)
}

pub async fn search_pv(
    pv_name: &str,
    udp_port: u16,
    timeout_dur: Duration,
    targets: &[SearchTarget],
    debug_enabled: bool,
) -> Result<(SocketAddr, [u8; 12]), PvGetError> {
    if targets.is_empty() {
        return Err(PvGetError::Search("no search targets"));
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seq = (now.as_nanos() as u32).wrapping_add(std::process::id());
    let cid = seq ^ 0x9E37_79B9;

    let deadline = tokio::time::Instant::now() + timeout_dur;

    let mut sockets = match open_search_sockets(
        udp_port,
        targets,
        debug_enabled,
        "search",
        |reply_port, reply_addr| {
            let requests = [(cid, pv_name)];
            encode_search_request(seq, 0x81, reply_port, reply_addr, &requests, 2, false)
        },
    )
    .await
    {
        Ok(s) => s,
        Err(Some(err)) => return Err(PvGetError::Io(err)),
        Err(None) => return Err(PvGetError::Timeout("search response")),
    };

    let found = run_search_recv_loop(
        &mut sockets,
        seq,
        deadline,
        debug_enabled,
        "search",
        // search_pv treats an undecodable response as a hard error.
        true,
        |payload, src| {
            if !payload.found {
                return ControlFlow::Continue(());
            }
            if !payload.cids.is_empty() && !payload.cids.contains(&cid) {
                return ControlFlow::Continue(());
            }
            let addr = decode_search_response_addr(payload.addr, payload.port, src);
            if debug_enabled {
                debug!("pva search response from {}", addr);
            }
            ControlFlow::Break((addr, payload.guid))
        },
    )
    .await?;

    found.ok_or(PvGetError::Timeout("search response"))
}

pub fn default_bind_ip() -> Option<IpAddr> {
    choose_default_bind_v4()
        .map(IpAddr::V4)
        .or_else(|| choose_default_bind_v6().map(IpAddr::V6))
}

/// Parse `EPICS_PVA_NAME_SERVERS` value into socket addresses.
/// Accepts space/comma separated entries: `host:port`, `ip`, `hostname`
/// (port defaults to 5075).
pub fn parse_name_servers(env_val: &str) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for token in env_val.split(|c| c == ',' || c == ' ' || c == '\t') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Ok(addr) = token.parse::<SocketAddr>() {
            out.push(addr);
            continue;
        }
        if let Ok(ip) = token.parse::<IpAddr>() {
            out.push(SocketAddr::new(ip, 5075));
            continue;
        }
        use std::net::ToSocketAddrs;
        if let Ok(mut addrs) = token.to_socket_addrs() {
            if let Some(addr) = addrs.next() {
                out.push(addr);
                continue;
            }
        }
        let with_port = format!("{}:5075", token);
        if let Ok(mut addrs) = with_port.to_socket_addrs() {
            if let Some(addr) = addrs.next() {
                out.push(addr);
            }
        }
    }
    out
}

/// Build a minimal PVA ConnectionValidation response for name server search.
fn encode_search_validation(version: u8, is_be: bool) -> Vec<u8> {
    let user = default_authnz_user();
    let host = default_authnz_host();
    encode_client_connection_validation(87_040, 32_767, 0, "ca", &user, &host, version, is_be)
}

/// Search for a PV via a TCP connection to a PVA name server.
///
/// Connects to the name server, performs the PVA handshake, sends a search
/// request over TCP, and returns the server address from the search response.
pub async fn search_pv_tcp(
    pv_name: &str,
    name_server: SocketAddr,
    timeout_dur: Duration,
    debug_enabled: bool,
) -> Result<(SocketAddr, [u8; 12]), PvGetError> {
    let deadline = tokio::time::Instant::now() + timeout_dur;

    let mut stream = tokio::time::timeout(timeout_dur, tokio::net::TcpStream::connect(name_server))
        .await
        .map_err(|_| PvGetError::Timeout("name server connect"))??;

    // One reassembler for this connection, shared by every read below.
    let mut reassembler = SegmentReassembler::new();

    let mut version = 2u8;
    let mut is_be = false;

    // Read SET_BYTE_ORDER + ConnectionValidation from name server.
    for _ in 0..2 {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(PvGetError::Timeout("name server handshake"));
        }
        let remaining = deadline - now;
        if let Ok(bytes) = read_packet(&mut stream, remaining, &mut reassembler).await {
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

    let validation = encode_search_validation(version, is_be);
    stream.write_all(&validation).await?;

    // Wait for ConnectionValidated.
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(PvGetError::Timeout("name server validated"));
        }
        let remaining = deadline - now;
        let bytes = read_packet(&mut stream, remaining, &mut reassembler).await?;
        let mut pkt = PvaPacket::new(&bytes);
        if let Some(cmd) = pkt.decode_payload() {
            if matches!(cmd, PvaPacketCommand::ConnectionValidated(_)) {
                break;
            }
        }
    }

    // Send search request over TCP.
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seq = (now_ts.as_nanos() as u32).wrapping_add(std::process::id());
    let cid = seq ^ 0x9E37_79B9;
    let requests = [(cid, pv_name)];
    let msg = encode_search_request(seq, 0x80, 0, [0u8; 16], &requests, version, is_be);
    stream.write_all(&msg).await?;

    if debug_enabled {
        debug!(
            "pva tcp search sent to name_server={} pv={}",
            name_server, pv_name
        );
    }

    // Read search response.
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(PvGetError::Timeout("name server search response"));
        }
        let remaining = deadline - now;
        let bytes = read_packet(&mut stream, remaining, &mut reassembler).await?;
        let mut pkt = PvaPacket::new(&bytes);
        if let Some(cmd) = pkt.decode_payload() {
            if let PvaPacketCommand::SearchResponse(payload) = cmd {
                if !payload.found {
                    continue;
                }
                if !payload.cids.is_empty() && !payload.cids.contains(&cid) {
                    continue;
                }
                let addr = decode_search_response_addr(payload.addr, payload.port, name_server);
                if debug_enabled {
                    debug!(
                        "pva tcp search response from name_server={}: {}",
                        name_server, addr
                    );
                }
                return Ok((addr, payload.guid));
            }
        }
    }
}

/// Resolve the PVA server for a PV using name servers (TCP) and/or UDP search.
///
/// - If `opts.server_addr` is set, returns it directly.
/// - Tries each name server from `opts.name_servers` and `EPICS_PVA_NAME_SERVERS`
///   via TCP search.
/// - Falls back to UDP search using `build_search_targets()`.
pub async fn resolve_pv_server(opts: &PvGetOptions) -> Result<(SocketAddr, [u8; 12]), PvGetError> {
    if let Some(addr) = opts.server_addr {
        // Unicast shortcut — there's no search response to read a guid
        // from, so the caller gets the "unknown" sentinel.
        return Ok((addr, [0u8; 12]));
    }

    let mut name_servers = opts.name_servers.clone();
    if let Ok(env) = std::env::var("EPICS_PVA_NAME_SERVERS") {
        name_servers.extend(parse_name_servers(&env));
    }

    let no_broadcast = opts.no_broadcast;

    // Fail fast when no search strategy is available.
    if no_broadcast && name_servers.is_empty() {
        return Err(PvGetError::Search(
            "no search strategy: specify --name-server or --server when using --no-broadcast",
        ));
    }

    // Launch all search strategies concurrently — TCP name servers + UDP broadcast.
    // Return the first successful result.
    let targets = build_search_targets(opts.search_addr, opts.bind_addr);

    let pv = opts.pv_name.clone();
    let timeout_dur = opts.timeout;
    let debug_enabled = opts.debug;
    let udp_port = opts.udp_port;

    let mut set = tokio::task::JoinSet::new();

    for ns in name_servers {
        let pv = pv.clone();
        set.spawn(async move {
            let result = search_pv_tcp(&pv, ns, timeout_dur, debug_enabled).await?;
            Ok::<(SocketAddr, [u8; 12]), PvGetError>(result)
        });
    }

    if !no_broadcast {
        let pv = pv.clone();
        let targets = targets.clone();
        set.spawn(async move {
            let result = search_pv(&pv, udp_port, timeout_dur, &targets, debug_enabled).await?;
            Ok(result)
        });
    }

    let mut last_err = None;
    while let Some(result) = set.join_next().await {
        match result {
            Ok(Ok(result)) => {
                set.abort_all();
                return Ok(result);
            }
            Ok(Err(e)) => {
                if debug_enabled {
                    debug!("pva search strategy failed: {}", e);
                }
                last_err = Some(e);
            }
            Err(join_err) => {
                if debug_enabled {
                    debug!("pva search task panicked: {}", join_err);
                }
            }
        }
    }

    Err(last_err.unwrap_or(PvGetError::Timeout("search response")))
}

pub async fn discover_servers(
    udp_port: u16,
    timeout_dur: Duration,
    targets: &[SearchTarget],
    debug_enabled: bool,
) -> Result<Vec<DiscoveredServer>, PvGetError> {
    if targets.is_empty() {
        return Err(PvGetError::Search("no search targets"));
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seq = (now.as_nanos() as u32).wrapping_add(std::process::id());

    let mut found: Vec<DiscoveredServer> = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout_dur;

    let mut sockets = match open_search_sockets(
        udp_port,
        targets,
        debug_enabled,
        "discover",
        // Server discovery sends an empty pv-request list (no cids).
        |reply_port, reply_addr| {
            encode_search_request(seq, 0x81, reply_port, reply_addr, &[], 2, false)
        },
    )
    .await
    {
        Ok(s) => s,
        Err(Some(err)) => return Err(PvGetError::Io(err)),
        // Discovery reports "no targets" rather than a timeout when no socket
        // could be opened.
        Err(None) => return Err(PvGetError::Search("no search targets")),
    };

    // Collect every responder until the deadline; discovery never stops early
    // and (unlike search_pv) skips undecodable packets rather than erroring.
    run_search_recv_loop(
        &mut sockets,
        seq,
        deadline,
        debug_enabled,
        "discover",
        false,
        |payload, src| -> ControlFlow<()> {
            let tcp_addr = decode_search_response_addr(payload.addr, payload.port, src);
            found.push(DiscoveredServer {
                guid: payload.guid,
                tcp_addr,
            });
            ControlFlow::Continue(())
        },
    )
    .await?;

    Ok(normalize_discovered_servers(found))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_codec::epics_decode::{PvaPacket, PvaPacketCommand};
    use spvirit_server::pva_server::PvaServer;
    use std::net::TcpListener;
    use std::net::UdpSocket as StdUdpSocket;

    fn free_tcp_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .expect("bind tcp")
            .local_addr()
            .expect("local_addr")
            .port()
    }

    fn free_udp_port() -> u16 {
        StdUdpSocket::bind("127.0.0.1:0")
            .expect("bind udp")
            .local_addr()
            .expect("local_addr")
            .port()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_pv_server_returns_responder_guid() {
        let g = [9u8; 12];
        let tcp_port = free_tcp_port();
        let udp_port = free_udp_port();

        let server = PvaServer::builder()
            .ai("GUIDPV", 1.0)
            .port(tcp_port)
            .udp_port(udp_port)
            .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .guid(g)
            .build();
        tokio::spawn(async move {
            let _ = server.run().await;
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut opts = PvGetOptions::new("GUIDPV".to_string());
        opts.udp_port = udp_port;
        opts.tcp_port = tcp_port;
        // CI containers often do not route UDP broadcast to loopback
        // listeners, so force explicit loopback discovery/bind.
        opts.search_addr = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
        opts.bind_addr = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));

        let (addr, guid) = resolve_pv_server(&opts).await.expect("resolve");
        assert_eq!(guid, g);
        assert!(addr.ip().is_loopback());
    }

    // Regression test for the UDP search-socket leak: every `search_pv` call
    // binds an ephemeral UDP socket and spawns a receiver task holding a clone
    // of it. If the function returns without cancelling that task, the task
    // stays parked in `recv_from().await` forever and the socket is never
    // closed — one leaked fd per search, which exhausts the gateway's fd table
    // under churn (observed in production as EMFILE). The timeout path leaks
    // identically to the success path, so we drive it with a dead target and
    // assert the process fd count does not grow across many searches.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn search_pv_does_not_leak_udp_sockets() {
        fn open_fd_count() -> usize {
            std::fs::read_dir("/proc/self/fd")
                .map(|d| d.count())
                .unwrap_or(0)
        }

        // A loopback UDP port nobody listens on: searches get no reply and
        // fall through to the timeout return path.
        let dead_port = free_udp_port();
        let targets = [SearchTarget {
            target: IpAddr::V4(Ipv4Addr::LOCALHOST),
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        }];
        let timeout = Duration::from_millis(60);

        // Warm up so runtime/one-time fds are allocated before we baseline.
        for _ in 0..3 {
            let _ = search_pv("NOPV", dead_port, timeout, &targets, false).await;
        }

        let before = open_fd_count();
        let iters = 40;
        for _ in 0..iters {
            let r = search_pv("NOPV", dead_port, timeout, &targets, false).await;
            assert!(r.is_err(), "search of a dead port must time out, not succeed");
        }
        // Let any just-cancelled receiver tasks drop their sockets.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let after = open_fd_count();

        let growth = after.saturating_sub(before);
        assert!(
            growth < 10,
            "leaked UDP search sockets: fd count grew by {growth} over {iters} \
             timed-out searches (before={before}, after={after})"
        );
    }

    #[test]
    fn encode_decode_search_request_roundtrip() {
        let seq = 1234;
        let cid = 42;
        let port = 5076;
        let pv_name = "TEST:PV";
        let reply_addr = ip_to_bytes(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)));
        let requests = [(cid, pv_name)];
        let msg = encode_search_request(seq, 0x81, port, reply_addr, &requests, 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::Search(payload) => {
                assert_eq!(payload.seq, seq);
                assert_eq!(payload.mask, 0x81);
                assert_eq!(payload.addr, reply_addr);
                assert_eq!(payload.port, port);
                assert_eq!(payload.protocols, vec!["tcp".to_string()]);
                assert_eq!(payload.pv_requests.len(), 1);
                assert_eq!(payload.pv_requests[0].0, cid);
                assert_eq!(payload.pv_requests[0].1, pv_name.to_string());
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_server_discovery_request_roundtrip() {
        let seq = 4321;
        let port = 5076;
        let reply_addr = ip_to_bytes(IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40)));
        let msg = encode_search_request(seq, 0x81, port, reply_addr, &[], 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::Search(payload) => {
                assert_eq!(payload.seq, seq);
                assert_eq!(payload.pv_requests.len(), 0);
                assert_eq!(payload.protocols, vec!["tcp".to_string()]);
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn normalize_discovered_servers_deduplicates_by_guid_and_addr() {
        let guid = [1u8; 12];
        let s1 = DiscoveredServer {
            guid,
            tcp_addr: "127.0.0.1:5075".parse().unwrap(),
        };
        let s2 = DiscoveredServer {
            guid,
            tcp_addr: "127.0.0.1:5075".parse().unwrap(),
        };
        let s3 = DiscoveredServer {
            guid: [2u8; 12],
            tcp_addr: "127.0.0.1:5075".parse().unwrap(),
        };
        let normalized = normalize_discovered_servers(vec![s1, s2, s3]);
        assert_eq!(normalized.len(), 2);
    }

    #[test]
    fn parse_addr_list_accepts_ip_and_ip_port() {
        let items = parse_addr_list("192.168.1.10 10.0.0.1:5076");
        assert!(items.contains(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));
        assert!(items.contains(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
    }

    #[test]
    fn discovery_target_falls_back_to_limited_broadcast_for_invalid_netmask() {
        let ip = Ipv4Addr::new(130, 246, 90, 92);
        assert_eq!(
            discovery_target_for(ip, Ipv4Addr::new(255, 255, 255, 255)),
            Ipv4Addr::new(255, 255, 255, 255)
        );
        assert_eq!(
            discovery_target_for(ip, Ipv4Addr::new(0, 0, 0, 0)),
            Ipv4Addr::new(255, 255, 255, 255)
        );
    }

    #[test]
    fn discovery_target_uses_directed_broadcast_for_normal_subnet() {
        let ip = Ipv4Addr::new(192, 168, 56, 1);
        let netmask = Ipv4Addr::new(255, 255, 255, 0);
        assert_eq!(
            discovery_target_for(ip, netmask),
            Ipv4Addr::new(192, 168, 56, 255)
        );
    }

    #[test]
    fn parse_name_servers_ip_with_port() {
        let addrs = parse_name_servers("192.168.1.10:5075");
        assert_eq!(
            addrs,
            vec!["192.168.1.10:5075".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn parse_name_servers_ip_without_port_defaults_to_5075() {
        let addrs = parse_name_servers("10.0.0.1");
        assert_eq!(
            addrs,
            vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                5075
            )]
        );
    }

    #[test]
    fn parse_name_servers_multiple_comma_separated() {
        let addrs = parse_name_servers("10.0.0.1:5075,10.0.0.2:9876");
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0], "10.0.0.1:5075".parse::<SocketAddr>().unwrap());
        assert_eq!(addrs[1], "10.0.0.2:9876".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_name_servers_multiple_space_separated() {
        let addrs = parse_name_servers("10.0.0.1 10.0.0.2:5075");
        assert_eq!(addrs.len(), 2);
        assert_eq!(
            addrs[0],
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5075)
        );
        assert_eq!(addrs[1], "10.0.0.2:5075".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_name_servers_empty_string() {
        let addrs = parse_name_servers("");
        assert!(addrs.is_empty());
    }

    #[test]
    fn parse_name_servers_whitespace_only() {
        let addrs = parse_name_servers("  \t  ");
        assert!(addrs.is_empty());
    }

    #[test]
    fn parse_name_servers_mixed_separators() {
        let addrs = parse_name_servers("10.0.0.1:5075, 10.0.0.2  ,  10.0.0.3:9999");
        assert_eq!(addrs.len(), 3);
        assert_eq!(addrs[0], "10.0.0.1:5075".parse::<SocketAddr>().unwrap());
        assert_eq!(
            addrs[1],
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 5075)
        );
        assert_eq!(addrs[2], "10.0.0.3:9999".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_name_servers_ipv6_with_port() {
        let addrs = parse_name_servers("[::1]:5075");
        assert_eq!(
            addrs,
            vec![SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 5075)]
        );
    }

    #[test]
    fn parse_name_servers_ipv6_without_port() {
        let addrs = parse_name_servers("::1");
        assert_eq!(
            addrs,
            vec![SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 5075)]
        );
    }

    #[test]
    fn decode_search_response_addr_falls_back_to_udp_source_when_unspecified() {
        let src: SocketAddr = "192.168.1.20:5076".parse().unwrap();
        let decoded = decode_search_response_addr([0u8; 16], 5075, src);
        assert_eq!(decoded, "192.168.1.20:5075".parse().unwrap());
    }
}
