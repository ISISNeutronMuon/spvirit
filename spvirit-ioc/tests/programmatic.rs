//! Records built in host code and records parsed from `.db` text must be
//! indistinguishable once built.
//!
//! This is the claim that keeps the two construction paths honest, and it is
//! cheap to hold because `RecordSpec` lowers to a `DbRecord` and goes through
//! the same `build_records`. The test is here to catch a future change that
//! adds a second interpretation path — a Python-side default, a "convenience"
//! coercion — rather than to catch today's code.

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_ioc::{IocSource, RecordSpec};
use spvirit_server::pvstore::Source;
use spvirit_types::{NtPayload, ScalarValue};

const DB: &str = "record(ao, \"RIG:SP\") {
    field(OUT, \"RIG:RBV.VAL\")
    field(FLNK, \"RIG:RBV\")
}
record(ai, \"RIG:RBV\") {
    field(INP, \"RIG:SP PP\")
    field(EGU, \"C\")
    field(HIHI, \"100\")
    field(HHSV, \"MAJOR\")
    field(MDEL, \"0.1\")
}
";

fn programmatic() -> Vec<RecordSpec> {
    use spvirit_ioc::alarm::Severity;
    vec![
        RecordSpec::ao("RIG:SP").out("RIG:RBV.VAL").flnk("RIG:RBV"),
        RecordSpec::ai("RIG:RBV")
            .inp("RIG:SP PP")
            .egu("C")
            .hihi(100.0)
            .hhsv(Severity::Major)
            .mdel(0.1),
    ]
}

/// Reduce a payload to the parts a monitor client can observe. Timestamps are
/// excluded deliberately: they are wall-clock and would make every comparison
/// flaky for a reason that has nothing to do with construction.
fn observable(p: &NtPayload) -> (ScalarValue, i32, i32, String, String) {
    match p {
        NtPayload::Scalar(s) => (
            match &s.value {
                ScalarValue::F64(v) => ScalarValue::F64(*v),
                other => other.clone(),
            },
            s.alarm_severity,
            s.alarm_status,
            s.alarm_message.clone(),
            s.units.clone(),
        ),
        other => panic!("expected a scalar payload, got {other:?}"),
    }
}

async fn event_stream(src: &IocSource, puts: &[(&str, f64)]) -> Vec<(String, (ScalarValue, i32, i32, String, String))> {
    let mut out = Vec::new();
    for (name, v) in puts {
        let events = src
            .put(name, &DecodedValue::Float64(*v))
            .await
            .expect("put must succeed");
        for (pv, payload) in &events {
            out.push((pv.clone(), observable(payload)));
        }
    }
    out
}

const PUTS: [(&str, f64); 4] = [
    ("RIG:SP", 20.0),
    ("RIG:SP", 20.05), // inside MDEL — must be suppressed identically on both
    ("RIG:SP", 95.0),
    ("RIG:SP", 150.0), // over HIHI — must raise MAJOR identically on both
];

#[tokio::test]
async fn a_db_file_and_the_equivalent_records_produce_the_same_event_stream() {
    let from_text = IocSource::from_db_str(DB).expect("db text must build");
    let from_code = IocSource::from_records(programmatic()).expect("records must build");

    let a = event_stream(&from_text, &PUTS).await;
    let b = event_stream(&from_code, &PUTS).await;

    assert_eq!(
        a, b,
        "the .db path and the programmatic path must be indistinguishable"
    );
    assert!(!a.is_empty(), "the put sequence must actually produce events");
}

#[tokio::test]
async fn both_paths_agree_on_the_served_field_values() {
    let from_text = IocSource::from_db_str(DB).expect("db text must build");
    let from_code = IocSource::from_records(programmatic()).expect("records must build");

    for field in ["RIG:RBV.EGU", "RIG:RBV.HIHI", "RIG:RBV.HHSV", "RIG:SP.OUT", "RIG:SP.FLNK"] {
        assert_eq!(
            from_text.get(field).await,
            from_code.get(field).await,
            "{field} must read the same on both construction paths"
        );
    }
}

#[tokio::test]
async fn both_paths_agree_on_the_record_namespace() {
    let from_text = IocSource::from_db_str(DB).expect("db text must build");
    let from_code = IocSource::from_records(programmatic()).expect("records must build");
    assert_eq!(from_text.names().await, from_code.names().await);
}

/// Ruling 3. `DRVH` is not modelled, and the point of carrying it is that the
/// two paths ignore it *identically* — a programmatic record that rejected it
/// while the `.db` path accepted it would break the round-trip guarantee.
#[tokio::test]
async fn an_unmodelled_field_is_ignored_the_same_way_on_both_paths() {
    let text = IocSource::from_db_str(
        "record(ao, \"X\") {\n    field(DRVH, \"100\")\n}\n",
    )
    .expect("db text with DRVH must build");
    let code = IocSource::from_records(vec![RecordSpec::ao("X").field("DRVH", "100")])
        .expect("records with DRVH must build");

    assert_eq!(text.get("X").await, code.get("X").await);
    assert_eq!(text.get("X.DRVH").await, None, "DRVH is not a served field");
    assert_eq!(code.get("X.DRVH").await, None, "DRVH is not a served field");
}

#[tokio::test]
async fn a_record_type_the_engine_does_not_support_is_rejected() {
    let err = IocSource::from_db_str("record(calc, \"X\") {\n}\n")
        .expect_err("calc is sub-project D");
    assert!(
        err.contains("sub-project D"),
        "the error should point at sub-project D, got: {err}"
    );
}

/// Records are fixed at build. The reason is `RecordId`, which is
/// `{set, index}` assigned by `RecordDb::build` partitioning the link graph:
/// a record whose links join two existing sets forces a repartition and
/// invalidates every outstanding id. Base has the same restriction, for the
/// same reason — `dbLoadRecords` after `iocInit` is unsupported.
///
/// Rust enforces this by having no method to call (ruling 5), so what this
/// test can check is that the wording Python raises with actually explains
/// the reason. A refusal that says only "not supported" would send a user
/// looking for a flag to turn on.
#[test]
fn the_immutability_reason_names_lock_sets_and_the_base_precedent() {
    let reason = spvirit_ioc::IocSource::LOCK_SET_IMMUTABILITY_REASON;
    assert!(reason.contains("lock set"), "the reason must name lock sets: {reason}");
    assert!(
        reason.contains("dbLoadRecords") || reason.contains("iocInit"),
        "the reason should cite the Base precedent: {reason}"
    );
    assert!(
        reason.contains("Ioc("),
        "the reason should point at the constructor that does work: {reason}"
    );
}
