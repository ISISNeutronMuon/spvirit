//! Upstream-disconnect propagation, end to end.
//!
//! The defect: a gateway monitor whose upstream server disappears goes silent
//! forever. No downstream client is told, the `MonitorCache` entry leaks, and
//! every later subscriber to that PV is silent from birth.
//!
//! These tests kill a real backend IOC. See `spawn_backend`'s doc comment for
//! why that has to be done by dropping a whole `tokio::runtime::Runtime` and
//! why `JoinHandle::abort()` is not an acceptable substitute.

use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use spvirit_client::{
    ChannelConn, PvOptions, establish_channel, read_until, resolve_pv_server,
};
use spvirit_codec::epics_decode::{PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::encode_monitor_request;
use tokio::io::AsyncWriteExt;

use spvirit_gateway::access::AccessControl;
use spvirit_gateway::cache::negative::NegativeCache;
use spvirit_gateway::config::GatewayConfig;
use spvirit_gateway::loopguard::LoopGuard;
use spvirit_gateway::proxy::GatewaySource;
use spvirit_gateway::upstream::UpstreamPool;
use spvirit_server::PvaServer;
use spvirit_server::pvstore::{Source, TryClaim};

/// Why a failed port probe is a FAILURE here and not a skip.
///
/// This file is the end-to-end proof of the whole upstream-disconnect branch.
/// A test that `eprintln!`s "skipping" and returns `Ok` reports success while
/// having asserted nothing — the exact "passing for the wrong reason" this
/// file exists to rule out, and invisible in a green run. Binding a loopback
/// port is not a legitimate environmental variation for a test suite that
/// already spawns servers on every other line; if it fails, the environment is
/// broken and the suite must say so out loud.
const PORT_BIND_NOTE: &str = "binding 127.0.0.1:0 failed, so this environment \
    cannot run the upstream-disconnect proofs. This is deliberately a FAILURE, \
    not a skip: a silent skip here would report the branch's end-to-end proof \
    as green without running it.";

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
///
/// The negative cache is deliberately given a very short TTL. A failed resolve
/// records a miss (`proxy.rs:255`) and every later `claim` short-circuits on it
/// (`proxy.rs:209`), so with a production-length TTL a single dropped loopback
/// search datagram — or one resolve attempted in the window while the IOC is
/// down — would make the *recovery* half of these tests unrecoverable for the
/// whole TTL, no matter how long they waited. That is a fixture artefact, not
/// the behaviour under test.
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
    let neg = Arc::new(NegativeCache::new(Duration::from_millis(100), 128));
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

/// Poll `claim` until it succeeds, for up to ~20s.
///
/// `claim` is a single UDP search with no retry of its own, so on a loaded
/// loopback one lost datagram is one spurious failure. Retrying is the
/// condition-based wait these tests need; it never turns "the PV is
/// unresolvable" into a pass, it only refuses to draw that conclusion from a
/// single attempt.
async fn claim_within(src: &Arc<GatewaySource>, pv: &str) -> bool {
    for _ in 0..100 {
        if src.claim(pv).await.is_some() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
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
        panic!("cannot bind a free loopback port: {PORT_BIND_NOTE}");
    };
    let mut backend = spawn_backend("GW:DIE", tcp, udp);
    let (src, _pool) = gateway_source(udp);

    assert!(
        claim_within(&src, "GW:DIE").await,
        "backend must be up"
    );
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
        panic!("cannot bind a free loopback port: {PORT_BIND_NOTE}");
    };
    let mut backend = spawn_backend("GW:E2E", up_tcp, up_udp);
    let (src, _pool) = gateway_source(up_udp);
    assert!(claim_within(&src, "GW:E2E").await, "backend must be up");

    // A downstream PvaServer publishing the gateway source — the same wiring
    // `spvirit_gateway::runtime` uses in production. Hold its registry so the
    // test can observe the teardown.
    let (down_tcp, down_udp, registry) = spawn_downstream(&src).await;

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
        claim_within(&src, "GW:E2E").await,
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
        panic!("cannot bind a free loopback port: {PORT_BIND_NOTE}");
    };
    let (Some(gw_tcp), Some(gw_udp)) = (free_tcp_port(), free_udp_port()) else {
        panic!("cannot bind a free gateway port: {PORT_BIND_NOTE}");
    };
    let Some(metrics_port) = free_tcp_port() else {
        panic!("cannot bind a free metrics port: {PORT_BIND_NOTE}");
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

// ---------------------------------------------------------------------------
// A genuine PVA client, on the real codec, over a real socket.
//
// The two tests above observe the teardown through the downstream server's own
// `MonitorRegistry` — an internal read. Neither of them would notice if the
// DESTROY_CHANNEL frame were never encoded, never addressed to the right
// channel, or never written to the socket. `spvirit_client::pvmonitor` cannot
// close that gap either: its monitor loop ignores command 8 (see the Task 8
// brief). So the client below is hand-driven: real handshake, real
// CREATE_CHANNEL, real MONITOR INIT/START, and a real read of whatever the
// server sends after the IOC dies.
// ---------------------------------------------------------------------------

/// The pvRequest bytes for an empty `field()` selection — the same constant
/// every raw client in this repo uses
/// (`spvirit-tools/src/spvirit_client/explore.rs:18`).
const PV_REQUEST_EMPTY: [u8; 6] = [0xfd, 0x02, 0x00, 0x80, 0x00, 0x00];

/// The ioid this file's raw clients use for their single monitor.
const MON_IOID: u32 = 1;

/// `establish_channel` always creates its channel on cid 1
/// (`spvirit-client/src/client.rs:136`), so that is the cid the server must
/// name when it destroys this client's channel.
const CLIENT_CID: u32 = 1;

fn raw_opts(pv: &str, tcp: u16, udp: u16, server_addr: Option<SocketAddr>) -> PvOptions {
    let mut opts = PvOptions::new(pv.to_string());
    opts.server_addr = server_addr;
    opts.tcp_port = tcp;
    opts.udp_port = udp;
    opts.search_addr = Some("127.0.0.1".parse().unwrap());
    opts.bind_addr = Some("127.0.0.1".parse().unwrap());
    opts.timeout = Duration::from_secs(15);
    opts
}

/// Connect, handshake, create the channel, start a monitor, and return only
/// once a real monitor DATA frame has arrived — so the caller knows the
/// subscription is genuinely live before it kills anything.
async fn open_monitor(addr: SocketAddr, opts: &PvOptions) -> ChannelConn {
    // CREATE_CHANNEL is retried. The downstream server answers it out of the
    // source's `try_claim`, which is momentarily `Unknown` (bindings lock held)
    // or `No` (a just-recorded negative-cache miss) while a resolve is in
    // flight — a real client retries a refusal rather than concluding the PV
    // does not exist. Every attempt is a genuine, complete handshake; none of
    // this can manufacture a channel the server refused.
    let mut attempt_opts = opts.clone();
    attempt_opts.timeout = Duration::from_secs(3);
    let mut established = None;
    let mut last_err = String::new();
    for _ in 0..20 {
        match establish_channel(addr, [0u8; 12], &attempt_opts).await {
            Ok(c) => {
                established = Some(c);
                break;
            }
            Err(e) => {
                last_err = e.to_string();
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
    let Some(mut conn) = established else {
        panic!("a real client must be able to create the channel; last error: {last_err}");
    };
    let (version, is_be) = (conn.version, conn.is_be);

    let init = encode_monitor_request(
        conn.sid,
        MON_IOID,
        0x08,
        &PV_REQUEST_EMPTY,
        version,
        is_be,
    );
    conn.stream
        .write_all(&init)
        .await
        .expect("write monitor init");
    read_until(
        &mut conn.stream,
        opts.timeout,
        &mut conn.reassembler,
        |cmd| {
            matches!(cmd, PvaPacketCommand::Op(op)
                if op.command == 13 && op.ioid == MON_IOID && (op.subcmd & 0x08) != 0)
        },
    )
    .await
    .expect("monitor init response");

    let start = encode_monitor_request(conn.sid, MON_IOID, 0x44, &[], version, is_be);
    conn.stream
        .write_all(&start)
        .await
        .expect("write monitor start");
    read_until(
        &mut conn.stream,
        opts.timeout,
        &mut conn.reassembler,
        |cmd| {
            matches!(cmd, PvaPacketCommand::Op(op)
                if op.command == 13 && op.ioid == MON_IOID && (op.subcmd & 0x08) == 0)
        },
    )
    .await
    .expect("the monitor must deliver real data while the backend is alive");

    conn
}

/// Spawn a downstream `PvaServer` publishing `src`, and wait until it accepts
/// TCP connections — no fixed sleep as the synchronisation mechanism.
async fn spawn_downstream(
    src: &Arc<GatewaySource>,
) -> (u16, u16, Arc<spvirit_server::monitor::MonitorRegistry>) {
    // The ports are chosen HERE, and a server that fails to come up on them is
    // retried on fresh ones. `free_tcp_port` only proves a port was free at the
    // instant it was probed: with several of these tests running concurrently
    // another one can take it in between, `run()` then fails its bind, and the
    // test that follows sees "connection refused" from a client that is
    // perfectly correct. Retrying the *fixture* removes that whole failure mode
    // without touching what is being asserted.
    let mut last = String::from("no attempt was made");
    for _ in 0..8 {
        let (Some(tcp), Some(udp)) = (free_tcp_port(), free_udp_port()) else {
            last = "cannot bind a free port in this environment".to_string();
            continue;
        };
        let published: Arc<dyn Source> = Arc::clone(src) as Arc<dyn Source>;
        let mut down = PvaServer::builder()
            .listen_ip("127.0.0.1".parse().unwrap())
            .advertise_ip("127.0.0.1".parse().unwrap())
            .port(tcp)
            .udp_port(udp)
            .source("gateway", 0, published)
            .build();
        // The registry is handed back so a caller can watch the teardown from
        // the inside as well as from the wire.
        let registry = down.monitor_registry();
        // Dropping the handle detaches the task, which is what the tests want:
        // the server must outlive this function.
        let handle = tokio::spawn(async move {
            let _ = down.run().await;
        });

        let mut ready = false;
        for _ in 0..200 {
            if handle.is_finished() {
                break;
            }
            if tokio::net::TcpStream::connect(("127.0.0.1", tcp))
                .await
                .is_ok()
            {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // Re-check after the probe: a bind failure can land just after the
        // first successful connect to whatever else holds the port.
        tokio::time::sleep(Duration::from_millis(50)).await;
        if ready && !handle.is_finished() {
            return (tcp, udp, registry);
        }
        last = match handle.is_finished() {
            true => "the downstream server's run() returned early (port taken?)".to_string(),
            false => "the downstream server never started listening".to_string(),
        };
    }
    panic!("could not start a downstream server: {last}");
}

/// Proof that the teardown reaches a REAL client, addressed to the REAL
/// channel: a hand-driven PVA client establishes a monitor through the
/// gateway, the IOC dies, and the client must read a DESTROY_CHANNEL naming
/// the very sid the server handed it and the cid it asked for.
///
/// This is what the registry-level assertions above cannot see. It fails if
/// the monitor's ioid is never bound to its channel at MONITOR INIT
/// (`spvirit-server/src/handler.rs:1910`), because then the teardown cannot
/// resolve a sid and sends nothing at all; and it fails if the frame is
/// encoded but never written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_client_receives_destroy_channel_naming_its_own_channel() {
    const PV: &str = "GW:WIRE";

    let (Some(up_tcp), Some(up_udp)) = (free_tcp_port(), free_udp_port()) else {
        panic!("cannot bind a free loopback port: {PORT_BIND_NOTE}");
    };
    let mut backend = spawn_backend(PV, up_tcp, up_udp);
    let (src, _pool) = gateway_source(up_udp);
    assert!(claim_within(&src, PV).await, "backend must be up");
    let (down_tcp, down_udp, _registry) = spawn_downstream(&src).await;

    let addr: SocketAddr = format!("127.0.0.1:{down_tcp}").parse().expect("loopback addr");
    let opts = raw_opts(PV, down_tcp, down_udp, Some(addr));
    let mut conn = open_monitor(addr, &opts).await;
    let sid = conn.sid;

    backend.kill();

    let raw = read_until(
        &mut conn.stream,
        Duration::from_secs(30),
        &mut conn.reassembler,
        |cmd| matches!(cmd, PvaPacketCommand::DestroyChannel(_)),
    )
    .await
    .expect(
        "a real PVA client must be sent DESTROY_CHANNEL once its upstream dies; \
         it got silence (or a dropped connection) instead",
    );
    let mut pkt = PvaPacket::new(&raw);
    let Some(PvaPacketCommand::DestroyChannel(destroy)) = pkt.decode_payload() else {
        panic!("the frame that matched DestroyChannel must decode as one");
    };
    assert_eq!(
        destroy.sid, sid,
        "DESTROY_CHANNEL must name the sid the server assigned this client's \
         channel, resolved from the live subscription's ioid"
    );
    assert_eq!(
        destroy.cid, CLIENT_CID,
        "DESTROY_CHANNEL must name the cid the client created the channel with"
    );
}

/// The recovery half, client-driven exactly as the spec intends: there is no
/// gateway-side reconnect loop, so the client acts on the DESTROY_CHANNEL by
/// re-searching. It must find the gateway over a real UDP search and get real
/// data through a fresh channel once the IOC is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_client_re_searches_and_gets_data_again_after_destroy_channel() {
    const PV: &str = "GW:REVIVE";

    let (Some(up_tcp), Some(up_udp)) = (free_tcp_port(), free_udp_port()) else {
        panic!("cannot bind a free loopback port: {PORT_BIND_NOTE}");
    };
    let mut backend = spawn_backend(PV, up_tcp, up_udp);
    let (src, _pool) = gateway_source(up_udp);
    assert!(claim_within(&src, PV).await, "backend must be up");
    let (down_tcp, down_udp, _registry) = spawn_downstream(&src).await;

    let addr: SocketAddr = format!("127.0.0.1:{down_tcp}").parse().expect("loopback addr");
    let opts = raw_opts(PV, down_tcp, down_udp, Some(addr));
    let mut conn = open_monitor(addr, &opts).await;

    backend.kill();

    // The client learns its monitor is dead the only way a real PVA client
    // can: the frame on the wire.
    read_until(
        &mut conn.stream,
        Duration::from_secs(30),
        &mut conn.reassembler,
        |cmd| matches!(cmd, PvaPacketCommand::DestroyChannel(_)),
    )
    .await
    .expect("the client must be told its channel is gone before it can recover");
    drop(conn);

    let _backend2 = spawn_backend(PV, up_tcp, up_udp);

    // A genuine UDP search, retried: this is the re-search a real client does
    // after a DESTROY_CHANNEL, and it must resolve to the gateway again.
    let search_opts = raw_opts(PV, down_tcp, down_udp, None);
    let mut resolved = None;
    for _ in 0..60 {
        if let Ok((found, _guid)) = resolve_pv_server(&search_opts).await {
            resolved = Some(found);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let Some(found) = resolved else {
        panic!("a re-searching client must find the gateway again after the IOC returns");
    };
    assert_eq!(
        found.port(),
        down_tcp,
        "the re-search must resolve to the gateway's own downstream server"
    );

    // `open_monitor` only returns once a real DATA frame has arrived, so
    // reaching the end of this test IS the proof that the client recovered.
    let _recovered = open_monitor(found, &opts).await;
}
