use std::io::BufRead;
use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use spvirit_client::pvget;
use spvirit_codec::spvd_decode::format_compact_value;
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
    let path = dir.join(format!("pva_test_{}.db", std::process::id()));
    let contents = r#"
record(ai, "SIM:AI") {
    field(VAL, "1.23")
}
record(ai, "SIM:AI_SIM") {
    field(VAL, "9.87")
    field(SIMM, "YES")
}
record(ao, "SIM:AO") {
    field(VAL, "2.34")
}
record(bi, "SIM:BI") {
    field(VAL, "1")
}
record(stringin, "SIM:STR") {
    field(VAL, "hello")
}
"#;
    std::fs::write(&path, contents).unwrap();
    path.to_string_lossy().to_string()
}

fn run_pvput(server: &str, pv: &str, value: &str) -> std::io::Result<()> {
    let pvput_bin = workspace_bin("spput");
    let status = Command::new(pvput_bin)
        .arg("--server")
        .arg(server)
        .arg(pv)
        .arg(value)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(
            "pvput failed",
        ));
    }
    Ok(())
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
async fn pvget_integration_smoke() {
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

    let opts = local_pvget_opts("SIM:AI", tcp_port, udp_port);
    let result = pvget(&opts).await.expect("pvget ok");
    let value = format_compact_value(&result.value);
    assert!(value.contains("1.23") || value.contains("1.230"));

    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pvput_integration_smoke() {
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

    let server_addr = format!("127.0.0.1:{}", tcp_port);
    run_pvput(&server_addr, "SIM:AO", "4.56").expect("pvput");

    let opts = local_pvget_opts("SIM:AO", tcp_port, udp_port);
    let result = pvget(&opts).await.expect("pvget ok");
    let value = format_compact_value(&result.value);
    assert!(value.contains("4.56") || value.contains("4.560"));

    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pvmonitor_integration_optional() {
    if std::env::var("PVA_TEST_MONITOR").is_err() {
        eprintln!("Skipping pvmonitor test: set PVA_TEST_MONITOR=1 to enable");
        return;
    }

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

    let server_addr = format!("127.0.0.1:{}", tcp_port);
    let pvmonitor_bin = workspace_bin("spmonitor");
    let mut monitor = Command::new(pvmonitor_bin)
        .arg("--server")
        .arg(&server_addr)
        .arg("SIM:AO")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pvmonitor");

    let (tx, rx) = mpsc::channel::<String>();
    let stdout = monitor.stdout.take().expect("pvmonitor stdout");
    thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        while reader
            .read_line(&mut line)
            .ok()
            .filter(|n| *n > 0)
            .is_some()
        {
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    thread::sleep(Duration::from_millis(300));
    run_pvput(&server_addr, "SIM:AO", "7.89").expect("pvput");

    let recv = rx.recv_timeout(Duration::from_secs(2));
    assert!(
        recv.map(|line| line.contains("7.89") || line.contains("7.890"))
            .unwrap_or(false),
        "expected pvmonitor output with updated value"
    );

    let _ = monitor.kill();
    let _ = monitor.wait();
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pvput_readonly_input_denied() {
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

    let server_addr = format!("127.0.0.1:{}", tcp_port);
    assert!(
        run_pvput(&server_addr, "SIM:AI", "4.56").is_err(),
        "expected pvput on input record to fail"
    );

    let opts = local_pvget_opts("SIM:AI", tcp_port, udp_port);
    let result = pvget(&opts).await.expect("pvget ok");
    let value = format_compact_value(&result.value);
    assert!(value.contains("1.23") || value.contains("1.230"));

    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pvput_ai_simulation_allowed() {
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

    let server_addr = format!("127.0.0.1:{}", tcp_port);
    run_pvput(&server_addr, "SIM:AI_SIM", "6.54").expect("pvput ai simulation");

    let opts = local_pvget_opts("SIM:AI_SIM", tcp_port, udp_port);
    let result = pvget(&opts).await.expect("pvget ok");
    let value = format_compact_value(&result.value);
    assert!(value.contains("6.54") || value.contains("6.540"));

    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pvget_external_optional() {
    let pv = match std::env::var("PVA_TEST_PV") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("Skipping external pvget: set PVA_TEST_PV to enable");
            return;
        }
    };
    let mut opts = PvGetOptions::new(pv);
    if let Ok(addr) = std::env::var("PVA_TEST_SERVER") {
        opts.server_addr = Some(addr.parse().expect("PVA_TEST_SERVER ip:port"));
    }
    if let Ok(addr) = std::env::var("PVA_TEST_SEARCH_ADDR") {
        opts.search_addr = Some(addr.parse().expect("PVA_TEST_SEARCH_ADDR ip"));
    }
    if let Ok(addr) = std::env::var("PVA_TEST_ADDR_LIST") {
        unsafe { std::env::set_var("EPICS_PVA_ADDR_LIST", addr) };
    }
    if let Ok(auto) = std::env::var("PVA_TEST_AUTO_ADDR_LIST") {
        unsafe { std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", auto) };
    }
    if let Ok(addr) = std::env::var("PVA_TEST_BIND_ADDR") {
        opts.bind_addr = Some(addr.parse().expect("PVA_TEST_BIND_ADDR ip"));
    }
    if let Ok(port) = std::env::var("PVA_TEST_UDP_PORT") {
        opts.udp_port = port.parse().expect("PVA_TEST_UDP_PORT");
    }
    if let Ok(port) = std::env::var("PVA_TEST_TCP_PORT") {
        opts.tcp_port = port.parse().expect("PVA_TEST_TCP_PORT");
    }

    let result = pvget(&opts).await.expect("pvget ok");
    let value = format_compact_value(&result.value);
    assert!(!value.is_empty(), "expected non-empty value");
}
