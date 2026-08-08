//! The engine as the server sees it: a `Source` that answers gets, accepts
//! puts, and returns the monitors a put caused.

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_ioc::IocSource;
use spvirit_server::pvstore::Source;
use spvirit_types::{NtPayload, ScalarValue};

const CHAIN: &str = "\
record(ao, \"PV:SET\") {
    field(OUT, \"PV:MID.VAL\")
    field(FLNK, \"PV:MID\")
}
record(ai, \"PV:MID\") {
    field(INP, \"PV:MID.VAL NPP\")
    field(FLNK, \"PV:OUT\")
}
record(ai, \"PV:OUT\") {
    field(INP, \"PV:MID PP\")
}
";

fn source() -> IocSource {
    IocSource::from_db_str(CHAIN).expect("the chain database loads")
}

fn double(v: f64) -> DecodedValue {
    DecodedValue::Float64(v)
}

fn scalar_of(payload: &NtPayload) -> f64 {
    match payload {
        NtPayload::Scalar(s) => match s.value {
            ScalarValue::F64(v) => v,
            ScalarValue::I32(v) => v as f64,
            ScalarValue::U16(v) => v as f64,
            ref other => panic!("unexpected scalar type {other:?}"),
        },
        other => panic!("expected a scalar payload, got {other:?}"),
    }
}

#[tokio::test]
async fn every_record_is_claimable_and_gettable() {
    let src = source();
    let names = src.names().await;
    assert_eq!(names.len(), 3, "got {names:?}");
    for name in &names {
        let info = src
            .claim(name)
            .await
            .unwrap_or_else(|| panic!("{name} must be claimable"));
        assert!(info.writable, "records accept puts");
        assert!(src.get(name).await.is_some(), "{name} must answer a get");
    }
}

#[tokio::test]
async fn an_unknown_pv_is_not_claimed() {
    let src = source();
    assert!(src.claim("PV:NOPE").await.is_none());
    assert!(src.get("PV:NOPE").await.is_none());
}

#[tokio::test]
async fn a_put_propagates_along_the_chain_and_returns_every_monitor() {
    let src = source();
    let events = src.put("PV:SET", &double(7.0)).await.expect("put succeeds");
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["PV:SET", "PV:MID", "PV:OUT"],
        "the put must return every monitor the pass produced, in order"
    );
    for (name, payload) in &events {
        assert_eq!(
            scalar_of(payload),
            7.0,
            "{name} must carry the written value"
        );
    }
}

#[tokio::test]
async fn a_subscriber_receives_the_monitors_a_later_put_causes() {
    let src = source();
    let mut rx = src
        .subscribe("PV:OUT")
        .await
        .expect("PV:OUT is subscribable");
    src.put("PV:SET", &double(3.0)).await.expect("put succeeds");
    let payload = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("a monitor must arrive within 5s")
        .expect("the channel must stay open");
    assert_eq!(scalar_of(&payload), 3.0);
}

#[tokio::test]
async fn a_put_to_an_unknown_pv_is_an_error_naming_it() {
    let src = source();
    let err = src
        .put("PV:NOPE", &double(1.0))
        .await
        .expect_err("unknown PVs are errors");
    assert!(err.contains("PV:NOPE"), "got {err}");
}

#[tokio::test]
async fn pini_records_process_at_startup_in_definition_order() {
    let src = IocSource::from_db_str(
        "record(ai, \"PV:SECOND\") {\n    field(PINI, \"YES\")\n    field(INP, \"2\")\n}\n\
         record(ai, \"PV:FIRST\") {\n    field(PINI, \"YES\")\n    field(INP, \"1\")\n}\n",
    )
    .expect("load");
    let events = src.process_pini();
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["PV:SECOND", "PV:FIRST"],
        "PINI order is the .db definition order, not alphabetical"
    );
}
