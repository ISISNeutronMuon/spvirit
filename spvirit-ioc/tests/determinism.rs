//! The same database plus the same puts must produce the same event stream,
//! every time. Lock-set numbering, slot assignment and monitor ordering are
//! all derived from the `.db` file order, so nothing here may depend on hash
//! iteration order.

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_ioc::IocSource;
use spvirit_server::pvstore::Source;

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

async fn run() -> Vec<String> {
    let src = IocSource::from_db_str(DB).expect("load");
    let mut trace = Vec::new();
    for v in [1.0f64, 2.0, 2.0, 5.0] {
        let events = src
            .put("PV:A", &DecodedValue::Float64(v))
            .await
            .expect("put succeeds");
        for (name, _) in events {
            trace.push(format!("{v}:{name}"));
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
        assert_eq!(next, first, "run {attempt} diverged");
    }
}

#[tokio::test]
async fn lock_set_partitioning_is_identical_across_runs() {
    let first = IocSource::from_db_str(DB).expect("load").graph().lock_sets;
    for attempt in 0..20 {
        let next = IocSource::from_db_str(DB).expect("load").graph().lock_sets;
        assert_eq!(next, first, "run {attempt} partitioned differently");
    }
}
