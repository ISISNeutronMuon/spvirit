//! `.ioc()` end to end: a server built with the engine actually serves both
//! the record PV and its `.FIELD` channels over the wire.
//!
//! Every other `.ioc()` test in the branch builds a `PvaServer` and drops it,
//! so the one line in `serve_after_start_hooks` that registers the engine
//! (`sources.add_store("ioc", 5, ...)`) was asserted in prose and nowhere
//! else — delete it and the whole suite still passed. This file is the
//! mutant-killer: with that line removed both gets below fail with
//! "PV not found".
//!
//! It lives in `spvirit-ioc`'s test tree, not `spvirit-server`'s, because
//! `spvirit-ioc` depends on `spvirit-server` and never the reverse; a test
//! that needs both `PvaServer` and `IocSource` can only live downstream.
//! `spvirit-client` is a dev-dependency here for the same reason it is
//! everywhere else: it depends on neither, so there is no cycle.

use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use spvirit_client::{PvGetOptions, pvget};
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_ioc::IocSource;
use spvirit_server::pva_server::PvaServer;

const DB: &str = "record(ai, \"IOC:A\") {
    field(DESC, \"served by the engine\")
    field(EGU, \"C\")
    field(INP, \"7\")
}
";

fn free_tcp_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

fn free_udp_port() -> Option<u16> {
    UdpSocket::bind("127.0.0.1:0")
        .ok()
        .and_then(|s| s.local_addr().ok())
        .map(|a| a.port())
}

/// Options aimed at one known local server: `server_addr` bypasses UDP
/// search entirely, which CI containers do not reliably route to loopback.
fn opts_for(pv: &str, tcp: u16, udp: u16) -> PvGetOptions {
    let mut opts = PvGetOptions::new(pv.to_string());
    opts.server_addr = Some(format!("127.0.0.1:{tcp}").parse().expect("loopback addr"));
    opts.tcp_port = tcp;
    opts.udp_port = udp;
    opts.search_addr = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
    opts.bind_addr = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
    opts.timeout = Duration::from_secs(5);
    opts
}

/// The top-level `value` field of a decoded NT structure.
fn value_field(v: &DecodedValue) -> &DecodedValue {
    match v {
        DecodedValue::Structure(fields) => fields
            .iter()
            .find(|(name, _)| name == "value")
            .map(|(_, val)| val)
            .unwrap_or_else(|| panic!("no 'value' field in {v:?}")),
        other => panic!("expected a structure, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ioc_built_server_serves_records_and_fields_over_the_wire() {
    let (Some(tcp), Some(udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("skipping: cannot bind a loopback port in this environment");
        return;
    };

    let ioc = Arc::new(IocSource::from_db_str(DB).expect("the database loads"));
    let server = PvaServer::builder()
        .port(tcp)
        .udp_port(udp)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .ioc(ioc)
        .build();
    // `run()`'s error type is not `Send`, so map it to a string inside the
    // task rather than carrying it across the spawn boundary.
    let handle = tokio::spawn(async move { server.run().await.map_err(|e| e.to_string()) });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The record PV: only the engine can answer it — the builtin store is
    // empty, so a miss here means `.ioc()` never reached the registry.
    let record = pvget(&opts_for("IOC:A", tcp, udp))
        .await
        .expect("IOC:A must resolve through the .ioc() registration");
    match value_field(&record.value) {
        DecodedValue::Float64(v) => assert_eq!(*v, 7.0, "IOC:A"),
        other => panic!("IOC:A: expected a double, got {other:?}"),
    }

    // ... and a `.FIELD` PV on the same record, which is the half the
    // `add_store` (rather than `add`) choice and the order-5 slot exist for.
    for (pv, expected) in [
        ("IOC:A.DESC", "served by the engine"),
        ("IOC:A.EGU", "C"),
        ("IOC:A.RTYP", "ai"),
    ] {
        let got = pvget(&opts_for(pv, tcp, udp))
            .await
            .unwrap_or_else(|e| panic!("{pv} must resolve through the .ioc() registration: {e}"));
        match value_field(&got.value) {
            DecodedValue::String(s) => assert_eq!(s, expected, "{pv}"),
            other => panic!("{pv}: expected a string, got {other:?}"),
        }
    }

    handle.abort();
}
