mod protocol;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use protocol::frame_harness::TestServer;
use spvirit_client::pvlist::pvlist_with_fallback;
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_tools::spvirit_client::explore::{fetch_snapshot_from_server, monitor_pv_from_server};
use spvirit_tools::spvirit_client::search::{SearchTarget, discover_servers};
use spvirit_tools::spvirit_client::types::{PvGetOptions, PvGetResult};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discover_servers_finds_test_server() {
    let server = match TestServer::spawn_with_args(&["--pvlist-mode", "discover"]) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("Skipping test, cannot spawn server: {}", msg);
            return;
        }
    };

    let targets = vec![SearchTarget {
        target: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        bind: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
    }];
    let discovered = discover_servers(server.udp_port, Duration::from_secs(2), &targets, false)
        .await
        .expect("discover servers");

    assert!(
        discovered
            .iter()
            .any(|s| s.tcp_addr.port() == server.tcp_port),
        "expected discovered list to include test server tcp port {}",
        server.tcp_port
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_pvs_with_fallback_returns_known_pvs() {
    let server = match TestServer::spawn_with_args(&["--pvlist-mode", "list"]) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("Skipping test, cannot spawn server: {}", msg);
            return;
        }
    };

    let mut opts = PvGetOptions::new(String::new());
    opts.timeout = Duration::from_secs(2);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), server.tcp_port);
    let (names, _source) = pvlist_with_fallback(&opts, addr).await.expect("list pvs");

    assert!(names.contains(&"SIM:AI".to_string()));
    assert!(names.contains(&"SIM:AO".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_snapshot_from_server_returns_value_and_structure() {
    let server = match TestServer::spawn() {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("Skipping test, cannot spawn server: {}", msg);
            return;
        }
    };

    let mut opts = PvGetOptions::new(String::new());
    opts.timeout = Duration::from_secs(2);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), server.tcp_port);
    let snapshot = fetch_snapshot_from_server(&opts, addr, "SIM:AI")
        .await
        .expect("snapshot");

    assert_eq!(snapshot.pv_name, "SIM:AI");
    assert!(!snapshot.introspection.fields.is_empty());
    assert!(
        snapshot
            .introspection
            .fields
            .iter()
            .any(|f| f.name == "value")
    );
    assert!(
        matches!(snapshot.value, DecodedValue::Structure(_)),
        "expected structured NT payload"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_pv_from_server_delivers_a_decoded_first_update() {
    let server = match TestServer::spawn() {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("Skipping test, cannot spawn server: {}", msg);
            return;
        }
    };
    let addr: SocketAddr = format!("127.0.0.1:{}", server.tcp_port)
        .parse()
        .expect("addr");

    let mut opts = PvGetOptions::new("SIM:AI".to_string());
    opts.server_addr = Some(addr);
    opts.timeout = Duration::from_secs(5);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<PvGetResult>(16);
    let handle = tokio::spawn(async move {
        let _ = monitor_pv_from_server(
            &opts,
            addr,
            "SIM:AI",
            |update| {
                let _ = tx.try_send(update);
            },
            |_| {},
        )
        .await;
    });

    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("an event within 5s")
        .expect("a monitor event");
    assert_eq!(first.pv_name, "SIM:AI");
    assert!(
        matches!(first.value, DecodedValue::Structure(_)),
        "the first monitor event must carry a decoded structure, got {:?}",
        first.value
    );
    handle.abort();
}
