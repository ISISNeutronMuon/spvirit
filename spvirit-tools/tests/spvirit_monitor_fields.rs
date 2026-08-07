//! End-to-end coverage for nested `pvRequest` field selection on MONITOR.
//!
//! Spawns the in-tree `spserver` binary against a temp DB and uses the
//! Rust client (`pvmonitor` / `pvmonitor_fields`) to verify:
//!
//! - subscribing without `fields` returns the full NTScalar structure, and
//! - subscribing with `fields = ["value"]` only returns the `value` leaf.
//!
//! These tests assert end-to-end behaviour of the pvRequest-encode →
//! server-filter → wire-format → client-decode pipeline introduced by the
//! partial-field selection feature.

mod protocol;

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use spvirit_client::PvaClient;
use spvirit_codec::spvd_decode::DecodedValue;

use protocol::frame_harness::TestServer;

const PV: &str = "SIM:AI";

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

/// Top-level field names of a structure value.
fn top_level_field_names(value: &DecodedValue) -> Vec<String> {
    match value {
        DecodedValue::Structure(fields) => fields.iter().map(|(n, _)| n.clone()).collect(),
        _ => Vec::new(),
    }
}

#[tokio::test]
async fn monitor_unfiltered_returns_full_structure() {
    let server = TestServer::spawn().expect("spawn server");
    let client = build_client(&server);

    let received: Arc<StdMutex<Option<DecodedValue>>> = Arc::new(StdMutex::new(None));
    let received_cb = received.clone();

    let monitor = client.pvmonitor(PV, move |update| {
        *received_cb.lock().unwrap() = Some(update.value.clone());
        ControlFlow::Break(())
    });

    tokio::time::timeout(Duration::from_secs(3), monitor)
        .await
        .expect("monitor timeout")
        .expect("monitor error");

    let value = received.lock().unwrap().take().expect("at least one frame");
    let names = top_level_field_names(&value);
    assert!(
        names.iter().any(|n| n == "value"),
        "unfiltered monitor should include 'value', got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "alarm"),
        "unfiltered monitor should include 'alarm', got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "timeStamp"),
        "unfiltered monitor should include 'timeStamp', got: {names:?}"
    );
}

#[tokio::test]
async fn monitor_value_only_returns_only_value_field() {
    let server = TestServer::spawn().expect("spawn server");
    let client = build_client(&server);

    let received: Arc<StdMutex<Option<DecodedValue>>> = Arc::new(StdMutex::new(None));
    let received_cb = received.clone();

    let monitor = client.pvmonitor_fields(PV, &["value"], move |update| {
        *received_cb.lock().unwrap() = Some(update.value.clone());
        ControlFlow::Break(())
    });

    tokio::time::timeout(Duration::from_secs(3), monitor)
        .await
        .expect("monitor timeout")
        .expect("monitor error");

    let value = received.lock().unwrap().take().expect("at least one frame");
    let names = top_level_field_names(&value);
    assert_eq!(
        names,
        vec!["value".to_string()],
        "filtered monitor should only contain 'value', got: {names:?}"
    );
}

#[tokio::test]
async fn monitor_nested_path_returns_only_alarm_severity() {
    let server = TestServer::spawn().expect("spawn server");
    let client = build_client(&server);

    let received: Arc<StdMutex<Option<DecodedValue>>> = Arc::new(StdMutex::new(None));
    let received_cb = received.clone();

    let monitor = client.pvmonitor_fields(PV, &["alarm.severity"], move |update| {
        *received_cb.lock().unwrap() = Some(update.value.clone());
        ControlFlow::Break(())
    });

    tokio::time::timeout(Duration::from_secs(3), monitor)
        .await
        .expect("monitor timeout")
        .expect("monitor error");

    let value = received.lock().unwrap().take().expect("at least one frame");
    let DecodedValue::Structure(fields) = &value else {
        panic!("expected top-level structure, got {value:?}");
    };
    assert_eq!(fields.len(), 1, "should only have 'alarm', got: {fields:?}");
    assert_eq!(fields[0].0, "alarm");
    let DecodedValue::Structure(inner) = &fields[0].1 else {
        panic!("expected alarm to be a structure, got {:?}", fields[0].1);
    };
    assert_eq!(inner.len(), 1, "alarm should only contain 'severity'");
    assert_eq!(inner[0].0, "severity");
}
