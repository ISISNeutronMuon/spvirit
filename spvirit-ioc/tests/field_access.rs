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
