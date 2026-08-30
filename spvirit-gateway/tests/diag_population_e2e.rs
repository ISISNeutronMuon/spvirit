//! Task 15: END-TO-END verification that the whole diagnostic-population
//! feature (Tasks 1-14) works as an assembled system, not just per-unit.
//!
//! This stands up a *real* gateway via [`Runtime::from_config`] — the exact
//! production wiring that threads ONE [`spvirit_server::diag::ClientRegistry`]
//! and ONE [`spvirit_server::diag::BandwidthCounters`] into every server and
//! every upstream client, runs the 1 Hz [`spvirit_gateway::status::
//! BandwidthSampler`], and serves the status PVs from real handles — in front
//! of a real in-process upstream [`PvaServer`]. It then:
//!
//!   1. connects a real downstream monitor client (asserting a `ca` user, so
//!      the client registry records it as `user@ip`);
//!   2. drives continuous upstream value changes (a background pump) so every
//!      change flows upstream-monitor -> gateway -> downstream-monitor,
//!      crediting `us_bypv_rx` (upstream RX), `ds_bypv_tx` (downstream per-PV
//!      TX) and the per-host registry TX (`ds_byhost_tx`) on every sampler
//!      interval — this is what keeps the per-second RATE rows positive
//!      rather than a single delta a later idle tick would zero out;
//!   3. also issues an explicit downstream GET through the gateway, forcing a
//!      real upstream `pvget` (a second, unambiguous "upstream fetch");
//!   4. polls the shared [`RateSnapshot`] (the sampler's live output) until
//!      the required rows appear — condition-based, not a bare sleep, so it is
//!      robust to sampler-tick alignment;
//!   5. asserts, against a [`StatusSource`] built from the SAME real handles
//!      the running gateway uses (same registry + rate-snapshot `Arc`s):
//!        - `clients` lists `user@ip` for the connected peer;
//!        - `ds:bypv:tx` AND `ds:byhost:tx` have non-empty rows, `rate > 0`;
//!        - `us:bypv:rx` (or `us:byhost:rx`) has a row after the upstream fetch;
//!        - encoding the `stats`, `clients`, AND a bandwidth-table payload each
//!          yields `timeStamp.secondsPastEpoch != 0` — the exact PVs that
//!          showed a 1990 timestamp on TALOS.
//!
//! The harness (free-port helpers, in-process upstream `PvaServer`, loopback
//! client wiring, `GatewayConfig` JSON, the downstream-TCP-monitor pattern)
//! mirrors `it_passthrough.rs` / `it_bidirectional.rs`; no new test deps are
//! introduced.

use std::collections::HashSet;
use std::net::{TcpListener, UdpSocket};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spvirit_client::{PvOptions, PvaClient, pvmonitor};
use spvirit_gateway::access::AccessControl;
use spvirit_gateway::cache::negative::NegativeCache;
use spvirit_gateway::config::GatewayConfig;
use spvirit_gateway::loopguard::LoopGuard;
use spvirit_gateway::proxy::GatewaySource;
use spvirit_gateway::runtime::Runtime;
use spvirit_gateway::status::{RateSnapshot, StatusHandles, StatusSource};
use spvirit_gateway::upstream::UpstreamPool;
use spvirit_server::PvaServer;
use spvirit_server::pvstore::Source;
use spvirit_types::{NtPayload, PvValue, ScalarValue};

const STATUS_PREFIX: &str = "GW:STS:";
const PV: &str = "E2E:PV";
const CA_USER: &str = "e2euser";

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

/// A direct loopback `PvaClient` (used to drive out-of-band upstream value
/// changes), mirroring `it_bidirectional.rs`'s `loopback_client`.
fn loopback_client(udp_port: u16) -> PvaClient {
    PvaClient::builder()
        .udp_port(udp_port)
        .search_addr("127.0.0.1".parse().unwrap())
        .bind_addr("127.0.0.1".parse().unwrap())
        .build()
}

/// The `secondsPastEpoch` of a [`NtPayload::ScalarArray`]'s (non-optional)
/// timestamp — used for the `clients` PV.
fn scalar_array_ts_secs(p: &NtPayload) -> i64 {
    let NtPayload::ScalarArray(a) = p else {
        panic!("expected NtPayload::ScalarArray, got {p:?}");
    };
    a.time_stamp.seconds_past_epoch
}

/// The `secondsPastEpoch` of a [`NtPayload::Table`]'s (optional) timestamp —
/// used for a bandwidth table.
fn table_ts_secs(p: &NtPayload) -> i64 {
    let NtPayload::Table(t) = p else {
        panic!("expected NtPayload::Table, got {p:?}");
    };
    t.time_stamp
        .as_ref()
        .expect("bandwidth table must carry a timestamp")
        .seconds_past_epoch
}

/// The first nested `timeStamp.secondsPastEpoch` inside a
/// [`NtPayload::Generic`] structure (the `stats` `epics:p2p/Stats:1.0` shape,
/// whose per-field `NTScalar` sub-structures each carry a `timeStamp`).
fn generic_first_ts_secs(p: &NtPayload) -> i64 {
    let NtPayload::Generic { fields, .. } = p else {
        panic!("expected NtPayload::Generic, got {p:?}");
    };
    for (_, v) in fields {
        let PvValue::Structure { fields: inner, .. } = v else {
            continue;
        };
        for (n, tv) in inner {
            if n != "timeStamp" {
                continue;
            }
            let PvValue::Structure { fields: tf, .. } = tv else {
                continue;
            };
            for (fname, fval) in tf {
                if fname == "secondsPastEpoch"
                    && let PvValue::Scalar(ScalarValue::I64(s)) = fval
                {
                    return *s;
                }
            }
        }
    }
    panic!("no nested timeStamp.secondsPastEpoch in {p:?}");
}

/// Rows for a plain `(name, rate)` bandwidth vector; `Some(rate)` for `name`.
fn rate_for(rows: &[(String, f64)], name: &str) -> Option<f64> {
    rows.iter().find(|(k, _)| k == name).map(|(_, r)| *r)
}

/// Poll the shared [`RateSnapshot`] until `pred` holds, returning the exact
/// snapshot that satisfied it (so assertions run against that clone, immune to
/// a later idle tick zeroing a rate). Condition-based with a coarse poll — the
/// non-flaky substitute for a bare fixed sleep, per the task's timing note.
async fn poll_snapshot(
    snapshot: &Arc<Mutex<RateSnapshot>>,
    timeout: Duration,
    pred: impl Fn(&RateSnapshot) -> bool,
) -> Option<RateSnapshot> {
    let deadline = Instant::now() + timeout;
    loop {
        {
            let s = snapshot.lock().unwrap();
            if pred(&s) {
                return Some(s.clone());
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn diagnostic_data_is_populated_end_to_end() {
    // ---- Ports ---------------------------------------------------------
    let (Some(up_tcp), Some(up_udp), Some(gw_tcp), Some(gw_udp)) = (
        free_tcp_port(),
        free_udp_port(),
        free_tcp_port(),
        free_udp_port(),
    ) else {
        eprintln!("Skipping test: cannot bind free ports in this environment");
        return;
    };

    // ---- Real upstream IOC (in-process) --------------------------------
    // `ao` so the pump below can `pvput` new values into it out-of-band.
    let upstream = PvaServer::builder()
        .ao(PV, 0.0)
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(up_tcp)
        .udp_port(up_udp)
        .build();
    tokio::spawn(async move {
        let _ = upstream.run().await;
    });
    tokio::time::sleep(Duration::from_millis(600)).await;

    // ---- Gateway config (with a status prefix) -------------------------
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
                "bcastport": {gw_udp},
                "statusprefix": "{STATUS_PREFIX}"
            }}]
        }}"#
    );
    let cfg = GatewayConfig::from_json_str(&cfg_json).expect("parse gateway config");

    // An inspection `GatewaySource` built from the SAME upstream config, used
    // only to construct the inspection `StatusSource` via the production
    // `from_gateway_with` builder (below). Built from `&cfg` before `cfg` is
    // moved into the `Runtime`.
    let insp_pool = Arc::new(UpstreamPool::from_config(&cfg));
    let insp_src = Arc::new(GatewaySource::new(
        insp_pool,
        vec!["up".into()],
        Arc::new(NegativeCache::new(Duration::from_secs(30), 128)),
        Arc::new(LoopGuard::build(&cfg, &cfg.servers[0], HashSet::new())),
        0,
        Arc::new(AccessControl::new(false, None, None)),
    ));

    // ---- Real gateway runtime (production wiring) ----------------------
    let rt = Runtime::from_config(cfg).expect("valid config builds a Runtime");
    // Clone the shared diag `Arc`s BEFORE `run()` consumes the runtime: these
    // are the SAME instances the running gateway's servers + sampler write
    // into, so the inspection `StatusSource` below sees exactly the live data
    // the gateway produces.
    let client_registry = rt.client_registry().clone();
    let rate_snapshot = rt.rate_snapshot().clone();

    // The inspection `StatusSource`: real handles over the shared registry +
    // rate snapshot — NOT `StatusHandles::test()`.
    let status = StatusSource::new(
        STATUS_PREFIX.to_string(),
        Arc::new(AccessControl::new(false, None, None)),
        StatusHandles::from_gateway_with(&insp_src, client_registry.clone(), rate_snapshot.clone()),
    );

    tokio::spawn(async move {
        let _ = rt.run().await;
    });
    tokio::time::sleep(Duration::from_millis(600)).await;

    // ---- Downstream monitor client (asserts a `ca` user) ---------------
    // A real TCP monitor client against the gateway's downstream server. It
    // asserts `authnz_user`, so the server's ConnectionValidation handling
    // records the connection in the registry as `e2euser@127.0.0.1`. The
    // callback never breaks, so the subscription (and its connection) stays
    // alive for the whole test, and each delivered update credits
    // `ds_bypv_tx` + the per-host registry TX.
    let mut mon_opts = PvOptions::new(PV.to_string());
    mon_opts.server_addr = Some(format!("127.0.0.1:{gw_tcp}").parse().expect("loopback addr"));
    mon_opts.tcp_port = gw_tcp;
    mon_opts.udp_port = gw_udp;
    mon_opts.search_addr = Some("127.0.0.1".parse().unwrap());
    mon_opts.bind_addr = Some("127.0.0.1".parse().unwrap());
    mon_opts.timeout = Duration::from_secs(10);
    mon_opts.authnz_user = Some(CA_USER.to_string());

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_mon = stop.clone();
    tokio::spawn(async move {
        let _ = pvmonitor(&mon_opts, move |_update| {
            if stop_for_mon.load(Ordering::Relaxed) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .await;
    });
    // Let the downstream monitor connect + arm the upstream monitor.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // ---- Explicit downstream GET through the gateway -------------------
    // Forces a real upstream `pvget` (an unambiguous "upstream fetch" on top
    // of the monitor RX), crediting `us_bypv_rx` / `us_byhost_rx`.
    {
        let mut get_opts = PvOptions::new(PV.to_string());
        get_opts.server_addr = Some(format!("127.0.0.1:{gw_tcp}").parse().expect("loopback addr"));
        get_opts.tcp_port = gw_tcp;
        get_opts.udp_port = gw_udp;
        get_opts.search_addr = Some("127.0.0.1".parse().unwrap());
        get_opts.bind_addr = Some("127.0.0.1".parse().unwrap());
        get_opts.timeout = Duration::from_secs(5);
        let _ = spvirit_client::pvget(&get_opts).await;
    }

    // ---- Background upstream pump --------------------------------------
    // A direct upstream client changes the value repeatedly, out-of-band. Each
    // distinct value flows: upstream monitor frame (us_bypv_rx) -> gateway ->
    // downstream monitor delivery (ds_bypv_tx + registry TX). Continuous
    // traffic keeps every rate positive across sampler intervals.
    let pump_stop = stop.clone();
    let pump = tokio::spawn(async move {
        let driver = loopback_client(up_udp);
        let mut i = 0u32;
        while !pump_stop.load(Ordering::Relaxed) {
            i = i.wrapping_add(1);
            let _ = driver.pvput(PV, i as f64).await;
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    });

    // ---- Poll the sampler until the required rate rows appear ----------
    // Ready when downstream per-PV TX and per-host TX rates are positive AND
    // some upstream RX row exists (bypv or byhost).
    let ready = poll_snapshot(&rate_snapshot, Duration::from_secs(25), |s| {
        let ds_pv = rate_for(&s.ds_bypv_tx, PV).is_some_and(|r| r > 0.0);
        let ds_host = s.ds_byhost_tx.iter().any(|(_, _, r)| *r > 0.0);
        let us_row = !s.us_bypv_rx.is_empty() || !s.us_byhost_rx.is_empty();
        ds_pv && ds_host && us_row
    })
    .await;

    // Stop the pump/monitor before asserting (tidy shutdown; assertions run
    // against the captured `ready` snapshot regardless).
    let snap = ready.unwrap_or_else(|| {
        stop.store(true, Ordering::Relaxed);
        panic!(
            "sampler never produced the expected positive rate rows within the timeout; \
             last snapshot: ds_bypv_tx={:?} ds_byhost_tx={:?} us_bypv_rx={:?} us_byhost_rx={:?}",
            rate_snapshot.lock().unwrap().ds_bypv_tx,
            rate_snapshot.lock().unwrap().ds_byhost_tx,
            rate_snapshot.lock().unwrap().us_bypv_rx,
            rate_snapshot.lock().unwrap().us_byhost_rx,
        )
    });

    // ---- Assertions: rate rows ----------------------------------------
    let ds_pv_rate = rate_for(&snap.ds_bypv_tx, PV)
        .unwrap_or_else(|| panic!("no {PV} row in ds:bypv:tx, got {:?}", snap.ds_bypv_tx));
    assert!(
        ds_pv_rate > 0.0,
        "ds:bypv:tx for {PV} must have a positive rate, got {ds_pv_rate}"
    );

    let ds_host_positive = snap.ds_byhost_tx.iter().any(|(_, _, r)| *r > 0.0);
    assert!(
        ds_host_positive,
        "ds:byhost:tx must have a positive-rate row, got {:?}",
        snap.ds_byhost_tx
    );

    assert!(
        !snap.us_bypv_rx.is_empty() || !snap.us_byhost_rx.is_empty(),
        "us:bypv:rx (or us:byhost:rx) must have a row after the upstream fetch; \
         us_bypv_rx={:?} us_byhost_rx={:?}",
        snap.us_bypv_rx,
        snap.us_byhost_rx
    );

    // ---- Assertions: clients lists user@ip -----------------------------
    let clients_payload = status
        .get(&format!("{STATUS_PREFIX}clients"))
        .await
        .expect("clients get");
    let NtPayload::ScalarArray(ref clients_arr) = clients_payload else {
        panic!("clients must be an NTScalarArray, got {clients_payload:?}");
    };
    let spvirit_types::ScalarArrayValue::Str(ref client_list) = clients_arr.value else {
        panic!("clients must be a string array, got {:?}", clients_arr.value);
    };
    let expected = format!("{CA_USER}@127.0.0.1");
    assert!(
        client_list.contains(&expected),
        "clients must list the connected peer as {expected:?}, got {client_list:?}"
    );

    // ---- Assertions: timestamps are non-zero on the 3 TALOS-1990 PVs ---
    // `clients` (ScalarArray, stamped now() at get-time).
    assert!(
        scalar_array_ts_secs(&clients_payload) != 0,
        "clients timeStamp.secondsPastEpoch must be non-zero (TALOS 1990 bug)"
    );

    // `stats` (Generic, nested per-field timeStamp, stamped now() at get-time).
    let stats_payload = status
        .get(&format!("{STATUS_PREFIX}stats"))
        .await
        .expect("stats get");
    assert!(
        generic_first_ts_secs(&stats_payload) != 0,
        "stats timeStamp.secondsPastEpoch must be non-zero (TALOS 1990 bug)"
    );

    // A bandwidth table (`ds:bypv:tx`, stamped with the sampler's snapshot ts).
    let bw_payload = status
        .get(&format!("{STATUS_PREFIX}ds:bypv:tx"))
        .await
        .expect("ds:bypv:tx get");
    assert!(
        table_ts_secs(&bw_payload) != 0,
        "ds:bypv:tx timeStamp.secondsPastEpoch must be non-zero (TALOS 1990 bug)"
    );

    // ---- Shutdown ------------------------------------------------------
    stop.store(true, Ordering::Relaxed);
    let _ = pump.await;
}
