//! Wire-level integration test for the PV-handle builder API.

use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::process::{Command, Stdio};
use std::time::Duration;

use spvirit_client::pvget;
use spvirit_codec::spvd_decode::format_compact_value;
use spvirit_tools::spvirit_client::types::PvGetOptions;
use spvirit_tools::spvirit_server::pv::{AnyPv, Pv};
use spvirit_tools::spvirit_server::pva_server::PvaServer;

fn workspace_bin(name: &str) -> String {
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let test_exe = std::env::current_exe().expect("cannot locate test executable");
    test_exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("{name}{ext}"))
        .to_string_lossy()
        .to_string()
}

fn free_tcp_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|addr| addr.port())
}

fn free_udp_port() -> Option<u16> {
    UdpSocket::bind("127.0.0.1:0")
        .ok()
        .and_then(|s| s.local_addr().ok())
        .map(|addr| addr.port())
}

fn local_pvget_opts(pv_name: &str, tcp_port: u16, udp_port: u16) -> PvGetOptions {
    let mut opts = PvGetOptions::new(pv_name.to_string());
    opts.udp_port = udp_port;
    opts.tcp_port = tcp_port;
    // CI containers often do not route UDP broadcast to loopback listeners.
    // For local test servers, force explicit loopback discovery/bind.
    opts.search_addr = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
    opts.bind_addr = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
    opts
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pv_handle_api_end_to_end() {
    let (Some(tcp_port), Some(udp_port)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping: cannot bind ports");
        return;
    };

    let temp = Pv::ai("HND:TEMP", 22.5)
        .units("C")
        .prec(2)
        .desc("Handle temp");
    let sp = Pv::ao("HND:SP", 25.0)
        .drive_limits(0.0, 100.0)
        .on_put(|_pv, v: f64| {
            if v > 100.0 {
                Err("over".into())
            } else {
                Ok(())
            }
        });

    let server = PvaServer::serve([AnyPv::from(temp.clone()), AnyPv::from(sp.clone())])
        .port(tcp_port)
        .udp_port(udp_port)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .start()
        .await
        .expect("server start hooks must succeed");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // GET sees the handle-built PV with metadata.
    let opts = local_pvget_opts("HND:TEMP", tcp_port, udp_port);
    let result = pvget(&opts).await.expect("pvget HND:TEMP");
    let value = format_compact_value(&result.value);
    assert!(
        value.contains("22.5"),
        "HND:TEMP: expected '22.5' in '{value}'"
    );

    // Field access works on handle-built records (spec requirement).
    let opts = local_pvget_opts("HND:TEMP.RTYP", tcp_port, udp_port);
    let result = pvget(&opts).await.expect("pvget .RTYP");
    let value = format_compact_value(&result.value);
    assert!(
        value.contains("ai"),
        "HND:TEMP.RTYP: expected 'ai' in '{value}'"
    );

    // set() posts a new value visible over the wire.
    temp.set(23.75).await.unwrap();
    let opts = local_pvget_opts("HND:TEMP", tcp_port, udp_port);
    let result = pvget(&opts).await.expect("pvget after set");
    let value = format_compact_value(&result.value);
    assert!(
        value.contains("23.75"),
        "HND:TEMP after set: expected '23.75' in '{value}'"
    );

    // Accepted PUT via spput; then rejected PUT must exit non-zero.
    let spput = workspace_bin("spput");
    let ok = Command::new(&spput)
        .args(["--server", &format!("127.0.0.1:{tcp_port}"), "HND:SP", "50"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spput ok");
    assert!(ok.success(), "in-range PUT must succeed");
    assert_eq!(sp.get().await, Ok(50.0));

    let rejected = Command::new(&spput)
        .args([
            "--server",
            &format!("127.0.0.1:{tcp_port}"),
            "HND:SP",
            "500",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spput rejected");
    assert!(
        !rejected.success(),
        "on_put Err must reject the PUT on the wire"
    );
    assert_eq!(sp.get().await, Ok(50.0), "value unchanged after rejection");

    server.abort();
}
