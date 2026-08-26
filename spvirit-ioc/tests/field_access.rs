//! `<record>.<FIELD>` served by the IOC store itself — one registration,
//! both behaviours.

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_ioc::IocSource;
use spvirit_server::pvstore::Source;
use spvirit_types::{NtPayload, ScalarArrayValue, ScalarValue};

const DB: &str = "record(ai, \"PV:A\") {
    field(DESC, \"a sample\")
    field(EGU, \"C\")
    field(SCAN, \"1 second\")
    field(INP, \"5\")
}
";

fn scalar(payload: &NtPayload) -> ScalarValue {
    match payload {
        NtPayload::Scalar(s) => s.value.clone(),
        other => panic!("expected a scalar, got {other:?}"),
    }
}

#[tokio::test]
async fn the_ioc_serves_its_own_field_pvs() {
    let src = IocSource::from_db_str(DB).expect("loads");
    for (name, expected) in [
        ("PV:A.RTYP", ScalarValue::Str("ai".into())),
        ("PV:A.NAME", ScalarValue::Str("PV:A".into())),
        ("PV:A.DESC", ScalarValue::Str("a sample".into())),
        ("PV:A.EGU", ScalarValue::Str("C".into())),
        ("PV:A.SCAN", ScalarValue::Str("1 second".into())),
    ] {
        assert!(src.claim(name).await.is_some(), "{name} must be claimed");
        assert_eq!(scalar(&src.get(name).await.expect("gettable")), expected, "{name}");
    }
}

#[tokio::test]
async fn field_pvs_are_read_only() {
    let src = IocSource::from_db_str(DB).expect("loads");
    let info = src.claim("PV:A.SCAN").await.expect("claimed");
    assert!(!info.writable, "field writes are sub-project B's");
    let err = src
        .put("PV:A.SCAN", &DecodedValue::Int32(1))
        .await
        .expect_err("puts to fields must fail");
    assert!(err.contains("read-only"), "got {err}");
}

#[tokio::test]
async fn the_long_string_form_works_on_the_ioc_too() {
    let src = IocSource::from_db_str(DB).expect("loads");
    match src.get("PV:A.DESC$").await.expect("payload") {
        NtPayload::ScalarArray(arr) => {
            let expected: Vec<i8> = "a sample".bytes().map(|b| b as i8).collect();
            assert_eq!(arr.value, ScalarArrayValue::I8(expected));
        }
        other => panic!("expected scalar array, got {other:?}"),
    }
    assert!(src.get("PV:A.MDEL$").await.is_none(), "$ on a non-string field");
}

#[tokio::test]
async fn unknown_records_and_fields_are_not_claimed() {
    let src = IocSource::from_db_str(DB).expect("loads");
    assert!(src.claim("PV:MISSING.RTYP").await.is_none());
    assert!(src.claim("PV:A.NOTAFIELD").await.is_none());
}

/// The record PV must keep working exactly as before — routing is additive.
#[tokio::test]
async fn the_record_pv_is_unaffected_by_field_routing() {
    let src = IocSource::from_db_str(DB).expect("loads");
    let info = src.claim("PV:A").await.expect("claimed");
    assert!(info.writable, "record PVs stay writable");
    assert_eq!(scalar(&src.get("PV:A").await.expect("gettable")), ScalarValue::F64(5.0));
}

/// The IOC registers itself as a store, so `.ioc()` is one call instead of
/// `.source("ioc", 0, …)` plus a second registration for `.FIELD`.
#[tokio::test]
async fn the_builder_registers_an_ioc_as_a_store() {
    let ioc = std::sync::Arc::new(IocSource::from_db_str(DB).expect("loads"));
    assert_eq!(
        spvirit_server::pvstore::StoreSource::record_names(&*ioc),
        vec!["PV:A".to_string()],
        "record_names must be the sorted record list"
    );
    let server = spvirit_server::pva_server::PvaServer::builder()
        .port(0)
        .udp_port(0)
        .ioc(ioc)
        .build();
    drop(server);
}

/// A second `.ioc()` is a mistake, not a second engine: the builder holds
/// one `Option`, so the first would be dropped silently and its records
/// would simply stop being served. Additional engines go through
/// `.source()`, and the panic message says so.
#[test]
#[should_panic(expected = "PvaServerBuilder::ioc may only be called once")]
fn calling_ioc_twice_panics() {
    let one = std::sync::Arc::new(IocSource::from_db_str(DB).expect("loads"));
    let two = std::sync::Arc::new(
        IocSource::from_db_str("record(ai, \"PV:Z\") {\n}\n").expect("loads"),
    );
    let _ = spvirit_server::pva_server::PvaServer::builder()
        .port(0)
        .udp_port(0)
        .ioc(one)
        .ioc(two);
}

/// `claim` must not take a lock set. Proven by holding one: a claim issued
/// while another task owns the record's lock set still answers.
#[tokio::test]
async fn claiming_a_field_does_not_take_the_records_lock_set() {
    let src = std::sync::Arc::new(IocSource::from_db_str(DB).expect("loads"));
    let held = src.clone();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        held.with_lock_set_for_test("PV:A", |_| {
            tx.send(()).expect("signal");
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
    });
    rx.recv().expect("the lock set is held");
    let claimed = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        src.claim("PV:A.RTYP"),
    )
    .await
    .expect("claim must not block on the lock set")
    .expect("claimed");
    assert!(!claimed.writable);
    holder.join().expect("holder thread");
}

/// Tier 3 (the IOC engine, `IocSource`) answers the same `.FIELD` script as
/// tier 2 (`SimplePvStore`, in `spvirit-server/tests/tier_parity.rs`): known
/// fields from the model, unknown-but-real fields from dbCommon, invented
/// fields not at all.
#[tokio::test]
async fn the_ioc_matches_the_field_contract_the_other_tiers_serve() {
    let src = IocSource::from_db_str(DB).expect("loads");
    assert_eq!(
        src.get("PV:A.DESC").await.map(|p| scalar(&p)),
        Some(ScalarValue::Str("a sample".into()))
    );
    assert_eq!(
        src.get("PV:A.PRIO").await.map(|p| scalar(&p)),
        Some(ScalarValue::Str("LOW".into())),
        "a field the engine does not model still reads as its dbCommon default"
    );
    assert!(src.get("PV:A.NOTAFIELD").await.is_none());
}

/// `field(EGU, …)` must reach both the record payload's units and the
/// `.EGU` field PV. Tier 2 (`SimplePvStore`) has always served units; before
/// A2 the engine dropped them at build time, so a client could tell the
/// tiers apart by asking for units.
#[tokio::test]
async fn egu_survives_from_db_text_to_the_served_payload() {
    let src = IocSource::from_db_str(DB).expect("loads");
    match src.get("PV:A").await.expect("gettable") {
        NtPayload::Scalar(s) => assert_eq!(s.units, "C"),
        other => panic!("expected a scalar, got {other:?}"),
    }
    assert_eq!(
        src.get("PV:A.EGU").await.map(|p| scalar(&p)),
        Some(ScalarValue::Str("C".into()))
    );
}

/// A record with no EGU serves empty units, not a missing field.
#[tokio::test]
async fn a_record_without_egu_serves_empty_units() {
    let src = IocSource::from_db_str("record(ai, \"PV:N\") {\n}\n").expect("loads");
    match src.get("PV:N").await.expect("gettable") {
        NtPayload::Scalar(s) => assert_eq!(s.units, ""),
        other => panic!("expected a scalar, got {other:?}"),
    }
    assert_eq!(
        src.get("PV:N.EGU").await.map(|p| scalar(&p)),
        Some(ScalarValue::Str(String::new()))
    );
}
/// The other half of the record-level `writable` divergence the A2 spec
/// keeps deliberately: tier 3 (`IocSource`) claims every record it owns
/// writable, `ai` included, because Base lets you `caput` to an `ai.VAL`
/// (it is simply overwritten on the next process). Tier 2's
/// (`SimplePvStore`'s) stricter per-kind rule is pinned by
/// `spvirit-server/tests/tier_parity.rs`'s
/// `an_input_record_is_not_writable_on_the_builtin_store`; between the two,
/// neither tier can drift without a test going red.
///
/// The flag is honest here too: the put is accepted and processed, not
/// advertised and then refused.
#[tokio::test]
async fn an_input_record_is_writable_on_the_ioc_engine() {
    let src = IocSource::from_db_str(DB).expect("loads");
    let info = src.claim("PV:A").await.expect("the ai record is served");
    assert!(
        info.writable,
        "tier 3 (`IocSource`) claims an input record writable, as Base does"
    );
    src.put("PV:A", &DecodedValue::Float64(2.5))
        .await
        .expect("and it means it: the write is accepted and processed");
    assert_eq!(
        src.get("PV:A").await.map(|p| scalar(&p)),
        Some(ScalarValue::F64(2.5))
    );
}

/// The registry the server owns must reach the engine, or a host-side write
/// has nowhere to publish. This asserts the wiring, not the publishing —
/// `tests/host_writes.rs` proves a monitor client actually sees the result.
///
/// Without this, `set_value` would look correct and notify nobody: for a
/// source-backed PV the only publication site is the handler, reading `put`'s
/// return value, and a host-side write never goes through the handler.
#[tokio::test]
async fn building_a_server_hands_the_engine_its_monitor_registry() {
    let ioc = std::sync::Arc::new(
        spvirit_ioc::IocSource::from_db_str(
            "record(ai, \"REG:A\") {\n    field(INP, \"1\")\n}\n",
        )
        .expect("db must build"),
    );
    assert!(
        ioc.monitor_registry().is_none(),
        "an engine that has never been served has no registry"
    );

    let server = spvirit_server::pva_server::PvaServer::builder()
        .ioc(ioc.clone())
        .build();
    server.run_start_hooks().await.expect("hooks must succeed");

    assert!(
        ioc.monitor_registry().is_some(),
        "PvaServer must hand the engine the registry it publishes through"
    );
}
