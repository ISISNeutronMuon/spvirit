//! The same database plus the same puts must produce the same event stream,
//! every time. Lock-set numbering, slot assignment and monitor ordering are
//! all derived from the `.db` file order, so nothing here may depend on hash
//! iteration order.
//!
//! What "the same" covers, per the spec and this crate's own doc comments,
//! is stronger than "the same event names in the same order": it is byte for
//! byte on lock-set slot assignment, on each posted payload's contents
//! (value, alarm severity and status), and on the load-time graph report
//! text. The only thing that is *not* pinned is the wall-clock timestamp
//! `record_body` stamps at process time (`now_ns`) -- comparing that would
//! make every one of these tests flaky, not more rigorous, so timestamps are
//! stripped out of the comparable representation below rather than ignored
//! by omission.

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_ioc::IocSource;
use spvirit_server::pvstore::Source;
use spvirit_types::NtPayload;

const DB: &str = "\
record(ao, \"PV:A\") {
    field(OUT, \"PV:B.VAL\")
    field(FLNK, \"PV:B\")
}
record(ai, \"PV:B\") {
    field(INP, \"PV:B.VAL NPP\")
    field(FLNK, \"PV:C\")
}
record(ai, \"PV:C\") {
    field(INP, \"PV:B PP\")
    field(FLNK, \"PV:D\")
}
record(longin, \"PV:D\") {
    field(INP, \"PV:C NPP\")
}
";

/// Everything about a posted payload that determinism must pin, with the
/// wall-clock timestamp deliberately excluded (see the module doc comment).
fn stable_payload(payload: &NtPayload) -> String {
    match payload {
        NtPayload::Scalar(s) => format!(
            "value={:?} severity={} status={} message={:?}",
            s.value, s.alarm_severity, s.alarm_status, s.alarm_message
        ),
        other => panic!("this suite's DB only posts NtPayload::Scalar, got {other:?}"),
    }
}

async fn run() -> Vec<String> {
    let src = IocSource::from_db_str(DB).expect("load");
    let mut trace = Vec::new();
    for v in [1.0f64, 2.0, 2.0, 5.0] {
        let events = src
            .put("PV:A", &DecodedValue::Float64(v))
            .await
            .expect("put succeeds");
        for (name, payload) in events {
            trace.push(format!("{v}:{name}:{}", stable_payload(&payload)));
        }
    }
    trace
}

#[tokio::test]
async fn the_event_stream_is_identical_across_runs() {
    let first = run().await;
    assert!(!first.is_empty(), "the sequence must produce events");
    for attempt in 0..20 {
        let next = run().await;
        assert_eq!(
            next, first,
            "run {attempt} diverged (names, order, or payload contents)"
        );
    }
}

#[tokio::test]
async fn lock_set_partitioning_is_identical_across_runs() {
    // `lock_sets` is `Vec<Vec<String>>`: the outer order is lock-set number,
    // the inner order is slot assignment within that set. `assert_eq!` on
    // nested `Vec`s is positional, so this already pins both -- not just
    // set *membership* but the exact slot each record lands in, which is
    // what a client's `RecordId { set, slot }` addressing (and therefore
    // process order within a lock set) depends on.
    let first = IocSource::from_db_str(DB).expect("load").graph().lock_sets;
    for attempt in 0..20 {
        let next = IocSource::from_db_str(DB).expect("load").graph().lock_sets;
        assert_eq!(
            next, first,
            "run {attempt} assigned lock sets or slots differently"
        );
        for (set_index, (a, b)) in next.iter().zip(first.iter()).enumerate() {
            assert_eq!(
                a, b,
                "run {attempt} disagreed on slot order within lock set {set_index}"
            );
        }
    }
}

#[tokio::test]
async fn the_graph_report_text_is_identical_across_runs() {
    // The load-time diagnostics (`DependencyGraph::report`) are as much a
    // pinned artifact as the event stream: a `.db` author diffing startup
    // logs across two otherwise-identical IOC runs must see identical text,
    // not text whose finding order shuffled because it was built over a
    // HashMap/HashSet somewhere in the graph pass.
    let db = "\
record(ai, \"PV:HUB\") {
}
record(ai, \"PV:LOOP_A\") {
    field(INP, \"PV:LOOP_B PP\")
}
record(ai, \"PV:LOOP_B\") {
    field(INP, \"PV:LOOP_A PP\")
}
record(ai, \"PV:ORPHAN\") {
}
record(ai, \"PV:DANGLING\") {
    field(INP, \"PV:MISSING PP\")
}
"
    .to_string()
        + &(0..12)
            .map(|i| format!("record(ai, \"PV:DEP{i}\") {{\n    field(INP, \"PV:HUB PP\")\n}}\n"))
            .collect::<String>();

    let first = IocSource::from_db_str(&db).expect("load").graph().report();
    assert!(
        !first.is_empty(),
        "this fixture must exercise every finding kind"
    );
    for attempt in 0..20 {
        let next = IocSource::from_db_str(&db).expect("load").graph().report();
        assert_eq!(next, first, "run {attempt} produced different report text");
    }
}
