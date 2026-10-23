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
    encode_control_message, encode_destroy_request, encode_get_field_request,
    encode_monitor_request, encode_put_request,
};

use crate::byte_sink::ByteSink;
use crate::client::{ChannelConn, ensure_status_ok, establish_channel};
use crate::put_encode::encode_put_payload;
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

/// Borrow the decodable body of a MONITOR data message addressed to
/// `expect_ioid`, or `None` for anything else.
///
/// The monitor loop previously built a `PvaPacket` (which copies the whole
/// frame) and then a `PvaOpPayload` (which copies the body again), read three
/// scalars out of them, threw both away, and decoded from the original
/// slice. All three scalars live at fixed offsets, so this reads them in
/// place: zero allocations per update.
///
/// PVA header layout (`PvaHeader::try_new`,
/// spvirit-codec/src/epics_decode.rs:142-160): magic, version, flags, command,
/// payload_length(4). Flags bit 0 set means a control frame (see
/// `SegmentReassembler::push`, spvirit-codec/src/segment.rs:66-68).
/// A server MONITOR body is ioid(4) then subcmd(1) then the update.
fn monitor_data_body(bytes: &[u8], expect_ioid: u32, is_be: bool) -> Option<&[u8]> {
    const HDR: usize = 8;
    const IOID_LEN: usize = 4;
    const SUBCMD_LEN: usize = 1;

    if bytes.len() < HDR + IOID_LEN + SUBCMD_LEN {
        return None;
    }
    if bytes[2] & 0x01 != 0 {
        return None; // control frame
    }
    if bytes[3] != 13 {
        return None; // not MONITOR
    }

    let body = &bytes[HDR..];
    let raw_ioid: [u8; 4] = body[..IOID_LEN].try_into().ok()?;
    let ioid = if is_be {
        u32::from_be_bytes(raw_ioid)
    } else {
        u32::from_le_bytes(raw_ioid)
    };
    if ioid != expect_ioid {
        return None;
    }
    if body[IOID_LEN] != 0x00 {
        return None; // not a data update (INIT replies set 0x08, etc.)
    }
    Some(&body[IOID_LEN + SUBCMD_LEN..])
}

/// Write one ECHO control frame and advance the token.
///
/// Returns the write error rather than swallowing it: a socket that cannot
/// accept an 8-byte keepalive is not going to deliver updates either, so the
/// caller ends the subscription.
async fn write_echo<W>(stream: &mut W, token: &mut u32, is_be: bool) -> Result<(), PvGetError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let msg = encode_control_message(false, is_be, PVA_VERSION, 3, *token);
    *token = token.wrapping_add(1);
    stream.write_all(&msg).await?;
    Ok(())
}

/// The monitor loop's 10 s keepalive interval.
///
/// Default `Burst` fires one catch-up tick per missed period, so any stall
/// longer than 10 s emits a burst of ECHOs at once (prior audit item C6).
/// `Delay` emits one and re-bases the schedule. Extracted so the behaviour is
/// testable without driving a whole subscription.
fn new_echo_interval() -> tokio::time::Interval {
    let mut i = interval(Duration::from_secs(10));
    i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    i
}

/// Marks the point at which an operation stops being safe to re-run, and
/// whether its connection may go back into the pool.
///
/// GET and GET_FIELD never call [`OpCtx::commit`]: they have no server-side
/// effect, so they are retryable for their whole duration. PUT calls it
/// immediately before writing the value, because after that write the server
/// may have applied the value even if it never replies.
///
/// An operation calls [`OpCtx::reusable`] only once the connection is at a
/// frame boundary *and* the server holds no per-request state for it (it sent
/// DESTROY_REQUEST, or the request never created any). Without that mark the
/// connection is closed rather than pooled.
#[derive(Debug, Default, Clone)]
pub(crate) struct OpCtx {
    committed: Arc<std::sync::atomic::AtomicBool>,
    reusable: Arc<std::sync::atomic::AtomicBool>,
}

impl OpCtx {
    pub(crate) fn commit(&self) {
        self.committed.store(true, Ordering::Relaxed);
    }
    fn is_committed(&self) -> bool {
        self.committed.load(Ordering::Relaxed)
    }
    pub(crate) fn reusable(&self) {
        self.reusable.store(true, Ordering::Relaxed);
    }
    fn is_reusable(&self) -> bool {
        self.reusable.load(Ordering::Relaxed)
    }
}

/// Errors worth one retry on a fresh connection when they hit a *reused*
/// one: the socket is dead or stalled (`Io`, `Timeout` — EOF is an `Io`), or
/// the server no longer knows the channel's sid. Every server answers an
/// operation on an unknown sid with an error; spvirit's text is
/// "Unknown SID".
fn is_reuse_failure(e: &PvGetError) -> bool {
    match e {
        PvGetError::Io(_) | PvGetError::Timeout(_) => true,
        PvGetError::Protocol(m) | PvGetError::Decode(m) => m.contains("Unknown SID"),
        _ => false,
    }
}

/// A server status reply refusing a channel create or an operation's INIT
/// ("PV not found", "Write access denied", ...). The reply was read whole, so
/// the connection is at a frame boundary, and a refused INIT leaves no
/// per-request state on the server: the connection is healthy and is kept.
/// The markers are the ones `create_channel_on`, `run_get_on`,
/// `decode_init_introspection` and `pvinfo_full` build from the status.
fn is_server_refusal(e: &PvGetError) -> bool {
    match e {
        PvGetError::Protocol(m) | PvGetError::Decode(m) => [
            "create_channel error:",
            "GET init error:",
            "PUT init error:",
            "GET_FIELD error:",
        ]
        .iter()
        .any(|p| m.contains(p)),
        _ => false,
    }
}

/// Free the server's per-request state for `ioid` (the spvirit server keeps a
/// descriptor and a binding row per GET/PUT until told otherwise) and, if the
/// write went through, mark the connection reusable. A failed write leaves it
/// unmarked, so it is closed instead of pooled.
async fn destroy_request(conn: &mut crate::cache::PooledConn, sid: u32, ioid: u32, ctx: &OpCtx) {
    let msg = encode_destroy_request(sid, ioid, conn.conn.version, conn.conn.is_be);
    if conn.conn.stream.write_all(&msg).await.is_ok() {
        ctx.reusable();
    }
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
            cache: Arc::new(crate::cache::ClientCache::new(
                crate::cache::CacheConfig::default(),
            )),
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
    /// Shared resolution cache and connection pool. Behind an `Arc` so every
    /// clone of this client warms and reads one cache — which is what makes
    /// the gateway's `Arc<PvaClient>` per upstream effective.
    cache: Arc<crate::cache::ClientCache>,
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
            .field("cache", &self.cache.stats())
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

    /// Replace this client's cache with a freshly configured one.
    ///
    /// Consumes and returns `self` so it composes with the builder. Call it
    /// before the client is cloned or shared: it installs a *new* cache, so
    /// existing clones keep the old one.
    pub fn with_cache_config(mut self, cfg: crate::cache::CacheConfig) -> Self {
        self.cache = Arc::new(crate::cache::ClientCache::new(cfg));
        self
    }

    /// A snapshot of this client's cache counters.
    pub fn cache_stats(&self) -> crate::cache::CacheStats {
        self.cache.stats()
    }

    /// Forget the cached resolution for `pv_name`, forcing the next
    /// operation to search again. Call this when a PV is known to have
    /// moved (an IOC restart, a retired gateway binding).
    pub fn invalidate_pv(&self, pv_name: &str) {
        self.cache.invalidate(pv_name);
    }

    /// Test hook: drop every idle pooled connection to the address `pv_name`
    /// currently resolves to, *without* clearing the resolution. Used to
    /// simulate a server that closed an idle connection.
    #[cfg(test)]
    pub(crate) fn close_pooled_for_test(&self, pv_name: &str) {
        self.cache.close_pooled_for_test(pv_name);
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

    /// Resolve a PV server (through the cache) and establish a channel,
    /// returning the raw connection.
    ///
    /// Long-lived callers (`pvmonitor`, `PvaChannel`) use this: they get the
    /// cached *resolution* but always dial their own connection, so a
    /// subscription can never occupy a pooled connection indefinitely.
    async fn open_channel(&self, pv_name: &str) -> Result<ChannelConn, PvGetError> {
        let opts = self.opts(pv_name);
        let resolved = self.cache.resolve(&opts).await?;
        match establish_channel(resolved.addr, resolved.guid, &opts).await {
            Ok(conn) => Ok(conn),
            Err(e) => {
                // The cached address did not yield a channel: either it is
                // gone or the PV moved. Either way the resolution is stale.
                self.cache.invalidate(pv_name);
                Err(e)
            }
        }
    }

    /// Run one short operation over a pooled connection to `pv_name`'s
    /// server, retrying exactly once on a fresh connection when a *reused*
    /// connection fails before the operation committed.
    ///
    /// `op` receives the connection, the server-side sid for `pv_name`, and
    /// an [`OpCtx`]; it returns the connection back (so it can be pooled) plus
    /// its result.
    ///
    /// A reused connection can fail because its socket is dead (the server
    /// closed an idle connection) *or* because the sid it has cached for
    /// `pv_name` is stale (the server destroyed that channel). Both surface as
    /// an `Err` from `op` before any commit ([`is_reuse_failure`]), and both
    /// take the same branch: the connection is discarded, never checked back
    /// in, and the operation re-runs on a freshly dialled connection that
    /// creates its own channel.
    ///
    /// Any other error is deterministic — a server refusal, a local encode
    /// failure — and would fail identically on a fresh connection, so it is
    /// returned at once. The connection is kept when it is known healthy
    /// ([`is_server_refusal`] or [`OpCtx::reusable`]) and closed otherwise.
    async fn with_pooled_channel<T, F, Fut>(&self, pv_name: &str, op: F) -> Result<T, PvGetError>
    where
        F: Fn(crate::cache::PooledConn, u32, OpCtx) -> Fut,
        Fut: std::future::Future<Output = (crate::cache::PooledConn, OpCtx, Result<T, PvGetError>)>,
    {
        let opts = self.opts(pv_name);
        let resolved = self.cache.resolve(&opts).await?;

        for attempt in 0..2 {
            // The retry dials rather than drawing from the pool again: the
            // other idle connections to this server may be just as dead.
            let acquired = if attempt == 0 {
                self.cache.acquire(&opts, resolved).await
            } else {
                self.cache.acquire_fresh(&opts, resolved).await
            };
            let mut conn = match acquired {
                Ok(c) => c,
                Err(e) => {
                    // The cached address will not accept a connection: the
                    // server is gone, so the resolution is stale too.
                    self.cache.invalidate_at(pv_name, resolved.addr);
                    return Err(e);
                }
            };
            let reused = conn.was_reused();

            let sid = match conn.channel_sid(pv_name, self.timeout).await {
                Ok(sid) => sid,
                Err(e) => {
                    let retryable = is_reuse_failure(&e);
                    if !retryable && is_server_refusal(&e) {
                        // The server answered and refused the channel: the
                        // connection itself is fine.
                        self.cache.release(conn);
                    } else {
                        drop(conn);
                    }
                    if retryable && reused && attempt == 0 {
                        crate::cache::bump_retried_dead(self.cache.counters());
                        continue;
                    }
                    // The address accepted a connection and a handshake, so
                    // only this PV's resolution is suspect; the other
                    // connections pooled for that server stay.
                    self.cache.invalidate(pv_name);
                    return Err(e);
                }
            };

            let ctx = OpCtx::default();
            let (conn, ctx, result) = op(conn, sid, ctx).await;
            match result {
                Ok(v) => {
                    if ctx.is_reusable() {
                        self.cache.release(conn);
                    }
                    return Ok(v);
                }
                Err(e) if is_reuse_failure(&e) => {
                    drop(conn);
                    if reused && attempt == 0 && !ctx.is_committed() {
                        crate::cache::bump_retried_dead(self.cache.counters());
                        continue;
                    }
                    return Err(e);
                }
                Err(e) => {
                    let mut conn = conn;
                    if is_server_refusal(&e) {
                        // The connection is healthy, but the refusal may be a
                        // stale sid worded in a way `is_reuse_failure` does
                        // not recognise. Forget the channel so the next op
                        // re-creates it; a genuine refusal (access denied)
                        // then costs one extra CREATE_CHANNEL.
                        conn.forget_channel(pv_name);
                        self.cache.release(conn);
                    } else if ctx.is_reusable() {
                        self.cache.release(conn);
                    }
                    return Err(e);
                }
            }
        }

        unreachable!("the loop returns on both attempts")
    }

    // ─── pvget ───────────────────────────────────────────────────────────

    /// Fetch the current value of a PV.
    pub async fn pvget(&self, pv_name: &str) -> Result<PvGetResult, PvGetError> {
        self.pvget_fields(pv_name, &[]).await
    }

    /// Fetch a PV with field filtering (equivalent to `pvget -r "field(value,alarm)"`).
    pub async fn pvget_fields(
        &self,
        pv_name: &str,
        fields: &[&str],
    ) -> Result<PvGetResult, PvGetError> {
        let timeout_dur = self.timeout;
        let owned: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        let result = self
            .with_pooled_channel(pv_name, move |mut conn, sid, ctx| {
                let owned = owned.clone();
                let pv_name = pv_name.to_string();
                async move {
                    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
                    let ioid = alloc_ioid();
                    // GET has no server-side effect: never `ctx.commit()`.
                    let r = crate::client::run_get_on(
                        &mut conn.conn,
                        sid,
                        &pv_name,
                        &refs,
                        ioid,
                        timeout_dur,
                    )
                    .await;
                    if r.is_ok() {
                        destroy_request(&mut conn, sid, ioid, &ctx).await;
                    }
                    (conn, ctx, r)
                }
            })
            .await?;
        if let Some(sink) = &self.byte_sink {
            sink.on_rx(pv_name, &result.server_host, result.raw_pva.len() as u64);
        }
        Ok(result)
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
        let timeout_dur = self.timeout;
        let owned: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        let sink = self.byte_sink.clone();
        let pv_owned = pv_name.to_string();

        self.with_pooled_channel(pv_name, move |mut conn, sid, ctx| {
            let json_val = json_val.clone();
            let owned = owned.clone();
            let sink = sink.clone();
            let pv_owned = pv_owned.clone();
            let ctx2 = ctx.clone();
            async move {
                let r = async {
                    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
                    let ioid = alloc_ioid();
                    let is_be = conn.conn.is_be;

                    let pv_request = build_pv_request(&refs, is_be);
                    let init =
                        encode_put_request(sid, ioid, QOS_INIT, &pv_request, PVA_VERSION, is_be);
                    conn.conn.stream.write_all(&init).await?;

                    // `op.ioid == ioid` added for the pool: this socket may
                    // carry replies from a previous operation.
                    let init_bytes = read_until(
                        &mut conn.conn.stream,
                        timeout_dur,
                        &mut conn.conn.reassembler,
                        |cmd| matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && op.ioid == ioid && (op.subcmd & 0x08) != 0),
                    )
                    .await?;
                    let desc = decode_init_introspection(&init_bytes, "PUT")?;

                    let payload = match encode_put_payload(&desc, &json_val, is_be) {
                        Ok(p) => p,
                        Err(e) => {
                            // A local failure: nothing was written past the
                            // INIT, whose server state is freed here.
                            destroy_request(&mut conn, sid, ioid, &ctx2).await;
                            return Err(PvGetError::Protocol(format!("put encode: {e}")));
                        }
                    };
                    let req = encode_put_request(sid, ioid, 0x00, &payload, PVA_VERSION, is_be);

                    // POINT OF NO RETURN: after this write the server may have
                    // applied the value even if it never replies, so the
                    // operation must never be re-run on a fresh connection.
                    ctx2.commit();
                    conn.conn.stream.write_all(&req).await?;

                    if let Some(sink) = &sink {
                        sink.on_tx(
                            &pv_owned,
                            &conn.conn.server_addr.to_string(),
                            req.len() as u64,
                        );
                    }

                    let resp_bytes = read_until(
                        &mut conn.conn.stream,
                        timeout_dur,
                        &mut conn.conn.reassembler,
                        |cmd| matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && op.ioid == ioid && op.subcmd == 0x00),
                    )
                    .await?;
                    // The reply was read whole either way, so free the
                    // request before reporting its status.
                    let status = ensure_status_ok(&resp_bytes, is_be, "PUT");
                    destroy_request(&mut conn, sid, ioid, &ctx2).await;
                    status
                }
                .await;
                (conn, ctx, r)
            }
        })
        .await
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

        // Formatted once per subscription rather than once per update: the
        // address cannot change for the life of this connection, and the
        // gateway always installs a ByteSink so this ran on every update.
        // `None` when no sink is installed, so an unsinked client pays
        // nothing at all.
        let sink_host: Option<String> = self.byte_sink.as_ref().map(|_| server_addr.to_string());

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
        let mut echo_interval = new_echo_interval();
        let mut echo_token: u32 = 1;

        // Persistent partial-read state: the echo branch below drops the read
        // future whenever it wins the `select!`. `read_frame_resumable` keeps
        // any in-progress frame's bytes here so the next read resumes instead
        // of losing them and desyncing the TCP framing.
        let mut frame_buf = FrameBuf::new();

        loop {
            tokio::select! {
                _ = echo_interval.tick() => {
                    write_echo(&mut stream, &mut echo_token, is_be).await?;
                }
                res = read_frame_resumable(&mut stream, self.timeout, &mut reassembler, &mut frame_buf) => {
                    let bytes = match res {
                        Ok(b) => b,
                        Err(PvGetError::Timeout(_)) => continue,
                        Err(e) => return Err(e),
                    };
                    if let (Some(sink), Some(host)) = (&self.byte_sink, &sink_host) {
                        sink.on_rx(pv_name, host, bytes.len() as u64);
                    }
                    let Some(update_body) = monitor_data_body(&bytes, ioid, is_be) else {
                        continue;
                    };
                    // Decode the update, but do NOT gate credit accounting on
                    // a successful decode. Under pipelining the server charges
                    // exactly one credit for this MONITOR data message whether
                    // or not the client can decode it, so the ACK (credit
                    // return) must run on *every* consume path — decode
                    // success and decode failure alike. Gating the ACK on a
                    // successful decode (as this once did) leaks one credit
                    // per decode failure; once the queue window drains the
                    // server stops sending and the monitor stalls forever.
                    // Only the callback / break handling is gated on a
                    // successful decode.
                    let decoded = decoder.decode_monitor_update(update_body, &field_desc);

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
        let timeout_dur = self.timeout;
        self.with_pooled_channel(pv_name, move |mut conn, sid, ctx| async move {
            let r = async {
                let ioid = alloc_ioid();
                let is_be = conn.conn.is_be;
                let msg = encode_get_field_request(sid, ioid, None, PVA_VERSION, is_be);
                conn.conn.stream.write_all(&msg).await?;

                // Pin the request id: on a pooled connection a GET_FIELD reply
                // from an earlier operation could otherwise be picked up here.
                // Note the decoder puts the *server* response's echoed
                // request_id in `cid`, not in `ioid` — `ioid` is always `None`
                // on the server direction
                // (`spvirit-codec/src/epics_decode.rs:763-788`), and
                // `encode_get_field_response`/`_error` both write
                // `request_id` first. So match on `cid`.
                let resp_bytes = read_until(
                    &mut conn.conn.stream,
                    timeout_dur,
                    &mut conn.conn.reassembler,
                    |cmd| matches!(cmd, PvaPacketCommand::GetField(p)
                        if p.is_server && p.cid == ioid),
                )
                .await?;

                let mut pkt = PvaPacket::new(&resp_bytes);
                let cmd = pkt.decode_payload().ok_or_else(|| {
                    PvGetError::Decode("GET_FIELD response decode failed".to_string())
                })?;
                match cmd {
                    PvaPacketCommand::GetField(payload) => {
                        if let Some(ref st) = payload.status {
                            if st.is_error() {
                                let msg = st
                                    .message
                                    .clone()
                                    .unwrap_or_else(|| format!("code={}", st.code));
                                return Err(PvGetError::Protocol(format!(
                                    "GET_FIELD error: {msg}"
                                )));
                            }
                        }
                        let desc = payload.introspection.ok_or_else(|| {
                            PvGetError::Decode("missing GET_FIELD introspection".to_string())
                        })?;
                        // GET_FIELD is one request, one reply: the server
                        // keeps nothing for it, so there is nothing to free.
                        ctx.reusable();
                        Ok((desc, conn.conn.server_addr, conn.conn.guid))
                    }
                    _ => Err(PvGetError::Protocol(
                        "unexpected GET_FIELD response".to_string(),
                    )),
                }
            }
            .await;
            (conn, ctx, r)
        })
        .await
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
    let mut pkt = PvaPacket::try_new(raw).ok_or_else(|| {
        PvGetError::Decode(format!(
            "{label} init response is {} bytes, shorter than the 8-byte PVA header",
            raw.len()
        ))
    })?;
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

    /// Spawn an in-process `PvaServer` holding one `ao` PV and return
    /// `(tcp_port, udp_port)`. Callers point a client at it either as a
    /// name server (`.name_server(addr).no_broadcast()`, which exercises a
    /// real resolution) or directly (`.server_addr(addr)`, which does not).
    async fn spawn_test_server(pv: &str) -> (u16, u16) {
        use spvirit_server::pva_server::PvaServer;
        use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket as StdUdpSocket};

        let tcp_port = TcpListener::bind("127.0.0.1:0")
            .expect("bind tcp")
            .local_addr()
            .expect("local_addr")
            .port();
        let udp_port = StdUdpSocket::bind("127.0.0.1:0")
            .expect("bind udp")
            .local_addr()
            .expect("local_addr")
            .port();

        let server = PvaServer::builder()
            .ao(pv, 1.0)
            .port(tcp_port)
            .udp_port(udp_port)
            .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .build();
        tokio::spawn(async move {
            let _ = server.run().await;
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        (tcp_port, udp_port)
    }

    /// Read one top-level `Float64`/`Float32` field out of a decoded structure.
    fn field_f64(v: &spvirit_codec::spvd_decode::DecodedValue, name: &str) -> Option<f64> {
        use spvirit_codec::spvd_decode::DecodedValue as D;
        let D::Structure(fields) = v else { return None };
        fields.iter().find(|(k, _)| k == name).and_then(|(_, val)| match val {
            D::Float64(f) => Some(*f),
            D::Float32(f) => Some(*f as f64),
            _ => None,
        })
    }

    /// Like [`spawn_test_server`], but serving several `ao` PVs and returning
    /// the [`RunningServer`] so a test can stop the listener mid-test.
    ///
    /// `RunningServer::abort` closes the TCP listener (and the UDP responder)
    /// but, by design, leaves already-accepted per-connection handlers
    /// running; tests that need an existing connection dead too close it
    /// client-side with `close_pooled_for_test`.
    async fn start_test_server(
        pvs: &[&str],
    ) -> (spvirit_server::pva_server::RunningServer, u16, u16) {
        use spvirit_server::pva_server::PvaServer;
        use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket as StdUdpSocket};

        let tcp_port = TcpListener::bind("127.0.0.1:0")
            .expect("bind tcp")
            .local_addr()
            .expect("local_addr")
            .port();
        let udp_port = StdUdpSocket::bind("127.0.0.1:0")
            .expect("bind udp")
            .local_addr()
            .expect("local_addr")
            .port();

        let mut b = PvaServer::builder();
        for pv in pvs {
            b = b.ao(*pv, 1.0);
        }
        let running = b
            .port(tcp_port)
            .udp_port(udp_port)
            .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .build()
            .start()
            .await
            .expect("server start");
        tokio::time::sleep(Duration::from_millis(300)).await;
        (running, tcp_port, udp_port)
    }

    fn ns_client(tcp_port: u16, udp_port: u16, timeout: Duration) -> PvaClient {
        let ns: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();
        PvaClient::builder()
            .port(tcp_port)
            .udp_port(udp_port)
            .timeout(timeout)
            .name_server(ns)
            .no_broadcast()
            .build()
    }

    /// A second get to the same PV performs zero additional UDP searches AND
    /// zero additional TCP handshakes: it reuses the pooled connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_get_reuses_the_pooled_connection() {
        const PV: &str = "POOL:REUSE";
        let (tcp_port, udp_port) = spawn_test_server(PV).await;
        let ns: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();

        let client = PvaClient::builder()
            .port(tcp_port)
            .udp_port(udp_port)
            .timeout(Duration::from_secs(5))
            .name_server(ns)
            .no_broadcast()
            .build();

        client.pvget(PV).await.expect("first pvget");
        let first = client.cache_stats();
        assert_eq!(first.conn_misses, 1);
        assert_eq!(first.conn_hits, 0);

        client.pvget(PV).await.expect("second pvget");
        let second = client.cache_stats();
        assert_eq!(second.conn_misses, 1, "no second handshake");
        assert_eq!(second.conn_hits, 1);
        assert_eq!(second.resolve_misses, 1, "no second search");
    }

    /// A pooled connection to a server that has gone away is discarded and the
    /// operation retried once on a fresh connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dead_pooled_connection_is_retried_once_on_a_fresh_one() {
        const PV: &str = "POOL:DEAD";
        let (tcp_port, udp_port) = spawn_test_server(PV).await;
        let ns: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();

        let client = PvaClient::builder()
            .port(tcp_port)
            .udp_port(udp_port)
            .timeout(Duration::from_secs(5))
            .name_server(ns)
            .no_broadcast()
            .build();

        // Warm the pool.
        client.pvget(PV).await.expect("first pvget");
        assert_eq!(client.cache_stats().conn_misses, 1);

        // Reach into the pool and half-close the idle connection's socket from
        // the client side, which makes the server drop its end. The next
        // checkout therefore hands out a connection whose peer is gone.
        client.close_pooled_for_test(PV);

        let result = client.pvget(PV).await;
        assert!(result.is_ok(), "the retry must succeed: {result:?}");

        let stats = client.cache_stats();
        assert_eq!(stats.conn_retried_dead, 1, "exactly one retry");
        assert_eq!(stats.conn_misses, 2, "the retry dialled a fresh connection");
        assert_eq!(stats.resolve_misses, 1, "the retry must not re-search");
    }

    /// A get on a PV the server does not host fails without wedging the cache or
    /// the pool: the next get of a PV it does host still works. (With
    /// `.no_broadcast()` + a name server the failure lands in `resolve` — the
    /// server never answers a search for an unknown PV — so this pins "a failed
    /// resolve is not cached and does not disturb the pool". The channel-create
    /// failure path is covered by `a_dead_pooled_connection_is_retried_once_on_a_fresh_one`.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_lookup_does_not_poison_the_cache_or_the_pool() {
        const PV: &str = "POOL:OK";
        let (tcp_port, udp_port) = spawn_test_server(PV).await;
        let ns: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();

        let client = PvaClient::builder()
            .port(tcp_port)
            .udp_port(udp_port)
            .timeout(Duration::from_secs(2))
            .name_server(ns)
            .no_broadcast()
            .build();

        assert!(client.pvget("POOL:NOSUCHPV").await.is_err());
        assert!(client.pvget(PV).await.is_ok());
    }

    /// A put still works over the pool, and the value lands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_then_get_over_a_pooled_connection_round_trips() {
        const PV: &str = "POOL:PUTGET";
        let (tcp_port, udp_port) = spawn_test_server(PV).await;
        let ns: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();

        let client = PvaClient::builder()
            .port(tcp_port)
            .udp_port(udp_port)
            .timeout(Duration::from_secs(5))
            .name_server(ns)
            .no_broadcast()
            .build();

        client.pvput(PV, 17.5).await.expect("pvput");
        let got = client.pvget(PV).await.expect("pvget");
        assert_eq!(field_f64(&got.value, "value"), Some(17.5));

        // Three operations (put INIT+data, get, and the put's own channel) over
        // one pooled connection: at most one handshake.
        assert_eq!(client.cache_stats().conn_misses, 1);
    }

    /// Task 4 review: `channel_sid` must reuse a pooled connection's sid for
    /// the same PV (one CREATE_CHANNEL, not one per operation), and hand two
    /// different PVs on one connection distinct cids and sids. Driven with
    /// real PVA traffic, inspected through the pooled connection's state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pooled_channels_are_reused_per_pv_and_distinct_across_pvs() {
        const A: &str = "POOL:CHAN:A";
        const B: &str = "POOL:CHAN:B";
        let (_server, tcp_port, udp_port) = start_test_server(&[A, B]).await;
        let client = ns_client(tcp_port, udp_port, Duration::from_secs(5));

        client.pvget(A).await.expect("first get A");
        client.pvinfo(A).await.expect("pvinfo A");
        client.pvget(A).await.expect("second get A");

        let pooled = client.cache.pooled_channels_for_test(A);
        assert_eq!(pooled.len(), 1, "one pooled connection");
        let (channels, next_cid) = &pooled[0];
        assert_eq!(channels.len(), 1, "one channel for one PV");
        assert_eq!(
            *next_cid, 2,
            "three operations on one PV issued exactly one CREATE_CHANNEL (cid 1)"
        );
        let sid_a = channels[A];

        client.pvget(B).await.expect("get B");
        let stats = client.cache_stats();
        assert_eq!(stats.conn_misses, 1, "B rode the same connection as A");
        assert_eq!(stats.conn_hits, 3);

        let pooled = client.cache.pooled_channels_for_test(B);
        assert_eq!(pooled.len(), 1);
        let (channels, next_cid) = &pooled[0];
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[A], sid_a, "A's sid survives B's channel create");
        assert_ne!(channels[B], sid_a, "distinct PVs get distinct sids");
        assert_eq!(*next_cid, 3, "B was created with a fresh cid (2), not cid 1 again");
    }

    /// Start a server serving writable `ao` PVs (and read-only `ai` PVs) and
    /// return its `MonitorRegistry` too, whose `channel_tables(conn_id)` is the
    /// live per-connection state the server mutates — the observable for
    /// "per-request rows are freed".
    async fn start_test_server_with_registry(
        ao: &[&str],
        ai: &[&str],
    ) -> (
        spvirit_server::pva_server::RunningServer,
        std::sync::Arc<spvirit_server::monitor::MonitorRegistry>,
        u16,
        u16,
    ) {
        use spvirit_server::pva_server::PvaServer;
        use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket as StdUdpSocket};

        let tcp_port = TcpListener::bind("127.0.0.1:0")
            .expect("bind tcp")
            .local_addr()
            .expect("local_addr")
            .port();
        let udp_port = StdUdpSocket::bind("127.0.0.1:0")
            .expect("bind udp")
            .local_addr()
            .expect("local_addr")
            .port();

        let mut b = PvaServer::builder();
        for pv in ao {
            b = b.ao(*pv, 1.0);
        }
        for pv in ai {
            b = b.ai(*pv, 1.0);
        }
        let mut server = b
            .port(tcp_port)
            .udp_port(udp_port)
            .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .build();
        let registry = server.monitor_registry();
        let running = server.start().await.expect("server start");
        tokio::time::sleep(Duration::from_millis(300)).await;
        (running, registry, tcp_port, udp_port)
    }

    /// Total per-request binding rows (`ioid_to_sid`) across every live
    /// server connection. Connection ids start at 1 and the tests here open
    /// only a handful, so scanning 1..=16 covers them all.
    async fn server_request_rows(
        registry: &spvirit_server::monitor::MonitorRegistry,
    ) -> usize {
        let mut total = 0;
        for conn_id in 1..=16u64 {
            if let Some(t) = registry.channel_tables(conn_id).await {
                total += t.lock().unwrap().ioid_to_sid.len();
            }
        }
        total
    }

    /// What a [`Relay`] does to the traffic it carries.
    const RELAY_PASS: u8 = 0;
    /// Forward the next client PUT data frame (cmd 11, subcmd 0x00), then
    /// close the client's connection: the server gets the value, the client
    /// never gets the reply. One-shot: the relay drops to `RELAY_BLACKHOLE`,
    /// so a test sets `RELAY_PASS` again afterwards.
    const RELAY_KILL_AFTER_PUT_DATA: u8 = 1;
    /// Swallow everything the server sends: the client's reads stall.
    const RELAY_BLACKHOLE: u8 = 2;
    /// Rewrite the server's "Unknown SID" status text to "Bad chan ID" (same
    /// length, so the length-prefixed string and the frame stay valid): a
    /// real stale-sid refusal, worded the way a non-spvirit server might.
    const RELAY_REWORD_STALE_SID: u8 = 3;

    /// A frame-aware TCP relay in front of a real server, so a test can break
    /// a connection at an exact protocol point. Point a client at it with
    /// `.server_addr(relay_addr)`.
    struct Relay {
        mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
        addr: SocketAddr,
    }

    impl Relay {
        async fn start(upstream: SocketAddr) -> Self {
            use std::sync::atomic::AtomicU8;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind relay");
            let addr = listener.local_addr().expect("relay addr");
            let mode = Arc::new(AtomicU8::new(RELAY_PASS));
            let m = mode.clone();
            tokio::spawn(async move {
                while let Ok((client, _)) = listener.accept().await {
                    let server = tokio::net::TcpStream::connect(upstream)
                        .await
                        .expect("relay connect upstream");
                    let (mut c_rd, mut c_wr) = client.into_split();
                    let (mut s_rd, mut s_wr) = server.into_split();
                    let kill = Arc::new(tokio::sync::Notify::new());

                    // server -> client: raw copy, unless black-holed or killed.
                    let (m2, k2) = (m.clone(), kill.clone());
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 64 * 1024];
                        loop {
                            tokio::select! {
                                r = s_rd.read(&mut buf) => match r {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => {
                                        let mode = m2.load(Ordering::SeqCst);
                                        if mode == RELAY_BLACKHOLE {
                                            continue;
                                        }
                                        if mode == RELAY_REWORD_STALE_SID {
                                            const FROM: &[u8] = b"Unknown SID";
                                            const TO: &[u8] = b"Bad chan ID";
                                            let mut i = 0;
                                            while i + FROM.len() <= n {
                                                if &buf[i..i + FROM.len()] == FROM {
                                                    buf[i..i + FROM.len()].copy_from_slice(TO);
                                                    i += FROM.len();
                                                } else {
                                                    i += 1;
                                                }
                                            }
                                        }
                                        if c_wr.write_all(&buf[..n]).await.is_err() {
                                            break;
                                        }
                                    }
                                },
                                _ = k2.notified() => break,
                            }
                        }
                        // Dropping `c_wr` shuts down the client's read side.
                    });

                    // client -> server: frame by frame, so a PUT data frame
                    // can be recognised.
                    let m3 = m.clone();
                    tokio::spawn(async move {
                        loop {
                            let mut hdr = [0u8; 8];
                            if c_rd.read_exact(&mut hdr).await.is_err() {
                                break;
                            }
                            let h = spvirit_codec::epics_decode::PvaHeader::new(&hdr);
                            let len = if h.flags.is_control {
                                0
                            } else {
                                h.payload_length as usize
                            };
                            let mut payload = vec![0u8; len];
                            if c_rd.read_exact(&mut payload).await.is_err() {
                                break;
                            }
                            let is_put_data = !h.flags.is_control
                                && h.command == 11
                                && payload.len() > 8
                                && payload[8] == 0x00;
                            // Black-hole the reply path BEFORE the value
                            // reaches the server, so its reply cannot slip
                            // through ahead of the kill.
                            let kill_now = is_put_data
                                && m3
                                    .compare_exchange(
                                        RELAY_KILL_AFTER_PUT_DATA,
                                        RELAY_BLACKHOLE,
                                        Ordering::SeqCst,
                                        Ordering::SeqCst,
                                    )
                                    .is_ok();
                            let mut frame = hdr.to_vec();
                            frame.extend_from_slice(&payload);
                            if s_wr.write_all(&frame).await.is_err() {
                                break;
                            }
                            if kill_now {
                                kill.notify_one();
                                break;
                            }
                        }
                    });
                }
            });
            Relay { mode, addr }
        }

        fn set(&self, mode: u8) {
            self.mode.store(mode, Ordering::SeqCst);
        }
    }

    fn relay_client(relay: &Relay) -> PvaClient {
        PvaClient::builder()
            .timeout(Duration::from_secs(2))
            .server_addr(relay.addr)
            .build()
    }

    /// Task 5 review, Important 1: every pooled GET and PUT frees its
    /// server-side request state (DESTROY_REQUEST) before the connection is
    /// pooled, so a busy pooled connection does not grow the server's
    /// per-request maps without bound.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pooled_operations_do_not_grow_server_request_state() {
        const PV: &str = "POOL:ROWS";
        let (_server, registry, tcp_port, udp_port) =
            start_test_server_with_registry(&[PV], &[]).await;
        let client = ns_client(tcp_port, udp_port, Duration::from_secs(5));

        for i in 0..20 {
            client.pvget(PV).await.expect("pvget");
            client.pvput(PV, i as f64).await.expect("pvput");
        }
        let s = client.cache_stats();
        assert_eq!(s.conn_misses, 1, "all 40 operations shared one connection");

        // DESTROY_REQUEST has no reply; give the server a moment to drain it.
        let mut rows = usize::MAX;
        for _ in 0..100 {
            rows = server_request_rows(&registry).await;
            if rows == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            rows, 0,
            "40 pooled operations must leave no per-request rows on the server"
        );
    }

    /// Task 5 review, Minor 2: a local encode failure is not a dead
    /// connection. It returns at once, is not retried, and keeps the healthy
    /// pooled connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_put_encode_error_is_not_retried_and_keeps_the_connection() {
        const PV: &str = "POOL:ENCODE";
        let (tcp_port, udp_port) = spawn_test_server(PV).await;
        let client = ns_client(tcp_port, udp_port, Duration::from_secs(5));

        client.pvget(PV).await.expect("warm the pool");
        let err = client
            .pvput(PV, "not a number")
            .await
            .expect_err("a string cannot be put to a float field");
        assert!(
            matches!(&err, PvGetError::Protocol(m) if m.contains("put encode")),
            "{err:?}"
        );
        let s = client.cache_stats();
        assert_eq!(s.conn_retried_dead, 0, "a deterministic error is not a retry");
        assert_eq!(s.conn_misses, 1, "no fresh dial");

        client.pvget(PV).await.expect("the connection is still pooled");
        let s = client.cache_stats();
        assert_eq!(s.conn_misses, 1, "the same connection served the next get");
        assert_eq!(s.conn_hits, 2);
    }

    /// Task 5 review, Minor 2: a server refusal at INIT ("Write access
    /// denied" on a read-only record) is returned at once, not retried, and
    /// the connection it came over stays pooled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_refusal_at_init_is_not_retried_and_keeps_the_connection() {
        const RO: &str = "POOL:READONLY";
        let (_server, _registry, tcp_port, udp_port) =
            start_test_server_with_registry(&[], &[RO]).await;
        let client = ns_client(tcp_port, udp_port, Duration::from_secs(5));

        client.pvget(RO).await.expect("warm the pool");
        let err = client.pvput(RO, 2.0).await.expect_err("ai is read-only");
        assert!(
            matches!(&err, PvGetError::Protocol(m) if m.contains("Write access denied")),
            "{err:?}"
        );
        let s = client.cache_stats();
        assert_eq!(s.conn_retried_dead, 0, "a refusal is not a dead connection");
        assert_eq!(s.conn_misses, 1);

        client.pvget(RO).await.expect("the connection is still pooled");
        assert_eq!(client.cache_stats().conn_misses, 1);
    }

    /// Task 5 review, Minor 3: a channel create the server refuses, over a
    /// connection that dialled and validated fine, invalidates only that PV's
    /// resolution. It is not a dead-connection retry, and it neither closes
    /// that connection nor purges the other connections to the server.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_channel_create_invalidates_only_that_pv() {
        const KEEP: &str = "POOL:KEEP";
        const GONE: &str = "POOL:GONE";
        let (server, _registry, tcp_port, udp_port) =
            start_test_server_with_registry(&[KEEP, GONE], &[]).await;
        let client = ns_client(tcp_port, udp_port, Duration::from_secs(5));

        // A pooled connection carrying KEEP's channel only...
        client.pvget(KEEP).await.expect("pool KEEP");
        // ...and a cached resolution for GONE, via a path that does not pool.
        drop(client.open_put_channel(GONE).await.expect("resolve GONE"));
        assert_eq!(client.cache_stats().resolve_misses, 2);

        // GONE disappears; its search no longer answers, its create is refused.
        assert!(server.store().remove(GONE).await);

        let err = client.pvget(GONE).await.expect_err("GONE was removed");
        assert!(
            matches!(&err, PvGetError::Protocol(m) if m.contains("create_channel error")),
            "{err:?}"
        );
        let s = client.cache_stats();
        assert_eq!(s.conn_retried_dead, 0, "a refusal is not a dead connection");
        assert_eq!(s.conn_misses, 1, "no fresh dial for the refusal");

        client.pvget(KEEP).await.expect("KEEP still works");
        let s = client.cache_stats();
        assert_eq!(s.conn_misses, 1, "KEEP's pooled connection survived");

        // GONE's resolution was invalidated: the next call searches again.
        let before = client.cache_stats().resolve_misses;
        assert!(client.pvget(GONE).await.is_err());
        assert_eq!(client.cache_stats().resolve_misses, before + 1);
    }

    /// Task 5 review, Minor 1a: a PUT that fails AFTER its value was written
    /// must not be re-sent, even on a reused connection whose failure looks
    /// exactly like a dead socket. The relay delivers the PUT data to the
    /// server and then closes the client's connection before the reply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_put_failing_after_commit_is_never_retried() {
        const PV: &str = "POOL:COMMIT";
        let (_server, _registry, tcp_port, _udp) =
            start_test_server_with_registry(&[PV], &[]).await;
        let upstream: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();
        let relay = Relay::start(upstream).await;
        let client = relay_client(&relay);

        client.pvget(PV).await.expect("warm the pool");
        relay.set(RELAY_KILL_AFTER_PUT_DATA);

        let r = tokio::time::timeout(Duration::from_secs(10), client.pvput(PV, 9.5))
            .await
            .expect("the put must not hang");
        assert!(r.is_err(), "the reply never arrived: {r:?}");
        let s = client.cache_stats();
        assert_eq!(s.conn_retried_dead, 0, "a committed PUT is never re-sent");
        assert_eq!(s.conn_misses, 1, "no second connection was dialled for it");

        // The server did apply the one write.
        relay.set(RELAY_PASS);
        let got = client.pvget(PV).await.expect("get after");
        assert_eq!(field_f64(&got.value, "value"), Some(9.5));
    }

    /// Task 5 review, Minor 1b: a pvget whose future is dropped mid-operation
    /// does not return its connection to the pool: the next operation dials.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_get_future_does_not_pool_its_connection() {
        const PV: &str = "POOL:CANCEL";
        let (_server, _registry, tcp_port, _udp) =
            start_test_server_with_registry(&[PV], &[]).await;
        let upstream: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();
        let relay = Relay::start(upstream).await;
        let client = relay_client(&relay);

        client.pvget(PV).await.expect("warm the pool");
        assert_eq!(client.cache_stats().conn_misses, 1);

        relay.set(RELAY_BLACKHOLE);
        let dropped = tokio::time::timeout(Duration::from_millis(300), client.pvget(PV)).await;
        assert!(dropped.is_err(), "the get must still be in flight when dropped");
        assert_eq!(client.cache_stats().conn_hits, 1, "it had checked out the pooled conn");
        relay.set(RELAY_PASS);

        tokio::time::timeout(Duration::from_secs(10), client.pvget(PV))
            .await
            .expect("must not hang")
            .expect("next get");
        let s = client.cache_stats();
        assert_eq!(s.conn_misses, 2, "the abandoned connection was not pooled");
        assert_eq!(s.conn_retried_dead, 0);
    }

    /// Task 5 re-review: an op-level server refusal is kept on a healthy
    /// connection, but the refused channel must be forgotten. Otherwise a
    /// stale sid refused in words other than "Unknown SID" (not classified as
    /// a reuse failure) stays cached, the LIFO pool hands the connection
    /// straight back, and every later op on that PV fails the same way.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_pooled_channel_is_forgotten_and_recreated() {
        const PV: &str = "POOL:REFUSED";
        let (_server, _registry, tcp_port, _udp) =
            start_test_server_with_registry(&[PV], &[]).await;
        let upstream: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();
        let relay = Relay::start(upstream).await;
        let client = relay_client(&relay);

        client.pvget(PV).await.expect("warm the pool");
        let old_sid = client.cache.pooled_channels_for_test(PV)[0].0[PV];

        // The server forgets the channel; its "Unknown SID" reply reaches the
        // client as "Bad chan ID", which is not a recognised stale-sid text.
        client.cache.destroy_pooled_channel_for_test(PV).await;
        relay.set(RELAY_REWORD_STALE_SID);

        let err = tokio::time::timeout(Duration::from_secs(10), client.pvget(PV))
            .await
            .expect("must not hang")
            .expect_err("the stale sid is refused");
        assert!(format!("{err:?}").contains("Bad chan ID"), "{err:?}");
        let s = client.cache_stats();
        assert_eq!(s.conn_retried_dead, 0, "not recognised as a reuse failure");
        assert_eq!(s.conn_misses, 1, "the healthy connection was kept");
        let pooled = client.cache.pooled_channels_for_test(PV);
        assert_eq!(pooled.len(), 1);
        assert!(
            !pooled[0].0.contains_key(PV),
            "the refused channel must be forgotten before the connection is pooled"
        );

        // The next op re-creates the channel on the same connection.
        let got = tokio::time::timeout(Duration::from_secs(10), client.pvget(PV))
            .await
            .expect("must not hang");
        assert!(got.is_ok(), "the next op must not reuse the dead sid: {got:?}");
        let s = client.cache_stats();
        assert_eq!(s.conn_misses, 1, "same connection");
        let pooled = client.cache.pooled_channels_for_test(PV);
        let (channels, next_cid) = &pooled[0];
        assert_ne!(channels[PV], old_sid, "a fresh CreateChannel gave a new sid");
        assert_eq!(*next_cid, 3, "exactly one more CreateChannel (cid 2)");
    }

    /// Task 4 review: a pooled connection whose cached sid the server has
    /// destroyed must be treated as a reuse failure. The operation sees an
    /// error reply for the stale sid *before* any commit, so it must reach the
    /// retry branch and succeed on a fresh connection — for GET, GET_FIELD and
    /// PUT alike (a PUT INIT error precedes the commit point).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stale_pooled_sid_is_retried_once_on_a_fresh_connection() {
        const PV: &str = "POOL:STALESID";
        let (_server, tcp_port, udp_port) = start_test_server(&[PV]).await;
        let client = ns_client(tcp_port, udp_port, Duration::from_secs(5));

        client.pvget(PV).await.expect("warm the pool");
        assert_eq!(client.cache_stats().conn_misses, 1);

        client.cache.destroy_pooled_channel_for_test(PV).await;
        let got = client.pvget(PV).await;
        assert!(got.is_ok(), "GET on a stale sid must retry: {got:?}");
        let s = client.cache_stats();
        assert_eq!(s.conn_retried_dead, 1);
        assert_eq!(s.conn_misses, 2, "the retry dialled a fresh connection");

        client.cache.destroy_pooled_channel_for_test(PV).await;
        let info = client.pvinfo(PV).await;
        assert!(info.is_ok(), "GET_FIELD on a stale sid must retry: {info:?}");
        let desc = info.unwrap();
        assert_ne!(
            desc.struct_id.as_deref(),
            Some("epics:pva/pvlist:1.0"),
            "the retry must return the PV's introspection, not the server's PV list"
        );
        let s = client.cache_stats();
        assert_eq!(s.conn_retried_dead, 2);
        assert_eq!(s.conn_misses, 3);

        client.cache.destroy_pooled_channel_for_test(PV).await;
        let put = client.pvput(PV, 4.25).await;
        assert!(put.is_ok(), "PUT whose INIT hits a stale sid must retry: {put:?}");
        let s = client.cache_stats();
        assert_eq!(s.conn_retried_dead, 3);
        assert_eq!(s.conn_misses, 4);
        assert_eq!(s.resolve_misses, 1, "no retry re-searched");

        let got = client.pvget(PV).await.expect("get after put");
        // The read-back proves the value landed; it cannot tell one write from
        // two. That the PUT was not re-sent is carried by the counters above
        // (each retry was pre-commit, at INIT).
        assert_eq!(
            field_f64(&got.value, "value"),
            Some(4.25),
            "the retried put's value must have landed"
        );
    }

    /// Task 2 review gap, `open_channel` path: a cached resolution whose
    /// address no longer accepts connections is invalidated by the failed
    /// establish, so the NEXT operation searches again instead of hitting the
    /// stale entry. `open_put_channel` is driven because it reaches the wire
    /// through `open_channel` (not the pool).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_establish_in_open_channel_forces_a_fresh_search() {
        const PV: &str = "CACHE:STALE:OPEN";
        let (server, tcp_port, udp_port) = start_test_server(&[PV]).await;
        let client = ns_client(tcp_port, udp_port, Duration::from_secs(5));

        drop(client.open_put_channel(PV).await.expect("warm the resolution"));
        assert_eq!(client.cache_stats().resolve_misses, 1);

        // Stop the listener: the cached address now refuses connections.
        server.abort();
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(client.open_put_channel(PV).await.is_err());
        let s = client.cache_stats();
        assert_eq!(s.resolve_hits, 1, "the failing call used the cached entry");
        assert_eq!(s.resolve_misses, 1);

        // The failure invalidated the entry, so this call searches again
        // (and fails: the name server is the stopped server).
        assert!(client.open_put_channel(PV).await.is_err());
        let s = client.cache_stats();
        assert_eq!(s.resolve_misses, 2, "the stale entry must not be served again");
        assert_eq!(s.resolve_hits, 1);
    }

    /// Task 2 review gap, pooled path: same property through
    /// `with_pooled_channel`. The pooled connection is closed too, so the
    /// call retries once, the fresh dial is refused, and that refusal
    /// invalidates the resolution.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_dial_in_the_pooled_path_forces_a_fresh_search() {
        const PV: &str = "CACHE:STALE:POOL";
        let (server, tcp_port, udp_port) = start_test_server(&[PV]).await;
        let client = ns_client(tcp_port, udp_port, Duration::from_secs(5));

        client.pvget(PV).await.expect("warm resolution and pool");
        server.abort();
        tokio::time::sleep(Duration::from_millis(200)).await;
        client.close_pooled_for_test(PV);

        assert!(client.pvget(PV).await.is_err());
        let s = client.cache_stats();
        assert_eq!(s.resolve_hits, 1);
        assert_eq!(s.resolve_misses, 1);
        assert_eq!(s.conn_retried_dead, 1, "the dead pooled conn was retried once");

        assert!(client.pvget(PV).await.is_err());
        let s = client.cache_stats();
        assert_eq!(s.resolve_misses, 2, "the stale entry must not be served again");
        assert_eq!(s.resolve_hits, 1);
    }

    /// Task 2 review gap: `invalidate_pv` turns the next lookup from a hit
    /// into a miss.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalidate_pv_forces_the_next_operation_to_search() {
        const PV: &str = "CACHE:INVALIDATE";
        let (tcp_port, udp_port) = spawn_test_server(PV).await;
        let client = ns_client(tcp_port, udp_port, Duration::from_secs(5));

        client.pvget(PV).await.expect("miss");
        client.pvget(PV).await.expect("hit");
        let s = client.cache_stats();
        assert_eq!((s.resolve_misses, s.resolve_hits), (1, 1));

        client.invalidate_pv(PV);

        client.pvget(PV).await.expect("miss again");
        let s = client.cache_stats();
        assert_eq!(s.resolve_misses, 2, "invalidate_pv must force a search");
        assert_eq!(s.resolve_hits, 1);
    }

    /// Two operations on the same PV through the same client must perform
    /// exactly ONE resolution. Asserted on the cache counters, not on timing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_operation_on_the_same_pv_performs_no_additional_search() {
        const PV: &str = "CACHE:RESOLVE";
        let (tcp_port, udp_port) = spawn_test_server(PV).await;
        let ns: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();

        // Resolve via the in-process server acting as a TCP name server. No
        // UDP broadcast, so this is deterministic on every platform.
        let client = PvaClient::builder()
            .port(tcp_port)
            .udp_port(udp_port)
            .timeout(Duration::from_secs(5))
            .name_server(ns)
            .no_broadcast()
            .build();

        client.pvget(PV).await.expect("first pvget");
        let after_first = client.cache_stats();
        assert_eq!(after_first.resolve_misses, 1);
        assert_eq!(after_first.resolve_hits, 0);

        client.pvget(PV).await.expect("second pvget");
        let after_second = client.cache_stats();
        assert_eq!(
            after_second.resolve_misses, 1,
            "the second get must not search again"
        );
        assert_eq!(after_second.resolve_hits, 1);
    }

    /// A client built with `CacheConfig::disabled()` behaves exactly as it did
    /// before this plan: every operation resolves.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_disabled_cache_searches_on_every_operation() {
        const PV: &str = "CACHE:DISABLED";
        let (tcp_port, udp_port) = spawn_test_server(PV).await;
        let ns: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();

        let client = PvaClient::builder()
            .port(tcp_port)
            .udp_port(udp_port)
            .timeout(Duration::from_secs(5))
            .name_server(ns)
            .no_broadcast()
            .build()
            .with_cache_config(crate::cache::CacheConfig::disabled());

        client.pvget(PV).await.expect("first pvget");
        client.pvget(PV).await.expect("second pvget");

        let stats = client.cache_stats();
        assert_eq!(stats.resolve_misses, 2);
        assert_eq!(stats.resolve_hits, 0);
    }

    /// Clones of a `PvaClient` share one cache — which is how the gateway's
    /// `Arc<PvaClient>` per upstream gets a warm cache across requests.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clones_of_a_client_share_one_cache() {
        const PV: &str = "CACHE:SHARED";
        let (tcp_port, udp_port) = spawn_test_server(PV).await;
        let ns: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();

        let client = PvaClient::builder()
            .port(tcp_port)
            .udp_port(udp_port)
            .timeout(Duration::from_secs(5))
            .name_server(ns)
            .no_broadcast()
            .build();
        let twin = client.clone();

        client.pvget(PV).await.expect("first pvget");
        twin.pvget(PV).await.expect("second pvget via clone");

        assert_eq!(client.cache_stats().resolve_misses, 1);
        assert_eq!(twin.cache_stats().resolve_hits, 1);
    }

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

    /// The monitor loop reports the same host string on every update, and it is
    /// the server's socket address. Pinning this lets the loop hoist the
    /// formatting out without changing what a ByteSink observes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn monitor_reports_a_stable_host_string_on_every_update() {
        use std::sync::Arc;

        const PV: &str = "HOSTSTR:TEST";
        let (tcp_port, udp_port) = spawn_test_server(PV).await;
        let server_addr: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();

        let recorder = Arc::new(RecordingSink::default());
        let mut client = PvaClient::builder()
            .port(tcp_port)
            .udp_port(udp_port)
            .timeout(Duration::from_secs(5))
            .server_addr(server_addr)
            .build();
        client.set_byte_sink(recorder.clone());

        // Take three updates, then stop.
        let mut seen = 0u32;
        let monitor = client.pvmonitor(PV, move |_update| {
            seen += 1;
            if seen >= 3 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        // The server pushes an initial update immediately; a put drives the rest.
        let driver = {
            let client = client.clone();
            tokio::spawn(async move {
                for v in [1.0f64, 2.0, 3.0] {
                    let _ = client.pvput(PV, v).await;
                }
            })
        };
        let _ = tokio::time::timeout(Duration::from_secs(10), monitor).await;
        let _ = driver.await;

        let rx = recorder.rx.lock().unwrap();
        assert!(
            rx.len() >= 2,
            "expected several rx events, got {}",
            rx.len()
        );
        for (pv, host, _n) in rx.iter() {
            assert_eq!(pv, PV);
            assert_eq!(host, &server_addr.to_string());
        }
    }

    // ─── echo keepalive ────────────────────────────────────────────────

    /// The echo keepalive surfaces its write error instead of swallowing it: a
    /// socket that cannot take an 8-byte control frame is not going to deliver updates, and the
    /// monitor loop propagates this with `?` to end the subscription.
    #[tokio::test]
    async fn write_echo_surfaces_the_write_error() {
        // A duplex pair whose far half is dropped: every write fails.
        let (mut near, far) = tokio::io::duplex(64);
        drop(far);

        let mut token = 1u32;
        let result = write_echo(&mut near, &mut token, false).await;
        assert!(
            result.is_err(),
            "writing to a dropped duplex half must return Err, not be discarded"
        );
    }

    /// The echo keepalive advances its token so successive ECHOs are
    /// distinguishable, and it writes a control frame each time.
    #[tokio::test]
    async fn write_echo_advances_the_token() {
        let (mut near, mut far) = tokio::io::duplex(256);
        let mut token = 1u32;
        write_echo(&mut near, &mut token, false)
            .await
            .expect("first echo");
        write_echo(&mut near, &mut token, false)
            .await
            .expect("second echo");
        assert_eq!(token, 3, "token must advance once per echo");

        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 16];
        far.read_exact(&mut buf)
            .await
            .expect("two 8-byte control frames");
        assert_eq!(buf[2] & 0x01, 0x01, "an ECHO is a control frame");
        assert_eq!(buf[3], 3, "control command 3 is ECHO");
    }

    /// Guards prior audit item C6 on the interval the monitor loop actually uses:
    /// with tokio's default `Burst`, a stall longer than the 10 s period makes
    /// `tick()` return immediately once per missed period, emitting a burst of
    /// ECHOs. `new_echo_interval` must set `Delay`.
    #[test]
    fn missed_echo_ticks_do_not_burst() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                let mut i = new_echo_interval();
                i.tick().await; // immediate first tick

                tokio::time::advance(Duration::from_secs(35)).await;
                i.tick().await; // the one catch-up tick

                // With Delay, the next tick is a full period away; with Burst two
                // more catch-up ticks would already be ready.
                let immediate = tokio::time::timeout(Duration::from_millis(1), i.tick()).await;
                assert!(
                    immediate.is_err(),
                    "a second tick must not be ready at once"
                );
            });
    }

    // ─── monitor_data_body ─────────────────────────────────────────────

    /// Build a PVA application frame: magic, version, flags, command, len(LE),
    /// then the body.
    fn app_frame(command: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0xCA, 2, 0x40 /* server, LSB, unsegmented */, command];
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    /// A MONITOR data body: ioid(4, LE) + subcmd(1) + payload.
    fn monitor_body(ioid: u32, subcmd: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = ioid.to_le_bytes().to_vec();
        v.push(subcmd);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn monitor_data_body_returns_the_payload_for_our_ioid() {
        let frame = app_frame(13, &monitor_body(0xDEAD_BEEF, 0x00, &[1, 2, 3, 4]));
        assert_eq!(
            monitor_data_body(&frame, 0xDEAD_BEEF, false),
            Some(&[1u8, 2, 3, 4][..])
        );
    }

    #[test]
    fn monitor_data_body_ignores_other_ioids_commands_and_subcommands() {
        let ours = 0xDEAD_BEEF;

        // Someone else's ioid.
        let other = app_frame(13, &monitor_body(0x1234_5678, 0x00, &[9]));
        assert_eq!(monitor_data_body(&other, ours, false), None);

        // MONITOR INIT reply (subcmd has the 0x08 bit), not a data update.
        let init = app_frame(13, &monitor_body(ours, 0x08, &[9]));
        assert_eq!(monitor_data_body(&init, ours, false), None);

        // A GET reply (command 10), not a MONITOR.
        let get = app_frame(10, &monitor_body(ours, 0x00, &[9]));
        assert_eq!(monitor_data_body(&get, ours, false), None);

        // A control frame (flags bit 0 set) that happens to look like one.
        let mut ctrl = app_frame(13, &monitor_body(ours, 0x00, &[9]));
        ctrl[2] |= 0x01;
        assert_eq!(monitor_data_body(&ctrl, ours, false), None);
    }

    #[test]
    fn monitor_data_body_reads_the_ioid_in_the_connections_byte_order() {
        let ours = 0x0000_0102u32;
        let mut frame = vec![0xCA, 2, 0xC0 /* server + MSB */, 13];
        let mut body = ours.to_be_bytes().to_vec();
        body.push(0x00);
        body.extend_from_slice(&[7, 7]);
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&body);

        assert_eq!(monitor_data_body(&frame, ours, true), Some(&[7u8, 7][..]));
        // The same bytes read little-endian address a different ioid.
        assert_eq!(monitor_data_body(&frame, ours, false), None);
    }

    #[test]
    fn monitor_data_body_rejects_a_frame_too_short_to_hold_ioid_and_subcmd() {
        // Header + 4 bytes: an ioid but no subcmd byte.
        let frame = app_frame(13, &0xDEAD_BEEFu32.to_le_bytes());
        assert_eq!(monitor_data_body(&frame, 0xDEAD_BEEF, false), None);

        // Nothing but a header.
        let frame = app_frame(13, &[]);
        assert_eq!(monitor_data_body(&frame, 0xDEAD_BEEF, false), None);

        // Not even a header.
        assert_eq!(monitor_data_body(&[0xCA, 2], 0xDEAD_BEEF, false), None);
    }
}
