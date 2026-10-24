//! Alarm limits set through a `Pv` handle drive server-side alarm computation.
//!
//! `Pv::alarm_limits(lolo, low, high, hihi)` must fill in the limits the alarm
//! evaluator reads (`NtScalar::alarm_*`), not only the published `valueAlarm`
//! block — otherwise `.compute_alarms(true)` never raises a severity for a
//! handle-built PV. Checked end to end: a real `PvaClient` writes and reads.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::time::Duration;

use spvirit_client::PvaClient;
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::{Pv, PvaServer, RunningServer};

const PV: &str = "HANDLE:ALARM";

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

/// An `ao` at 50 with lolo/low/high/hihi = 5/10/90/110, served with
/// `compute_alarms(true)`.
async fn start() -> (RunningServer, Pv<f64>, PvaClient) {
    let (tcp_port, udp_port) = free_ports();
    let pv = Pv::ao(PV, 50.0).alarm_limits(5.0, 10.0, 90.0, 110.0);
    let running = PvaServer::serve([pv.clone()])
        .compute_alarms(true)
        .port(tcp_port)
        .udp_port(udp_port)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
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
    (running, pv, client)
}

fn field<'a>(v: &'a DecodedValue, name: &str) -> &'a DecodedValue {
    match v {
        DecodedValue::Structure(fields) => fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("no field {name} in {v:?}")),
        other => panic!("expected a structure, got {other:?}"),
    }
}

/// `(alarm.severity, alarm.message)` as a client sees them.
async fn alarm(client: &PvaClient) -> (i32, String) {
    let got = client.pvget(PV).await.expect("pvget");
    let alarm = field(&got.value, "alarm");
    let severity = match field(alarm, "severity") {
        DecodedValue::Int32(s) => *s,
        other => panic!("severity is not an int: {other:?}"),
    };
    let message = match field(alarm, "message") {
        DecodedValue::String(s) => s.clone(),
        other => panic!("message is not a string: {other:?}"),
    };
    (severity, message)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wire_put_past_handle_limits_raises_and_clears_the_alarm() {
    let (_running, _pv, client) = start().await;
    assert_eq!(alarm(&client).await, (0, String::new()), "50 is in range");

    client.pvput(PV, 120.0).await.expect("put past HIHI");
    assert_eq!(alarm(&client).await, (2, "HIHI".to_string()));

    client.pvput(PV, 95.0).await.expect("put past HIGH");
    assert_eq!(alarm(&client).await, (1, "HIGH".to_string()));

    client.pvput(PV, 7.0).await.expect("put past LOW");
    assert_eq!(alarm(&client).await, (1, "LOW".to_string()));

    client.pvput(PV, 0.0).await.expect("put past LOLO");
    assert_eq!(alarm(&client).await, (2, "LOLO".to_string()));

    client.pvput(PV, 50.0).await.expect("put back in range");
    assert_eq!(alarm(&client).await, (0, String::new()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_set_past_handle_limits_raises_the_alarm() {
    let (_running, pv, client) = start().await;

    pv.set(120.0).await.expect("host set past HIHI");
    assert_eq!(alarm(&client).await, (2, "HIHI".to_string()));

    pv.set(95.0).await.expect("host set past HIGH");
    assert_eq!(alarm(&client).await, (1, "HIGH".to_string()));

    pv.set(50.0).await.expect("host set back in range");
    assert_eq!(alarm(&client).await, (0, String::new()));
}
