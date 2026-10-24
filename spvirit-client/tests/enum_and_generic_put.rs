//! Wire PUTs to enum and generic records either change the record or are
//! answered with a PUT error — never reported as success with no change.
//!
//! A real NTEnum PUT carries `value` as the sub-structure `{index, choices}`;
//! the server must unwrap `index`. An out-of-range index is refused with the
//! record left untouched (not even restamped). `mbbi` is deliberately
//! read-only over the wire in spvirit (EPICS Base allows VAL writes to input
//! records); `mbbo`/`bo` stay writable. A `Generic`
//! record has no wire PUT path, so a PUT to it is refused rather than silently
//! dropped. Checked end to end: a real `PvaClient` against a real `PvaServer`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::time::Duration;

use spvirit_client::PvaClient;
use spvirit_server::{PvaServer, RunningServer, SimplePvStore};
use spvirit_types::{NtPayload, NtTimeStamp, PvValue, ScalarValue};

const MBBO: &str = "EPUT:MBBO";
const MBBI: &str = "EPUT:MBBI";
const BO: &str = "EPUT:BO";
const GENERIC: &str = "EPUT:GENERIC";

fn free_ports() -> (u16, u16) {
    let tcp = TcpListener::bind("127.0.0.1:0")
        .expect("bind tcp")
        .local_addr()
        .expect("local_addr")
        .port();
    let udp = UdpSocket::bind("127.0.0.1:0")
        .expect("bind udp")
        .local_addr()
        .expect("local_addr")
        .port();
    (tcp, udp)
}

fn choices() -> Vec<String> {
    vec!["Stop".into(), "Run".into(), "Fault".into()]
}

async fn start() -> (RunningServer, PvaClient) {
    let (tcp_port, udp_port) = free_ports();
    let running = PvaServer::builder()
        .mbbo(MBBO, choices(), 0)
        .mbbi(MBBI, choices(), 0)
        .bo(BO, false)
        .generic(
            GENERIC,
            "epics:nt/NTScalar:1.0",
            vec![("value".to_string(), PvValue::Scalar(ScalarValue::I32(3)))],
        )
        .port(tcp_port)
        .udp_port(udp_port)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .build()
        .start()
        .await
        .expect("server start");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let ns: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();
    let client = PvaClient::builder()
        .port(tcp_port)
        .udp_port(udp_port)
        .timeout(Duration::from_secs(5))
        .name_server(ns)
        .no_broadcast()
        .build();
    (running, client)
}

async fn enum_state(store: &SimplePvStore, name: &str) -> (i32, NtTimeStamp) {
    match store.get_nt(name).await.expect("enum record") {
        NtPayload::Enum(nt) => (nt.index, nt.time_stamp),
        other => panic!("expected an enum payload, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_range_enum_index_put_changes_the_pv_and_reads_back() {
    let (running, client) = start().await;

    client
        .pvput(MBBO, serde_json::json!({"value": {"index": 2}}))
        .await
        .expect("an in-range index is accepted");

    assert_eq!(enum_state(running.store(), MBBO).await.0, 2);
    let got = client.pvget(MBBO).await.expect("pvget");
    let rendered = format!("{:?}", got.value);
    assert!(
        rendered.contains("Int32(2)"),
        "the client must read back index 2, got {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_out_of_range_enum_index_put_is_refused_and_changes_nothing() {
    let (running, client) = start().await;
    let (index_before, ts_before) = enum_state(running.store(), MBBO).await;

    let result = client
        .pvput(MBBO, serde_json::json!({"value": {"index": 7}}))
        .await;

    assert!(
        result.is_err(),
        "index 7 is out of range for three choices; the client must see a PUT error, got {result:?}"
    );
    let (index_after, ts_after) = enum_state(running.store(), MBBO).await;
    assert_eq!(index_after, index_before, "a refused PUT must not mutate");
    assert_eq!(ts_after, ts_before, "a refused PUT must not restamp");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_put_to_an_mbbi_is_refused() {
    let (running, client) = start().await;

    let result = client
        .pvput(MBBI, serde_json::json!({"value": {"index": 1}}))
        .await;

    assert!(
        result.is_err(),
        "mbbi is read-only over the wire; the client must see a PUT error, got {result:?}"
    );
    assert_eq!(enum_state(running.store(), MBBI).await.0, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mbbi_can_still_be_driven_server_side() {
    let (running, _client) = start().await;

    assert!(
        running.store().set_value(MBBI, ScalarValue::I32(2)).await,
        "the host may still update an mbbi"
    );
    assert_eq!(enum_state(running.store(), MBBI).await.0, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_put_to_a_bo_still_works() {
    let (running, client) = start().await;

    client.pvput(BO, true).await.expect("bo is writable");

    match running.store().get_nt(BO).await.expect("bo record") {
        NtPayload::Scalar(nt) => assert_eq!(nt.value, ScalarValue::Bool(true)),
        other => panic!("expected a scalar payload, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_put_to_a_generic_record_is_refused() {
    let (running, client) = start().await;

    let result = client.pvput(GENERIC, 5).await;

    assert!(
        result.is_err(),
        "a Generic record has no wire PUT path; the client must see a PUT error, got {result:?}"
    );
    match running
        .store()
        .get_nt(GENERIC)
        .await
        .expect("generic record")
    {
        NtPayload::Generic { fields, .. } => assert_eq!(
            fields,
            vec![("value".to_string(), PvValue::Scalar(ScalarValue::I32(3)))]
        ),
        other => panic!("expected a generic payload, got {other:?}"),
    }
}
