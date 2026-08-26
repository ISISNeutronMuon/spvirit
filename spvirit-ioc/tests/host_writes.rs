//! Writes that start in host code, not as a client PUT.
//!
//! The regression this file exists for: `Source::put` on an `IocSource`
//! processes correctly, but monitor clients see nothing, because for a
//! source-backed PV the sole publication site is the handler reading `put`'s
//! return value. A host-side write bypasses the handler entirely. A naive
//! `set_value` therefore looks completely correct from inside the engine —
//! the record moves, the chain propagates, the alarm raises — and notifies
//! nobody. Only an end-to-end monitor can catch it, so these tests run a
//! real server and a real client.

use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use spvirit_client::{PvOptions, pvmonitor};
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_ioc::{IocSource, RecordSpec};
use spvirit_server::pva_server::PvaServer;
use spvirit_server::pvstore::Source;

/// `RIG:SP` drives `RIG:RBV` through OUT + FLNK; `RIG:RBV` alarms over HIHI.
fn rig() -> Vec<RecordSpec> {
    use spvirit_ioc::alarm::Severity;
    vec![
        RecordSpec::ao("RIG:SP").out("RIG:RBV.VAL").flnk("RIG:RBV"),
        RecordSpec::ai("RIG:RBV")
            .inp("RIG:SP PP")
            .egu("C")
            .hihi(100.0)
            .hhsv(Severity::Major),
    ]
}

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
    opts.timeout = Duration::from_secs(5);
    opts
}

/// A host-side write, with no client PUT anywhere, must be seen by a
/// subscribed monitor client. This is the publication-trap regression.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_side_write_reaches_a_monitor_client() {
    let (Some(tcp), Some(udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("skipping: no free loopback ports");
        return;
    };

    // `from_records` already returns `Arc<IocSource>` (it binds the specs to
    // it), so there is no second `Arc::new` here.
    let ioc = IocSource::from_records(rig()).expect("records must build");
    let server = PvaServer::builder()
        .ioc(ioc.clone())
        .port(tcp)
        .udp_port(udp)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .build();
    // `run()`'s error type is not `Send`, so map it to a string inside the
    // task rather than carrying it across the spawn boundary (mirrors
    // `tests/ioc_server.rs`).
    tokio::spawn(async move { server.run().await.map_err(|e| e.to_string()) });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (tx, rx) = mpsc::channel();
    let mon = opts_for("RIG:RBV", tcp, udp);
    // The callback controls flow control itself (there is no
    // `MonitorOptions::max_events`): break after the second update, which is
    // the initial value plus the one the host-side write produces.
    let received = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        let _ = pvmonitor(&mon, move |update| {
            let _ = tx.send(update.clone());
            if received.fetch_add(1, Ordering::SeqCst) + 1 >= 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .await;
    });
    // Let the monitor establish and deliver its initial update before writing.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let _initial = rx.recv_timeout(Duration::from_secs(5)).expect("initial update");

    ioc.set_value("RIG:SP", DecodedValue::Float64(95.0))
        .await
        .expect("host-side write must succeed");

    let update = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("a host-side write must reach a subscribed monitor client");
    let text = format!("{update:?}");
    assert!(
        text.contains("95"),
        "the monitor should carry the written value, got: {text}"
    );
}

/// An outside-in write moves the linked record and raises its alarm — the same
/// chain a client PUT drives. This runs against the engine directly, with no
/// server, because it is about processing rather than publication.
#[tokio::test]
async fn a_host_side_write_propagates_down_the_chain() {
    let ioc = IocSource::from_records(rig()).expect("records must build");

    ioc.set_value("RIG:SP", DecodedValue::Float64(95.0))
        .await
        .expect("write must succeed");
    let rbv = format!("{:?}", ioc.get("RIG:RBV").await.expect("RIG:RBV exists"));
    assert!(rbv.contains("95"), "RIG:RBV should have followed RIG:SP, got: {rbv}");

    ioc.set_value("RIG:SP", DecodedValue::Float64(150.0))
        .await
        .expect("write must succeed");
    let alarmed = format!("{:?}", ioc.get("RIG:RBV").await.expect("RIG:RBV exists"));
    assert!(
        alarmed.contains("150"),
        "RIG:RBV should have followed RIG:SP over HIHI, got: {alarmed}"
    );
}

/// `set_value` and a client PUT drive the identical pass, so their returned
/// event streams must match exactly. If they ever diverge, one of the two
/// paths has grown its own semantics.
#[tokio::test]
async fn a_host_side_write_and_a_client_put_produce_the_same_events() {
    let via_put = IocSource::from_records(rig()).expect("records must build");
    let via_set = IocSource::from_records(rig()).expect("records must build");

    let a = via_put
        .put("RIG:SP", &DecodedValue::Float64(95.0))
        .await
        .expect("put must succeed");
    let b = via_set
        .set_value("RIG:SP", DecodedValue::Float64(95.0))
        .await
        .expect("set_value must succeed");

    let names = |v: &Vec<(String, spvirit_types::NtPayload)>| {
        v.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>()
    };
    assert_eq!(
        names(&a),
        names(&b),
        "set_value and put must drive the same pass in the same order"
    );
}

#[tokio::test]
async fn writing_an_unknown_record_is_an_error_not_a_silent_no_op() {
    let ioc = IocSource::from_records(rig()).expect("records must build");
    let err = ioc
        .set_value("RIG:NOPE", DecodedValue::Float64(1.0))
        .await
        .expect_err("an unknown record must not be writable");
    assert!(err.contains("RIG:NOPE"), "the error should name the record, got: {err}");
}

/// `writing_an_unknown_record_is_an_error_not_a_silent_no_op` above only
/// checks that the record's name appears somewhere in the error, and both
/// the "no record named" and the "field PV is read-only" messages happen to
/// contain it — so that assertion alone cannot tell the two refusal paths
/// apart. Pin the exact wording so a completely unknown name is never
/// reported as a read-only field.
#[tokio::test]
async fn an_unknown_record_is_reported_as_missing_not_as_a_readonly_field() {
    let ioc = IocSource::from_records(rig()).expect("records must build");
    let err = ioc
        .set_value("RIG:NOPE", DecodedValue::Float64(1.0))
        .await
        .expect_err("an unknown record must not be writable");
    assert_eq!(err, "no record named 'RIG:NOPE'");
}

/// Field writes are sub-project B. Until then the refusal has to be explicit,
/// because a silent no-op here is indistinguishable from a working write.
#[tokio::test]
async fn writing_a_field_is_refused_with_a_reason() {
    let ioc = IocSource::from_records(rig()).expect("records must build");
    let err = ioc
        .set_value("RIG:RBV.EGU", DecodedValue::Float64(1.0))
        .await
        .expect_err("field writes are not in A3");
    assert!(
        err.contains("read-only"),
        "the error should say the field PV is read-only, got: {err}"
    );
}

use spvirit_ioc::SpecError;
use spvirit_types::ScalarValue;

/// A bound spec reads and writes its own record; the write drives the chain.
#[tokio::test]
async fn a_bound_spec_handle_gets_and_sets() {
    let sp = RecordSpec::ao("RIG:SP").out("RIG:RBV.VAL").flnk("RIG:RBV");
    let rbv = RecordSpec::ai("RIG:RBV").inp("RIG:SP PP").egu("C");
    // Clones share the binding slot, so binding inside `from_records` binds
    // these handles too.
    let _ioc = IocSource::from_records(vec![sp.clone(), rbv.clone()])
        .expect("records must build");

    sp.set(ScalarValue::F64(42.0)).await.expect("bound set must succeed");
    let read = sp.get().await.expect("bound get must succeed");
    assert!(
        matches!(read, ScalarValue::F64(v) if (v - 42.0).abs() < 1e-9),
        "the setpoint should read back what was written, got: {read:?}"
    );

    let downstream = rbv.get().await.expect("bound get must succeed");
    assert!(
        matches!(downstream, ScalarValue::F64(v) if (v - 42.0).abs() < 1e-9),
        "RIG:RBV should have followed RIG:SP, got: {downstream:?}"
    );
}

/// A pending (unbound) spec refuses get and set — the tier-3 analogue of
/// `Pv::store()` returning `Unbound` while pending.
#[tokio::test]
async fn a_pending_spec_handle_is_unbound() {
    let pending = RecordSpec::ai("RIG:RBV");
    assert!(
        matches!(pending.get().await, Err(SpecError::Unbound)),
        "get on a pending spec must be Unbound"
    );
    assert!(
        matches!(pending.set(ScalarValue::F64(1.0)).await, Err(SpecError::Unbound)),
        "set on a pending spec must be Unbound"
    );
}

/// A scalar variant the engine cannot write to `VAL` is refused, not panicked.
#[tokio::test]
async fn a_bound_spec_refuses_an_unsupported_scalar() {
    let sp = RecordSpec::ao("RIG:SP");
    let _ioc = IocSource::from_records(vec![sp.clone()]).expect("records must build");
    assert!(
        matches!(sp.set(ScalarValue::Str("nope".into())).await, Err(SpecError::Unsupported(_))),
        "a string is not a writable VAL scalar"
    );
}

/// A bound spec reads its own fields by verbatim EPICS name — the Rust home
/// the Python `rec["EGU"]` handle delegates to.
#[tokio::test]
async fn a_bound_spec_reads_a_field_by_epics_name() {
    let rbv = RecordSpec::ai("RIG:RBV").egu("C");
    let _ioc = IocSource::from_records(vec![rbv.clone()]).expect("records must build");
    match rbv.get_field("EGU").await.expect("EGU is readable") {
        ScalarValue::Str(s) => assert_eq!(s, "C"),
        other => panic!("EGU should read as a string, got {other:?}"),
    }
    assert!(
        matches!(rbv.get_field("NOSUCHFIELD").await, Err(SpecError::NotFound(_))),
        "an absent field must be NotFound, not a panic"
    );
}
