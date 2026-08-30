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
//! [`crate::proxy::GatewaySource::upstream_monitor_count`]); the remaining
//! diagnostics (`refs`/`threads`/`stats` and the per-PV/host bandwidth
//! counters) have no M1 data source and are emitted as shape-complete
//! `0`-valued gauges with correct `# HELP`/`# TYPE` lines.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The Prometheus text exposition content-type (format version 0.0.4).
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// Max bytes we read from a request before giving up (we only need the
/// request line; this caps a misbehaving client).
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// How long a single connection may take to send its request line before we
/// drop it, so a slow/silent client never pins a task forever.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Shape-complete bandwidth stubs (bytes), mirroring the status source's
    /// always-zero `ds:`/`us:` `bypv`/`byhost` `rx`/`tx` counters.
    pub ds_bypv_rx_bytes: u64,
    pub ds_bypv_tx_bytes: u64,
    pub ds_byhost_rx_bytes: u64,
    pub ds_byhost_tx_bytes: u64,
    pub us_bypv_rx_bytes: u64,
    pub us_bypv_tx_bytes: u64,
    pub us_byhost_rx_bytes: u64,
    pub us_byhost_tx_bytes: u64,
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
        "Downstream bytes received, per PV (shape-complete, data pending).",
        s.ds_bypv_rx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_ds_bypv_tx_bytes",
        "Downstream bytes sent, per PV (shape-complete, data pending).",
        s.ds_bypv_tx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_ds_byhost_rx_bytes",
        "Downstream bytes received, per host (shape-complete, data pending).",
        s.ds_byhost_rx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_ds_byhost_tx_bytes",
        "Downstream bytes sent, per host (shape-complete, data pending).",
        s.ds_byhost_tx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_us_bypv_rx_bytes",
        "Upstream bytes received, per PV (shape-complete, data pending).",
        s.us_bypv_rx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_us_bypv_tx_bytes",
        "Upstream bytes sent, per PV (shape-complete, data pending).",
        s.us_bypv_tx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_us_byhost_rx_bytes",
        "Upstream bytes received, per host (shape-complete, data pending).",
        s.us_byhost_rx_bytes,
    );
    gauge(
        &mut out,
        "spgateway_us_byhost_tx_bytes",
        "Upstream bytes sent, per host (shape-complete, data pending).",
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
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let path = path.clone();
                let snapshot = provider();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &path, snapshot).await {
                        tracing::debug!("metrics: connection error: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("metrics: accept error: {e}");
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
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
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
        for name in [
            "spgateway_refs",
            "spgateway_threads",
            "spgateway_stats",
            "spgateway_ds_bypv_rx_bytes",
            "spgateway_ds_bypv_tx_bytes",
            "spgateway_ds_byhost_rx_bytes",
            "spgateway_ds_byhost_tx_bytes",
            "spgateway_us_bypv_rx_bytes",
            "spgateway_us_bypv_tx_bytes",
            "spgateway_us_byhost_rx_bytes",
            "spgateway_us_byhost_tx_bytes",
        ] {
            assert!(body.contains(&format!("# TYPE {name} gauge\n")), "missing TYPE for {name}");
            assert!(body.contains(&format!("\n{name} 0\n")), "missing zero value for {name}");
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
}
