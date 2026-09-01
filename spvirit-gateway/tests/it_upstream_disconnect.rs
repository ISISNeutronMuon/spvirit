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

/// One HTTP/1.1 `GET`, hand-rolled to match the hand-rolled responder (same
/// shape as `it_metrics.rs`: the crate adds no HTTP client dependency and the
/// endpoint answers a single `Connection: close` request).
async fn http_get(port: u16, path: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
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

/// The whole path: kill the IOC, and the downstream server must drop the
/// monitor subscription (it sends DESTROY_CHANNEL to do so) rather than
/// holding a live-looking, permanently silent monitor. Then, once the IOC is
/// back, a fresh subscriber must get real data — the recovery half of spec
/// section 4, which is client-driven precisely because the gateway retires the
/// binding instead of running a reconnect loop.
///
/// Pre-fix, the subscription list still holds the subscriber forever: the
/// gateway's `MonitorEntry` keeps its senders alive, so the server's pump
/// never sees end-of-stream and never destroys anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_downstream_monitor_subscription_is_torn_down_when_the_upstream_dies() {
    use std::ops::ControlFlow;

    use spvirit_client::{PvOptions, pvmonitor};

    let (Some(up_tcp), Some(up_udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };
    let (Some(down_tcp), Some(down_udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping test: cannot bind a free downstream port");
        return;
    };

    let mut backend = spawn_backend("GW:E2E", up_tcp, up_udp);
    let (src, _pool) = gateway_source(up_udp);
    assert!(src.claim("GW:E2E").await.is_some(), "backend must be up");

    // A downstream PvaServer publishing the gateway source — the same wiring
    // `spvirit_gateway::runtime` uses in production. Hold its registry so the
    // test can observe the teardown.
    let published: Arc<dyn Source> = Arc::clone(&src) as Arc<dyn Source>;
    let mut down = PvaServer::builder()
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(down_tcp)
        .udp_port(down_udp)
        .source("gateway", 0, published)
        .build();
    let registry = down.monitor_registry();
    tokio::spawn(async move {
        let _ = down.run().await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // A real TCP monitor client. Explicit `server_addr` bypasses UDP search.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut opts = PvOptions::new("GW:E2E".to_string());
    opts.server_addr = Some(
        format!("127.0.0.1:{down_tcp}")
            .parse()
            .expect("loopback addr"),
    );
    opts.tcp_port = down_tcp;
    opts.udp_port = down_udp;
    opts.search_addr = Some("127.0.0.1".parse().unwrap());
    opts.bind_addr = Some("127.0.0.1".parse().unwrap());
    opts.timeout = Duration::from_secs(5);
    tokio::spawn(async move {
        let _ = pvmonitor(&opts, move |update| {
            let _ = tx.send(format!("{update:?}"));
            ControlFlow::Continue(())
        })
        .await;
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("the initial monitor update must arrive while the backend is alive");
    assert!(
        registry
            .monitors
            .lock()
            .await
            .get("GW:E2E")
            .is_some_and(|l| !l.is_empty()),
        "the downstream subscription must be registered before the kill"
    );

    // Kill the IOC by dropping its runtime. NOT `JoinHandle::abort()` — see
    // `spawn_backend`'s doc comment.
    backend.kill();

    let torn_down = {
        let mut ok = false;
        for _ in 0..200 {
            let empty = registry
                .monitors
                .lock()
                .await
                .get("GW:E2E")
                .is_none_or(|l| l.is_empty());
            if empty {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        ok
    };
    assert!(
        torn_down,
        "after the upstream died the downstream subscription must be destroyed \
         (DESTROY_CHANNEL), not left holding a monitor that never speaks again"
    );
    assert_eq!(
        src.upstream_monitor_count(),
        0,
        "the dead upstream's cache entry must be retired"
    );
    assert!(
        src.upstream_monitor_deaths() >= 1,
        "the death must be counted"
    );

    // Recovery is client-driven: bring the IOC back and a *fresh* subscriber
    // must get real data, proving the gateway did not leave a dead entry that
    // makes every later subscriber silent from birth.
    let _backend2 = spawn_backend("GW:E2E", up_tcp, up_udp);
    assert!(
        src.claim("GW:E2E").await.is_some(),
        "a re-resolved PV must be claimable again"
    );
    let mut rx2 = src.subscribe("GW:E2E").await.expect("re-subscribe");
    let got = tokio::time::timeout(Duration::from_secs(10), rx2.recv())
        .await
        .ok()
        .flatten();
    assert!(
        got.is_some(),
        "a subscriber created after the death must receive data, not silence"
    );
}

/// The operator-visible half: a live `/metrics` from a real `Runtime::run`
/// must report the death this test caused, and must show the upstream-monitor
/// gauge back at zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_metrics_endpoint_reports_the_upstream_death() {
    use spvirit_gateway::runtime::Runtime;

    let (Some(up_tcp), Some(up_udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };
    let (Some(gw_tcp), Some(gw_udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping test: cannot bind a free gateway port");
        return;
    };
    let Some(metrics_port) = free_tcp_port() else {
        eprintln!("Skipping test: cannot bind a free metrics port");
        return;
    };
    const PATH: &str = "/metrics";

    let mut backend = spawn_backend("GW:METRIC", up_tcp, up_udp);

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

    // Establish a real monitor through the running gateway.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut opts = spvirit_client::PvOptions::new("GW:METRIC".to_string());
    opts.server_addr = Some(format!("127.0.0.1:{gw_tcp}").parse().expect("loopback addr"));
    opts.tcp_port = gw_tcp;
    opts.udp_port = gw_udp;
    opts.search_addr = Some("127.0.0.1".parse().unwrap());
    opts.bind_addr = Some("127.0.0.1".parse().unwrap());
    opts.timeout = Duration::from_secs(5);
    tokio::spawn(async move {
        let _ = spvirit_client::pvmonitor(&opts, move |update| {
            let _ = tx.send(format!("{update:?}"));
            std::ops::ControlFlow::Continue(())
        })
        .await;
    });
    rx.recv_timeout(Duration::from_secs(15))
        .expect("the monitor must be established through the running gateway");

    backend.kill();

    let mut deaths = 0;
    let mut live = u64::MAX;
    for _ in 0..200 {
        if let Some(body) = http_get(metrics_port, PATH).await {
            deaths = metric(&body, "spgateway_upstream_monitor_deaths_total").unwrap_or(0);
            live = metric(&body, "spgateway_upstream_monitors").unwrap_or(u64::MAX);
            if deaths >= 1 && live == 0 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        deaths >= 1,
        "a live /metrics must report the upstream monitor death this test caused; \
         got spgateway_upstream_monitor_deaths_total = {deaths}"
    );
    assert_eq!(
        live, 0,
        "the upstream-monitor gauge must fall back to zero once the dead \
         entry is retired"
    );

    run.abort();
}
