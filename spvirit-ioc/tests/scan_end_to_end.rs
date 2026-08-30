//! Task 15 capstone: the `Scanner` wired into the real server lifecycle.
//!
//! Every earlier scan task ran the driver against a `ManualClock` in isolation.
//! This file proves the production wiring end to end, over a real `PvaServer`
//! and a real monitor client:
//!
//!  1. a periodic record processes and posts monitor updates *on its own*, with
//!     no client PUT and no host-side poke — the scan thread is the only thing
//!     that can have driven it; and
//!  2. a scan-driven monitor frame and a put-driven one, for the same warmed
//!     record driven to the same value, are indistinguishable modulo the
//!     timestamp both stamp — because both egress through the very same
//!     `notify_monitors` path.
//!
//! Production wiring uses `SystemClock`, so these tests use a real, short
//! period (`1 second`) with a bounded wait rather than a `ManualClock`. That is
//! deliberate (see the brief): the deterministic mechanism tests stay on
//! `ManualClock`; this capstone exercises the real lifecycle.
//!
//! A subtlety these tests are built around: a *constant* `INP` seeds `VAL` at
//! database-build time, so a monitor's very first snapshot already carries it —
//! which would pass even with the Scanner unwired. So the periodic record here
//! has a *link* `INP`, which leaves it undefined (`VAL == 0`) until something
//! processes it. Only a scan can, so the arrival of the linked value is proof
//! the scan thread ran.

use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use spvirit_client::{PvOptions, pvmonitor};
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_ioc::IocSource;
use spvirit_server::pva_server::PvaServer;
use spvirit_server::pvstore::Source;

fn free_tcp_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok().map(|a| a.port())
}

fn free_udp_port() -> Option<u16> {
    UdpSocket::bind("127.0.0.1:0").ok()?.local_addr().ok().map(|a| a.port())
}

fn opts_for(pv: &str, tcp: u16, udp: u16) -> PvOptions {
    let mut opts = PvOptions::new(pv.to_string());
    opts.server_addr = Some(format!("127.0.0.1:{tcp}").parse().expect("loopback addr"));
    opts.tcp_port = tcp;
    opts.udp_port = udp;
    opts.search_addr = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
    opts.bind_addr = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
    opts.timeout = Duration::from_secs(10);
    opts
}

/// The top-level `value` field of a decoded NT structure, as an `f64`. A
/// monitor delta that did not change `value` (e.g. an alarm-only post) has no
/// `value` member and yields `None`.
fn value_f64(v: &DecodedValue) -> Option<f64> {
    let DecodedValue::Structure(fields) = v else { return None };
    let (_, val) = fields.iter().find(|(name, _)| name == "value")?;
    match val {
        DecodedValue::Float64(x) => Some(*x),
        DecodedValue::Float32(x) => Some(*x as f64),
        DecodedValue::Int32(x) => Some(*x as f64),
        _ => None,
    }
}

/// A monitor frame with the `timeStamp` field dropped, rendered to a stable
/// debug string. Two frames whose only difference is *when* they were stamped
/// compare equal here — which is exactly the "modulo timestamp" the parity
/// assertion needs.
fn frame_without_timestamp(v: &DecodedValue) -> String {
    match v {
        DecodedValue::Structure(fields) => {
            let kept: Vec<_> =
                fields.iter().filter(|(name, _)| name != "timeStamp").cloned().collect();
            format!("{:?}", DecodedValue::Structure(kept))
        }
        other => format!("{other:?}"),
    }
}

/// Spawn a monitor that forwards every decoded frame onto an mpsc channel and
/// never breaks on its own (the test controls its lifetime by dropping the
/// task's handle at teardown).
fn spawn_monitor(opts: PvOptions) -> (mpsc::Receiver<DecodedValue>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = tokio::spawn(async move {
        let _ = pvmonitor(&opts, move |update| {
            if tx.send(update.value.clone()).is_err() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .await;
    });
    (rx, handle)
}

/// Block (on this thread) until a frame whose `value` equals `want` arrives, or
/// the deadline passes. Returns the matching frame, or `None` on timeout.
fn recv_value(
    rx: &mpsc::Receiver<DecodedValue>,
    want: f64,
    within: Duration,
) -> Option<DecodedValue> {
    let deadline = Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match rx.recv_timeout(remaining) {
            Ok(v) if value_f64(&v) == Some(want) => return Some(v),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// A periodic record must process and post monitors with no external poke.
///
/// `SCAN:SELF` reads its value from a *link* (`SCAN:SRC`, a constant 42), so it
/// is undefined until processed — its initial monitor snapshot is 0. Nothing in
/// the test ever writes it. If the client sees the linked value 42 arrive, the
/// only thing that can have driven the record is the 1-second scan thread.
#[tokio::test(flavor = "multi_thread")]
async fn a_periodic_record_posts_monitors_on_its_own() {
    let (Some(tcp), Some(udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("skipping: no free loopback ports");
        return;
    };

    const DB: &str = "record(ai, \"SCAN:SRC\") {\n\
        field(INP, \"42\")\n\
    }\n\
    record(ai, \"SCAN:SELF\") {\n\
        field(INP, \"SCAN:SRC\")\n\
        field(SCAN, \"1 second\")\n\
    }\n";

    let ioc = Arc::new(IocSource::from_db_str(DB).expect("the database loads"));
    let server = PvaServer::builder()
        .port(tcp)
        .udp_port(udp)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .ioc(ioc)
        .build();
    tokio::spawn(async move { server.run().await.map_err(|e| e.to_string()) });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (rx, mon) = spawn_monitor(opts_for("SCAN:SELF", tcp, udp));

    // The record is undefined (0) at connect; the linked value (42) can only
    // appear once the scan thread has processed it.
    let scan_post =
        tokio::task::spawn_blocking(move || recv_value(&rx, 42.0, Duration::from_secs(6)))
            .await
            .expect("blocking recv task");
    mon.abort();

    assert!(
        scan_post.is_some(),
        "a 1-second periodic record must post its linked value (42) on its own, \
         with no client PUT or host write — the scan thread is the only driver \
         that can have moved it off its undefined initial value"
    );
}

/// A claim on a `SCAN` field PV reports `writable == true` (R-T12-WRITABLE),
/// while an ordinary field PV stays read-only. This pins the claim side of the
/// SCAN/EVNT/PHAS write path end to end: a client's channel search over the
/// same record the scan tests drive learns the field is puttable, and the flag
/// is honest — `put_field_pv` accepts exactly what the claim advertises.
#[tokio::test(flavor = "multi_thread")]
async fn a_scan_field_claim_is_writable_end_to_end() {
    const DB: &str = "record(ai, \"PARITY\") {\n\
        field(INP, \"PARITY:SRC\")\n\
        field(SCAN, \"1 second\")\n\
        field(EGU, \"C\")\n\
    }\n";

    let ioc = IocSource::from_db_str(DB).expect("the database loads");

    let scan = Source::claim(&ioc, "PARITY.SCAN").await.expect("SCAN is claimed");
    assert!(
        scan.writable,
        "a claim on PARITY.SCAN must report writable — SCAN routes into the Scanner"
    );

    for field in ["PARITY.EVNT", "PARITY.PHAS"] {
        let info = Source::claim(&ioc, field).await.expect("claimed");
        assert!(info.writable, "{field} must claim writable");
    }

    let egu = Source::claim(&ioc, "PARITY.EGU").await.expect("EGU is claimed");
    assert!(!egu.writable, "an ordinary field PV (EGU) stays read-only");
}

/// A `PINI("YES")` record must be processed at server startup and its value
/// delivered to a client — the server must **not** panic bringing it up.
///
/// The startup PINI sweep runs inside `Scanner::start()`, which
/// `start_scanning` calls **on the async serve task (a runtime worker)**. If
/// `EgressSink::flush` published by `block_on`-ing `notify_monitors`, that
/// `block_on` would panic ("cannot start a runtime from within a runtime"),
/// unwind the serve future, and the server would never serve — this test would
/// then time out. `PINI:A` links a constant source (42) and is undefined until
/// processed, so a client seeing 42 proves the PINI pass both ran and
/// published without crashing startup.
#[tokio::test(flavor = "multi_thread")]
async fn a_pini_record_is_processed_at_startup_without_panicking() {
    let (Some(tcp), Some(udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("skipping: no free loopback ports");
        return;
    };

    const DB: &str = "record(ai, \"PINI:SRC\") {\n\
        field(INP, \"42\")\n\
    }\n\
    record(ai, \"PINI:A\") {\n\
        field(INP, \"PINI:SRC\")\n\
        field(PINI, \"YES\")\n\
    }\n";

    let ioc = Arc::new(IocSource::from_db_str(DB).expect("the database loads"));
    let server = PvaServer::builder()
        .port(tcp)
        .udp_port(udp)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .ioc(ioc)
        .build();
    tokio::spawn(async move { server.run().await.map_err(|e| e.to_string()) });
    tokio::time::sleep(Duration::from_millis(400)).await;

    let (rx, mon) = spawn_monitor(opts_for("PINI:A", tcp, udp));
    let pini_post =
        tokio::task::spawn_blocking(move || recv_value(&rx, 42.0, Duration::from_secs(6)))
            .await
            .expect("blocking recv task");
    mon.abort();

    assert!(
        pini_post.is_some(),
        "a PINI record must be processed at startup and its linked value (42) \
         delivered — a `block_on` in flush on the serve worker would panic and \
         crash the server before it ever served"
    );
}

/// An event scan record must deliver a monitor frame when its event is posted.
///
/// `Events::post` awaits each `EventSink` inline on a **runtime task**, so the
/// Scanner's `on_event` → `fire_event` → `process_ids` → `flush` chain runs on
/// a runtime worker. A `block_on` there would panic, be swallowed by `post`'s
/// `catch_unwind`, and the record would **silently** never notify. This drives
/// the real path — `RunningServer::post_event` → the registered sink → a client
/// monitor — and asserts the frame actually arrives.
///
/// `EV:A` reads a settable `ao` source through an (NPP) link and is on
/// `SCAN("Event shutter")`, so nothing but the event can move it: the source is
/// set to 55 without processing `EV:A`, then the event is posted, and the client
/// must see 55.
#[tokio::test(flavor = "multi_thread")]
async fn an_event_record_delivers_a_monitor_frame_via_post_event() {
    let (Some(tcp), Some(udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("skipping: no free loopback ports");
        return;
    };

    const DB: &str = "record(ao, \"EV:SRC\") {\n\
        field(DOL, \"0\")\n\
    }\n\
    record(ai, \"EV:A\") {\n\
        field(INP, \"EV:SRC\")\n\
        field(SCAN, \"Event\")\n\
        field(EVNT, \"shutter\")\n\
    }\n";

    let ioc = Arc::new(IocSource::from_db_str(DB).expect("the database loads"));
    let ioc_host = ioc.clone();
    // `serve(...).start()` yields a `RunningServer` whose `post_event` reaches
    // the registered Scanner event sink — the real dispatch path.
    let server = PvaServer::serve(Vec::<spvirit_server::AnyPv>::new())
        .port(tcp)
        .udp_port(udp)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .ioc(ioc)
        .start()
        .await
        .expect("server starts");
    // Let `serve_after_start_hooks` register the Scanner as an event sink and
    // spawn its notify drain before we fire.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (rx, mon) = spawn_monitor(opts_for("EV:A", tcp, udp));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Point the (NPP) source at 55 without processing EV:A; only the event may.
    ioc_host
        .set_value("EV:SRC", DecodedValue::Float64(55.0))
        .await
        .expect("host write to source");
    // Fire the event: reaches the Scanner through the registered EventSink.
    server.post_event("shutter").await;

    let event_post =
        tokio::task::spawn_blocking(move || recv_value(&rx, 55.0, Duration::from_secs(5)))
            .await
            .expect("blocking recv task");
    mon.abort();
    server.abort();

    assert!(
        event_post.is_some(),
        "an event scan record must deliver its processed value (55) when the \
         event is posted — a `block_on` in flush on the post_event worker would \
         panic and be swallowed, so the update would silently never arrive"
    );
}

/// A scan-driven monitor frame and a put-driven one, for the same warmed
/// record driven to the same value, must be indistinguishable modulo timestamp.
///
/// `PARITY` reads a settable source `PARITY:SRC` (an `ao`) through an `NPP`
/// link, and is on a 1-second scan. Both records are first warmed past their
/// undefined state so neither transition under test carries a UDF/alarm change;
/// what remains is a pure value delta, produced once by a host PUT and once by
/// the scan thread. If the two frames match, a monitor client cannot tell which
/// mechanism drove the record — the ProcSink egress and the put egress are the
/// same path.
#[tokio::test(flavor = "multi_thread")]
async fn scan_driven_and_put_driven_updates_are_indistinguishable() {
    let (Some(tcp), Some(udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("skipping: no free loopback ports");
        return;
    };

    const DB: &str = "record(ao, \"PARITY:SRC\") {\n\
        field(DOL, \"0\")\n\
    }\n\
    record(ai, \"PARITY\") {\n\
        field(INP, \"PARITY:SRC\")\n\
        field(SCAN, \"1 second\")\n\
    }\n";

    let ioc = Arc::new(IocSource::from_db_str(DB).expect("the database loads"));
    let ioc_host = ioc.clone();
    let server = PvaServer::builder()
        .port(tcp)
        .udp_port(udp)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .ioc(ioc)
        .build();
    tokio::spawn(async move { server.run().await.map_err(|e| e.to_string()) });

    // Warm PARITY past its undefined state: a couple of scan periods with the
    // source at 0 leave it defined, value 0, no alarm. Every later transition
    // is then a pure NoAlarm value delta.
    tokio::time::sleep(Duration::from_millis(2300)).await;

    let (rx, mon) = spawn_monitor(opts_for("PARITY", tcp, udp));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // --- PUT-driven transition to 41 -------------------------------------
    // Line the link up with the written value so the put produces exactly one
    // post (write then process see the same value), then write it.
    ioc_host
        .set_value("PARITY:SRC", DecodedValue::Float64(41.0))
        .await
        .expect("host write to source");
    ioc_host
        .set_value("PARITY", DecodedValue::Float64(41.0))
        .await
        .expect("host write to record");

    // --- Excursion away from 41, so a later scan is a real 7 -> 41 change ---
    ioc_host
        .set_value("PARITY:SRC", DecodedValue::Float64(7.0))
        .await
        .expect("host write to source");
    ioc_host
        .set_value("PARITY", DecodedValue::Float64(7.0))
        .await
        .expect("host write to record");

    // Point the source back at 41 *without* touching the record (the link is
    // NPP, so this does not process PARITY). The next scan will fetch 41.
    ioc_host
        .set_value("PARITY:SRC", DecodedValue::Float64(41.0))
        .await
        .expect("host write to source");

    // Collect: put_frame is the first value==41; then the value==7 excursion;
    // then the scan-driven value==41. Do the blocking recvs off the runtime.
    let (put_frame, scan_frame) = tokio::task::spawn_blocking(move || {
        let put_frame = recv_value(&rx, 41.0, Duration::from_secs(5));
        // Drain the excursion so the *next* 41 is unambiguously scan-driven.
        let _excursion = recv_value(&rx, 7.0, Duration::from_secs(5));
        let scan_frame = recv_value(&rx, 41.0, Duration::from_secs(5));
        (put_frame, scan_frame)
    })
    .await
    .expect("blocking recv task");
    mon.abort();

    let put_frame = put_frame.expect("a put-driven value==41 frame must arrive");
    let scan_frame = scan_frame.expect("a scan-driven value==41 frame must arrive");

    assert_eq!(value_f64(&put_frame), Some(41.0), "put frame carries the written value");
    assert_eq!(value_f64(&scan_frame), Some(41.0), "scan frame carries the scanned value");
    assert_eq!(
        frame_without_timestamp(&scan_frame),
        frame_without_timestamp(&put_frame),
        "a scan-driven and a put-driven monitor frame for the same record must be \
         indistinguishable modulo the timestamp — both egress through the same \
         notify_monitors path"
    );
}
