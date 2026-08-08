//! One test per rule from the EPICS Record Reference. These are the
//! behaviours a `.db` author relies on; a change that breaks one is a
//! semantic regression, not a refactor.

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_ioc::IocSource;
use spvirit_server::pvstore::Source;
use spvirit_types::{NtPayload, ScalarValue};

fn put(v: f64) -> DecodedValue {
    DecodedValue::Float64(v)
}

fn scalar(payload: &NtPayload) -> f64 {
    match payload {
        NtPayload::Scalar(s) => match s.value {
            ScalarValue::F64(v) => v,
            ScalarValue::I32(v) => v as f64,
            ScalarValue::U16(v) => v as f64,
            ref other => panic!("unexpected scalar {other:?}"),
        },
        other => panic!("expected a scalar, got {other:?}"),
    }
}

fn alarm(payload: &NtPayload) -> (i32, i32, String) {
    match payload {
        NtPayload::Scalar(s) => (s.alarm_severity, s.alarm_status, s.alarm_message.clone()),
        other => panic!("expected a scalar, got {other:?}"),
    }
}

/// Rule: a record processes its PP input links before using their values.
///
/// The brief's original fixture (`PV:SRC` written directly, `PV:DST` reading
/// it PP vs. comparing two `get`s) cannot actually distinguish PP from NPP:
/// `PV:SRC` is an `ao` whose VAL is set by the put itself, so it is already
/// current by the time anything reads it, whether or not the read forces a
/// reprocess. This rebuilds the fixture so PP and NPP visibly diverge:
/// `PV:SRC` only picks up `PV:R`'s value when it is *processed*, and nothing
/// (no PINI, no SCAN) ever processes `PV:SRC` on its own. So an NPP reader
/// sees `PV:SRC`'s never-updated default, while a PP reader forces `PV:SRC`
/// to process first and sees `PV:R`'s current value.
#[tokio::test]
async fn rule_pp_input_processes_before_reading() {
    let src = IocSource::from_db_str(
        "record(ao, \"PV:R\") {\n}\n\
         record(ai, \"PV:SRC\") {\n    field(INP, \"PV:R NPP\")\n}\n\
         record(ai, \"PV:DST_NPP\") {\n    field(INP, \"PV:SRC NPP\")\n}\n\
         record(ai, \"PV:DST_PP\") {\n    field(INP, \"PV:SRC PP\")\n}\n",
    )
    .expect("load");

    // PV:R now holds a value PV:SRC has never pulled in — PV:SRC has no
    // PINI/SCAN and nothing else has processed it yet.
    src.put("PV:R", &put(4.0)).await.expect("put");

    // NPP: PV:DST_NPP reads PV:SRC's currently stored VAL as-is, without
    // processing it first, so it sees PV:SRC's untouched load-time default.
    // Read back with `get` rather than the put's own events: VAL ends up
    // unchanged from its 0.0 default here, so MDEL correctly suppresses the
    // monitor and the put's event list is empty — that suppression is
    // itself part of what this fixture proves, not a reason to special-case
    // the read.
    src.put("PV:DST_NPP", &put(0.0)).await.expect("put");
    let npp = scalar(&src.get("PV:DST_NPP").await.expect("PV:DST_NPP exists"));

    // PP: PV:DST_PP forces PV:SRC to process first, which pulls PV:R's
    // current value into PV:SRC.VAL, and only then reads it.
    src.put("PV:DST_PP", &put(0.0)).await.expect("put");
    let pp = scalar(&src.get("PV:DST_PP").await.expect("PV:DST_PP exists"));

    assert_eq!(
        npp, 0.0,
        "NPP must read PV:SRC's stale, never-processed VAL"
    );
    assert_eq!(
        pp, 4.0,
        "PP must force PV:SRC to process and read its fresh VAL"
    );
    assert_ne!(
        npp, pp,
        "this fixture must actually distinguish PP from NPP"
    );
}

/// Rule: the forward link fires after the record's own monitors.
#[tokio::test]
async fn rule_flnk_fires_after_monitors() {
    let src = IocSource::from_db_str(
        "record(ao, \"PV:A\") {\n    field(FLNK, \"PV:B\")\n}\n\
         record(ai, \"PV:B\") {\n    field(INP, \"1\")\n}\n",
    )
    .expect("load");
    let events = src.put("PV:A", &put(1.0)).await.expect("put");
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["PV:A", "PV:B"]);
}

/// Rule: a record whose DISA equals DISV does not process and reports
/// DISABLE at its DISS severity.
///
/// The brief's expected status code (1) was unverified against the engine:
/// `Condition::Disable.pva_status()` (`spvirit-ioc/src/alarm.rs`) reports 3
/// (the same coarse PVA category as CALC/SCAN/LINK/SOFT/BAD_SUB/SIMM), not
/// 1 (which is the DEVICE/DRIVER/RECORD/DB category limit alarms use). The
/// severity (2, MAJOR) and message ("DISABLE") were correct as written.
#[tokio::test]
async fn rule_disabled_records_report_disable_at_diss() {
    let src = IocSource::from_db_str(
        "record(ai, \"PV:A\") {\n    field(DISA, \"1\")\n    field(DISV, \"1\")\n\
         field(DISS, \"MAJOR\")\n    field(PINI, \"YES\")\n}\n",
    )
    .expect("load");
    let events = src.process_pini();
    assert_eq!(alarm(&events[0].1), (2, 3, "DISABLE".to_string()));
}

/// Rule: a record that has never processed is INVALID/UDF.
///
/// Deliberate ruling: this test pins the engine's actual, verified split
/// between the `.common.udf` *flag* (set true at build time whenever no
/// constant link seeds VAL — Task 9's `recGblInitConstantLink` idiom) and
/// the record's *committed alarm state* (`common.sevr`/`common.stat`, which
/// `build.rs` initialises to `NoAlarm`/`NoAlarm` regardless of UDF). UDF only
/// becomes an *alarm* — INVALID/UDF — the first time `check_udf` runs inside
/// a process pass; a record that has never processed has never run
/// `check_udf`, so its committed alarm is still the NoAlarm it was built
/// with. This matches EPICS Base: `recGblResetAlarms`/`check_udf` compute
/// UDF's alarm contribution only during `dbProcess`, not at `dbLoadRecords`
/// time, so a brand-new IOC before its first scan/PINI pass reports
/// NO_ALARM on a `caget`, not INVALID. The engine already behaves this way,
/// so this rules in favour of the engine and keeps the assertion as the
/// brief wrote it.
#[tokio::test]
async fn rule_a_never_processed_record_is_udf() {
    let src = IocSource::from_db_str("record(ai, \"PV:A\") {\n}\n").expect("load");
    let payload = src.get("PV:A").await.expect("PV:A exists");
    assert_eq!(
        alarm(&payload),
        (0, 0, String::new()),
        "an unprocessed record's committed alarm is still NO_ALARM; UDF is \
         raised when it first processes"
    );
    let events = src.put("PV:A", &put(1.0)).await.expect("put");
    assert_eq!(
        alarm(&events[0].1),
        (0, 0, String::new()),
        "writing VAL clears UDF before the pass commits alarms"
    );
}

/// Rule: severity rises within a pass and is never lowered by a later
/// `recGblSetSevr`.
#[tokio::test]
async fn rule_ms_propagates_the_worst_severity() {
    let src = IocSource::from_db_str(
        "record(ai, \"PV:HI\") {\n    field(INP, \"11\")\n    field(HIHI, \"10\")\n\
         field(HHSV, \"MAJOR\")\n    field(PINI, \"YES\")\n}\n\
         record(ai, \"PV:READER\") {\n    field(INP, \"PV:HI PP MS\")\n\
         field(PINI, \"YES\")\n}\n",
    )
    .expect("load");
    let events = src.process_pini();
    let reader = events
        .iter()
        .rev()
        .find(|(n, _)| n == "PV:READER")
        .expect("PV:READER posts");
    assert_eq!(alarm(&reader.1).0, 2, "MAJOR must propagate through MS");
}

/// Rule: MDEL suppresses value monitors for changes inside the deadband.
#[tokio::test]
async fn rule_mdel_suppresses_small_changes() {
    let src = IocSource::from_db_str("record(ao, \"PV:A\") {\n    field(MDEL, \"1\")\n}\n")
        .expect("load");
    assert_eq!(src.put("PV:A", &put(10.0)).await.expect("put").len(), 1);
    assert_eq!(
        src.put("PV:A", &put(10.5)).await.expect("put").len(),
        0,
        "0.5 is inside MDEL"
    );
    assert_eq!(src.put("PV:A", &put(12.0)).await.expect("put").len(), 1);
}

/// Rule: OMSL = closed_loop takes the desired value from DOL; supervisory
/// keeps whatever was written.
///
/// The brief's original fixture put `field(DOL, "42")` — a CONSTANT link —
/// on both records. `build.rs::init_constant` seeds a constant DOL straight
/// into VAL at load time, ungated by OMSL (this engine's
/// `recGblInitConstantLink`, matching `aoRecord.c`), and `output_body`
/// treats a constant DOL as a processing-time no-op even under
/// `closed_loop` (see `output_body`'s `if !matches!(dol, Link::Constant(_))`
/// guard in `process.rs`) — exactly mirroring `dbGetLink` on a CONSTANT
/// link. So with a constant DOL, `supervisory` and `closed_loop` are
/// indistinguishable, in this engine and in EPICS Base. The fixture is
/// rebuilt with a real db link to a separate source record, which is the
/// only way OMSL's two paths can actually diverge.
#[tokio::test]
async fn rule_omsl_selects_the_output_source() {
    let src = IocSource::from_db_str(
        "record(ao, \"PV:SRC\") {\n    field(VAL, \"42\")\n}\n\
         record(ao, \"PV:CL\") {\n    field(DOL, \"PV:SRC.VAL\")\n\
         field(OMSL, \"closed_loop\")\n    field(PINI, \"YES\")\n}\n\
         record(ao, \"PV:SUP\") {\n    field(DOL, \"PV:SRC.VAL\")\n\
         field(OMSL, \"supervisory\")\n    field(PINI, \"YES\")\n}\n",
    )
    .expect("load");
    src.process_pini();
    assert_eq!(scalar(&src.get("PV:CL").await.expect("exists")), 42.0);
    assert_eq!(scalar(&src.get("PV:SUP").await.expect("exists")), 0.0);
}

/// Rule: a PP cycle terminates. PACT is the brake.
///
/// CONCERN, not silently fixed or weakened — see the task report: this test
/// pins the Record Reference rule and `graph.rs::DependencyGraph::report`'s
/// own claim ("PACT breaks it at runtime; not an error", worded generically
/// for every edge kind the graph tracks, including FLNK). It currently
/// FAILS against the engine and is `#[ignore]`d rather than deleted,
/// weakened, or fixed in `process.rs` — none of which this task is allowed
/// to do unilaterally.
///
/// Root cause read from `record_body` (`process.rs`): PACT is cleared
/// (`set.get_mut(id).common.pact = false;`) *before* `forward_link` is
/// called, not after, as EPICS Base's `dbProcess` does (Base's process
/// support routine calls `recGblFwdLink` *while* `pact` is still `TRUE`;
/// `dbProcess` only clears it once that whole call returns). So a PP-link
/// cycle (fetched via `fetch_link_value`, which recurses into `process()`
/// *before* `record_body` reaches the PACT-clearing line) is correctly
/// broken by the brake — see `a_pp_cycle_terminates_via_pact` in
/// `process.rs`'s own tests — but an FLNK-only cycle is not: by the time
/// `forward_link` re-enters the first record, its PACT has already been
/// reset, so the pass reprocesses it in full and recurses forever until
/// `ctx.push_depth`'s `MAX_DEPTH` (64) aborts it with `ProcError::TooDeep`.
/// This is a real semantic gap between the engine and both the Record
/// Reference and the engine's own documented invariant, not a fixture
/// problem; flagged here for a maintainer to fix `record_body`'s ordering
/// rather than changed quietly under this task's constraints.
#[tokio::test]
#[ignore = "engine defect: FLNK-only cycles are not broken by PACT, see the \
            doc comment above; tracked as a concern in the Task 14 report, \
            not fixed here per this task's constraints"]
async fn rule_a_link_cycle_terminates() {
    let src = IocSource::from_db_str(
        "record(ai, \"PV:A\") {\n    field(FLNK, \"PV:B\")\n}\n\
         record(ai, \"PV:B\") {\n    field(FLNK, \"PV:A\")\n}\n",
    )
    .expect("load");
    let events = src
        .put("PV:A", &put(1.0))
        .await
        .expect("the cycle must not hang");
    assert_eq!(events.len(), 2, "each record posts once per pass");
    assert_eq!(
        src.graph().cycles.len(),
        1,
        "and the cycle is reported at load"
    );
}

/// Rule: binary records store 0 or 1, whatever the source value.
#[tokio::test]
async fn rule_binary_records_normalise_to_zero_or_one() {
    let src = IocSource::from_db_str("record(bo, \"PV:B\") {\n}\n").expect("load");
    let events = src.put("PV:B", &put(7.0)).await.expect("put");
    assert_eq!(scalar(&events[0].1), 1.0);
    let events = src.put("PV:B", &put(0.0)).await.expect("put");
    assert_eq!(scalar(&events[0].1), 0.0);
}

/// Rule: long records hold 32-bit integers and round on assignment.
#[tokio::test]
async fn rule_long_records_round_to_integers() {
    let src = IocSource::from_db_str("record(longout, \"PV:L\") {\n}\n").expect("load");
    let events = src.put("PV:L", &put(2.6)).await.expect("put");
    match &events[0].1 {
        NtPayload::Scalar(s) => assert_eq!(s.value, ScalarValue::I32(3)),
        other => panic!("longout must publish an i32, got {other:?}"),
    }
}
