/// Standalone PVA server serving a single NtNdArray PV with a rotating
/// wireframe dodecahedron rendered as a grayscale image.
///
/// Usage:
///   pvdodeca [--pv NAME] [--width N] [--height N] [--rate HZ]
///            [--tcp-port PORT] [--udp-port PORT] [--debug]
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use argparse::{ArgumentParser, Store, StoreTrue};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info};

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvd_encode::nt_payload_desc;
use spvirit_codec::spvirit_encode::{
    encode_beacon, encode_connection_validated, encode_connection_validation,
    encode_control_message, encode_create_channel_error, encode_create_channel_response,
    encode_destroy_channel_response, encode_monitor_data_response_payload, encode_op_error,
    encode_op_get_data_response_payload, encode_op_init_response_desc, encode_op_put_response,
    encode_search_response, ip_to_bytes,
};
use spvirit_server::conn_writer::ConnWriter;
use spvirit_server::handler::{rand_guid, search_reply_target};
use spvirit_types::{
    NdCodec, NdDimension, NtAlarm, NtNdArray, NtPayload, NtScalar, NtTimeStamp, ScalarArrayValue,
    ScalarValue,
};

// ---------------------------------------------------------------------------
// Dodecahedron geometry
// ---------------------------------------------------------------------------

/// Regular dodecahedron has 20 vertices defined via the golden ratio.
fn dodecahedron_vertices() -> Vec<[f64; 3]> {
    let phi: f64 = (1.0 + 5.0_f64.sqrt()) / 2.0;
    let inv = 1.0 / phi;
    let mut verts = Vec::with_capacity(20);
    // 8 cube vertices (±1, ±1, ±1)
    for &sx in &[-1.0, 1.0] {
        for &sy in &[-1.0, 1.0] {
            for &sz in &[-1.0, 1.0] {
                verts.push([sx, sy, sz]);
            }
        }
    }
    // 4 vertices (0, ±1/φ, ±φ)
    for &sa in &[-1.0, 1.0] {
        for &sb in &[-1.0, 1.0] {
            verts.push([0.0, sa * inv, sb * phi]);
        }
    }
    // 4 vertices (±1/φ, ±φ, 0)
    for &sa in &[-1.0, 1.0] {
        for &sb in &[-1.0, 1.0] {
            verts.push([sa * inv, sb * phi, 0.0]);
        }
    }
    // 4 vertices (±φ, 0, ±1/φ)
    for &sa in &[-1.0, 1.0] {
        for &sb in &[-1.0, 1.0] {
            verts.push([sa * phi, 0.0, sb * inv]);
        }
    }
    verts
}

/// Compute edge list: two vertices are connected if their Euclidean distance
/// equals 2/φ (the edge length of a unit dodecahedron).
fn dodecahedron_edges(verts: &[[f64; 3]]) -> Vec<(usize, usize)> {
    let phi: f64 = (1.0 + 5.0_f64.sqrt()) / 2.0;
    let edge_len = 2.0 / phi;
    let tol = 0.1;
    let mut edges = Vec::new();
    for i in 0..verts.len() {
        for j in (i + 1)..verts.len() {
            let dx = verts[i][0] - verts[j][0];
            let dy = verts[i][1] - verts[j][1];
            let dz = verts[i][2] - verts[j][2];
            let dist = (dx * dx + dy * dy + dz * dz).sqrt();
            if (dist - edge_len).abs() < tol {
                edges.push((i, j));
            }
        }
    }
    edges
}

// ---------------------------------------------------------------------------
// 3D rotation & projection
// ---------------------------------------------------------------------------

fn rotate_y(v: [f64; 3], angle: f64) -> [f64; 3] {
    let (s, c) = angle.sin_cos();
    [c * v[0] + s * v[2], v[1], -s * v[0] + c * v[2]]
}

fn rotate_x(v: [f64; 3], angle: f64) -> [f64; 3] {
    let (s, c) = angle.sin_cos();
    [v[0], c * v[1] - s * v[2], s * v[1] + c * v[2]]
}

fn rotate_z(v: [f64; 3], angle: f64) -> [f64; 3] {
    let (s, c) = angle.sin_cos();
    [c * v[0] - s * v[1], s * v[0] + c * v[1], v[2]]
}

/// Perspective project a 3D point to 2D screen coords.
fn project(v: [f64; 3], w: usize, h: usize) -> (f64, f64) {
    let cam_z = 5.0;
    let fov = 2.5;
    let scale = fov / (cam_z + v[2]);
    let cx = w as f64 / 2.0;
    let cy = h as f64 / 2.0;
    let size = w.min(h) as f64 / 2.0;
    (cx + v[0] * scale * size, cy - v[1] * scale * size)
}

// ---------------------------------------------------------------------------
// Rasterization
// ---------------------------------------------------------------------------

/// Bresenham line drawing with anti‑aliased width.
fn draw_line(buf: &mut [u8], w: usize, h: usize, x0: f64, y0: f64, x1: f64, y1: f64, val: u8) {
    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();
    let steps = dx.max(dy).ceil() as usize;
    if steps == 0 {
        return;
    }
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let x = (x0 + t * (x1 - x0)).round() as isize;
        let y = (y0 + t * (y1 - y0)).round() as isize;
        if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
            let idx = y as usize * w + x as usize;
            // apply max so overlapping edges stay bright
            buf[idx] = buf[idx].max(val);
        }
    }
}

/// Render the dodecahedron wireframe at the given rotation angles into a
/// grayscale u8 buffer of size width × height.
fn render_dodecahedron(width: usize, height: usize, angle_y: f64, angle_x: f64) -> Vec<u8> {
    let verts = dodecahedron_vertices();
    let edges = dodecahedron_edges(&verts);

    let mut buf = vec![0u8; width * height];

    // Transform vertices.
    let projected: Vec<(f64, f64)> = verts
        .iter()
        .map(|&v| {
            let v = rotate_y(v, angle_y);
            let v = rotate_x(v, angle_x);
            let v = rotate_z(v, angle_y * 0.3);
            project(v, width, height)
        })
        .collect();

    // Draw edges.
    for &(i, j) in &edges {
        let (x0, y0) = projected[i];
        let (x1, y1) = projected[j];
        draw_line(&mut buf, width, height, x0, y0, x1, y1, 255);
    }

    // Draw brighter vertex dots (3×3).
    for &(px, py) in &projected {
        for dy in -1..=1_isize {
            for dx in -1..=1_isize {
                let x = (px.round() as isize + dx) as usize;
                let y = (py.round() as isize + dy) as usize;
                if x < width && y < height {
                    buf[y * width + x] = 255;
                }
            }
        }
    }

    buf
}

/// Like `render_dodecahedron` but with an independent Z-axis angle.
fn render_dodecahedron_3axis(
    width: usize,
    height: usize,
    angle_y: f64,
    angle_x: f64,
    angle_z: f64,
) -> Vec<u8> {
    let verts = dodecahedron_vertices();
    let edges = dodecahedron_edges(&verts);

    let mut buf = vec![0u8; width * height];

    let projected: Vec<(f64, f64)> = verts
        .iter()
        .map(|&v| {
            let v = rotate_y(v, angle_y);
            let v = rotate_x(v, angle_x);
            let v = rotate_z(v, angle_z);
            project(v, width, height)
        })
        .collect();

    for &(i, j) in &edges {
        let (x0, y0) = projected[i];
        let (x1, y1) = projected[j];
        draw_line(&mut buf, width, height, x0, y0, x1, y1, 255);
    }

    for &(px, py) in &projected {
        for dy in -1..=1_isize {
            for dx in -1..=1_isize {
                let x = (px.round() as isize + dx) as usize;
                let y = (py.round() as isize + dy) as usize;
                if x < width && y < height {
                    buf[y * width + x] = 255;
                }
            }
        }
    }

    buf
}

// ---------------------------------------------------------------------------
// NtNdArray construction
// ---------------------------------------------------------------------------

fn make_ndarray(width: usize, height: usize, pixels: Vec<u8>, unique_id: i32) -> NtNdArray {
    let total = (width * height) as i64;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    NtNdArray {
        value: ScalarArrayValue::U8(pixels),
        codec: NdCodec {
            name: String::new(),
            parameters: HashMap::new(),
        },
        compressed_size: total,
        uncompressed_size: total,
        dimension: vec![
            NdDimension {
                size: width as i32,
                offset: 0,
                full_size: width as i32,
                binning: 1,
                reverse: false,
            },
            NdDimension {
                size: height as i32,
                offset: 0,
                full_size: height as i32,
                binning: 1,
                reverse: false,
            },
        ],
        unique_id,
        data_time_stamp: NtTimeStamp {
            seconds_past_epoch: now.as_secs() as i64,
            nanoseconds: now.subsec_nanos() as i32,
            user_tag: 0,
        },
        attribute: Vec::new(),
        descriptor: Some("dodecahedron".to_string()),
        alarm: Some(NtAlarm::default()),
        time_stamp: Some(NtTimeStamp {
            seconds_past_epoch: now.as_secs() as i64,
            nanoseconds: now.subsec_nanos() as i32,
            user_tag: 0,
        }),
        display: None,
    }
}

// ---------------------------------------------------------------------------
// Shared server state
// ---------------------------------------------------------------------------

struct ServerState {
    pv_name: String,
    /// All PV names this server serves (image + scalar control PVs).
    all_pvs: Vec<String>,
    current: RwLock<NtNdArray>,
    /// Spin speeds in rad/s for X, Y, Z axes.
    speed_x: RwLock<f64>,
    speed_y: RwLock<f64>,
    speed_z: RwLock<f64>,
    monitors: Mutex<Vec<MonitorSub>>,
    conns: Mutex<HashMap<u64, Arc<ConnWriter>>>,
    sid_counter: AtomicU32,
    conn_counter: AtomicU64,
    beacon_change: AtomicU16,
}

use std::sync::atomic::AtomicU64;

#[derive(Debug, Clone)]
struct MonitorSub {
    conn_id: u64,
    ioid: u32,
    sid: u32,
    pv_name: String,
    version: u8,
    is_be: bool,
}

impl ServerState {
    fn new(pv_name: String, initial: NtNdArray) -> Self {
        let prefix = pv_name.split(':').next().unwrap_or("DODECA").to_string();
        let all_pvs = vec![
            pv_name.clone(),
            format!("{}:SPEED_X", prefix),
            format!("{}:SPEED_Y", prefix),
            format!("{}:SPEED_Z", prefix),
        ];
        Self {
            pv_name,
            all_pvs,
            current: RwLock::new(initial),
            speed_x: RwLock::new(0.3),
            speed_y: RwLock::new(0.5),
            speed_z: RwLock::new(0.15),
            monitors: Mutex::new(Vec::new()),
            conns: Mutex::new(HashMap::new()),
            sid_counter: AtomicU32::new(1),
            conn_counter: AtomicU64::new(1),
            beacon_change: AtomicU16::new(0),
        }
    }

    fn is_known_pv(&self, name: &str) -> bool {
        self.all_pvs.iter().any(|p| p == name)
    }

    fn is_speed_pv(&self, name: &str) -> bool {
        name.ends_with(":SPEED_X") || name.ends_with(":SPEED_Y") || name.ends_with(":SPEED_Z")
    }

    async fn get_speed(&self, pv_name: &str) -> Option<f64> {
        if pv_name.ends_with(":SPEED_X") {
            Some(*self.speed_x.read().await)
        } else if pv_name.ends_with(":SPEED_Y") {
            Some(*self.speed_y.read().await)
        } else if pv_name.ends_with(":SPEED_Z") {
            Some(*self.speed_z.read().await)
        } else {
            None
        }
    }

    async fn set_speed(&self, pv_name: &str, val: f64) -> bool {
        if pv_name.ends_with(":SPEED_X") {
            *self.speed_x.write().await = val;
            true
        } else if pv_name.ends_with(":SPEED_Y") {
            *self.speed_y.write().await = val;
            true
        } else if pv_name.ends_with(":SPEED_Z") {
            *self.speed_z.write().await = val;
            true
        } else {
            false
        }
    }

    async fn make_speed_scalar(&self, pv_name: &str) -> NtScalar {
        let val = self.get_speed(pv_name).await.unwrap_or(0.0);
        NtScalar::from_value(ScalarValue::F64(val))
            .with_units("rad/s".to_string())
            .with_description(format!("{} rotation speed", pv_name))
    }

    async fn send_msg(&self, conn_id: u64, msg: Vec<u8>) {
        let cw = {
            let conns = self.conns.lock().await;
            conns.get(&conn_id).cloned()
        };
        if let Some(cw) = cw {
            cw.send_control(msg).await;
        }
    }
}

// ---------------------------------------------------------------------------
// UDP search responder
// ---------------------------------------------------------------------------

async fn run_udp_search(
    state: Arc<ServerState>,
    addr: SocketAddr,
    tcp_port: u16,
    guid: [u8; 12],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
        if let PvaPacketCommand::Search(payload) = cmd {
            let accepts_tcp = payload.protocols.is_empty()
                || payload
                    .protocols
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case("tcp"));
            if !accepts_tcp {
                continue;
            }
            let mut cids = Vec::new();
            for (cid, name) in &payload.pv_requests {
                if state.is_known_pv(name) {
                    cids.push(*cid);
                }
            }
            let server_discovery_ping = payload.pv_requests.is_empty();
            let found = server_discovery_ping || !cids.is_empty();
            let response_required = (payload.mask & 0x01) != 0;
            if !found && !response_required {
                continue;
            }
            let resp_ip = infer_response_ip(addr.ip(), peer);
            let addr_bytes = ip_to_bytes(resp_ip);
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
            let _ = socket.send_to(&response, reply_target).await;
            debug!(
                "UDP search: responded found={} matches={} to {}",
                found,
                cids.len(),
                reply_target
            );
        }
    }
}

fn infer_response_ip(listen: IpAddr, peer: SocketAddr) -> IpAddr {
    if !listen.is_unspecified() {
        return listen;
    }
    let bind_addr = if peer.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    if let Ok(sock) = std::net::UdpSocket::bind(bind_addr) {
        if sock.connect(peer).is_ok() {
            if let Ok(local) = sock.local_addr() {
                if !local.ip().is_unspecified() {
                    return local.ip();
                }
            }
        }
    }
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

// ---------------------------------------------------------------------------
// TCP connection handler
// ---------------------------------------------------------------------------

async fn handle_connection(
    state: Arc<ServerState>,
    stream: TcpStream,
    conn_id: u64,
    conn_timeout: Duration,
) {
    let (mut reader, writer) = stream.into_split();

    {
        let mut conns = state.conns.lock().await;
        conns.insert(conn_id, ConnWriter::new(writer));
    }

    // PVA handshake: SET_BYTE_ORDER then CONNECTION_VALIDATION.
    let set_byte_order = encode_control_message(true, false, 2, 2, 0);
    state.send_msg(conn_id, set_byte_order).await;

    let server_validation =
        encode_connection_validation(16_384, 512, &["anonymous", "ca"], 2, false);
    state.send_msg(conn_id, server_validation).await;

    let mut last_activity = Instant::now();
    let mut conn_state = ConnState::default();

    loop {
        let mut header = [0u8; 8];
        let elapsed = last_activity.elapsed();
        if elapsed >= conn_timeout {
            debug!("Conn {} idle timeout", conn_id);
            break;
        }
        let remaining = conn_timeout - elapsed;
        match tokio::time::timeout(remaining, reader.read_exact(&mut header)).await {
            Ok(Ok(_)) => {}
            _ => break,
        }
        let header_pkt = PvaPacket::new(&header);
        let payload_len = if header_pkt.header.flags.is_control {
            0usize
        } else {
            header_pkt.header.payload_length as usize
        };
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            match tokio::time::timeout(
                conn_timeout.saturating_sub(last_activity.elapsed()),
                reader.read_exact(&mut payload),
            )
            .await
            {
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        last_activity = Instant::now();

        let mut full = header.to_vec();
        full.extend_from_slice(&payload);
        let mut pkt = PvaPacket::new(&full);
        let Some(cmd) = pkt.decode_payload() else {
            continue;
        };
        let version = pkt.header.version;
        let is_be = pkt.header.flags.is_msb;
        let cmd_code = pkt.header.command;
        let _payload_slice = if full.len() >= 8 { &full[8..] } else { &[] };

        // Connection validation (cmd=1): respond with CONNECTION_VALIDATED.
        if cmd_code == 1 {
            let resp = encode_connection_validated(true, version, is_be);
            state.send_msg(conn_id, resp).await;
            continue;
        }

        match cmd {
            PvaPacketCommand::Control(ctrl) => {
                // Echo request (cmd=3) → echo response (cmd=4).
                if ctrl.command == 3 {
                    let resp = encode_control_message(true, is_be, version, 4, ctrl.data);
                    state.send_msg(conn_id, resp).await;
                }
            }
            PvaPacketCommand::CreateChannel(ch) => {
                for (cid, pv_name) in ch.channels {
                    if state.is_known_pv(&pv_name) {
                        let sid = state.sid_counter.fetch_add(1, Ordering::SeqCst);
                        conn_state.cid_to_sid.insert(cid, sid);
                        conn_state.sid_to_pv.insert(sid, pv_name.clone());
                        let resp = encode_create_channel_response(cid, sid, version, is_be);
                        state.send_msg(conn_id, resp).await;
                        info!(
                            "Conn {}: channel '{}' cid={} sid={}",
                            conn_id, pv_name, cid, sid
                        );
                    } else {
                        let resp = encode_create_channel_error(cid, "PV not found", version, is_be);
                        state.send_msg(conn_id, resp).await;
                    }
                }
            }
            PvaPacketCommand::Op(op) => {
                if op.is_server {
                    continue;
                }
                let sid = op.sid_or_cid;
                let ioid = op.ioid;
                let is_init = (op.subcmd & 0x08) != 0;

                let Some(pv_name) = conn_state.sid_to_pv.get(&sid).cloned() else {
                    state
                        .send_msg(
                            conn_id,
                            encode_op_error(
                                op.command,
                                op.subcmd,
                                ioid,
                                "Unknown SID",
                                version,
                                is_be,
                            ),
                        )
                        .await;
                    continue;
                };

                match op.command {
                    10 => {
                        // GET
                        let nt = if state.is_speed_pv(&pv_name) {
                            NtPayload::Scalar(state.make_speed_scalar(&pv_name).await)
                        } else {
                            let current = state.current.read().await;
                            NtPayload::NdArray(current.clone())
                        };
                        if is_init {
                            let desc = nt_payload_desc(&nt);
                            conn_state.ioid_to_desc.insert(ioid, desc.clone());
                            conn_state.ioid_to_pv.insert(ioid, pv_name.clone());
                            let resp = encode_op_init_response_desc(
                                op.command, ioid, 0x08, &desc, version, is_be,
                            );
                            debug!("GET INIT resp len={}", resp.len());
                            state.send_msg(conn_id, resp).await;
                        } else {
                            let resp =
                                encode_op_get_data_response_payload(ioid, &nt, version, is_be);
                            debug!(
                                "GET DATA resp len={} first_40={:02x?}",
                                resp.len(),
                                &resp[..std::cmp::min(40, resp.len())]
                            );
                            state.send_msg(conn_id, resp).await;
                        }
                    }
                    11 => {
                        // PUT (scalar speed PVs only)
                        if !state.is_speed_pv(&pv_name) {
                            state
                                .send_msg(
                                    conn_id,
                                    encode_op_error(
                                        op.command,
                                        op.subcmd,
                                        ioid,
                                        "PUT not supported on this PV",
                                        version,
                                        is_be,
                                    ),
                                )
                                .await;
                            continue;
                        }
                        if is_init {
                            let nt = NtPayload::Scalar(state.make_speed_scalar(&pv_name).await);
                            let desc = nt_payload_desc(&nt);
                            conn_state.ioid_to_desc.insert(ioid, desc.clone());
                            conn_state.ioid_to_pv.insert(ioid, pv_name.clone());
                            let resp = encode_op_init_response_desc(
                                op.command, ioid, 0x08, &desc, version, is_be,
                            );
                            state.send_msg(conn_id, resp).await;
                            info!("Conn {}: put init pv='{}' ioid={}", conn_id, pv_name, ioid);
                        } else {
                            // Decode the put value
                            if let Some(new_val) = decode_put_value(&op.body, is_be) {
                                state.set_speed(&pv_name, new_val).await;
                                info!("Conn {}: put pv='{}' = {}", conn_id, pv_name, new_val);
                                // Notify speed monitors
                                notify_speed_monitors(&state, &pv_name).await;
                            }
                            let resp = encode_op_put_response(ioid, op.subcmd, version, is_be);
                            state.send_msg(conn_id, resp).await;
                        }
                    }
                    13 => {
                        // MONITOR
                        if is_init {
                            let nt = if state.is_speed_pv(&pv_name) {
                                NtPayload::Scalar(state.make_speed_scalar(&pv_name).await)
                            } else {
                                let current = state.current.read().await;
                                NtPayload::NdArray(current.clone())
                            };
                            let desc = nt_payload_desc(&nt);
                            conn_state.ioid_to_desc.insert(ioid, desc.clone());
                            conn_state.ioid_to_pv.insert(ioid, pv_name.clone());
                            let resp = encode_op_init_response_desc(
                                op.command, ioid, 0x08, &desc, version, is_be,
                            );
                            state.send_msg(conn_id, resp).await;
                            let mut monitors = state.monitors.lock().await;
                            monitors.push(MonitorSub {
                                conn_id,
                                ioid,
                                sid,
                                pv_name: pv_name.clone(),
                                version,
                                is_be,
                            });
                            conn_state.ioid_to_sid.insert(ioid, sid);
                            info!(
                                "Conn {}: monitor init ioid={} sid={} pv={}",
                                conn_id, ioid, sid, pv_name
                            );
                        } else if (op.subcmd & 0x10) != 0 {
                            // MONITOR stop/destroy
                            let nt = if state.is_speed_pv(&pv_name) {
                                NtPayload::Scalar(state.make_speed_scalar(&pv_name).await)
                            } else {
                                let current = state.current.read().await;
                                NtPayload::NdArray(current.clone())
                            };
                            let resp = encode_monitor_data_response_payload(
                                ioid, 0x10, &nt, version, is_be,
                            );
                            state.send_msg(conn_id, resp).await;
                            let mut monitors = state.monitors.lock().await;
                            monitors.retain(|m| !(m.conn_id == conn_id && m.ioid == ioid));
                            info!("Conn {}: monitor end ioid={}", conn_id, ioid);
                        } else if (op.subcmd & 0x04) != 0 {
                            // MONITOR start: send initial snapshot.
                            let nt = if state.is_speed_pv(&pv_name) {
                                NtPayload::Scalar(state.make_speed_scalar(&pv_name).await)
                            } else {
                                let current = state.current.read().await;
                                NtPayload::NdArray(current.clone())
                            };
                            let resp = encode_monitor_data_response_payload(
                                ioid, 0x00, &nt, version, is_be,
                            );
                            state.send_msg(conn_id, resp).await;
                        }
                    }
                    _ => {
                        state
                            .send_msg(
                                conn_id,
                                encode_op_error(
                                    op.command,
                                    op.subcmd,
                                    ioid,
                                    "Unsupported operation",
                                    version,
                                    is_be,
                                ),
                            )
                            .await;
                    }
                }
            }
            PvaPacketCommand::DestroyChannel(dc) => {
                // Remove all monitors for this connection + SID
                {
                    let mut monitors = state.monitors.lock().await;
                    monitors.retain(|m| !(m.conn_id == conn_id && m.sid == dc.sid));
                }
                // Clean up IOID mappings for this SID
                let ioids: Vec<u32> = conn_state
                    .ioid_to_sid
                    .iter()
                    .filter(|(_, s)| **s == dc.sid)
                    .map(|(i, _)| *i)
                    .collect();
                for ioid in &ioids {
                    conn_state.ioid_to_desc.remove(ioid);
                    conn_state.ioid_to_pv.remove(ioid);
                    conn_state.ioid_to_sid.remove(ioid);
                }
                conn_state.cid_to_sid.remove(&dc.cid);
                conn_state.sid_to_pv.remove(&dc.sid);
                // Send destroy channel response
                let resp = encode_destroy_channel_response(dc.sid, dc.cid, version, is_be);
                state.send_msg(conn_id, resp).await;
                info!(
                    "Conn {}: destroy channel cid={} sid={}",
                    conn_id, dc.cid, dc.sid
                );
            }
            _ => {}
        }
    }

    // Cleanup: remove monitors and connection.
    {
        let mut monitors = state.monitors.lock().await;
        monitors.retain(|m| m.conn_id != conn_id);
    }
    {
        let mut conns = state.conns.lock().await;
        conns.remove(&conn_id);
    }
    info!("Conn {} closed", conn_id);
}

/// Minimal per-connection state.
#[derive(Default)]
struct ConnState {
    cid_to_sid: HashMap<u32, u32>,
    sid_to_pv: HashMap<u32, String>,
    ioid_to_desc: HashMap<u32, spvirit_codec::spvd_decode::StructureDesc>,
    ioid_to_pv: HashMap<u32, String>,
    ioid_to_sid: HashMap<u32, u32>,
}

// ---------------------------------------------------------------------------
// PUT decode helper — extract f64 value from a PUT body
// ---------------------------------------------------------------------------

fn decode_put_value(body: &[u8], is_be: bool) -> Option<f64> {
    // Try several strategies to find the double value in the PUT body.
    // Strategy 1: bitset-prefixed decode (standard PVA PUT body format)
    //   Body layout: bitset_size(1) + bitset_bytes + encoded_values
    //   The bitset tells which fields are present.
    //   For NtScalar, field 0 is the value.
    if body.len() >= 10 {
        // Try: 1-byte bitset-size, then bitset, then f64 value
        let bs_size = body[0] as usize;
        if bs_size > 0 && bs_size < body.len() {
            let data_start = 1 + bs_size;
            if data_start + 8 <= body.len() {
                let bytes: [u8; 8] = body[data_start..data_start + 8].try_into().ok()?;
                let val = if is_be {
                    f64::from_be_bytes(bytes)
                } else {
                    f64::from_le_bytes(bytes)
                };
                if val.is_finite() {
                    return Some(val);
                }
            }
        }
    }

    // Strategy 2: skip first byte (status 0xFF) then try bitset
    if body.len() >= 11 && body[0] == 0xFF {
        let bs_size = body[1] as usize;
        if bs_size > 0 {
            let data_start = 2 + bs_size;
            if data_start + 8 <= body.len() {
                let bytes: [u8; 8] = body[data_start..data_start + 8].try_into().ok()?;
                let val = if is_be {
                    f64::from_be_bytes(bytes)
                } else {
                    f64::from_le_bytes(bytes)
                };
                if val.is_finite() {
                    return Some(val);
                }
            }
        }
    }

    // Strategy 3: last 8 bytes as f64 (value-only fallback)
    if body.len() >= 8 {
        let start = body.len() - 8;
        let bytes: [u8; 8] = body[start..start + 8].try_into().ok()?;
        let val = if is_be {
            f64::from_be_bytes(bytes)
        } else {
            f64::from_le_bytes(bytes)
        };
        if val.is_finite() {
            return Some(val);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Notify speed monitors when a speed PV changes
// ---------------------------------------------------------------------------

async fn notify_speed_monitors(state: &Arc<ServerState>, pv_name: &str) {
    let nt = NtPayload::Scalar(state.make_speed_scalar(pv_name).await);
    let subs = {
        let monitors = state.monitors.lock().await;
        monitors
            .iter()
            .filter(|m| m.pv_name == pv_name)
            .cloned()
            .collect::<Vec<_>>()
    };
    for sub in &subs {
        let resp =
            encode_monitor_data_response_payload(sub.ioid, 0x00, &nt, sub.version, sub.is_be);
        state.send_msg(sub.conn_id, resp).await;
    }
}

// ---------------------------------------------------------------------------
// Image update task — rotates the dodecahedron and notifies monitors
// ---------------------------------------------------------------------------

async fn run_image_updater(state: Arc<ServerState>, width: usize, height: usize, rate_hz: f64) {
    let tick_dur = Duration::from_secs_f64(1.0 / rate_hz);
    let mut interval = tokio::time::interval(tick_dur);
    let start = tokio::time::Instant::now();
    let mut frame_id: i32 = 0;

    let mut angle_x: f64 = 0.0;
    let mut angle_y: f64 = 0.0;
    let mut angle_z: f64 = 0.0;
    let mut last_t = 0.0_f64;

    loop {
        interval.tick().await;
        let t = start.elapsed().as_secs_f64();
        let dt = t - last_t;
        last_t = t;

        let sx = *state.speed_x.read().await;
        let sy = *state.speed_y.read().await;
        let sz = *state.speed_z.read().await;
        angle_x += sx * dt;
        angle_y += sy * dt;
        angle_z += sz * dt;

        let pixels = render_dodecahedron_3axis(width, height, angle_y, angle_x, angle_z);
        let nt = make_ndarray(width, height, pixels, frame_id);
        frame_id = frame_id.wrapping_add(1);

        // Update shared state.
        {
            let mut current = state.current.write().await;
            *current = nt;
        }

        // Fan out to image monitors only (speed monitors are notified on PUT).
        let subs = {
            let monitors = state.monitors.lock().await;
            monitors
                .iter()
                .filter(|m| m.pv_name == state.pv_name)
                .cloned()
                .collect::<Vec<_>>()
        };
        if subs.is_empty() {
            continue;
        }
        let payload = {
            let current = state.current.read().await;
            NtPayload::NdArray(current.clone())
        };
        // Resolve the ConnWriter for each subscriber under a single lock,
        // then hand each its encoded frame. ConnWriter's monitor lane
        // coalesces latest-per-ioid and drops on dead sockets, so slow
        // clients never stall the update loop.
        let targets: Vec<(Arc<ConnWriter>, u32, u8, bool)> = {
            let conns = state.conns.lock().await;
            subs.iter()
                .filter_map(|sub| {
                    conns
                        .get(&sub.conn_id)
                        .cloned()
                        .map(|cw| (cw, sub.ioid, sub.version, sub.is_be))
                })
                .collect()
        };
        for (cw, ioid, version, is_be) in targets {
            let resp = encode_monitor_data_response_payload(ioid, 0x00, &payload, version, is_be);
            cw.send_monitor(ioid, resp).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Beacon broadcast task
// ---------------------------------------------------------------------------

async fn run_beacon(
    state: Arc<ServerState>,
    udp_port: u16,
    tcp_port: u16,
    guid: [u8; 12],
    listen_ip: IpAddr,
) {
    let dest = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), udp_port);
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let Ok(socket) = UdpSocket::bind(bind).await else {
        error!("Failed to bind beacon socket");
        return;
    };
    let _ = socket.set_broadcast(true);
    let mut seq: u8 = 0;
    // Fast beacons initially, then slow down.
    let mut beacon_interval = Duration::from_millis(100);
    let max_interval = Duration::from_secs(15);

    loop {
        tokio::time::sleep(beacon_interval).await;
        let change = state.beacon_change.load(Ordering::Relaxed);
        let addr_bytes = ip_to_bytes(listen_ip);
        let msg = encode_beacon(guid, seq, change, addr_bytes, tcp_port, "tcp", 2, false);
        let _ = socket.send_to(&msg, dest).await;
        seq = seq.wrapping_add(1);
        if beacon_interval < max_interval {
            beacon_interval = (beacon_interval * 2).min(max_interval);
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut pv_name = "DODECA:IMAGE".to_string();
    let mut width: usize = 256;
    let mut height: usize = 256;
    let mut rate_hz: f64 = 10.0;
    let mut tcp_port: u16 = 5075;
    let mut udp_port: u16 = 5076;
    let mut conn_timeout_secs: u64 = 60;
    let mut listen_addr = "0.0.0.0".to_string();
    let mut debug = false;

    {
        let mut ap = ArgumentParser::new();
        ap.set_description(
            "Serve a rotating dodecahedron wireframe as an NtNdArray PV via PVAccess",
        );
        ap.refer(&mut pv_name)
            .add_option(&["--pv"], Store, "PV name (default: DODECA:IMAGE)");
        ap.refer(&mut width).add_option(
            &["--width"],
            Store,
            "Image width in pixels (default: 256)",
        );
        ap.refer(&mut height).add_option(
            &["--height"],
            Store,
            "Image height in pixels (default: 256)",
        );
        ap.refer(&mut rate_hz).add_option(
            &["--rate"],
            Store,
            "Frame update rate in Hz (default: 10)",
        );
        ap.refer(&mut tcp_port).add_option(
            &["--tcp-port"],
            Store,
            "TCP server port (default: 5075)",
        );
        ap.refer(&mut udp_port).add_option(
            &["--udp-port"],
            Store,
            "UDP search port (default: 5076)",
        );
        ap.refer(&mut conn_timeout_secs).add_option(
            &["--conn-timeout"],
            Store,
            "Connection idle timeout in seconds (default: 60)",
        );
        ap.refer(&mut listen_addr).add_option(
            &["--listen-addr"],
            Store,
            "Listen address (default: 0.0.0.0)",
        );
        ap.refer(&mut debug)
            .add_option(&["--debug"], StoreTrue, "Enable debug logging");
        ap.parse_args_or_exit();
    }

    let max_level = if debug {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };
    tracing_subscriber::fmt().with_max_level(max_level).init();

    let initial_pixels = render_dodecahedron(width, height, 0.0, 0.0);
    let initial_nt = make_ndarray(width, height, initial_pixels, 0);
    let state = Arc::new(ServerState::new(pv_name.clone(), initial_nt));

    let guid = rand_guid();
    let listen_ip: IpAddr = listen_addr.parse().expect("invalid --listen-addr");
    let conn_timeout = Duration::from_secs(conn_timeout_secs);

    info!(
        "pvdodeca: serving '{}' {}x{} @ {} Hz on tcp={} udp={}",
        pv_name, width, height, rate_hz, tcp_port, udp_port
    );

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let udp_state = state.clone();
        let udp_addr = SocketAddr::new(listen_ip, udp_port);
        tokio::spawn(async move {
            if let Err(e) = run_udp_search(udp_state, udp_addr, tcp_port, guid).await {
                error!("UDP search error: {}", e);
            }
        });

        let beacon_state = state.clone();
        tokio::spawn(async move {
            run_beacon(beacon_state, udp_port, tcp_port, guid, listen_ip).await;
        });

        let updater_state = state.clone();
        tokio::spawn(async move {
            run_image_updater(updater_state, width, height, rate_hz).await;
        });

        let tcp_addr = SocketAddr::new(listen_ip, tcp_port);
        let listener = TcpListener::bind(tcp_addr).await.expect("TCP bind failed");
        info!("Listening on {}", tcp_addr);

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    error!("TCP accept error: {}", e);
                    continue;
                }
            };
            info!("New connection from {}", peer);
            let conn_id = state.conn_counter.fetch_add(1, Ordering::SeqCst);
            let conn_state = state.clone();
            tokio::spawn(async move {
                handle_connection(conn_state, stream, conn_id, conn_timeout).await;
            });
        }
    });

    Ok(())
}
