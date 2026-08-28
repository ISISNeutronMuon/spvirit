use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use spvirit_tools::spvirit_client::search::{SearchTarget, discover_servers, search_pv};
use spvirit_tools::spvirit_client::types::PvGetError;

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
        .and_then(|listener| listener.local_addr().ok())
        .map(|address| address.port())
}

fn free_udp_port() -> Option<u16> {
    UdpSocket::bind("127.0.0.1:0")
        .ok()
        .and_then(|socket| socket.local_addr().ok())
        .map(|address| address.port())
}

fn write_temp_db() -> String {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "pva_search_resilience_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let contents = r#"
record(ai, "SIM:AI") {
    field(VAL, "1.23")
}
"#;
    std::fs::write(&path, contents).expect("write temp db");
    path.to_string_lossy().to_string()
}

fn start_test_server() -> Option<(std::process::Child, u16, u16)> {
    let tcp_port = free_tcp_port()?;
    let udp_port = free_udp_port()?;
    let db_path = write_temp_db();

    let server_bin = workspace_bin("spserver");
    let child = Command::new(server_bin)
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
        .ok()?;

    thread::sleep(Duration::from_millis(400));
    Some((child, tcp_port, udp_port))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_pv_skips_bad_target_and_succeeds_on_next() {
    let (mut server, _tcp_port, udp_port) = match start_test_server() {
        Some(parts) => parts,
        None => {
            eprintln!("Skipping test: cannot spawn local spvirit_server");
            return;
        }
    };

    let bad_bind = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let targets = vec![
        SearchTarget {
            target: loopback,
            bind: bad_bind,
        },
        SearchTarget {
            target: loopback,
            bind: loopback,
        },
    ];

    let search_result = search_pv("SIM:AI", udp_port, Duration::from_secs(3), &targets, false)
        .await
        .expect("search should succeed after skipping bad target");

    assert_eq!(search_result.0.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));

    let _ = server.kill();
    let _ = server.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discover_servers_skips_bad_target_and_returns_server() {
    let (mut server, tcp_port, udp_port) = match start_test_server() {
        Some(parts) => parts,
        None => {
            eprintln!("Skipping test: cannot spawn local spvirit_server");
            return;
        }
    };

    let bad_bind = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let targets = vec![
        SearchTarget {
            target: loopback,
            bind: bad_bind,
        },
        SearchTarget {
            target: loopback,
            bind: loopback,
        },
    ];

    let discovered = discover_servers(udp_port, Duration::from_secs(3), &targets, false)
        .await
        .expect("discover should succeed after skipping bad target");

    assert!(
        discovered
            .iter()
            .any(|entry| entry.tcp_addr.ip() == loopback && entry.tcp_addr.port() == tcp_port),
        "expected to discover local server {}",
        tcp_port
    );

    let _ = server.kill();
    let _ = server.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_pv_all_targets_bad_returns_io() {
    let bad_bind = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));
    let bad_target = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 10));
    let targets = vec![
        SearchTarget {
            target: bad_target,
            bind: bad_bind,
        },
        SearchTarget {
            target: bad_target,
            bind: bad_bind,
        },
    ];

    let err = search_pv("SIM:AI", 5076, Duration::from_secs(1), &targets, false)
        .await
        .expect_err("expected IO error when all targets are unusable");

    assert!(
        matches!(err, PvGetError::Io(_)),
        "expected IO error, got {err}"
    );
}
