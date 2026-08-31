//! Upstream-disconnect propagation, end to end.
//!
//! The defect: a gateway monitor whose upstream server disappears goes silent
//! forever. No downstream client is told, the `MonitorCache` entry leaks, and
//! every later subscriber to that PV is silent from birth.
//!
//! These tests kill a real backend IOC. See `spawn_backend`'s doc comment for
//! why that has to be done by dropping a whole `tokio::runtime::Runtime` and
//! why `JoinHandle::abort()` is not an acceptable substitute.

use std::net::{TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use spvirit_gateway::access::AccessControl;
use spvirit_gateway::cache::negative::NegativeCache;
use spvirit_gateway::config::GatewayConfig;
use spvirit_gateway::loopguard::LoopGuard;
use spvirit_gateway::proxy::GatewaySource;
use spvirit_gateway::upstream::UpstreamPool;
use spvirit_server::PvaServer;
use spvirit_server::pvstore::{Source, TryClaim};

fn free_tcp_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

fn free_udp_port() -> Option<u16> {
    UdpSocket::bind("127.0.0.1:0")
        .ok()
        .and_then(|s| s.local_addr().ok())
        .map(|a| a.port())
}

/// A backend IOC running on its OWN thread with its OWN tokio runtime, killed
/// by dropping that runtime.
///
/// WARNING — do NOT "simplify" this into `tokio::spawn(server.run())` on the
/// test's runtime plus `JoinHandle::abort()`. It does not work here.
/// `run_tcp_server` (`spvirit-server/src/handler.rs` ~:1105-1112) hands every
/// accepted connection to a **detached** `tokio::spawn`, so aborting the accept
/// task leaves the already-accepted upstream TCP connection open and the
/// gateway's monitor alive and happy. A test built on `abort()` passes while
/// proving nothing at all. Dropping the runtime shuts its worker threads down
/// and closes every socket those detached handlers own — that is the only kill
/// that reproduces a vanished IOC.
struct Backend {
    kill: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Backend {
    /// Kill the IOC and wait until its runtime is fully dropped, so that when
    /// this returns the upstream sockets are genuinely gone.
    fn kill(&mut self) {
        // Hanging up the channel wakes the backend thread's blocking `recv`.
        drop(self.kill.take());
        if let Some(t) = self.thread.take() {
            t.join()
                .expect("backend thread exits after dropping its runtime");
        }
    }
}

fn spawn_backend(pv: &str, tcp_port: u16, udp_port: u16) -> Backend {
    let pv = pv.to_string();
    let (kill_tx, kill_rx) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("backend runtime builds");
        rt.spawn(async move {
            let server = PvaServer::builder()
                .ao(pv, 1.0)
                .listen_ip("127.0.0.1".parse().unwrap())
                .advertise_ip("127.0.0.1".parse().unwrap())
                .port(tcp_port)
                .udp_port(udp_port)
                .build();
            let _ = server.run().await;
        });
        let _ = ready_tx.send(());
        // Park until the test hangs up `kill_tx`, then DROP the runtime. The
        // drop IS the kill.
        let _ = kill_rx.recv();
        drop(rt);
    });
    ready_rx.recv().expect("backend thread starts");
    std::thread::sleep(Duration::from_millis(300));
    Backend {
        kill: Some(kill_tx),
        thread: Some(thread),
    }
}

/// A `GatewaySource` wired to reach a backend on `udp_port` over loopback.
/// Mirrors `it_passthrough.rs`'s `spawn_gateway_with_access` (see that file's
/// module doc for why `autoaddrlist` stays at its default `true`).
fn gateway_source(udp_port: u16) -> (Arc<GatewaySource>, Arc<UpstreamPool>) {
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
    let guard = Arc::new(LoopGuard::build(
        &cfg,
        &cfg.servers[0],
        std::collections::HashSet::new(),
    ));
    let access = Arc::new(AccessControl::new(false, None, None));
    let src = Arc::new(GatewaySource::new(
        pool.clone(),
        vec!["it-client".into()],
        neg,
        guard,
        0,
        access,
    ));
    (src, pool)
}

/// Layer 1, end to end: when the upstream IOC vanishes, the gateway must
/// notice, close every downstream subscriber, retire the cache entry and the
/// binding, and count the death.
///
/// Pre-fix every assertion below fails: `pvmonitor`'s return is discarded
/// (`let _ =`, proxy.rs:329), the spawned task just ends, the `MonitorEntry`
/// stays in the map holding live `Sender`s, so `rx` never closes and
/// `upstream_monitor_count()` never returns to zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_death_closes_subscribers_and_retires_the_entry_and_binding() {
    let (Some(tcp), Some(udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };
    let mut backend = spawn_backend("GW:DIE", tcp, udp);
    let (src, _pool) = gateway_source(udp);

    assert!(src.claim("GW:DIE").await.is_some(), "backend must be up");
    let mut rx = src.subscribe("GW:DIE").await.expect("subscribe");
    assert_eq!(src.upstream_monitor_count(), 1);

    // Let the upstream MONITOR INIT complete before the kill, so the monitor
    // is genuinely established rather than still connecting.
    tokio::time::sleep(Duration::from_millis(500)).await;

    backend.kill();

    // The downstream receiver must CLOSE, not merely go quiet. `recv()`
    // returning `None` is precisely the signal the server-side pump needs.
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if rx.recv().await.is_none() {
                return true;
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        closed,
        "after the upstream died the downstream receiver must be closed, not \
         silently held open forever by a leaked MonitorEntry"
    );

    // The leak half.
    assert_eq!(
        src.upstream_monitor_count(),
        0,
        "the dead upstream's MonitorCache entry must be retired"
    );
    assert!(
        src.upstream_monitor_deaths() >= 1,
        "the upstream-ended teardown must be counted"
    );

    // Spec section 4: the binding must go too, or try_claim keeps answering
    // Yes from memory and the client hot-loops.
    assert!(
        matches!(src.try_claim("GW:DIE"), TryClaim::Unknown),
        "a retired binding must make try_claim fall through to a real resolve"
    );

    // The Site B regression, and the half most likely to rot: a FRESH
    // subscriber after the death must not be silent from birth. With the
    // backend still down the honest outcome is a clean failure (no binding to
    // resolve), never a live-looking receiver wired to a dead upstream.
    assert!(
        src.subscribe("GW:DIE").await.is_none(),
        "a fresh subscribe after the upstream died must fail cleanly rather \
         than hand back a receiver attached to a dead entry"
    );
}
