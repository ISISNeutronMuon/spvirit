//! End-to-end coverage for PUT restamping through the `spserver` daemon.
//!
//! Regression guard for the fix where a client PUT that left a scalar's
//! value unchanged did not advance `timeStamp`. Drives the daemon as a
//! subprocess (matching the other `protocol::frame_harness` based tests)
//! and uses the in-process `PvaClient` for the GET/PUT round-trips:
//!
//! - PUTting a *different* value must advance `timeStamp`, and
//! - PUTting the *same* value again must also advance `timeStamp` — that
//!   second assertion is the one that would have failed before the fix.

mod protocol;

use std::time::Duration;

use spvirit_client::PvaClient;
use spvirit_codec::spvd_decode::DecodedValue;

use protocol::frame_harness::TestServer;

const PV: &str = "SIM:AO";

fn build_client(server: &TestServer) -> PvaClient {
    let addr: std::net::SocketAddr = format!("127.0.0.1:{}", server.tcp_port)
        .parse()
        .expect("server addr parse");
    PvaClient::builder()
        .port(server.tcp_port)
        .udp_port(server.udp_port)
        .timeout(Duration::from_secs(2))
        .server_addr(addr)
        .build()
}

/// Extract `(secondsPastEpoch, nanoseconds)` from a decoded NTScalar value's
/// top-level `timeStamp` structure.
fn extract_time_stamp(value: &DecodedValue) -> (i64, i32) {
    let DecodedValue::Structure(fields) = value else {
        panic!("expected top-level structure, got {value:?}");
    };
    let (_, ts) = fields
        .iter()
        .find(|(n, _)| n == "timeStamp")
        .expect("timeStamp field present");
    let DecodedValue::Structure(ts_fields) = ts else {
        panic!("expected timeStamp to be a structure, got {ts:?}");
    };
    let secs = ts_fields
        .iter()
        .find(|(n, _)| n == "secondsPastEpoch")
        .map(|(_, v)| match v {
            DecodedValue::Int64(s) => *s,
            other => panic!("expected secondsPastEpoch to be Int64, got {other:?}"),
        })
        .expect("secondsPastEpoch field present");
    let nanos = ts_fields
        .iter()
        .find(|(n, _)| n == "nanoseconds")
        .map(|(_, v)| match v {
            DecodedValue::Int32(n) => *n,
            other => panic!("expected nanoseconds to be Int32, got {other:?}"),
        })
        .expect("nanoseconds field present");
    (secs, nanos)
}

#[tokio::test]
async fn put_advances_timestamp_even_when_value_unchanged() {
    let server = TestServer::spawn().expect("spawn server");
    let client = build_client(&server);

    let initial = client.pvget(PV).await.expect("initial pvget");
    let (secs0, nanos0) = extract_time_stamp(&initial.value);

    // PUT a different value: timeStamp must advance.
    client
        .pvput(PV, 9.99_f64)
        .await
        .expect("pvput different value");

    let after_change = client.pvget(PV).await.expect("pvget after value change");
    let (secs1, nanos1) = extract_time_stamp(&after_change.value);
    assert!(
        (secs1, nanos1) > (secs0, nanos0),
        "timeStamp did not advance after a value-changing PUT: \
         before=({secs0}, {nanos0}), after=({secs1}, {nanos1})"
    );

    // Real wall-clock gap so the second/nanosecond comparison below is not
    // racing clock resolution.
    tokio::time::sleep(Duration::from_millis(25)).await;

    // PUT the *same* value again: timeStamp must still advance. This is the
    // assertion that would have failed before the restamp-always fix, since
    // an unchanged value used to skip the timeStamp update entirely.
    client
        .pvput(PV, 9.99_f64)
        .await
        .expect("pvput same value again");

    let after_repeat = client.pvget(PV).await.expect("pvget after repeat PUT");
    let (secs2, nanos2) = extract_time_stamp(&after_repeat.value);
    assert!(
        (secs2, nanos2) > (secs1, nanos1),
        "timeStamp did not advance after a value-unchanged PUT: \
         before=({secs1}, {nanos1}), after=({secs2}, {nanos2})"
    );
}
