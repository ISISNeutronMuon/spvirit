//! High-level PVAccess client — one-liner get, put, monitor, info.
//!
//! # Example
//!
//! ```rust,ignore
//! use spvirit_client::PvaClient;
//!
//! let client = PvaClient::builder().build();
//! let result = client.pvget("MY:PV").await?;
//! client.pvput("MY:PV", 42.0).await?;
//! ```

use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::task::JoinHandle;
use tokio::time::{Instant, interval};

use spvirit_codec::MonitorUpdate;
use spvirit_codec::epics_decode::{PvaPacket, PvaPacketCommand};
use spvirit_codec::spvd_decode::{PvdDecoder, StructureDesc};
use spvirit_codec::spvd_encode::{encode_pv_request, encode_pv_request_with_options};
use spvirit_codec::spvirit_encode::{
    encode_control_message, encode_get_field_request, encode_monitor_request, encode_put_request,
};

use crate::byte_sink::ByteSink;
use crate::client::{ChannelConn, ensure_status_ok, establish_channel, pvget as low_level_pvget};
use crate::put_encode::encode_put_payload;
use crate::search::resolve_pv_server;
use crate::transport::{FrameBuf, read_frame, read_frame_resumable, read_until};
use crate::types::{PvGetError, PvGetResult, PvOptions};

/// PVA protocol version used in headers.
const PVA_VERSION: u8 = 2;
/// QoS / subcommand flag: INIT.
const QOS_INIT: u8 = 0x08;

static NEXT_IOID: AtomicU32 = AtomicU32::new(1);
fn alloc_ioid() -> u32 {
    NEXT_IOID.fetch_add(1, Ordering::Relaxed)
}

/// Build the pvRequest body for a GET / PUT / MONITOR INIT.
///
/// Returns the canonical "all fields" pvRequest (`field()`) when `fields` is
/// empty, otherwise delegates to [`encode_pv_request`] which supports dotted
/// nested paths like `"alarm.severity"`.
fn build_pv_request(fields: &[&str], is_be: bool) -> Vec<u8> {
    if fields.is_empty() {
        // Empty pvRequest \u2014 server returns full descriptor / all fields.
        vec![0xfd, 0x02, 0x00, 0x80, 0x00, 0x00]
    } else {
        encode_pv_request(fields, is_be)
    }
}

/// Options controlling a monitor subscription.
///
/// By default a monitor runs without flow control (the server streams
/// updates as they are produced). Set [`MonitorOptions::pipeline`] to a
/// positive `queueSize` to request PVAccess monitor pipelining: the server
/// will send at most `queueSize` updates before waiting for an `ACK`, and
/// the client automatically replies with ACK messages as it consumes them.
#[derive(Debug, Clone, Copy, Default)]
pub struct MonitorOptions {
    /// Request monitor pipelining with the given initial queue size.
    ///
    /// `None` (or `Some(0)`) disables pipelining.
    pub pipeline: Option<u32>,
}

impl MonitorOptions {
    /// Enable pipelining with the given initial `queueSize`.
    pub fn pipelined(queue_size: u32) -> Self {
        Self {
            pipeline: if queue_size == 0 {
                None
            } else {
                Some(queue_size)
            },
        }
    }
}

// ─── PvaClientBuilder ────────────────────────────────────────────────────────

/// Builder for [`PvaClient`].
///
/// ```rust,ignore
/// let client = PvaClient::builder()
///     .timeout(Duration::from_secs(10))
///     .port(5075)
///     .build();
/// ```
pub struct PvaClientBuilder {
    udp_port: u16,
    tcp_port: u16,
    timeout: Duration,
    no_broadcast: bool,
    name_servers: Vec<SocketAddr>,
    authnz_user: Option<String>,
    authnz_host: Option<String>,
    server_addr: Option<SocketAddr>,
    search_addr: Option<std::net::IpAddr>,
    bind_addr: Option<std::net::IpAddr>,
    debug: bool,
}

impl PvaClientBuilder {
    fn new() -> Self {
        Self {
            udp_port: 5076,
            tcp_port: 5075,
            timeout: Duration::from_secs(5),
            no_broadcast: false,
            name_servers: Vec::new(),
            authnz_user: None,
            authnz_host: None,
            server_addr: None,
            search_addr: None,
            bind_addr: None,
            debug: false,
        }
    }

    /// Set the TCP port (default 5075).
    pub fn port(mut self, port: u16) -> Self {
        self.tcp_port = port;
        self
    }

    /// Set the UDP search port (default 5076).
    pub fn udp_port(mut self, port: u16) -> Self {
        self.udp_port = port;
        self
    }

    /// Set the operation timeout (default 5 s).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Disable UDP broadcast search (use name servers only).
    pub fn no_broadcast(mut self) -> Self {
        self.no_broadcast = true;
        self
    }

    /// Add a PVA name-server address for TCP search.
    pub fn name_server(mut self, addr: SocketAddr) -> Self {
        self.name_servers.push(addr);
        self
    }

    /// Override the authentication user.
    pub fn authnz_user(mut self, user: impl Into<String>) -> Self {
        self.authnz_user = Some(user.into());
        self
    }

    /// Override the authentication host.
    pub fn authnz_host(mut self, host: impl Into<String>) -> Self {
        self.authnz_host = Some(host.into());
        self
    }

    /// Set an explicit server address, bypassing UDP search.
    pub fn server_addr(mut self, addr: SocketAddr) -> Self {
        self.server_addr = Some(addr);
        self
    }

    /// Set the search target IP address.
    pub fn search_addr(mut self, addr: std::net::IpAddr) -> Self {
        self.search_addr = Some(addr);
        self
    }

    /// Set the local bind IP for UDP search.
    pub fn bind_addr(mut self, addr: std::net::IpAddr) -> Self {
        self.bind_addr = Some(addr);
        self
    }

    /// Enable debug logging.
    pub fn debug(mut self) -> Self {
        self.debug = true;
        self
    }

    /// Build the [`PvaClient`].
    pub fn build(self) -> PvaClient {
        PvaClient {
            udp_port: self.udp_port,
            tcp_port: self.tcp_port,
            timeout: self.timeout,
            no_broadcast: self.no_broadcast,
            name_servers: self.name_servers,
            authnz_user: self.authnz_user,
            authnz_host: self.authnz_host,
            server_addr: self.server_addr,
            search_addr: self.search_addr,
            bind_addr: self.bind_addr,
            debug: self.debug,
            byte_sink: None,
        }
    }
}

// ─── PvaClient ───────────────────────────────────────────────────────────────

/// High-level PVAccess client.
///
/// Provides `pvget`, `pvput`, `pvmonitor`, and `pvinfo` methods that hide
/// the underlying protocol handshake.
///
/// ```rust,ignore
/// let client = PvaClient::builder().build();
/// let val = client.pvget("MY:PV").await?;
/// ```
#[derive(Clone)]
pub struct PvaClient {
    udp_port: u16,
    tcp_port: u16,
    timeout: Duration,
    no_broadcast: bool,
    name_servers: Vec<SocketAddr>,
    authnz_user: Option<String>,
    authnz_host: Option<String>,
    server_addr: Option<SocketAddr>,
    search_addr: Option<std::net::IpAddr>,
    bind_addr: Option<std::net::IpAddr>,
    debug: bool,
    /// Optional upstream wire-byte accounting hook (see [`ByteSink`]).
    /// `None` by default; when set, `on_tx`/`on_rx` are called at the
    /// encode-send / recv-decode boundaries with the real PV name, server
    /// host, and wire-byte count.
    byte_sink: Option<Arc<dyn ByteSink>>,
}

// `dyn ByteSink` does not implement `Debug`, so this is written by hand
// rather than derived; it reports only whether a sink is installed.
impl std::fmt::Debug for PvaClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PvaClient")
            .field("udp_port", &self.udp_port)
            .field("tcp_port", &self.tcp_port)
            .field("timeout", &self.timeout)
            .field("no_broadcast", &self.no_broadcast)
            .field("name_servers", &self.name_servers)
            .field("authnz_user", &self.authnz_user)
            .field("authnz_host", &self.authnz_host)
            .field("server_addr", &self.server_addr)
            .field("search_addr", &self.search_addr)
            .field("bind_addr", &self.bind_addr)
            .field("debug", &self.debug)
            .field("byte_sink", &self.byte_sink.is_some())
            .finish()
    }
}

impl PvaClient {
    /// Create a builder for configuring a [`PvaClient`].
    pub fn builder() -> PvaClientBuilder {
        PvaClientBuilder::new()
    }

    /// Install an optional [`ByteSink`] to observe upstream wire bytes.
    ///
    /// `None` by default (the no-op default): no host/name computation and
    /// no trait call happens on the hot path unless a sink is installed.
    pub fn set_byte_sink(&mut self, sink: Arc<dyn ByteSink>) {
        self.byte_sink = Some(sink);
    }

    /// Build [`PvOptions`] for a given PV name, inheriting client-level settings.
    fn opts(&self, pv_name: &str) -> PvOptions {
        let mut o = PvOptions::new(pv_name.to_string());
        o.udp_port = self.udp_port;
        o.tcp_port = self.tcp_port;
        o.timeout = self.timeout;
        o.no_broadcast = self.no_broadcast;
        o.name_servers.clone_from(&self.name_servers);
        o.authnz_user.clone_from(&self.authnz_user);
        o.authnz_host.clone_from(&self.authnz_host);
        o.server_addr = self.server_addr;
        o.search_addr = self.search_addr;
        o.bind_addr = self.bind_addr;
        o.debug = self.debug;
        o
    }

    /// Resolve a PV server and establish a channel, returning the raw connection.
    async fn open_channel(&self, pv_name: &str) -> Result<ChannelConn, PvGetError> {
        let opts = self.opts(pv_name);
        let (target, guid) = resolve_pv_server(&opts).await?;
        establish_channel(target, guid, &opts).await
    }

    // ─── pvget ───────────────────────────────────────────────────────────

    /// Fetch the current value of a PV.
    pub async fn pvget(&self, pv_name: &str) -> Result<PvGetResult, PvGetError> {
        let opts = self.opts(pv_name);
        low_level_pvget(&opts).await
    }

    /// Fetch a PV with field filtering (equivalent to `pvget -r "field(value,alarm)"`).
    pub async fn pvget_fields(
        &self,
        pv_name: &str,
        fields: &[&str],
    ) -> Result<PvGetResult, PvGetError> {
        let opts = self.opts(pv_name);
        crate::client::pvget_fields(&opts, fields).await
    }

    // ─── pvput ───────────────────────────────────────────────────────────

    /// Write a value to a PV.
    ///
    /// Accepts anything convertible to `serde_json::Value`:
    /// ```rust,ignore
    /// client.pvput("MY:PV", 42.0).await?;
    /// client.pvput("MY:PV", "hello").await?;
    /// client.pvput("MY:PV", serde_json::json!({"value": 1.5})).await?;
    /// ```
    pub async fn pvput(&self, pv_name: &str, value: impl Into<Value>) -> Result<(), PvGetError> {
        // Default: PUT only the `value` field (the universal PVA convention).
        // Use [`pvput_fields`](Self::pvput_fields) for richer selections.
        self.pvput_fields(pv_name, value, &["value"]).await
    }

    /// Write to a PV with explicit field selection (dotted paths).
    ///
    /// `fields` is forwarded as the PUT pvRequest. An empty slice is treated
    /// as "all fields" (server returns full descriptor on INIT).
    pub async fn pvput_fields(
        &self,
        pv_name: &str,
        value: impl Into<Value>,
        fields: &[&str],
    ) -> Result<(), PvGetError> {
        let json_val = value.into();
        let ChannelConn {
            mut stream,
            sid,
            version: _,
            is_be,
            mut reassembler,
            server_addr,
            ..
        } = self.open_channel(pv_name).await?;

        let ioid = alloc_ioid();

        // PUT INIT — pvRequest from caller-supplied field paths.
        let pv_request = build_pv_request(fields, is_be);
        let init = encode_put_request(sid, ioid, QOS_INIT, &pv_request, PVA_VERSION, is_be);
        stream.write_all(&init).await?;

        // Read INIT response — extract introspection
        let init_bytes = read_until(&mut stream, self.timeout, &mut reassembler, |cmd| {
            matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && (op.subcmd & 0x08) != 0)
        })
        .await?;

        let desc = decode_init_introspection(&init_bytes, "PUT")?;

        // Encode and send the value
        let payload = encode_put_payload(&desc, &json_val, is_be)
            .map_err(|e| PvGetError::Protocol(format!("put encode: {e}")))?;
        let req = encode_put_request(sid, ioid, 0x00, &payload, PVA_VERSION, is_be);
        stream.write_all(&req).await?;
        if let Some(sink) = &self.byte_sink {
            sink.on_tx(pv_name, &server_addr.to_string(), req.len() as u64);
        }

        // Read PUT response — verify status
        let resp_bytes = read_until(
            &mut stream,
            self.timeout,
            &mut reassembler,
            |cmd| matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && op.subcmd == 0x00),
        )
        .await?;
        ensure_status_ok(&resp_bytes, is_be, "PUT")?;

        Ok(())
    }

    // ─── open_put_channel ────────────────────────────────────────────────

    /// Open a persistent channel for high-rate PUT streaming.
    ///
    /// Resolves the PV, establishes a channel, and completes the PUT INIT
    /// handshake. The returned [`PvaChannel`] is ready for immediate
    /// [`put`](PvaChannel::put) calls.
    pub async fn open_put_channel(&self, pv_name: &str) -> Result<PvaChannel, PvGetError> {
        self.open_put_channel_fields(pv_name, &["value"]).await
    }

    /// Open a persistent PUT channel with explicit field selection.
    ///
    /// An empty `fields` slice requests all fields from the server.
    pub async fn open_put_channel_fields(
        &self,
        pv_name: &str,
        fields: &[&str],
    ) -> Result<PvaChannel, PvGetError> {
        let ChannelConn {
            mut stream,
            sid,
            version,
            is_be,
            mut reassembler,
            ..
        } = self.open_channel(pv_name).await?;

        let ioid = alloc_ioid();

        // PUT INIT
        let pv_request = build_pv_request(fields, is_be);
        let init = encode_put_request(sid, ioid, QOS_INIT, &pv_request, PVA_VERSION, is_be);
        stream.write_all(&init).await?;

        let init_bytes = read_until(&mut stream, self.timeout, &mut reassembler, |cmd| {
            matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && (op.subcmd & 0x08) != 0)
        })
        .await?;

        let desc = decode_init_introspection(&init_bytes, "PUT")?;

        // Split stream; background reader logs PUT errors
        let (mut reader, writer) = stream.into_split();
        let reader_is_be = is_be;
        // The reassembler is created once, outside the loop: a per-iteration
        // one would discard the segments of a message split across frames.
        // It carries over the state established during the INIT handshake.
        let reader_handle = tokio::spawn(async move {
            // The original reader blocked indefinitely; keep that by treating
            // a lapsed poll interval as "nothing yet, read again".
            let poll = Duration::from_secs(3600);
            loop {
                let msg = match read_frame(&mut reader, poll, &mut reassembler).await {
                    Ok(m) => m,
                    Err(PvGetError::Timeout(_)) => continue,
                    Err(_) => break,
                };
                let hdr = spvirit_codec::epics_decode::PvaHeader::new(&msg[..8]);
                let payload = &msg[8..];
                if hdr.command == 11 && !hdr.flags.is_control && payload.len() >= 5 {
                    if let Some(st) =
                        spvirit_codec::epics_decode::decode_status(&payload[5..], reader_is_be).0
                    {
                        if st.code != 0 {
                            let msg = st.message.unwrap_or_else(|| format!("code={}", st.code));
                            eprintln!("PvaChannel put error: {msg}");
                        }
                    }
                }
            }
        });

        Ok(PvaChannel {
            writer,
            sid,
            ioid,
            version,
            is_be,
            put_desc: desc,
            echo_token: 1,
            last_echo: Instant::now(),
            _reader_handle: reader_handle,
        })
    }

    // ─── pvmonitor ───────────────────────────────────────────────────────

    /// Subscribe to a PV and receive live updates via a callback.
    ///
    /// The callback returns [`ControlFlow::Continue`] to keep listening or
    /// [`ControlFlow::Break`] to stop the subscription.
    ///
    /// ```rust,ignore
    /// use std::ops::ControlFlow;
    ///
    /// client.pvmonitor("MY:PV", |update| {
    ///     println!("{:?}", update.value);
    ///     ControlFlow::Continue(())
    /// }).await?;
    /// ```
    pub async fn pvmonitor<F>(&self, pv_name: &str, callback: F) -> Result<(), PvGetError>
    where
        F: FnMut(&MonitorUpdate) -> ControlFlow<()>,
    {
        // Default: subscribe to the entire structure. Use
        // [`pvmonitor_fields`](Self::pvmonitor_fields) for filtered subscriptions.
        self.pvmonitor_fields(pv_name, &[], callback).await
    }

    /// Subscribe to a PV with explicit field selection (dotted paths).
    ///
    /// `fields` is the MONITOR pvRequest. Each entry may be a top-level
    /// field (`"value"`) or a dotted nested path (`"alarm.severity"`). An
    /// empty slice requests all fields.
    pub async fn pvmonitor_fields<F>(
        &self,
        pv_name: &str,
        fields: &[&str],
        callback: F,
    ) -> Result<(), PvGetError>
    where
        F: FnMut(&MonitorUpdate) -> ControlFlow<()>,
    {
        self.pvmonitor_with_options(pv_name, fields, MonitorOptions::default(), callback)
            .await
    }

    /// Subscribe to a PV with explicit field selection and monitor options.
    ///
    /// See [`MonitorOptions`] — in particular, set `pipeline` to request
    /// PVAccess monitor pipelining (flow-controlled delivery with client
    /// ACKs). When pipelining is disabled this behaves identically to
    /// [`pvmonitor_fields`](Self::pvmonitor_fields).
    pub async fn pvmonitor_with_options<F>(
        &self,
        pv_name: &str,
        fields: &[&str],
        options: MonitorOptions,
        mut callback: F,
    ) -> Result<(), PvGetError>
    where
        F: FnMut(&MonitorUpdate) -> ControlFlow<()>,
    {
        let ChannelConn {
            mut stream,
            sid,
            version: _,
            is_be,
            mut reassembler,
            server_addr,
            ..
        } = self.open_channel(pv_name).await?;

        let ioid = alloc_ioid();
        let decoder = PvdDecoder::new(is_be);

        let pipeline_queue = options.pipeline.filter(|&n| n > 0);

        // MONITOR INIT — pvRequest from caller-supplied field paths. If
        // pipelining is enabled, encode `record._options.pipeline=true,
        // queueSize=N` in the pvRequest (for server-side option parsing,
        // e.g. pvxs/Java) and append the queueSize u32 to the INIT body
        // (which the spvirit server reads directly), and set the 0x80
        // pipeline bit on the INIT subcommand.
        let (pv_request, init_subcmd) = if let Some(qsize) = pipeline_queue {
            let qs_str = qsize.to_string();
            let mut body = encode_pv_request_with_options(
                fields,
                &[("pipeline", "true"), ("queueSize", qs_str.as_str())],
                is_be,
            );
            let qs_bytes = if is_be {
                qsize.to_be_bytes()
            } else {
                qsize.to_le_bytes()
            };
            body.extend_from_slice(&qs_bytes);
            (body, QOS_INIT | 0x80)
        } else {
            (build_pv_request(fields, is_be), QOS_INIT)
        };

        let init = encode_monitor_request(sid, ioid, init_subcmd, &pv_request, PVA_VERSION, is_be);
        stream.write_all(&init).await?;

        // Read INIT response — extract introspection
        let init_bytes = read_until(&mut stream, self.timeout, &mut reassembler, |cmd| {
            matches!(cmd, PvaPacketCommand::Op(op) if op.command == 13 && (op.subcmd & 0x08) != 0)
        })
        .await?;

        let field_desc = decode_init_introspection(&init_bytes, "MONITOR")?;

        // Start subscription: START (0x04) | GET (0x40) = 0x44. The pipeline
        // bit 0x80 must NOT be set here — on a non-INIT MONITOR message the
        // 0x80 bit means "ACK with u32 nack body" (see pvxs servermon.cpp).
        // Mixing START with an ACK bit on an empty body would make the
        // server fail to read the u32 and drop the TCP connection.
        let start = encode_monitor_request(sid, ioid, 0x44, &[], PVA_VERSION, is_be);
        stream.write_all(&start).await?;

        // Pipeline credit tracking. `consumed_since_ack` counts updates we
        // have received but not yet acknowledged; when it reaches the ACK
        // threshold (half of queueSize, minimum 1) we send an ACK message
        // to return credits to the server.
        let mut consumed_since_ack: u32 = 0;
        let ack_threshold: u32 = pipeline_queue.map(|q| (q / 2).max(1)).unwrap_or(0);

        // Event loop — with echo keepalive and timeout resilience
        let mut echo_interval = interval(Duration::from_secs(10));
        let mut echo_token: u32 = 1;

        // Persistent partial-read state: the echo branch below drops the read
        // future whenever it wins the `select!`. `read_frame_resumable` keeps
        // any in-progress frame's bytes here so the next read resumes instead
        // of losing them and desyncing the TCP framing.
        let mut frame_buf = FrameBuf::new();

        loop {
            tokio::select! {
                _ = echo_interval.tick() => {
                    let msg = encode_control_message(false, is_be, PVA_VERSION, 3, echo_token);
                    echo_token = echo_token.wrapping_add(1);
                    let _ = stream.write_all(&msg).await;
                }
                res = read_frame_resumable(&mut stream, self.timeout, &mut reassembler, &mut frame_buf) => {
                    let bytes = match res {
                        Ok(b) => b,
                        Err(PvGetError::Timeout(_)) => continue,
                        Err(e) => return Err(e),
                    };
                    if let Some(sink) = &self.byte_sink {
                        sink.on_rx(pv_name, &server_addr.to_string(), bytes.len() as u64);
                    }
                    let mut pkt = PvaPacket::new(&bytes);
                    if let Some(PvaPacketCommand::Op(op)) = pkt.decode_payload() {
                        if op.command == 13 && op.ioid == ioid && op.subcmd == 0x00 {
                            let payload = &bytes[8..]; // skip header
                            let pos = 5; // skip ioid(4) + subcmd(1)
                            // Decode the update, but do NOT gate credit
                            // accounting on a successful decode. Under
                            // pipelining the server charges exactly one credit
                            // for this MONITOR data message whether or not the
                            // client can decode it, so the ACK (credit return)
                            // must run on *every* consume path — decode success
                            // and decode failure alike. Gating the ACK on a
                            // successful decode (as this once did) leaks one
                            // credit per decode failure; once the queue window
                            // drains the server stops sending and the monitor
                            // stalls forever. Only the callback / break
                            // handling is gated on a successful decode.
                            let decoded =
                                decoder.decode_monitor_update(&payload[pos..], &field_desc);

                            if pipeline_queue.is_some() {
                                if let Some(ack) = pipeline_ack_on_consume(
                                    &mut consumed_since_ack,
                                    ack_threshold,
                                    sid,
                                    ioid,
                                    is_be,
                                ) {
                                    if stream.write_all(&ack).await.is_err() {
                                        return Ok(());
                                    }
                                }
                            }

                            if let Ok(update) = decoded {
                                if callback(&update).is_break() {
                                    // Best-effort DESTROY so the server releases
                                    // its per-subscription state promptly.
                                    let destroy = encode_monitor_request(
                                        sid,
                                        ioid,
                                        0x10,
                                        &[],
                                        PVA_VERSION,
                                        is_be,
                                    );
                                    let _ = stream.write_all(&destroy).await;
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ─── pvinfo ──────────────────────────────────────────────────────────

    /// Retrieve the field/structure description (introspection) for a PV.
    pub async fn pvinfo(&self, pv_name: &str) -> Result<StructureDesc, PvGetError> {
        let result = self.pvinfo_full(pv_name).await?;
        Ok(result.0)
    }

    /// Retrieve introspection and server address for a PV.
    pub async fn pvinfo_full(
        &self,
        pv_name: &str,
    ) -> Result<(StructureDesc, SocketAddr, [u8; 12]), PvGetError> {
        let ChannelConn {
            mut stream,
            sid,
            version: _,
            is_be,
            server_addr,
            guid,
            mut reassembler,
        } = self.open_channel(pv_name).await?;

        let ioid = alloc_ioid();
        let msg = encode_get_field_request(sid, ioid, None, PVA_VERSION, is_be);
        stream.write_all(&msg).await?;

        let resp_bytes = read_until(&mut stream, self.timeout, &mut reassembler, |cmd| {
            matches!(cmd, PvaPacketCommand::GetField(_))
        })
        .await?;

        let mut pkt = PvaPacket::new(&resp_bytes);
        let cmd = pkt
            .decode_payload()
            .ok_or_else(|| PvGetError::Decode("GET_FIELD response decode failed".to_string()))?;
        match cmd {
            PvaPacketCommand::GetField(payload) => {
                if let Some(ref st) = payload.status {
                    if st.is_error() {
                        let msg = st
                            .message
                            .clone()
                            .unwrap_or_else(|| format!("code={}", st.code));
                        return Err(PvGetError::Protocol(format!("GET_FIELD error: {msg}")));
                    }
                }
                let desc = payload.introspection.ok_or_else(|| {
                    PvGetError::Decode("missing GET_FIELD introspection".to_string())
                })?;
                Ok((desc, server_addr, guid))
            }
            _ => Err(PvGetError::Protocol(
                "unexpected GET_FIELD response".to_string(),
            )),
        }
    }

    // ─── pvlist ──────────────────────────────────────────────────────────

    /// List PV names served by a specific server (via `__pvlist` GET).
    pub async fn pvlist(&self, server_addr: SocketAddr) -> Result<Vec<String>, PvGetError> {
        let opts = self.opts("__pvlist");
        crate::pvlist::pvlist(&opts, server_addr).await
    }

    /// List PV names with automatic fallback through all strategies.
    ///
    /// Tries: `__pvlist` → GET_FIELD (opt-in) → Server RPC → Server GET.
    pub async fn pvlist_with_fallback(
        &self,
        server_addr: SocketAddr,
    ) -> Result<(Vec<String>, crate::pvlist::PvListSource), PvGetError> {
        let opts = self.opts("__pvlist");
        crate::pvlist::pvlist_with_fallback(&opts, server_addr).await
    }
}

// ─── PvaChannel ──────────────────────────────────────────────────────────────

/// A persistent PVA channel for high-rate streaming PUT operations.
///
/// Created via [`PvaClient::open_put_channel`], this keeps the TCP connection
/// open and reuses the PUT introspection for repeated writes without
/// per-operation handshake overhead.
///
/// # Example
///
/// ```rust,ignore
/// let client = PvaClient::builder().build();
/// let mut channel = client.open_put_channel("MY:PV").await?;
/// for value in 0..100 {
///     channel.put(value as f64).await?;
/// }
/// ```
pub struct PvaChannel {
    writer: OwnedWriteHalf,
    sid: u32,
    ioid: u32,
    version: u8,
    is_be: bool,
    put_desc: StructureDesc,
    echo_token: u32,
    last_echo: Instant,
    _reader_handle: JoinHandle<()>,
}

impl PvaChannel {
    /// Write a value over the persistent channel.
    ///
    /// Automatically sends echo keepalive pings when more than 10 seconds
    /// have elapsed since the last one.
    pub async fn put(&mut self, value: impl Into<Value>) -> Result<(), PvGetError> {
        // Echo keepalive
        if self.last_echo.elapsed() >= Duration::from_secs(10) {
            let msg = encode_control_message(false, self.is_be, self.version, 3, self.echo_token);
            self.echo_token = self.echo_token.wrapping_add(1);
            let _ = self.writer.write_all(&msg).await;
            self.last_echo = Instant::now();
        }

        let json_val = value.into();
        let payload = encode_put_payload(&self.put_desc, &json_val, self.is_be)
            .map_err(|e| PvGetError::Protocol(format!("put encode: {e}")))?;
        let req = encode_put_request(
            self.sid,
            self.ioid,
            0x00,
            &payload,
            self.version,
            self.is_be,
        );
        self.writer.write_all(&req).await?;
        Ok(())
    }

    /// Returns the PUT introspection for this channel.
    pub fn introspection(&self) -> &StructureDesc {
        &self.put_desc
    }
}

impl Drop for PvaChannel {
    fn drop(&mut self) {
        self._reader_handle.abort();
    }
}

// ─── Standalone convenience functions ────────────────────────────────────────

/// Write a value to a PV (one-shot).
///
/// ```rust,ignore
/// use spvirit_client::{pvput, PvOptions};
///
/// pvput(&PvOptions::new("MY:PV".into()), 42.0).await?;
/// ```
pub async fn pvput(opts: &PvOptions, value: impl Into<Value>) -> Result<(), PvGetError> {
    let client = client_from_opts(opts);
    client.pvput(&opts.pv_name, value).await
}

/// Subscribe to a PV and receive live updates (one-shot).
///
/// The callback returns [`ControlFlow::Continue`] to keep listening or
/// [`ControlFlow::Break`] to stop. Subscribes to the full structure;
/// see [`pvmonitor_fields`] for filtered subscriptions.
pub async fn pvmonitor<F>(opts: &PvOptions, callback: F) -> Result<(), PvGetError>
where
    F: FnMut(&MonitorUpdate) -> ControlFlow<()>,
{
    let client = client_from_opts(opts);
    client.pvmonitor(&opts.pv_name, callback).await
}

/// Subscribe to a PV with explicit field selection (dotted paths).
pub async fn pvmonitor_fields<F>(
    opts: &PvOptions,
    fields: &[&str],
    callback: F,
) -> Result<(), PvGetError>
where
    F: FnMut(&MonitorUpdate) -> ControlFlow<()>,
{
    let client = client_from_opts(opts);
    client
        .pvmonitor_fields(&opts.pv_name, fields, callback)
        .await
}

/// Write a value to a PV with explicit field selection (one-shot).
pub async fn pvput_fields(
    opts: &PvOptions,
    value: impl Into<Value>,
    fields: &[&str],
) -> Result<(), PvGetError> {
    let client = client_from_opts(opts);
    client.pvput_fields(&opts.pv_name, value, fields).await
}

/// Retrieve the field/structure description for a PV (one-shot).
pub async fn pvinfo(opts: &PvOptions) -> Result<StructureDesc, PvGetError> {
    let client = client_from_opts(opts);
    client.pvinfo(&opts.pv_name).await
}

// ─── Internal helpers ────────────────────────────────────────────────────────

/// Build a PvaClient inheriting configuration from PvOptions.
pub fn client_from_opts(opts: &PvOptions) -> PvaClient {
    let mut b = PvaClient::builder()
        .port(opts.tcp_port)
        .udp_port(opts.udp_port)
        .timeout(opts.timeout);
    if opts.no_broadcast {
        b = b.no_broadcast();
    }
    for ns in &opts.name_servers {
        b = b.name_server(*ns);
    }
    if let Some(ref u) = opts.authnz_user {
        b = b.authnz_user(u.clone());
    }
    if let Some(ref h) = opts.authnz_host {
        b = b.authnz_host(h.clone());
    }
    if let Some(addr) = opts.server_addr {
        b = b.server_addr(addr);
    }
    if let Some(addr) = opts.search_addr {
        b = b.search_addr(addr);
    }
    if let Some(addr) = opts.bind_addr {
        b = b.bind_addr(addr);
    }
    if opts.debug {
        b = b.debug();
    }
    b.build()
}

/// Decode an INIT response to extract the introspection StructureDesc.
pub fn decode_init_introspection(raw: &[u8], label: &str) -> Result<StructureDesc, PvGetError> {
    let mut pkt = PvaPacket::new(raw);
    let cmd = pkt
        .decode_payload()
        .ok_or_else(|| PvGetError::Decode(format!("{label} init response decode failed")))?;

    match cmd {
        PvaPacketCommand::Op(op) => {
            if let Some(ref st) = op.status {
                if st.is_error() {
                    let msg = st
                        .message
                        .clone()
                        .unwrap_or_else(|| format!("code={}", st.code));
                    return Err(PvGetError::Protocol(format!("{label} init error: {msg}")));
                }
            }
            op.introspection
                .ok_or_else(|| PvGetError::Decode(format!("missing {label} introspection")))
        }
        _ => Err(PvGetError::Protocol(format!(
            "unexpected {label} init response"
        ))),
    }
}

/// Pipeline ACK-credit accounting for one consumed MONITOR data message.
///
/// Increments the count of updates consumed since the last ACK and, once that
/// count reaches `ack_threshold`, returns the encoded ACK message to send
/// (returning the accumulated credit to the server) and resets the counter.
/// Returns `None` when no ACK is due yet.
///
/// This MUST be called once for *every* consumed server MONITOR data message —
/// including updates that fail to decode — so a decode failure can never leak a
/// pipeline credit and permanently stall the subscription. Keeping the
/// accounting in one place (rather than inline inside the decode-success guard)
/// makes that invariant hard to break; see the
/// `pipeline_ack_returns_credit_even_when_updates_fail_to_decode` test.
///
/// Callers only invoke this when pipelining is enabled, so `ack_threshold` is
/// always `>= 1` (it is `(queueSize / 2).max(1)`); an ACK is therefore never
/// emitted for a zero consume count.
fn pipeline_ack_on_consume(
    consumed_since_ack: &mut u32,
    ack_threshold: u32,
    sid: u32,
    ioid: u32,
    is_be: bool,
) -> Option<Vec<u8>> {
    *consumed_since_ack = consumed_since_ack.saturating_add(1);
    if *consumed_since_ack >= ack_threshold {
        let n = *consumed_since_ack;
        let ack_bytes = if is_be {
            n.to_be_bytes()
        } else {
            n.to_le_bytes()
        };
        let ack = encode_monitor_request(sid, ioid, 0x80, &ack_bytes, PVA_VERSION, is_be);
        *consumed_since_ack = 0;
        Some(ack)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the pipelined-monitor ACK-credit leak.
    ///
    /// Under monitor pipelining the server grants a bounded window of credits
    /// and stops sending once the window drains, resuming only when the client
    /// ACKs the updates it consumed. The bug: credit accounting used to live
    /// *inside* the `if let Ok(update)` decode-success guard, so any update
    /// that failed to decode consumed a server credit but never returned it —
    /// after `queueSize` decode failures the window is empty, no ACK has been
    /// sent, and the monitor stalls forever.
    ///
    /// [`pipeline_ack_on_consume`] is now called on every consume path,
    /// decode success or failure. This test drives many more "consumed but
    /// undecodable" updates than the credit window and asserts that credit
    /// keeps being returned to the wire (ACKs are emitted and the total credit
    /// returned equals the number consumed), so the stream never stalls. With
    /// the old decode-gated accounting this loop would emit zero ACKs.
    #[test]
    fn pipeline_ack_returns_credit_even_when_updates_fail_to_decode() {
        let queue_size: u32 = 8;
        // Mirror the client's threshold: half the window, at least 1.
        let ack_threshold: u32 = (queue_size / 2).max(1);
        let sid = 7u32;
        let ioid = 42u32;
        let is_be = false;

        // Decode `queue_size` full windows' worth of updates, all of which
        // "fail to decode" (we never call a callback — we only account the
        // consume, exactly as the monitor loop does on a decode error).
        let total_consumed = queue_size * 4;
        let mut consumed_since_ack: u32 = 0;
        let mut acks_sent = 0u32;
        let mut credit_returned: u32 = 0;

        for _ in 0..total_consumed {
            if let Some(ack) =
                pipeline_ack_on_consume(&mut consumed_since_ack, ack_threshold, sid, ioid, is_be)
            {
                acks_sent += 1;
                // The ACK body is a u32 credit count at the tail of the
                // op-request payload: 8-byte header + sid(4) + ioid(4) +
                // subcmd(1), then the 4-byte count.
                let count_off = 8 + 4 + 4 + 1;
                let count = u32::from_le_bytes(
                    ack[count_off..count_off + 4].try_into().expect("ack u32"),
                );
                credit_returned += count;
            }
        }

        // Credit must keep flowing: at least one ACK per window consumed.
        assert!(
            acks_sent >= total_consumed / ack_threshold,
            "expected at least {} ACKs, got {acks_sent}",
            total_consumed / ack_threshold
        );
        // Every consumed credit that reached a threshold boundary is returned;
        // only the sub-threshold remainder stays unacked (and is < threshold,
        // so it never starves a window >= 2*threshold).
        let remainder = consumed_since_ack;
        assert!(remainder < ack_threshold, "remainder must be below threshold");
        assert_eq!(
            credit_returned + remainder,
            total_consumed,
            "all consumed credit is either returned or below the ACK threshold"
        );
    }

    #[test]
    fn builder_defaults() {
        let c = PvaClient::builder().build();
        assert_eq!(c.tcp_port, 5075);
        assert_eq!(c.udp_port, 5076);
        assert_eq!(c.timeout, Duration::from_secs(5));
        assert!(!c.no_broadcast);
        assert!(c.name_servers.is_empty());
    }

    #[test]
    fn builder_overrides() {
        let c = PvaClient::builder()
            .port(9075)
            .udp_port(9076)
            .timeout(Duration::from_secs(10))
            .no_broadcast()
            .name_server("127.0.0.1:5075".parse().unwrap())
            .authnz_user("testuser")
            .authnz_host("testhost")
            .build();
        assert_eq!(c.tcp_port, 9075);
        assert_eq!(c.udp_port, 9076);
        assert_eq!(c.timeout, Duration::from_secs(10));
        assert!(c.no_broadcast);
        assert_eq!(c.name_servers.len(), 1);
        assert_eq!(c.authnz_user.as_deref(), Some("testuser"));
        assert_eq!(c.authnz_host.as_deref(), Some("testhost"));
    }

    #[test]
    fn opts_inherits_client_config() {
        let c = PvaClient::builder()
            .port(9075)
            .udp_port(9076)
            .timeout(Duration::from_secs(10))
            .no_broadcast()
            .build();
        let o = c.opts("TEST:PV");
        assert_eq!(o.pv_name, "TEST:PV");
        assert_eq!(o.tcp_port, 9075);
        assert_eq!(o.udp_port, 9076);
        assert_eq!(o.timeout, Duration::from_secs(10));
        assert!(o.no_broadcast);
    }

    #[test]
    fn client_from_opts_roundtrip() {
        let mut opts = PvOptions::new("X:Y".into());
        opts.tcp_port = 8075;
        opts.udp_port = 8076;
        opts.timeout = Duration::from_secs(3);
        opts.no_broadcast = true;
        let c = client_from_opts(&opts);
        assert_eq!(c.tcp_port, 8075);
        assert_eq!(c.udp_port, 8076);
        assert!(c.no_broadcast);
    }

    #[test]
    fn pv_get_options_alias_works() {
        // PvGetOptions is a type alias for PvOptions — verify it compiles and works
        let opts: crate::types::PvGetOptions = PvOptions::new("ALIAS:TEST".into());
        assert_eq!(opts.pv_name, "ALIAS:TEST");
    }

    // ─── ByteSink accounting ───────────────────────────────────────────

    /// Recording [`ByteSink`] used to assert exact tx/rx calls below.
    #[derive(Default)]
    struct RecordingSink {
        tx: std::sync::Mutex<Vec<(String, String, u64)>>,
        rx: std::sync::Mutex<Vec<(String, String, u64)>>,
    }

    impl crate::byte_sink::ByteSink for RecordingSink {
        fn on_tx(&self, pv: &str, host: &str, n: u64) {
            self.tx
                .lock()
                .unwrap()
                .push((pv.to_string(), host.to_string(), n));
        }
        fn on_rx(&self, pv: &str, host: &str, n: u64) {
            self.rx
                .lock()
                .unwrap()
                .push((pv.to_string(), host.to_string(), n));
        }
    }

    /// A `ByteSink` installed on a `PvaClient` observes the real PV name,
    /// the real server host, and the exact wire-byte length for both a PUT
    /// (encode-send boundary in `pvput_fields`) and a MONITOR update
    /// (recv-decode boundary in `pvmonitor_with_options`).
    ///
    /// Drives a genuine in-process `spvirit-server` `PvaServer` end to end —
    /// not a mock of the call sites — so this exercises the real
    /// `stream.write_all` / `read_frame_resumable` chokepoints.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn byte_sink_records_real_pv_host_and_wire_bytes_for_put_and_monitor() {
        use spvirit_server::pva_server::PvaServer;
        use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket as StdUdpSocket};
        use std::sync::Arc;

        const PV: &str = "BYTESINK:TEST";

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

        let tcp_port = free_tcp_port();
        let udp_port = free_udp_port();

        let server = PvaServer::builder()
            .ao(PV, 1.0)
            .port(tcp_port)
            .udp_port(udp_port)
            .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .build();
        tokio::spawn(async move {
            let _ = server.run().await;
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        let server_addr: SocketAddr = format!("127.0.0.1:{tcp_port}")
            .parse()
            .expect("server addr parse");

        let recorder = Arc::new(RecordingSink::default());
        let mut client = PvaClient::builder()
            .port(tcp_port)
            .udp_port(udp_port)
            .timeout(Duration::from_secs(5))
            .server_addr(server_addr)
            .build();
        client.set_byte_sink(recorder.clone());

        // PUT — exercises the encode-send boundary in `pvput_fields`.
        client.pvput(PV, 42.0).await.expect("pvput");

        // MONITOR — exercises the recv-decode boundary in
        // `pvmonitor_with_options`; break after the first update.
        let monitor = client.pvmonitor(PV, move |_update| ControlFlow::Break(()));
        tokio::time::timeout(Duration::from_secs(5), monitor)
            .await
            .expect("monitor timed out")
            .expect("monitor error");

        let tx = recorder.tx.lock().unwrap();
        assert!(!tx.is_empty(), "expected at least one on_tx call");
        for (pv, host, n) in tx.iter() {
            assert_eq!(pv, PV, "on_tx must report the real PV name");
            assert_eq!(
                host,
                &server_addr.to_string(),
                "on_tx must report the real server host"
            );
            assert!(*n > 0, "on_tx must report a nonzero wire-byte length");
        }

        let rx = recorder.rx.lock().unwrap();
        assert!(!rx.is_empty(), "expected at least one on_rx call");
        for (pv, host, n) in rx.iter() {
            assert_eq!(pv, PV, "on_rx must report the real PV name");
            assert_eq!(
                host,
                &server_addr.to_string(),
                "on_rx must report the real server host"
            );
            assert!(*n > 0, "on_rx must report a nonzero wire-byte length");
        }
    }

    /// With no sink installed, PUT/MONITOR behave exactly as before — the
    /// `Option<Arc<dyn ByteSink>>` guard is a pure no-op. Mirrors the sink
    /// test above but proves the default (no sink) path is unaffected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_byte_sink_is_a_pure_noop() {
        use spvirit_server::pva_server::PvaServer;
        use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket as StdUdpSocket};

        const PV: &str = "BYTESINK:NOOP";

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

        let tcp_port = free_tcp_port();
        let udp_port = free_udp_port();

        let server = PvaServer::builder()
            .ao(PV, 1.0)
            .port(tcp_port)
            .udp_port(udp_port)
            .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .build();
        tokio::spawn(async move {
            let _ = server.run().await;
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        let server_addr: SocketAddr = format!("127.0.0.1:{tcp_port}")
            .parse()
            .expect("server addr parse");

        // No `set_byte_sink` call — `byte_sink` stays `None`.
        let client = PvaClient::builder()
            .port(tcp_port)
            .udp_port(udp_port)
            .timeout(Duration::from_secs(5))
            .server_addr(server_addr)
            .build();

        client.pvput(PV, 7.0).await.expect("pvput");

        let monitor = client.pvmonitor(PV, move |_update| ControlFlow::Break(()));
        tokio::time::timeout(Duration::from_secs(5), monitor)
            .await
            .expect("monitor timed out")
            .expect("monitor error");
    }
}
