//! Integration tests for IOC/QSRV-style record field access
//! (`<pvname>.<FIELD>`, `<FIELD>$`) served by `spserver`.

use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use spvirit_client::pvget;
use spvirit_codec::spvd_decode::{DecodedValue, format_compact_value};
use spvirit_tools::spvirit_client::types::PvGetOptions;

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

fn write_temp_db() -> String {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("pva_fields_test_{}.db", std::process::id()));
    let contents = r#"
record(ao, "SIM:AO") {
    field(VAL, "2.34")
    field(DESC, "Analog output")
}
"#;
    std::fs::write(&path, contents).unwrap();
    path.to_string_lossy().to_string()
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

/// Extract the top-level "value" field from a decoded NT structure.
fn value_field(v: &DecodedValue) -> Option<&DecodedValue> {
    match v {
        DecodedValue::Structure(fields) => fields
            .iter()
            .find(|(name, _)| name == "value")
            .map(|(_, val)| val),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ioc_field_access_integration() {
    let tcp_port = match free_tcp_port() {
        Some(p) => p,
        None => {
            eprintln!("Skipping test: cannot bind TCP port in this environment");
            return;
        }
    };
    let udp_port = match free_udp_port() {
        Some(p) => p,
        None => {
            eprintln!("Skipping test: cannot bind UDP port in this environment");
            return;
        }
    };
    let db_path = write_temp_db();

    let server_bin = workspace_bin("spserver");
    let mut child = Command::new(server_bin)
        .arg("--db-file")
        .arg(&db_path)
        .arg("--listen-addr")
        .arg("127.0.0.1")
        .arg("--tcp-port")
        .arg(tcp_port.to_string())
        .arg("--udp-port")
        .arg(udp_port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn spvirit_server");

    thread::sleep(Duration::from_millis(300));

    // String-valued metadata fields.
    for (field_pv, expected) in [
        ("SIM:AO.RTYP", "ao"),
        ("SIM:AO.NAME", "SIM:AO"),
        ("SIM:AO.DESC", "Analog output"),
        ("SIM:AO.SCAN", "Passive"), // dbCommon default, absent from the .db
    ] {
        let opts = local_pvget_opts(field_pv, tcp_port, udp_port);
        let result = pvget(&opts).await.unwrap_or_else(|e| {
            let _ = child.kill();
            panic!("pvget {field_pv} failed: {e}");
        });
        let value = format_compact_value(&result.value);
        assert!(
            value.contains(expected),
            "{field_pv}: expected '{expected}' in '{value}'"
        );
    }

    // Obsolete '$' long-string form: DESC as an Int8 array of UTF-8 bytes.
    let opts = local_pvget_opts("SIM:AO.DESC$", tcp_port, udp_port);
    let result = pvget(&opts).await.unwrap_or_else(|e| {
        let _ = child.kill();
        panic!("pvget SIM:AO.DESC$ failed: {e}");
    });
    match value_field(&result.value) {
        Some(DecodedValue::Array(items)) => {
            let bytes: Vec<u8> = items
                .iter()
                .map(|item| match item {
                    DecodedValue::Int8(b) => *b as u8,
                    other => panic!("expected Int8 element, got {other:?}"),
                })
                .collect();
            assert_eq!(String::from_utf8_lossy(&bytes), "Analog output");
        }
        other => {
            let _ = child.kill();
            panic!("SIM:AO.DESC$ value is not an array: {other:?}");
        }
    }

    // Field PVs are read-only: spput must exit non-zero.
    let pvput_bin = workspace_bin("spput");
    let status = Command::new(pvput_bin)
        .arg("--server")
        .arg(format!("127.0.0.1:{}", tcp_port))
        .arg("SIM:AO.DESC")
        .arg("nope")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run spput");
    assert!(!status.success(), "spput to a read-only field PV must fail");

    let _ = child.kill();
    let _ = child.wait();
}
