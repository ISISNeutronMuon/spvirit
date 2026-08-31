//! V3 NEW-1: the `/metrics` **endpoint**, scraped over a real socket from a
//! real [`Runtime::run`].
//!
//! This finding has moved up a frame twice. `93d362f` tested
//! `metrics::apply_resolve_stats`; `58c81de` lifted the scrape closure into
//! `build_snapshot_provider` and tested that. Both times the *call site* — the
//! line in `Runtime::run` that actually installs the provider — stayed
//! unreached: a verifier replaced it with a provider omitting
//! `apply_resolve_stats` and the entire gateway suite stayed green while a
//! live `/metrics` read zero for every resolver counter. `spvirit-gateway
//! /tests/` contained no metrics test at all.
//!
//! So this one stops at the endpoint. It stands up a real `Runtime` from a
//! real config with metrics enabled, lets `Runtime::run` bind and serve it,
//! performs an actual HTTP GET, and asserts the resolver counters carry values
//! this test caused. There is no helper between the assertion and the socket.

use std::net::{TcpListener, UdpSocket};
use std::time::Duration;

use spvirit_gateway::config::GatewayConfig;
use spvirit_gateway::runtime::Runtime;
use spvirit_server::pvstore::TryClaim;
use spvirit_server::search_resolve::{global_stats, note_pattern_enum_shed, note_try_claim};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn free_tcp_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral tcp")
        .local_addr()
        .expect("tcp addr")
        .port()
}

fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .expect("bind ephemeral udp")
        .local_addr()
        .expect("udp addr")
        .port()
}

/// One HTTP/1.1 `GET`, hand-rolled to match the hand-rolled responder: the
/// crate adds no HTTP client dependency, and the endpoint answers a single
/// `Connection: close` request.
async fn http_get(port: u16, path: &str) -> Option<String> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.ok()?;
    let mut body = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut body))
        .await
        .ok()?
        .ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// Read `name`'s value out of a Prometheus text body.
fn metric(body: &str, name: &str) -> Option<u64> {
    body.lines()
        .find_map(|l| l.strip_prefix(name)?.trim().parse::<u64>().ok())
}

#[tokio::test]
async fn the_running_gateway_serves_the_resolver_counters_on_its_metrics_endpoint() {
    let metrics_port = free_tcp_port();
    let gw_tcp = free_tcp_port();
    let gw_udp = free_udp_port();
    let up_udp = free_udp_port();
    const PATH: &str = "/metrics";

    let cfg_json = format!(
        r#"{{
            "version": 2,
            "clients": [{{
                "name": "up",
                "addrlist": "127.0.0.1",
                "bcastport": {up_udp},
                "interface": ["127.0.0.1"]
            }}],
            "servers": [{{
                "name": "gw",
                "clients": ["up"],
                "interface": ["127.0.0.1"],
                "serverport": {gw_tcp},
                "bcastport": {gw_udp}
            }}],
            "x-spvirit": {{
                "metrics": {{
                    "enabled": true,
                    "listen": "127.0.0.1:{metrics_port}",
                    "path": "{PATH}"
                }}
            }}
        }}"#
    );
    let cfg = GatewayConfig::from_json_str(&cfg_json).expect("parse gateway config");
    let rt = Runtime::from_config(cfg).expect("valid config builds a Runtime");
    let run = tokio::spawn(async move {
        let _ = rt.run().await;
    });

    // The counters are process-wide and monotonic, so the assertions below are
    // on a *delta this test causes*; a neighbour bumping them concurrently can
    // only make the delta larger. Recorded before the scrape, after the
    // runtime is up.
    let before = global_stats();
    const YES: u64 = 5;
    const SHED: u64 = 3;
    for _ in 0..YES {
        note_try_claim(TryClaim::Yes);
    }
    for _ in 0..SHED {
        note_pattern_enum_shed();
    }

    // `Runtime::run` binds the listener itself; poll until it answers rather
    // than sleeping blindly.
    let mut body = None;
    for _ in 0..100 {
        if let Some(b) = http_get(metrics_port, PATH).await {
            body = Some(b);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let body = body.unwrap_or_else(|| {
        panic!("the running gateway never answered a GET {PATH} on 127.0.0.1:{metrics_port}")
    });

    assert!(
        body.starts_with("HTTP/1.1 200 OK\r\n"),
        "expected a 200 from the live endpoint, got:\n{body}"
    );
    assert!(
        body.contains("text/plain; version=0.0.4"),
        "expected the Prometheus content-type, got:\n{body}"
    );

    let yes = metric(&body, "spgateway_search_try_claim_yes_total").unwrap_or_else(|| {
        panic!("/metrics carried no `spgateway_search_try_claim_yes_total` line at all:\n{body}")
    });
    assert!(
        yes >= before.try_claim_yes + YES,
        "a live /metrics reported try_claim_yes = {yes} after this test caused \
         {YES} more than the {} already recorded; `Runtime::run` is not \
         installing a provider that applies the resolver stats",
        before.try_claim_yes
    );

    let shed = metric(&body, "spgateway_search_pattern_enum_shed_total").unwrap_or_else(|| {
        panic!(
            "/metrics carried no `spgateway_search_pattern_enum_shed_total` line at all — \
             a shed pattern query is silent on the wire, so this counter is its only \
             trace:\n{body}"
        )
    });
    assert!(
        shed >= before.pattern_enum_shed + SHED,
        "a live /metrics reported pattern_enum_shed = {shed} after this test \
         caused {SHED} more than the {} already recorded",
        before.pattern_enum_shed
    );

    run.abort();
}
