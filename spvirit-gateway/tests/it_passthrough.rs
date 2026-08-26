//! End-to-end integration test: a real upstream `spvirit-server` (in-process)
//! plus a gateway `GatewaySource` talking to it over loopback UDP+TCP.
//!
//! Loopback discovery notes (see task-9-report.md for the full story): unlike
//! `spvirit-tools/tests/spvirit_get.rs`'s `local_pvget_opts`, this test does
//! *not* set `autoaddrlist: false` on the client config. `PvaClient`'s
//! `no_broadcast` (which is what `autoaddrlist: false` maps to in
//! `upstream::build_client`) disables the entire UDP search branch in
//! `resolve_pv_server`, not just true subnet broadcast — so pairing it with
//! `search_addr` alone leaves no search strategy at all unless a TCP name
//! server also happens to be configured. Leaving `autoaddrlist` at its
//! default `true` keeps the UDP search branch active while `addrlist` /
//! `interface` still redirect it to unicast loopback, exactly like
//! `local_pvget_opts` does for the top-level `pvget`/`pvinfo` functions.
use std::net::{TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use spvirit_gateway::cache::negative::NegativeCache;
use spvirit_gateway::config::GatewayConfig;
use spvirit_gateway::loopguard::LoopGuard;
use spvirit_gateway::proxy::GatewaySource;
use spvirit_gateway::upstream::UpstreamPool;
use spvirit_server::PvaServer;
use spvirit_server::pvstore::Source;
use spvirit_types::{NtPayload, PvValue, ScalarValue};

fn free_tcp_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|addr| addr.port())
}

fn free_udp_port() -> Option<u16> {
    UdpSocket::bind("127.0.0.1:0")
        .ok()
        .and_then(|s| s.local_addr().ok())
        .map(|addr| addr.port())
}

/// Spins up an in-process upstream server (built by `configure`) plus a
/// gateway `GatewaySource`/`UpstreamPool` pair wired to reach it over
/// loopback, exactly as in `claim_resolves_a_real_upstream_pv` (see the
/// module doc comment for why `autoaddrlist` stays at its default `true`).
/// Returns `None` if a free TCP/UDP port could not be bound in this
/// environment (test should skip in that case).
async fn spawn_gateway(
    configure: impl FnOnce(spvirit_server::PvaServerBuilder) -> spvirit_server::PvaServerBuilder,
    getholdoff_ms: u32,
) -> Option<(GatewaySource, Arc<UpstreamPool>)> {
    let tcp_port = free_tcp_port()?;
    let udp_port = free_udp_port()?;

    let builder = PvaServer::builder()
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(tcp_port)
        .udp_port(udp_port);
    let server = configure(builder).build();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let cfg_json = format!(
        r#"{{
            "version": 2,
            "clients": [{{
                "name": "it-client",
                "addrlist": "127.0.0.1",
                "bcastport": {udp_port},
                "interface": ["127.0.0.1"]
            }}],
            "servers": [{{
                "name": "it-server",
                "clients": ["it-client"]
            }}]
        }}"#
    );
    let cfg = GatewayConfig::from_json_str(&cfg_json).expect("parse gateway config");

    let pool = Arc::new(UpstreamPool::from_config(&cfg));
    let neg = Arc::new(NegativeCache::new(Duration::from_secs(30), 128));
    let guard = Arc::new(LoopGuard::build(&cfg, &cfg.servers[0]));

    let src = GatewaySource::new(
        pool.clone(),
        vec!["it-client".into()],
        neg,
        guard,
        getholdoff_ms,
    );

    Some((src, pool))
}

/// Digs the scalar `F64` out of an `NtPayload::Generic`'s `"value"` field,
/// as produced by `bridge::nt_payload_from_get` for an NTScalar `ai`/`ao`
/// upstream record.
fn extract_f64_value(p: &NtPayload) -> f64 {
    let NtPayload::Generic { fields, .. } = p else {
        panic!("expected NtPayload::Generic, got {p:?}");
    };
    for (name, v) in fields {
        if name == "value"
            && let PvValue::Scalar(ScalarValue::F64(x)) = v
        {
            return *x;
        }
    }
    panic!("no scalar F64 \"value\" field in {fields:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_resolves_a_real_upstream_pv() {
    let tcp_port = match free_tcp_port() {
        Some(p) => p,
        None => {
            eprintln!("Skipping test: cannot bind TCP port in this environment");
            return;
        }
    };
    let udp_port = match free_udp_port() {
        Some(p) => p,
        None => {
            eprintln!("Skipping test: cannot bind UDP port in this environment");
            return;
        }
    };

    let server = PvaServer::builder()
        .ai("IT:TEMP", 22.5)
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(tcp_port)
        .udp_port(udp_port)
        .build();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let cfg_json = format!(
        r#"{{
            "version": 2,
            "clients": [{{
                "name": "it-client",
                "addrlist": "127.0.0.1",
                "bcastport": {udp_port},
                "interface": ["127.0.0.1"]
            }}],
            "servers": [{{
                "name": "it-server",
                "clients": ["it-client"]
            }}]
        }}"#
    );
    let cfg = GatewayConfig::from_json_str(&cfg_json).expect("parse gateway config");

    let pool = Arc::new(UpstreamPool::from_config(&cfg));
    let neg = Arc::new(NegativeCache::new(Duration::from_secs(30), 128));
    let guard = Arc::new(LoopGuard::build(&cfg, &cfg.servers[0]));

    let src = GatewaySource::new(pool, vec!["it-client".into()], neg, guard, 0);

    assert!(src.claim("IT:TEMP").await.is_some());
    assert!(src.claim("IT:MISSING").await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_returns_the_upstream_value() {
    let Some((src, _pool)) = spawn_gateway(|b| b.ai("IT:TEMP", 22.5), 0).await else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };

    assert!(src.claim("IT:TEMP").await.is_some());

    let payload = src.get("IT:TEMP").await.expect("get should return Some");
    let value = extract_f64_value(&payload);
    assert!((value - 22.5).abs() < 1e-6, "expected ~22.5, got {value}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn getholdoff_holds_the_cached_value() {
    let Some((src, pool)) = spawn_gateway(|b| b.ao("IT:SETPOINT", 1.0), 10_000).await else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };

    assert!(src.claim("IT:SETPOINT").await.is_some());

    // First get populates the getholdoff cache with the initial value.
    let payload = src.get("IT:SETPOINT").await.expect("get should return Some");
    let value = extract_f64_value(&payload);
    assert!((value - 1.0).abs() < 1e-6, "expected ~1.0, got {value}");

    // Change the upstream value directly, bypassing the gateway entirely.
    let upstream_client = pool.client("it-client").expect("client in pool");
    upstream_client
        .pvput("IT:SETPOINT", 2.0f64)
        .await
        .expect("direct upstream pvput should succeed");

    // A second get, still within the (large) getholdoff window, must
    // return the cached value rather than round-tripping upstream again —
    // this is the getholdoff proof.
    let payload_again = src
        .get("IT:SETPOINT")
        .await
        .expect("get should return Some");
    let value_again = extract_f64_value(&payload_again);
    assert!(
        (value_again - 1.0).abs() < 1e-6,
        "expected getholdoff to still return ~1.0 (cached), got {value_again}"
    );
}
