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

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_gateway::access::AccessControl;
use spvirit_gateway::cache::negative::NegativeCache;
use spvirit_gateway::config::GatewayConfig;
use spvirit_gateway::loopguard::LoopGuard;
use spvirit_gateway::proxy::GatewaySource;
use spvirit_gateway::upstream::UpstreamPool;
use spvirit_server::PvaServer;
use spvirit_server::pvstore::Source;
use spvirit_types::{NtPayload, PvValue, ScalarValue};

/// Shorthand for a decoded `f64` scalar, used by the access-control tests
/// below that only need *some* value to attempt a `put` with.
fn decoded_f64(v: f64) -> DecodedValue {
    DecodedValue::Float64(v)
}

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
    spawn_gateway_with_access(
        configure,
        getholdoff_ms,
        Arc::new(AccessControl::new(false, None, None)),
    )
    .await
}

/// Like [`spawn_gateway`], but with an explicit `AccessControl` (permissive
/// by default there) gating the returned `GatewaySource`. Serves a single
/// `ao("IT:PV", ...)` upstream, which is enough for the readOnly/pvlist-deny
/// tests that only need *some* claimable PV name.
async fn spawn_gateway_with_ac(access: AccessControl) -> Option<(GatewaySource, Arc<UpstreamPool>)> {
    spawn_gateway_with_ac_serving(access, "IT:PV").await
}

/// Like [`spawn_gateway_with_ac`], but serving `serve_name` upstream instead
/// of the fixed `"IT:PV"` — needed by the alias test, where the downstream
/// claim name (`PUB:PV`) differs from the upstream name it must resolve to
/// (`REAL:PV`).
async fn spawn_gateway_with_ac_serving(
    access: AccessControl,
    serve_name: &str,
) -> Option<(GatewaySource, Arc<UpstreamPool>)> {
    let serve_name = serve_name.to_string();
    spawn_gateway_with_access(move |b| b.ao(serve_name, 1.0), 0, Arc::new(access)).await
}

async fn spawn_gateway_with_access(
    configure: impl FnOnce(spvirit_server::PvaServerBuilder) -> spvirit_server::PvaServerBuilder,
    getholdoff_ms: u32,
    access: Arc<AccessControl>,
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
    let guard = Arc::new(LoopGuard::build(&cfg, &cfg.servers[0], std::collections::HashSet::new()));

    let src = GatewaySource::new(
        pool.clone(),
        vec!["it-client".into()],
        neg,
        guard,
        getholdoff_ms,
        access,
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
    let guard = Arc::new(LoopGuard::build(&cfg, &cfg.servers[0], std::collections::HashSet::new()));

    let access = Arc::new(AccessControl::new(false, None, None));
    let src = GatewaySource::new(pool, vec!["it-client".into()], neg, guard, 0, access);

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_dedup_and_fanout() {
    let Some((src, pool)) = spawn_gateway(|b| b.ao("IT:MON", 1.0), 0).await else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };

    assert!(src.claim("IT:MON").await.is_some());

    let mut rx1 = src
        .subscribe("IT:MON")
        .await
        .expect("first subscribe should succeed");
    let mut rx2 = src
        .subscribe("IT:MON")
        .await
        .expect("second subscribe should succeed");

    assert_eq!(
        src.upstream_monitor_count(),
        1,
        "two subscribes to the same PV must share a single upstream monitor"
    );

    // Give the upstream monitor a moment to complete its MONITOR INIT and
    // deliver the initial value before we drive a change.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let upstream_client = pool.client("it-client").expect("client in pool");
    upstream_client
        .pvput("IT:MON", 5.0f64)
        .await
        .expect("direct upstream pvput should succeed");

    async fn wait_for_value(rx: &mut tokio::sync::mpsc::Receiver<NtPayload>, target: f64) -> bool {
        loop {
            match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
                Ok(Some(payload)) => {
                    let v = extract_f64_value(&payload);
                    if (v - target).abs() < 1e-6 {
                        return true;
                    }
                    // Might be the initial ~1.0 update; keep draining.
                }
                Ok(None) => return false, // channel closed
                Err(_) => return false,   // timed out
            }
        }
    }

    assert!(
        wait_for_value(&mut rx1, 5.0).await,
        "first subscriber should observe the updated value ~5.0"
    );
    assert!(
        wait_for_value(&mut rx2, 5.0).await,
        "second subscriber should observe the updated value ~5.0"
    );
}

/// FIX 2 regression: monitored payloads must carry the real upstream
/// `struct_id`, not an empty string — a downstream NT-aware client can't
/// recognize monitored values otherwise (`get` already reports this
/// correctly via `nt_payload_from_get`; `subscribe` used to hardcode
/// `String::new()` in its call to `nt_payload_from_decoded`).
///
/// The in-process `ao` record's `pvinfo` reports
/// `struct_id: Some("epics:nt/NTScalar:1.0")` (see
/// `spvirit_server::simple_store::nt_scalar_desc`), so this asserts the
/// exact literal rather than only asserting parity with `get`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_payload_carries_the_real_struct_id() {
    let Some((src, pool)) = spawn_gateway(|b| b.ao("IT:MON2", 1.0), 0).await else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };

    assert!(src.claim("IT:MON2").await.is_some());

    // Parity check: `get`'s struct_id is the ground truth this PV reports.
    let get_payload = src.get("IT:MON2").await.expect("get should return Some");
    let NtPayload::Generic {
        struct_id: get_struct_id,
        ..
    } = &get_payload
    else {
        panic!("expected NtPayload::Generic from get");
    };
    assert_eq!(
        get_struct_id, "epics:nt/NTScalar:1.0",
        "sanity check: an ao record must report NTScalar via get"
    );

    let mut rx = src
        .subscribe("IT:MON2")
        .await
        .expect("subscribe should succeed");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let upstream_client = pool.client("it-client").expect("client in pool");
    upstream_client
        .pvput("IT:MON2", 9.0f64)
        .await
        .expect("direct upstream pvput should succeed");

    let payload = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("monitor update should arrive within timeout")
        .expect("channel should not be closed");
    let NtPayload::Generic {
        struct_id: sub_struct_id,
        ..
    } = &payload
    else {
        panic!("expected NtPayload::Generic from subscribe");
    };

    assert!(
        !sub_struct_id.is_empty(),
        "monitored payload must carry a non-empty struct_id"
    );
    assert_eq!(
        sub_struct_id, get_struct_id,
        "subscribe's struct_id must match get's struct_id for the same PV"
    );
    assert_eq!(
        sub_struct_id, "epics:nt/NTScalar:1.0",
        "exact upstream struct_id for an ao/NTScalar record"
    );
}

/// Task 12: `Source::put` resolves the binding, converts the `DecodedValue`
/// to JSON, and forwards it upstream via `pvput`. Uses `getholdoff_ms = 0`
/// so the post-put `get` round-trips upstream instead of returning the
/// getholdoff-cached pre-put value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_forwards_the_value_upstream() {
    let Some((src, _pool)) = spawn_gateway(|b| b.ao("IT:SETPOINT", 1.0), 0).await else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };

    assert!(src.claim("IT:SETPOINT").await.is_some());

    let result = src
        .put("IT:SETPOINT", &DecodedValue::Float64(5.0))
        .await
        .expect("put to a claimed PV should succeed");
    assert!(
        result.is_empty(),
        "gateway put has no forward-link propagation to report"
    );

    let payload = src
        .get("IT:SETPOINT")
        .await
        .expect("get should return Some");
    let value = extract_f64_value(&payload);
    assert!(
        (value - 5.0).abs() < 1e-6,
        "expected the get to reflect the put, got {value}"
    );
}

/// A `put` to a name that has never been `claim`ed has no binding to resolve
/// and must fail rather than silently succeed or panic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_to_unclaimed_name_is_an_error() {
    let Some((src, _pool)) = spawn_gateway(|b| b.ao("IT:SETPOINT2", 1.0), 0).await else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };

    // Deliberately not calling `src.claim(...)` first.
    let result = src.put("IT:SETPOINT2", &DecodedValue::Float64(5.0)).await;
    assert!(result.is_err(), "put to an unclaimed name must return Err");
}

/// M1's `names()` deliberately reports the set of PVs this `GatewaySource`
/// has already claimed (the keys of its binding cache), not a full upstream
/// namespace enumeration via `pvlist` fan-out — the gateway has no per-
/// upstream `SocketAddr` to drive `pvlist`/`pvlist_with_fallback` with (see
/// the doc comment on `GatewaySource::names` for the full rationale). This
/// test pins that contract: empty before any `claim`, and exactly the
/// claimed names (sorted, deduped) afterward.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn names_reflects_claimed_bindings() {
    let Some((src, _pool)) =
        spawn_gateway(|b| b.ai("IT:AAA", 1.0).ai("IT:BBB", 2.0), 0).await
    else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };

    assert!(
        src.names().await.is_empty(),
        "names() must be empty before anything is claimed"
    );

    assert!(src.claim("IT:AAA").await.is_some());
    assert!(src.claim("IT:BBB").await.is_some());

    let mut names = src.names().await;
    names.sort();
    assert_eq!(names, vec!["IT:AAA".to_string(), "IT:BBB".to_string()]);
}

/// End-to-end regression for "monitors don't get updates from gateways": a
/// real TCP monitor client, connected to a downstream `PvaServer` that
/// publishes a `GatewaySource`, must receive ongoing updates when the value
/// changes upstream — not just the initial snapshot.
///
/// The earlier `monitor_dedup_and_fanout` test reads `src.subscribe()`
/// directly and so proves the gateway's own subscribe/upstream-monitor path
/// works; it never exercises the *server's* TCP monitor delivery. That seam
/// is where the bug lived: the `cmd=13` monitor handler sent one snapshot at
/// monitor-start and then relied solely on `notify_monitors`, which fires
/// only for local PUTs / self-notifying stores. A `GatewaySource` never
/// self-notifies — its updates arrive only via `Source::subscribe()`, which
/// the handler never drained — so a downstream monitor client saw the first
/// value and then silence.
///
/// Pre-fix this times out waiting for the second update. Post-fix the server
/// pumps `Source::subscribe()` into the monitor registry for sources that do
/// not push their own updates, and the update arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_monitor_delivers_ongoing_updates_to_a_tcp_client() {
    use std::ops::ControlFlow;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc as std_mpsc;

    use spvirit_client::{PvOptions, pvmonitor};

    // Upstream server hosting the PV, plus a gateway source wired to it.
    let Some((src, pool)) = spawn_gateway(|b| b.ao("GW:MON", 1.0), 0).await else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };
    assert!(src.claim("GW:MON").await.is_some());

    // A downstream PvaServer that publishes the gateway source to TCP clients
    // — the same wiring `spvirit_gateway::runtime` uses in production.
    let (Some(down_tcp), Some(down_udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping test: cannot bind a free downstream port");
        return;
    };
    let down = PvaServer::builder()
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(down_tcp)
        .udp_port(down_udp)
        .source("gateway", 0, Arc::new(src))
        .build();
    tokio::spawn(async move {
        let _ = down.run().await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // A real TCP monitor client against the downstream server. Explicit
    // `server_addr` bypasses UDP search (mirrors `spvirit-ioc`'s
    // `host_writes.rs`). The callback breaks after two updates: the initial
    // snapshot plus the out-of-band change below.
    let (tx, rx) = std_mpsc::channel();
    let mut opts = PvOptions::new("GW:MON".to_string());
    opts.server_addr = Some(format!("127.0.0.1:{down_tcp}").parse().expect("loopback addr"));
    opts.tcp_port = down_tcp;
    opts.udp_port = down_udp;
    opts.search_addr = Some("127.0.0.1".parse().unwrap());
    opts.bind_addr = Some("127.0.0.1".parse().unwrap());
    opts.timeout = Duration::from_secs(5);

    let count = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        let _ = pvmonitor(&opts, move |update| {
            let _ = tx.send(format!("{update:?}"));
            if count.fetch_add(1, Ordering::SeqCst) + 1 >= 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .await;
    });

    // The initial snapshot must arrive (this part already worked pre-fix).
    let _initial = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("initial monitor update");

    // Let the downstream pump and the upstream monitor both arm before the
    // change, so the update is delivered live rather than missed.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Change the value out-of-band, directly upstream — exactly what a real
    // device does. The gateway never sees a local PUT.
    let upstream = pool.client("it-client").expect("client in pool");
    upstream
        .pvput("GW:MON", 42.0f64)
        .await
        .expect("direct upstream pvput should succeed");

    // The ongoing update must reach the TCP monitor client. This is the
    // regression: pre-fix the second update never arrives and this times out.
    let update = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("an ongoing gateway update must reach a TCP monitor client");
    assert!(
        update.contains("42"),
        "monitor should carry the updated value ~42.0, got: {update}"
    );
}

/// RPC forwarding to upstream servers is out of scope for M1: `PvaClient`
/// exposes no general-purpose RPC call, only an internal `pvlist`-via-RPC
/// helper. `GatewaySource::rpc` therefore always returns an explicit,
/// gateway-specific error rather than silently inheriting the `Source`
/// trait's generic default (spec §14 gap, deferred to a later milestone).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_is_not_implemented_in_m1() {
    let Some((src, _pool)) = spawn_gateway(|b| b.ai("IT:AAA", 1.0), 0).await else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };

    assert!(src.claim("IT:AAA").await.is_some());

    let result = src.rpc("IT:AAA", &DecodedValue::Null).await;
    assert!(result.is_err(), "gateway rpc() must return Err in M1");
}

/// A `readOnly: true` `AccessControl` denies `put` outright even though the
/// PV was successfully claimed — readOnly overrides everything for
/// writes/RPCs (`Op::Get` is unaffected, hence the successful `claim` above
/// it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readonly_rejects_put() {
    let ac = AccessControl::new(true, None, None);
    let Some((src, _pool)) = spawn_gateway_with_ac(ac).await else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };
    src.claim("IT:PV").await.expect("claim");
    let err = src
        .put("IT:PV", &decoded_f64(1.0))
        .await
        .unwrap_err();
    assert!(
        err.to_lowercase().contains("read") || err.to_lowercase().contains("denied"),
        "expected a readOnly/denied error, got: {err}"
    );
}

/// A pvlist `DENY` rule makes the matched PV invisible: `claim` must return
/// `None` rather than surfacing an upstream-miss-shaped error — the PV is
/// simply not there as far as this gateway is concerned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pvlist_deny_makes_pv_unclaimable() {
    let ac = AccessControl::new(
        false,
        Some(spvirit_gateway::access::pvlist::parse_pvlist("IT:.* DENY\n.* ALLOW").unwrap()),
        None,
    );
    let Some((src, _pool)) = spawn_gateway_with_ac(ac).await else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };
    assert!(src.claim("IT:PV").await.is_none());
}

/// A pvlist `ALIAS` rule rewrites the *upstream* target while the downstream
/// key stays the requested name: clients see `PUB:PV`, but the gateway
/// resolves and reads it against the real upstream `REAL:PV`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alias_claims_upstream_name() {
    let ac = AccessControl::new(
        false,
        Some(
            spvirit_gateway::access::pvlist::parse_pvlist(
                "PUB:(.*) ALIAS REAL:\\1\n.* ALLOW",
            )
            .unwrap(),
        ),
        None,
    );
    let Some((src, _pool)) = spawn_gateway_with_ac_serving(ac, "REAL:PV").await else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };

    src.claim("PUB:PV").await.expect("claim should alias to REAL:PV upstream");

    // get routes to REAL:PV upstream (seeded to 1.0 by
    // `spawn_gateway_with_ac_serving`'s `b.ao(serve_name, 1.0)`) and returns
    // its value under the downstream name.
    let payload = src.get("PUB:PV").await.expect("get");
    let value = extract_f64_value(&payload);
    assert!((value - 1.0).abs() < 1e-6, "expected ~1.0 from REAL:PV upstream, got {value}");
}

/// End-to-end HAG coverage: a real TCP client connects over loopback, and an
/// ACF `HAG` rule keyed on the client's actual peer IP gates whether its
/// `put` is allowed. This is deliberately the host/HAG half only — the
/// user/UAG half is already covered at the `AccessControl::decide` unit
/// level (Task 9's `acf_grants_write_by_uag_and_hag` and friends), since
/// simulating a decoded `ca` *user* through a real in-crate client would
/// require driving `PvOptions::authnz_host`/credentials plumbing this test
/// doesn't otherwise need; the peer IP, on the other hand, is real and free
/// to assert on over loopback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hag_rule_gates_put_from_a_real_tcp_client() {
    use spvirit_client::{PvOptions, pvput};
    use spvirit_gateway::access::acf::parse_acf;
    use spvirit_gateway::access::pvlist::parse_pvlist;

    // HAG(ctl) deliberately does NOT include 127.0.0.1 -- the loopback
    // address the real TCP client below connects from -- so the WRITE grant
    // never applies and the put must be denied.
    let pvlist = parse_pvlist(".* ALLOW RW 1").unwrap();
    let acf = parse_acf("HAG(ctl){10.0.0.9}\nASG(RW){ RULE(1, WRITE){ HAG(ctl) } }").unwrap();
    let access = Arc::new(AccessControl::new(false, Some(pvlist), Some(acf)));

    let Some((src, _pool)) =
        spawn_gateway_with_access(|b| b.ao("HAG:PV", 1.0), 0, access).await
    else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };
    src.claim("HAG:PV").await.expect("claim");

    let (Some(down_tcp), Some(down_udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping test: cannot bind a free downstream port");
        return;
    };
    let down = PvaServer::builder()
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(down_tcp)
        .udp_port(down_udp)
        .source("gateway", 0, Arc::new(src))
        .build();
    tokio::spawn(async move {
        let _ = down.run().await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut opts = PvOptions::new("HAG:PV".to_string());
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

    let result = pvput(&opts, 42.0f64).await;
    assert!(
        result.is_err(),
        "put from a peer host outside HAG(ctl) must be denied"
    );
}

/// Regression for the trust-boundary bug: a client that spoofs a trusted
/// hostname via its `ca` connection-validation credentials must NOT be able
/// to satisfy a HAG rule that the real socket peer IP does not match. HAG
/// (ctl) here lists `trusted.example.invalid` — a name the real TCP client
/// below is not connecting from, but *does* assert via
/// `PvOptions::authnz_host` (which flows into the `ca` credential exchange
/// per `client.rs`/`search.rs`'s `resolved_authnz_host`, and is decoded
/// downstream by `handler.rs`'s `ConnectionValidation` handling into
/// `request_ctx::set_credentials`, exactly the same path a real spoofing
/// client would use). If `current_identity()` preferred that asserted host
/// over the real peer IP (the bug), the HAG rule would match and the put
/// would be wrongly allowed; with peer-IP-authoritative matching (the fix)
/// the loopback peer never matches `HAG(ctl)` and the put is denied.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hag_rule_ignores_a_spoofed_ca_host() {
    use spvirit_client::{PvOptions, pvput};
    use spvirit_gateway::access::acf::parse_acf;
    use spvirit_gateway::access::pvlist::parse_pvlist;

    // HAG(ctl) lists only a hostname the real TCP client below never
    // connects from -- it connects over 127.0.0.1. The client asserts this
    // *exact* trusted hostname via its `ca` credentials, so if that
    // self-asserted host were ever preferred over the real peer IP, this
    // rule would (wrongly) match and the put would be allowed.
    let pvlist = parse_pvlist(".* ALLOW RW 1").unwrap();
    let acf = parse_acf(
        "HAG(ctl){trusted.example.invalid}\nASG(RW){ RULE(1, WRITE){ HAG(ctl) } }",
    )
    .unwrap();
    let access = Arc::new(AccessControl::new(false, Some(pvlist), Some(acf)));

    let Some((src, _pool)) =
        spawn_gateway_with_access(|b| b.ao("HAG:SPOOF:PV", 1.0), 0, access).await
    else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };
    src.claim("HAG:SPOOF:PV").await.expect("claim");

    let (Some(down_tcp), Some(down_udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping test: cannot bind a free downstream port");
        return;
    };
    let down = PvaServer::builder()
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(down_tcp)
        .udp_port(down_udp)
        .source("gateway", 0, Arc::new(src))
        .build();
    tokio::spawn(async move {
        let _ = down.run().await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut opts = PvOptions::new("HAG:SPOOF:PV".to_string());
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
    // The spoof: assert the exact hostname HAG(ctl) trusts, even though the
    // TCP connection is really coming from 127.0.0.1.
    opts.authnz_host = Some("trusted.example.invalid".to_string());

    let result = pvput(&opts, 42.0f64).await;
    assert!(
        result.is_err(),
        "a client asserting a HAG-trusted hostname via ca credentials must \
         still be denied, because the real peer IP (127.0.0.1) is not in \
         HAG(ctl) -- the self-asserted host must never satisfy the rule"
    );
}

/// End-to-end UAG coverage, mirroring `hag_rule_gates_put_from_a_real_tcp_client`
/// but gating on the decoded `ca` *user* instead of the peer host. Per spec
/// §6.3, UAG is authorization, not authentication: the `user` a client
/// asserts via its `ca` credential exchange is trusted as-is (unlike `host`,
/// where the real socket peer IP is authoritative -- see
/// `hag_rule_ignores_a_spoofed_ca_host`). So a real TCP client that sets
/// `PvOptions::authnz_user` to a UAG-listed name legitimately gains access;
/// a client asserting an unlisted user is denied. The identity flows through
/// exactly the same path the HAG tests exercise for host: `client.rs`/
/// `search.rs`'s `resolved_authnz_user` puts it on the wire via the `ca`
/// credential exchange, `handler.rs`'s `ConnectionValidation` handling
/// decodes it into `request_ctx::set_credentials`, and `proxy.rs`'s
/// `current_identity()` reads it back out as `Identity::user` for
/// `AccessControl::decide`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uag_rule_gates_put_by_ca_user() {
    use spvirit_client::{PvOptions, pvput};
    use spvirit_gateway::access::acf::parse_acf;
    use spvirit_gateway::access::pvlist::parse_pvlist;

    // UAG(ops) lists only "operator". ASG(RW)'s WRITE rule is gated on
    // UAG(ops); there is no unconditional fallback rule, so any user other
    // than "operator" (including no asserted user at all) is denied.
    let pvlist = parse_pvlist(".* ALLOW RW 1").unwrap();
    let acf = parse_acf("UAG(ops){operator}\nASG(RW){ RULE(1, WRITE){ UAG(ops) } }").unwrap();
    let access = Arc::new(AccessControl::new(false, Some(pvlist), Some(acf)));

    let Some((src, _pool)) =
        spawn_gateway_with_access(|b| b.ao("UAG:PV", 1.0), 0, access).await
    else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };
    src.claim("UAG:PV").await.expect("claim");

    let (Some(down_tcp), Some(down_udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping test: cannot bind a free downstream port");
        return;
    };
    let src = Arc::new(src);
    let down = PvaServer::builder()
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(down_tcp)
        .udp_port(down_udp)
        .source("gateway", 0, src.clone())
        .build();
    tokio::spawn(async move {
        let _ = down.run().await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    fn base_opts(down_tcp: u16, down_udp: u16) -> PvOptions {
        let mut opts = PvOptions::new("UAG:PV".to_string());
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
        opts
    }

    // Negative half first: a client asserting no ca user at all is not in
    // UAG(ops), so the put must be denied.
    let deny_opts = base_opts(down_tcp, down_udp);
    let deny_result = pvput(&deny_opts, 7.0f64).await;
    assert!(
        deny_result.is_err(),
        "put from a client asserting no ca user must be denied (not in UAG(ops))"
    );

    // Positive half: a client asserting the UAG-listed user "operator" via
    // its ca credentials must have its put succeed.
    let mut allow_opts = base_opts(down_tcp, down_udp);
    allow_opts.authnz_user = Some("operator".to_string());
    let allow_result = pvput(&allow_opts, 42.0f64).await;
    assert!(
        allow_result.is_ok(),
        "put from a client asserting the UAG(ops)-listed user \"operator\" \
         must succeed, got: {allow_result:?}"
    );

    let payload = src.get("UAG:PV").await.expect("get should return Some");
    let value = extract_f64_value(&payload);
    assert!(
        (value - 42.0).abs() < 1e-6,
        "expected the get to reflect the allowed put, got {value}"
    );
}

/// Spec §4's hot-loop breaker, in isolation: with a binding present
/// `try_claim` answers `Yes` from the bindings map alone, without contacting
/// upstream — so a client that was just told DESTROY_CHANNEL would re-search,
/// be answered `Yes` immediately, and hit the same dead upstream again. After
/// `retire_binding` the answer must fall back to `Unknown`, which routes the
/// search through a real background `claim` instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retire_binding_turns_try_claim_from_yes_back_into_unknown() {
    use spvirit_server::pvstore::TryClaim;

    let Some((src, _pool)) = spawn_gateway(|b| b.ao("IT:RETIRE", 1.0), 0).await else {
        eprintln!("Skipping test: cannot bind a free port in this environment");
        return;
    };

    assert!(src.claim("IT:RETIRE").await.is_some());
    assert!(
        matches!(src.try_claim("IT:RETIRE"), TryClaim::Yes),
        "a claimed name must be answerable from the bindings map with no I/O"
    );
    assert_eq!(src.names().await, vec!["IT:RETIRE".to_string()]);
    assert_eq!(
        src.upstream_monitor_deaths(),
        0,
        "no upstream has died in this test"
    );

    src.retire_binding("IT:RETIRE");

    assert!(
        matches!(src.try_claim("IT:RETIRE"), TryClaim::Unknown),
        "after the binding is retired the gateway must no longer answer Yes \
         from memory; it must fall through to a real background resolve"
    );
    assert!(
        src.names().await.is_empty(),
        "a retired binding must also leave names()"
    );
}
