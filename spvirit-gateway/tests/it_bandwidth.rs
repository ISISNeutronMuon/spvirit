//! Task 11 integration test: the gateway's upstream traffic must land in a
//! single, shared [`BandwidthCounters`] — the same instance threaded into
//! every upstream [`PvaClient`]'s [`ByteSink`] and into every [`PvaServer`]
//! this gateway builds.
//!
//! This drives REAL upstream traffic (a real loopback [`PvaServer`] plus a
//! real [`UpstreamPool`]-built [`PvaClient`]) through the exact methods the
//! gateway's production code path uses (`proxy::GatewaySource` calls
//! `PvaClient::pvget` for GET pass-through and `PvaClient::pvmonitor` for
//! subscriptions) — not a hand-rolled mock of the wiring.

use std::sync::Arc;
use std::time::Duration;

use spvirit_gateway::config::GatewayConfig;
use spvirit_gateway::upstream::UpstreamPool;
use spvirit_server::PvaServer;
use spvirit_server::diag::BandwidthCounters;

fn free_tcp_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

fn free_udp_port() -> Option<u16> {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .ok()
        .and_then(|s| s.local_addr().ok())
        .map(|a| a.port())
}

/// A `pvget` (the method `spvirit_gateway::proxy::GatewaySource` actually
/// calls to pass a downstream GET through to an upstream IOC) issued through
/// an `UpstreamPool` client built with a `BandwidthCounters`-backed
/// `ByteSink` installed must land in `counters.us_bypv_rx` under the real PV
/// name, and in `counters.us_byhost_rx` under the real upstream host.
///
/// This is the coverage gap Task 10's reviewer flagged: `pvget`/
/// `pvget_fields` were NOT instrumented (only `pvput`/`pvput_fields` and
/// `pvmonitor_with_options` were). Before the Task 11 fix to
/// `spvirit-client`'s `pvget_fields` this assertion fails (`us_bypv_rx` has
/// no row for the PV) even though the sink is genuinely installed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_pvget_through_the_real_pool_populates_shared_counters() {
    let Some(tcp_port) = free_tcp_port() else {
        eprintln!("Skipping test: cannot bind a free TCP port in this environment");
        return;
    };
    let Some(udp_port) = free_udp_port() else {
        eprintln!("Skipping test: cannot bind a free UDP port in this environment");
        return;
    };

    let upstream = PvaServer::builder()
        .ai("BW:GET", 42.0)
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(tcp_port)
        .udp_port(udp_port)
        .build();
    tokio::spawn(async move {
        let _ = upstream.run().await;
    });
    tokio::time::sleep(Duration::from_millis(600)).await;

    let cfg_json = format!(
        r#"{{
            "version": 2,
            "clients": [{{
                "name": "bw-client",
                "addrlist": "127.0.0.1",
                "bcastport": {udp_port},
                "interface": ["127.0.0.1"]
            }}],
            "servers": [{{
                "name": "bw-server",
                "clients": ["bw-client"]
            }}]
        }}"#
    );
    let cfg = GatewayConfig::from_json_str(&cfg_json).expect("parse gateway config");

    // The SAME counters instance a `Runtime` would build and thread into
    // both the upstream sinks and `PvaServer::bandwidth_counters` — built
    // via the real `UpstreamPool::from_config_with_counters` production
    // constructor, not a hand-rolled sink mock.
    let counters = Arc::new(BandwidthCounters::new());
    let pool = UpstreamPool::from_config_with_counters(&cfg, Some(&counters));
    let client = pool.client("bw-client").expect("client must be in the pool");

    // Sanity: before any traffic, the row must not exist yet.
    assert!(
        counters.us_bypv_rx.snapshot().is_empty(),
        "no upstream traffic has happened yet"
    );

    let result = tokio::time::timeout(Duration::from_secs(5), client.pvget("BW:GET"))
        .await
        .expect("pvget should not time out")
        .expect("pvget BW:GET should succeed");
    assert!(!result.raw_pva.is_empty());

    let by_pv = counters.us_bypv_rx.snapshot();
    let pv_row = by_pv
        .iter()
        .find(|(k, _)| k == "BW:GET")
        .unwrap_or_else(|| panic!("expected a BW:GET row in us_bypv_rx, got {by_pv:?}"));
    assert!(pv_row.1 > 0, "expected a positive byte count for BW:GET");

    let by_host = counters.us_byhost_rx.snapshot();
    assert!(
        by_host.iter().any(|(_, n)| *n > 0),
        "expected a positive-byte host row in us_byhost_rx, got {by_host:?}"
    );
}

/// Symmetric proof for the pre-existing (Task 10) monitor RX instrumentation
/// path, exercised through the SAME `UpstreamPool`-built client with the
/// same shared `counters` — the update fires through
/// `PvaClient::pvmonitor_fields` -> `pvmonitor_with_options`, which was
/// already wired to the `ByteSink` before this task; this proves the Task
/// 11 wiring correctly threads the gateway's real upstream monitor traffic
/// into the same unified counters, not a second/duplicate instance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_pvmonitor_through_the_real_pool_populates_shared_counters() {
    let Some(tcp_port) = free_tcp_port() else {
        eprintln!("Skipping test: cannot bind a free TCP port in this environment");
        return;
    };
    let Some(udp_port) = free_udp_port() else {
        eprintln!("Skipping test: cannot bind a free UDP port in this environment");
        return;
    };

    let upstream = PvaServer::builder()
        .ai("BW:MON", 7.0)
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(tcp_port)
        .udp_port(udp_port)
        .build();
    tokio::spawn(async move {
        let _ = upstream.run().await;
    });
    tokio::time::sleep(Duration::from_millis(600)).await;

    let cfg_json = format!(
        r#"{{
            "version": 2,
            "clients": [{{
                "name": "bw-mon-client",
                "addrlist": "127.0.0.1",
                "bcastport": {udp_port},
                "interface": ["127.0.0.1"]
            }}],
            "servers": [{{
                "name": "bw-mon-server",
                "clients": ["bw-mon-client"]
            }}]
        }}"#
    );
    let cfg = GatewayConfig::from_json_str(&cfg_json).expect("parse gateway config");

    let counters = Arc::new(BandwidthCounters::new());
    let pool = UpstreamPool::from_config_with_counters(&cfg, Some(&counters));
    let client = pool
        .client("bw-mon-client")
        .expect("client must be in the pool");

    let counters_for_task = counters.clone();
    let done = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvmonitor("BW:MON", move |_update| std::ops::ControlFlow::Break(())),
    )
    .await
    .expect("pvmonitor should not time out");
    assert!(done.is_ok(), "pvmonitor BW:MON should succeed: {done:?}");

    let by_pv = counters_for_task.us_bypv_rx.snapshot();
    assert!(
        by_pv.iter().any(|(k, n)| k == "BW:MON" && *n > 0),
        "expected a positive BW:MON row in us_bypv_rx, got {by_pv:?}"
    );
}
