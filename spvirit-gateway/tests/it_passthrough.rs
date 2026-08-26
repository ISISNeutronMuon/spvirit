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
