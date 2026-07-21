//! Dynamic add / edit / remove of PVs against a running server, over the wire.

use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::time::Duration;

use spvirit_client::pvget;
use spvirit_codec::spvd_decode::format_compact_value;
use spvirit_tools::spvirit_client::types::PvGetOptions;
use spvirit_tools::spvirit_server::pv::AnyPv;
use spvirit_tools::spvirit_server::pva_server::PvaServer;
use spvirit_types::ScalarValue;

fn free_tcp_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok().map(|a| a.port())
}
fn free_udp_port() -> Option<u16> {
    UdpSocket::bind("127.0.0.1:0").ok()?.local_addr().ok().map(|a| a.port())
}
fn opts(pv: &str, tcp: u16, udp: u16, timeout: Duration) -> PvGetOptions {
    let mut o = PvGetOptions::new(pv.to_string());
    o.tcp_port = tcp;
    o.udp_port = udp;
    o.timeout = timeout;
    o.search_addr = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
    o.bind_addr = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
    o
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_add_edit_remove_over_wire() {
    let (Some(tcp), Some(udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping: cannot bind ports");
        return;
    };

    let server = PvaServer::serve(Vec::<AnyPv>::new())
        .port(tcp)
        .udp_port(udp)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .start()
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // ADD: writable i32 PV appears and is gettable.
    let _h = server.add_scalar("DYN:VAL", ScalarValue::I32(42), true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = pvget(&opts("DYN:VAL", tcp, udp, Duration::from_secs(5)))
        .await
        .expect("pvget after add");
    assert!(
        format_compact_value(&r.value).contains("42"),
        "expected 42, got {}",
        format_compact_value(&r.value)
    );

    // EDIT: value updates over the wire.
    server.store().set_value("DYN:VAL", ScalarValue::I32(99)).await;
    let r = pvget(&opts("DYN:VAL", tcp, udp, Duration::from_secs(5)))
        .await
        .expect("pvget after edit");
    assert!(
        format_compact_value(&r.value).contains("99"),
        "expected 99, got {}",
        format_compact_value(&r.value)
    );

    // REMOVE: PV no longer resolves (short timeout — negative lookup).
    assert!(server.store().remove("DYN:VAL").await);
    let res = pvget(&opts("DYN:VAL", tcp, udp, Duration::from_millis(800))).await;
    assert!(res.is_err(), "removed PV should not resolve, got {res:?}");

    server.abort();
}
