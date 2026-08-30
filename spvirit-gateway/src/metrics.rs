//! `metrics` — a minimal, dependency-free Prometheus `/metrics` responder for
//! the gateway's diagnostic counters.
//!
//! The endpoint is enabled either by the `x-spvirit.metrics.enabled` config
//! flag or, unconditionally, by the `--metrics` CLI flag (see `spgateway`).
//! When enabled, [`crate::runtime::Runtime::run`] binds a listener and spawns
//! [`serve`] as a tokio task tied to the same shutdown as the PVA servers.
//!
//! # Why hand-rolled
//!
//! The crate must add **zero** new dependencies, so rather than pull in
//! axum/hyper/warp we implement just enough of HTTP/1.1 over
//! [`tokio::net::TcpListener`] to answer a single `GET`: read the request
//! line (bounded, with a read timeout so a slow/silent client can't hang a
//! task), then reply `200` with the Prometheus text body for the configured
//! path, `404` for any other path, or `405` for any non-`GET` method. This is
//! a read-only, single-shot, `Connection: close` responder — the smallest
//! surface that satisfies a Prometheus scraper.
//!
//! # Metric content
//!
//! `/metrics` is a numeric mirror of the gateway's status PVs: it exposes the
//! underlying *counts* as gauges (Prometheus has no array/table shape). Real
//! data is sourced from the same low-level accessors the status source uses
//! ([`crate::upstream::UpstreamPool::names`] and
//! [`crate::proxy::GatewaySource::upstream_monitor_count`]); the per-PV/host
//! bandwidth gauges are cumulative totals summed from the shared
//! [`BandwidthCounters`] and [`ClientRegistry`] (see [`snapshot_from_bandwidth`]).
//! The remaining diagnostics (`refs`/`threads`/`stats`) have no M1 data
//! source and are emitted as shape-complete `0`-valued gauges with correct
//! `# HELP`/`# TYPE` lines.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use spvirit_server::diag::{BandwidthCounters, ClientRegistry};

/// The Prometheus text exposition content-type (format version 0.0.4).
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// Max bytes we read from a request before giving up (we only need the
/// request line; this caps a misbehaving client).
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// How long a single connection may take to send its request line before we
/// drop it, so a slow/silent client never pins a task forever.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How long we allow writing+flushing the response to take before dropping the
/// connection, so a client that sends a valid request then stops reading
/// (response-side slowloris) cannot hold the handler + socket open forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Ceiling on concurrently-handled connections. A permit is held for each
/// in-flight handler; the accept loop waits for one before accepting, bounding
/// the FDs the endpoint can accumulate (which otherwise feeds an EMFILE
/// condition under a connection flood).
const MAX_CONCURRENT_CONNECTIONS: usize = 64;

/// Backoff applied after a failed `accept()` so a *persistent* accept error
/// (EMFILE/ENFILE — the process/system FD table is full, which returns Err
/// immediately and repeatedly) cannot busy-spin a core or flood the logs.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// A self-contained snapshot of the gateway's diagnostic counters at one
/// instant, rendered by [`render_prometheus`]. Kept free of any I/O or live
/// gateway handles so rendering is unit-testable in isolation.
///
/// `clients` and `upstream_monitors` carry real data; the remaining fields
/// are shape-complete stubs (always `0` in M1 — no data source exists yet).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Number of upstream client networks configured on the shared pool.
    pub clients: u64,
    /// Number of distinct upstream monitors currently running (the status
    /// source's "cache" count), summed across every server's source.
    pub upstream_monitors: u64,
    /// Shape-complete stub: per-binding refcount (no M1 source).
    pub refs: u64,
    /// Shape-complete stub: thread-pool size (no M1 source).
    pub threads: u64,
    /// Shape-complete stub: aggregate request stats (no M1 source).
    pub stats: u64,
    /// Cumulative bandwidth totals (bytes), mirroring the status source's
    /// `ds:`/`us:` `bypv`/`byhost` `rx`/`tx` counters, summed across every
    /// key in the underlying per-PV/host map. Populated by
    /// [`snapshot_from_bandwidth`].
    ///
    /// Exception: `ds_byhost_rx_bytes`/`ds_byhost_tx_bytes` below are NOT
    /// cumulative — they are summed from `ClientRegistry::byhost`, which
    /// tracks only currently-connected downstream hosts, so those two
    /// values fall when a client disconnects.
    pub ds_bypv_rx_bytes: u64,
    pub ds_bypv_tx_bytes: u64,
    pub ds_byhost_rx_bytes: u64,
    pub ds_byhost_tx_bytes: u64,
    pub us_bypv_rx_bytes: u64,
    pub us_bypv_tx_bytes: u64,
    pub us_byhost_rx_bytes: u64,
    pub us_byhost_tx_bytes: u64,
}

/// Sum a `ByteMap`/`ClientRegistry::byhost` style snapshot's byte counts
/// into a single cumulative total. `pairs` is an iterator of tuples whose
/// last element is the byte count (`(key, bytes)` for a `ByteMap`
/// snapshot, `(account, client_ip, bytes)` for `ClientRegistry::byhost`).
fn sum_last<T, I: IntoIterator<Item = T>>(pairs: I, last: impl Fn(&T) -> u64) -> u64 {
    pairs.into_iter().map(|t| last(&t)).sum()
}

/// Build the six `ByteMap`-backed byte-gauge fields (`ds_bypv_*`,
/// `us_bypv_*`, `us_byhost_*`) plus the two registry-derived `ds_byhost_*`
/// fields from the shared cumulative counters, leaving every other
/// [`MetricsSnapshot`] field at its default. Callers (the runtime's
/// `SnapshotProvider`) fill in `clients`/`upstream_monitors` on top via a
/// struct-update.
///
/// `ds_byhost_{tx,rx}` have no `ByteMap` (see [`BandwidthCounters`]'s docs)
/// and are instead derived from `ClientRegistry::byhost`.
pub fn snapshot_from_bandwidth(
    counters: &BandwidthCounters,
    registry: &ClientRegistry,
) -> MetricsSnapshot {
    MetricsSnapshot {
        ds_bypv_tx_bytes: sum_last(counters.ds_bypv_tx.snapshot(), |(_, b)| *b),
        ds_bypv_rx_bytes: sum_last(counters.ds_bypv_rx.snapshot(), |(_, b)| *b),
        us_bypv_tx_bytes: sum_last(counters.us_bypv_tx.snapshot(), |(_, b)| *b),
        us_bypv_rx_bytes: sum_last(counters.us_bypv_rx.snapshot(), |(_, b)| *b),
        us_byhost_tx_bytes: sum_last(counters.us_byhost_tx.snapshot(), |(_, b)| *b),
        us_byhost_rx_bytes: sum_last(counters.us_byhost_rx.snapshot(), |(_, b)| *b),
        ds_byhost_tx_bytes: sum_last(registry.byhost(true), |(_, _, b)| *b),
        ds_byhost_rx_bytes: sum_last(registry.byhost(false), |(_, _, b)| *b),
        ..Default::default()
    }
}

/// A cheap, cloneable "produce the current snapshot" callback. The runtime
/// builds one that reads live gateway handles; tests use a fixed closure.
pub type SnapshotProvider = Arc<dyn Fn() -> MetricsSnapshot + Send + Sync>;

/// Render a snapshot into Prometheus text exposition format.
///
/// Every metric carries a `# HELP` and `# TYPE` line and uses the stable
/// `spgateway_` prefix with snake_case names. Pure and total — no I/O.
pub fn render_prometheus(s: &MetricsSnapshot) -> String {
    let mut out = String::with_capacity(2048);

    fn gauge(out: &mut String, name: &str, help: &str, value: u64) {
        out.push_str("# HELP ");
        out.push_str(name);
        out.push(' ');
        out.push_str(help);
        out.push('\n');
        out.push_str("# TYPE ");
        out.push_str(name);
        out.push_str(" gauge\n");
        out.push_str(name);
        out.push(' ');
        out.push_str(&value.to_string());
        out.push('\n');
    }

    gauge(
        &mut out,
        "spgateway_clients",
        "Number of upstream client networks configured.",
        s.clients,
    );
    gauge(
        &mut out,
        "spgateway_upstream_monitors",
        "Number of distinct upstream monitors currently running.",
        s.upstream_monitors,
    );
    gauge(
        &mut out,
        "spgateway_refs",
        "Per-binding reference count (shape-complete, data pending).",
        s.refs,
    );
    gauge(
        &mut out,
        "spgateway_threads",
        "Worker thread count (shape-complete, data pending).",
        s.threads,
    );
    gauge(
        &mut out,
        "spgateway_stats",
        "Aggregate request stats (shape-complete, data pending).",
        s.stats,
    );
    gauge(
        &mut out,
        "spgateway_ds_bypv_rx_bytes",
        "Cumulative downstream bytes received, summed across all PVs.",
        s.ds_bypv_rx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_ds_bypv_tx_bytes",
        "Cumulative downstream bytes sent, summed across all PVs.",
        s.ds_bypv_tx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_ds_byhost_rx_bytes",
        "Downstream bytes received, summed across currently-connected hosts (drops on disconnect, not cumulative).",
        s.ds_byhost_rx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_ds_byhost_tx_bytes",
        "Downstream bytes sent, summed across currently-connected hosts (drops on disconnect, not cumulative).",
        s.ds_byhost_tx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_us_bypv_rx_bytes",
        "Cumulative upstream bytes received, summed across all PVs.",
        s.us_bypv_rx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_us_bypv_tx_bytes",
        "Cumulative upstream bytes sent, summed across all PVs.",
        s.us_bypv_tx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_us_byhost_rx_bytes",
        "Cumulative upstream bytes received, summed across all hosts.",
        s.us_byhost_rx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_us_byhost_tx_bytes",
        "Cumulative upstream bytes sent, summed across all hosts.",
        s.us_byhost_tx_bytes,
    );

    out
}

/// Bind a TCP listener for the metrics endpoint. A bind failure is returned
/// to the caller so the runtime can treat it as a fatal startup error (the
/// user explicitly asked for the endpoint; silently continuing without it
/// would be wrong).
pub async fn bind(addr: &str) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

/// Serve the metrics endpoint forever on `listener`, answering `GET <path>`
/// with the rendered snapshot from `provider`. Runs until the future is
/// dropped (the runtime ties this to the same shutdown as the PVA servers).
///
/// Per-connection errors (accept failures, slow clients) are logged and
/// otherwise ignored — one bad client must never take the endpoint down.
pub async fn serve(listener: TcpListener, path: String, provider: SnapshotProvider) {
    let limiter = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    loop {
        // Backpressure: acquire a connection permit BEFORE accepting, so a
        // burst of clients cannot spawn unbounded handler tasks (each holding
        // an FD). The owned permit is moved into the handler and released when
        // it finishes. The semaphore is never closed, so `acquire_owned` only
        // errors on a poisoned/closed semaphore — treat that as unreachable.
        let Ok(permit) = limiter.clone().acquire_owned().await else {
            return;
        };
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let path = path.clone();
                let provider = provider.clone();
                tokio::spawn(async move {
                    // Hold the permit for the handler's lifetime; released on drop.
                    let _permit = permit;
                    // Produce the snapshot INSIDE the task so the accept loop
                    // never blocks on it (it clones + discards a Vec via len()).
                    let snapshot = provider();
                    if let Err(e) = handle_connection(stream, &path, snapshot).await {
                        tracing::debug!("metrics: connection error: {e}");
                    }
                });
            }
            Err(e) => {
                // A persistent accept error (EMFILE/ENFILE) returns Err
                // immediately and repeatedly; back off so it cannot busy-spin.
                tracing::warn!("metrics: accept error: {e}");
                drop(permit);
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
            }
        }
    }
}

/// Read one request line (bounded, with a timeout), then write the matching
/// HTTP/1.1 response and close the connection.
async fn handle_connection(
    mut stream: TcpStream,
    path: &str,
    snapshot: MetricsSnapshot,
) -> std::io::Result<()> {
    let request_line = match tokio::time::timeout(READ_TIMEOUT, read_request_line(&mut stream)).await
    {
        Ok(Ok(line)) => line,
        // Timed out or the peer hung up before sending a full request line;
        // there is nothing to answer, so just drop the connection.
        Ok(Err(e)) => return Err(e),
        Err(_) => return Ok(()),
    };

    let response = route(&request_line, path, &snapshot);
    // Bound the write+flush: a client that sent a valid request line then
    // stopped reading must not pin the handler + socket open indefinitely.
    let write = async {
        stream.write_all(response.as_bytes()).await?;
        stream.flush().await
    };
    match tokio::time::timeout(WRITE_TIMEOUT, write).await {
        Ok(r) => r,
        // Timed out mid-response: drop the connection.
        Err(_) => Ok(()),
    }
}

/// Read from `stream` until the first `\r\n` (end of the request line) or the
/// byte cap, returning the request line as a lossy UTF-8 string.
async fn read_request_line(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut tmp = [0u8; 512];
    loop {
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            return Ok(String::from_utf8_lossy(&buf[..pos]).into_owned());
        }
        if buf.len() >= MAX_REQUEST_BYTES {
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            // EOF before a full request line.
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// Pure routing: given a request line like `GET /metrics HTTP/1.1`, the
/// configured path, and a snapshot, produce the full HTTP response text.
fn route(request_line: &str, path: &str, snapshot: &MetricsSnapshot) -> String {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    if method != "GET" {
        return http_response(
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            "method not allowed\n",
        );
    }
    // Ignore any query string on the target.
    let target_path = target.split('?').next().unwrap_or(target);
    if target_path == path {
        http_response("200 OK", CONTENT_TYPE, &render_prometheus(snapshot))
    } else {
        http_response("404 Not Found", "text/plain; charset=utf-8", "not found\n")
    }
}

/// Build a complete HTTP/1.1 response with a `Connection: close` header.
fn http_response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MetricsSnapshot {
        MetricsSnapshot {
            clients: 3,
            upstream_monitors: 7,
            ..Default::default()
        }
    }

    #[test]
    fn render_has_help_type_and_value_lines_for_real_gauges() {
        let body = render_prometheus(&sample());
        assert!(body.contains("# HELP spgateway_clients Number of upstream client networks configured.\n"));
        assert!(body.contains("# TYPE spgateway_clients gauge\n"));
        assert!(body.contains("\nspgateway_clients 3\n"));
        assert!(body.contains("# TYPE spgateway_upstream_monitors gauge\n"));
        assert!(body.contains("\nspgateway_upstream_monitors 7\n"));
    }

    #[test]
    fn render_emits_shape_complete_zero_stubs() {
        let body = render_prometheus(&MetricsSnapshot::default());
        for name in ["spgateway_refs", "spgateway_threads", "spgateway_stats"] {
            assert!(body.contains(&format!("# TYPE {name} gauge\n")), "missing TYPE for {name}");
            assert!(body.contains(&format!("\n{name} 0\n")), "missing zero value for {name}");
        }
    }

    /// `snapshot_from_bandwidth` sums each `BandwidthCounters` `ByteMap`
    /// across all its keys (per-PV / per-upstream-host) and, for the two
    /// `ds_byhost_*` fields (no `ByteMap`, see `BandwidthCounters`'s docs),
    /// sums `ClientRegistry::byhost`'s third element instead — proving the
    /// `/metrics` byte gauges reflect real cumulative totals rather than the
    /// old always-zero stub.
    #[test]
    fn snapshot_from_bandwidth_sums_every_byte_gauge_and_renders_gauge_type() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let counters = BandwidthCounters::new();
        counters.ds_bypv_tx.add("PV:A", 10);
        counters.ds_bypv_tx.add("PV:B", 5); // -> ds_bypv_tx total 15
        counters.ds_bypv_rx.add("PV:A", 1);
        counters.ds_bypv_rx.add("PV:B", 2); // -> ds_bypv_rx total 3
        counters.us_bypv_tx.add("PV:A", 100); // -> us_bypv_tx total 100
        counters.us_bypv_rx.add("PV:A", 7);
        counters.us_bypv_rx.add("PV:B", 8); // -> us_bypv_rx total 15
        counters.us_byhost_tx.add("ioc1", 40);
        counters.us_byhost_tx.add("ioc2", 60); // -> us_byhost_tx total 100
        counters.us_byhost_rx.add("ioc1", 4); // -> us_byhost_rx total 4

        let registry = ClientRegistry::new();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
        let peer1 = SocketAddr::new(ip, 40000);
        let peer2 = SocketAddr::new(ip, 40001);
        registry.connect(1, peer1);
        registry.connect(2, peer2);
        registry.add_tx(1, 20);
        registry.add_tx(2, 30); // -> ds_byhost_tx total 50
        registry.add_rx(1, 9); // -> ds_byhost_rx total 9

        let snap = snapshot_from_bandwidth(&counters, &registry);
        assert_eq!(snap.ds_bypv_tx_bytes, 15);
        assert_eq!(snap.ds_bypv_rx_bytes, 3);
        assert_eq!(snap.us_bypv_tx_bytes, 100);
        assert_eq!(snap.us_bypv_rx_bytes, 15);
        assert_eq!(snap.us_byhost_tx_bytes, 100);
        assert_eq!(snap.us_byhost_rx_bytes, 4);
        assert_eq!(snap.ds_byhost_tx_bytes, 50);
        assert_eq!(snap.ds_byhost_rx_bytes, 9);
        // Fields outside the bandwidth helper's scope stay at their default.
        assert_eq!(snap.clients, 0);
        assert_eq!(snap.upstream_monitors, 0);

        let body = render_prometheus(&snap);
        for (name, expected) in [
            ("spgateway_ds_bypv_tx_bytes", 15),
            ("spgateway_ds_bypv_rx_bytes", 3),
            ("spgateway_us_bypv_tx_bytes", 100),
            ("spgateway_us_bypv_rx_bytes", 15),
            ("spgateway_us_byhost_tx_bytes", 100),
            ("spgateway_us_byhost_rx_bytes", 4),
            ("spgateway_ds_byhost_tx_bytes", 50),
            ("spgateway_ds_byhost_rx_bytes", 9),
        ] {
            assert!(
                body.contains(&format!("# TYPE {name} gauge\n")),
                "expected {name} to stay a gauge"
            );
            assert!(
                body.contains(&format!("\n{name} {expected}\n")),
                "expected `{name} {expected}` in body, got:\n{body}"
            );
        }
    }

    #[test]
    fn route_get_configured_path_is_200_with_prometheus_body() {
        let resp = route("GET /metrics HTTP/1.1", "/metrics", &sample());
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp.contains("Content-Type: text/plain; version=0.0.4\r\n"));
        assert!(resp.contains("spgateway_clients 3\n"));
    }

    #[test]
    fn route_get_other_path_is_404() {
        let resp = route("GET /nope HTTP/1.1", "/metrics", &sample());
        assert!(resp.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[test]
    fn route_non_get_method_is_405() {
        let resp = route("POST /metrics HTTP/1.1", "/metrics", &sample());
        assert!(resp.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));
    }

    #[test]
    fn route_ignores_query_string() {
        let resp = route("GET /metrics?foo=bar HTTP/1.1", "/metrics", &sample());
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
    }

    #[tokio::test]
    async fn end_to_end_over_a_real_socket() {
        let listener = bind("127.0.0.1:0").await.expect("bind ephemeral");
        let addr = listener.local_addr().unwrap();
        let provider: SnapshotProvider = Arc::new(|| MetricsSnapshot {
            clients: 5,
            upstream_monitors: 2,
            ..Default::default()
        });
        let server = tokio::spawn(serve(listener, "/metrics".to_string(), provider));

        // Happy path: GET /metrics -> 200 + body.
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8_lossy(&resp);
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "resp was: {resp}");
        assert!(resp.contains("Content-Type: text/plain; version=0.0.4\r\n"));
        assert!(resp.contains("spgateway_clients 5\n"));
        assert!(resp.contains("spgateway_upstream_monitors 2\n"));

        // A different path -> 404.
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /nope HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8_lossy(&resp);
        assert!(resp.starts_with("HTTP/1.1 404 Not Found\r\n"), "resp was: {resp}");

        server.abort();
    }

    /// Helper: spawn a metrics server on an ephemeral port, returning its addr
    /// and the JoinHandle.
    async fn spawn_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = bind("127.0.0.1:0").await.expect("bind ephemeral");
        let addr = listener.local_addr().unwrap();
        let provider: SnapshotProvider = Arc::new(|| MetricsSnapshot {
            clients: 1,
            ..Default::default()
        });
        let handle = tokio::spawn(serve(listener, "/metrics".to_string(), provider));
        (addr, handle)
    }

    /// Helper: one full request/response round-trip, returning the response.
    async fn get(addr: std::net::SocketAddr, req: &[u8]) -> String {
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(req).await.unwrap();
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        String::from_utf8_lossy(&resp).into_owned()
    }

    /// (a) A client that opens a connection and sends only a partial,
    /// never-terminated request line must not wedge the server: a subsequent
    /// well-formed request is still served. The stuck client is held open for
    /// the duration so its handler is genuinely still in flight.
    #[tokio::test]
    async fn partial_request_does_not_wedge_the_server() {
        let (addr, server) = spawn_server().await;

        // Stuck client: valid start, no CRLF, never closes.
        let mut stuck = TcpStream::connect(addr).await.unwrap();
        stuck.write_all(b"GET /metr").await.unwrap();

        // A fresh client is still served promptly.
        let resp = get(addr, b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "resp was: {resp}");

        drop(stuck);
        server.abort();
    }

    /// (b) An oversized request line (> `MAX_REQUEST_BYTES`) is bounded: the
    /// server does not panic, still answers, and keeps serving afterwards.
    #[tokio::test]
    async fn oversized_request_line_is_bounded() {
        let (addr, server) = spawn_server().await;

        // ~9 KiB single token with no CRLF until the very end. The server
        // caps its read at MAX_REQUEST_BYTES and closes after responding;
        // because unread inbound bytes remain, the peer may observe a RST
        // (esp. on Windows) rather than a clean response — either is fine, the
        // point is the server neither panics nor wedges. Writes/reads here are
        // therefore best-effort.
        let mut req = Vec::from(&b"GET /"[..]);
        req.resize(req.len() + 9 * 1024, b'a');
        req.extend_from_slice(b" HTTP/1.1\r\nHost: x\r\n\r\n");
        if let Ok(mut client) = TcpStream::connect(addr).await {
            let _ = client.write_all(&req).await;
            let mut resp = Vec::new();
            let _ = client.read_to_end(&mut resp).await;
        }

        // Server survives and serves the next request cleanly.
        let resp = get(addr, b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "resp was: {resp}");

        server.abort();
    }

    /// (c) A non-GET method returns 405 over a real socket.
    #[tokio::test]
    async fn wrong_method_returns_405_over_socket() {
        let (addr, server) = spawn_server().await;
        let resp = get(addr, b"DELETE /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(
            resp.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
            "resp was: {resp}"
        );
        server.abort();
    }
}
