//! Deliverable 3 (bidirectional end-to-end) and Deliverable 4 (the
//! `autoaddrlist`/`no_broadcast` regression) integration tests for Task 14.
//!
//! ## Bidirectional topology
//!
//! Two independent "upstream" nets, each an in-process [`PvaServer`] on
//! loopback with its own PV:
//!
//! - netA: hosts `A:PV` (11.0)
//! - netB: hosts `B:PV` (22.0)
//!
//! Two gateway upstream clients, one per net:
//!
//! - `clientA` -> netA's UDP search port
//! - `clientB` -> netB's UDP search port
//!
//! Two gateway-facing [`PvaServer`]s built by [`Runtime::from_config`], each
//! *cross-wired* to the other net's client so that asking one gateway server
//! for the other net's PV proves the bridge actually crosses networks:
//!
//! - `serverA` (its own TCP/UDP ports) has `clients: ["clientB"]` — a
//!   downstream client connecting to serverA and asking for `B:PV` is
//!   answered via clientB, i.e. netA's gateway front-end resolves netB.
//! - `serverB` has `clients: ["clientA"]` — symmetric: netB's gateway
//!   front-end resolves netA's `A:PV`.
//!
//! A downstream [`PvaClient`] then connects to serverA and fetches `B:PV`
//! (expect ~22.0), and to serverB and fetches `A:PV` (expect ~11.0). Getting
//! the *other* net's PV through each gateway server is what makes this
//! bidirectional rather than a single pass-through hop.

use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use spvirit_client::PvaClient;
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_gateway::access::AccessControl;
use spvirit_gateway::config::GatewayConfig;
use spvirit_gateway::loopguard::LoopGuard;
use spvirit_gateway::proxy::GatewaySource;
use spvirit_gateway::runtime::Runtime;
use spvirit_gateway::upstream::UpstreamPool;
use spvirit_server::PvaServer;
use spvirit_server::pvstore::Source;

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

/// Pulls the scalar `F64` "value" field out of an NTScalar `DecodedValue`
/// (as reported by `PvaClient::pvget` for an `ai`/`ao` upstream record).
fn extract_f64(v: &DecodedValue) -> f64 {
    match v {
        DecodedValue::Float64(x) => *x,
        DecodedValue::Structure(fields) => {
            for (name, fv) in fields {
                if name == "value"
                    && let DecodedValue::Float64(x) = fv
                {
                    return *x;
                }
            }
            panic!("no scalar F64 \"value\" field in structure {fields:?}");
        }
        other => panic!("expected Float64 or NTScalar Structure, got {other:?}"),
    }
}

/// Grabs 4 free loopback ports up front (2x TCP, 2x UDP) so the whole
/// topology's config JSON can be built in one shot. Returns `None` (test
/// should skip) if the environment can't bind that many free ports.
struct Ports {
    net_a_tcp: u16,
    net_a_udp: u16,
    net_b_tcp: u16,
    net_b_udp: u16,
    gw_a_tcp: u16,
    gw_a_udp: u16,
    gw_b_tcp: u16,
    gw_b_udp: u16,
}

fn allocate_ports() -> Option<Ports> {
    Some(Ports {
        net_a_tcp: free_tcp_port()?,
        net_a_udp: free_udp_port()?,
        net_b_tcp: free_tcp_port()?,
        net_b_udp: free_udp_port()?,
        gw_a_tcp: free_tcp_port()?,
        gw_a_udp: free_udp_port()?,
        gw_b_tcp: free_tcp_port()?,
        gw_b_udp: free_udp_port()?,
    })
}

fn loopback_client(udp_port: u16) -> PvaClient {
    PvaClient::builder()
        .udp_port(udp_port)
        .search_addr("127.0.0.1".parse().unwrap())
        .bind_addr("127.0.0.1".parse().unwrap())
        .build()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bidirectional_gateway_bridges_both_nets() {
    let Some(ports) = allocate_ports() else {
        eprintln!("Skipping test: cannot bind free ports in this environment");
        return;
    };

    // Upstream net A: hosts A:PV.
    let net_a = PvaServer::builder()
        .ai("A:PV", 11.0)
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(ports.net_a_tcp)
        .udp_port(ports.net_a_udp)
        .build();
    tokio::spawn(async move {
        let _ = net_a.run().await;
    });

    // Upstream net B: hosts B:PV.
    let net_b = PvaServer::builder()
        .ai("B:PV", 22.0)
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(ports.net_b_tcp)
        .udp_port(ports.net_b_udp)
        .build();
    tokio::spawn(async move {
        let _ = net_b.run().await;
    });

    tokio::time::sleep(Duration::from_millis(600)).await;

    let cfg_json = format!(
        r#"{{
            "version": 2,
            "clients": [
                {{
                    "name": "clientA",
                    "addrlist": "127.0.0.1",
                    "bcastport": {net_a_udp},
                    "interface": ["127.0.0.1"]
                }},
                {{
                    "name": "clientB",
                    "addrlist": "127.0.0.1",
                    "bcastport": {net_b_udp},
                    "interface": ["127.0.0.1"]
                }}
            ],
            "servers": [
                {{
                    "name": "serverA",
                    "clients": ["clientB"],
                    "interface": ["127.0.0.1"],
                    "serverport": {gw_a_tcp},
                    "bcastport": {gw_a_udp}
                }},
                {{
                    "name": "serverB",
                    "clients": ["clientA"],
                    "interface": ["127.0.0.1"],
                    "serverport": {gw_b_tcp},
                    "bcastport": {gw_b_udp}
                }}
            ]
        }}"#,
        net_a_udp = ports.net_a_udp,
        net_b_udp = ports.net_b_udp,
        gw_a_tcp = ports.gw_a_tcp,
        gw_a_udp = ports.gw_a_udp,
        gw_b_tcp = ports.gw_b_tcp,
        gw_b_udp = ports.gw_b_udp,
    );
    let cfg = GatewayConfig::from_json_str(&cfg_json).expect("parse gateway config");

    // LoopGuard assertion: the guard built for serverA must ban serverA's
    // own downstream server SOCKET (127.0.0.1:gw_a_tcp) — the gateway must
    // never treat its own listening socket as a valid upstream resolution
    // target, or it could loop a search back into itself. Crucially, it must
    // NOT ban the legitimate upstream on the same loopback IP but a different
    // port (127.0.0.1:net_a_tcp), which is exactly why the ban is
    // socket-granular rather than IP-wide.
    let guard_a = LoopGuard::build(&cfg, &cfg.servers[0], std::collections::HashSet::new());
    assert!(
        guard_a.is_banned(SocketAddr::new(
            "127.0.0.1".parse::<IpAddr>().unwrap(),
            ports.gw_a_tcp
        )),
        "LoopGuard must ban the gateway's own server socket"
    );
    assert!(
        !guard_a.is_banned(SocketAddr::new(
            "127.0.0.1".parse::<IpAddr>().unwrap(),
            ports.net_a_tcp
        )),
        "LoopGuard must NOT ban a legitimate upstream sharing the loopback IP on a different port"
    );

    let runtime = Runtime::from_config(cfg).expect("valid config builds a Runtime");
    tokio::spawn(async move {
        let _ = runtime.run().await;
    });
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Connect to gateway serverA (netA-facing) and ask for B:PV — this only
    // resolves if serverA's GatewaySource bridges through clientB to netB.
    let client_to_a = loopback_client(ports.gw_a_udp);
    let result = tokio::time::timeout(Duration::from_secs(5), client_to_a.pvget("B:PV"))
        .await
        .expect("pvget to serverA should not time out")
        .expect("pvget B:PV via serverA should succeed");
    let value = extract_f64(&result.value);
    assert!(
        (value - 22.0).abs() < 1e-6,
        "expected serverA to resolve B:PV (~22.0) via clientB, got {value}"
    );

    // Connect to gateway serverB (netB-facing) and ask for A:PV — symmetric
    // proof that the bridge works in the other direction too.
    let client_to_b = loopback_client(ports.gw_b_udp);
    let result = tokio::time::timeout(Duration::from_secs(5), client_to_b.pvget("A:PV"))
        .await
        .expect("pvget to serverB should not time out")
        .expect("pvget A:PV via serverB should succeed");
    let value = extract_f64(&result.value);
    assert!(
        (value - 11.0).abs() < 1e-6,
        "expected serverB to resolve A:PV (~11.0) via clientA, got {value}"
    );
}

/// Deliverable 4 regression test: the carried-forward `autoaddrlist`/
/// `no_broadcast` defect. `spvirit-client`'s `no_broadcast()` disables the
/// *entire* UDP search branch (including the unicast `search_addr`), so the
/// pre-fix `build_client` — which called `no_broadcast()` whenever
/// `autoaddrlist == false`, regardless of whether an explicit `addrlist`
/// unicast target was set — silently resolved nothing for a standard p4p
/// `{autoaddrlist:false, addrlist:"<ip>"}` config. This asserts that such a
/// config now resolves via the unicast `search_addr` alone.
///
/// Before the fix in `upstream::build_client` this test failed (`claim`
/// returned `None`, because `no_broadcast()` was applied even though
/// `addrlist` carried an explicit unicast target). After the fix (only
/// disabling UDP search when there is neither `autoaddrlist` nor an
/// explicit `addrlist` target), it passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn autoaddrlist_false_with_explicit_addrlist_still_resolves() {
    let Some(tcp_port) = free_tcp_port() else {
        eprintln!("Skipping test: cannot bind a free TCP port in this environment");
        return;
    };
    let Some(udp_port) = free_udp_port() else {
        eprintln!("Skipping test: cannot bind a free UDP port in this environment");
        return;
    };

    let upstream = PvaServer::builder()
        .ai("IT:UNI", 7.0)
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(tcp_port)
        .udp_port(udp_port)
        .build();
    tokio::spawn(async move {
        let _ = upstream.run().await;
    });
    tokio::time::sleep(Duration::from_millis(600)).await;

    // A standard p4p unicast client config: autoaddrlist explicitly
    // disabled, with an explicit addrlist unicast target instead.
    let cfg_json = format!(
        r#"{{
            "version": 2,
            "clients": [{{
                "name": "uni-client",
                "addrlist": "127.0.0.1",
                "autoaddrlist": false,
                "bcastport": {udp_port},
                "interface": ["127.0.0.1"]
            }}],
            "servers": [{{
                "name": "uni-server",
                "clients": ["uni-client"]
            }}]
        }}"#
    );
    let cfg = GatewayConfig::from_json_str(&cfg_json).expect("parse gateway config");
    assert!(!cfg.clients[0].autoaddrlist, "sanity: autoaddrlist is false");

    let pool = Arc::new(UpstreamPool::from_config(&cfg));
    let neg = Arc::new(spvirit_gateway::cache::negative::NegativeCache::new(
        Duration::from_secs(30),
        128,
    ));
    let guard = Arc::new(LoopGuard::build(
        &cfg,
        &cfg.servers[0],
        std::collections::HashSet::new(),
    ));
    let access = Arc::new(AccessControl::new(false, None, None));
    let src = GatewaySource::new(pool, vec!["uni-client".into()], neg, guard, 0, access);

    let claimed = tokio::time::timeout(Duration::from_secs(5), src.claim("IT:UNI"))
        .await
        .expect("claim should not time out");
    assert!(
        claimed.is_some(),
        "an autoaddrlist:false config with an explicit addrlist unicast target must still resolve"
    );
}

/// Loop / self-connection prevention, exercised end-to-end through
/// `GatewaySource::claim` (not just against a standalone `LoopGuard`).
///
/// A real upstream is spun up on `127.0.0.1:<tcp_port>`, and the gateway's own
/// server is configured with `interface:["127.0.0.1"]` and `serverport` set to
/// that same `<tcp_port>`. `LoopGuard::build` therefore bans exactly the socket
/// the upstream resolves to — simulating the gateway resolving a search back
/// into its own listening socket. `claim` must consult the guard, see the
/// resolved address is banned, and refuse to bind (return `None`) even though
/// `pvinfo_full` itself succeeds. A control assertion first proves the same
/// upstream resolves fine when the guard does NOT ban its socket, so the
/// `None` is attributable to the guard and not to a resolution failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_refuses_resolution_into_a_banned_own_server_socket() {
    let Some(tcp_port) = free_tcp_port() else {
        eprintln!("Skipping test: cannot bind a free TCP port in this environment");
        return;
    };
    let Some(udp_port) = free_udp_port() else {
        eprintln!("Skipping test: cannot bind a free UDP port in this environment");
        return;
    };

    let upstream = PvaServer::builder()
        .ai("IT:LOOP", 3.0)
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(tcp_port)
        .udp_port(udp_port)
        .build();
    tokio::spawn(async move {
        let _ = upstream.run().await;
    });
    tokio::time::sleep(Duration::from_millis(600)).await;

    let client_json = format!(
        r#"{{
            "name": "loop-client",
            "addrlist": "127.0.0.1",
            "bcastport": {udp_port},
            "interface": ["127.0.0.1"]
        }}"#
    );

    // Control: gateway server on a DIFFERENT serverport than the upstream, so
    // the guard does not ban the upstream socket — resolution must succeed.
    let Some(other_port) = free_tcp_port() else {
        eprintln!("Skipping test: cannot bind a free TCP port in this environment");
        return;
    };
    let control_cfg = GatewayConfig::from_json_str(&format!(
        r#"{{
            "version": 2,
            "clients": [{client_json}],
            "servers": [{{
                "name": "loop-server",
                "clients": ["loop-client"],
                "interface": ["127.0.0.1"],
                "serverport": {other_port}
            }}]
        }}"#
    ))
    .expect("parse control config");
    let control_guard = Arc::new(LoopGuard::build(
        &control_cfg,
        &control_cfg.servers[0],
        std::collections::HashSet::new(),
    ));
    let control_src = GatewaySource::new(
        Arc::new(UpstreamPool::from_config(&control_cfg)),
        vec!["loop-client".into()],
        Arc::new(spvirit_gateway::cache::negative::NegativeCache::new(
            Duration::from_secs(30),
            128,
        )),
        control_guard,
        0,
        Arc::new(AccessControl::new(false, None, None)),
    );
    let control_claim = tokio::time::timeout(Duration::from_secs(5), control_src.claim("IT:LOOP"))
        .await
        .expect("control claim should not time out");
    assert!(
        control_claim.is_some(),
        "control: the upstream must resolve when its socket is not banned"
    );

    // Now: gateway server whose serverport EQUALS the upstream's port, so the
    // guard bans 127.0.0.1:<tcp_port> — the exact socket the upstream resolves
    // to. claim must refuse.
    let loop_cfg = GatewayConfig::from_json_str(&format!(
        r#"{{
            "version": 2,
            "clients": [{client_json}],
            "servers": [{{
                "name": "loop-server",
                "clients": ["loop-client"],
                "interface": ["127.0.0.1"],
                "serverport": {tcp_port}
            }}]
        }}"#
    ))
    .expect("parse loop config");
    let loop_guard = LoopGuard::build(
        &loop_cfg,
        &loop_cfg.servers[0],
        std::collections::HashSet::new(),
    );
    assert!(
        loop_guard.is_banned(SocketAddr::new("127.0.0.1".parse().unwrap(), tcp_port)),
        "sanity: guard must ban the own-server socket the upstream resolves to"
    );
    let loop_src = GatewaySource::new(
        Arc::new(UpstreamPool::from_config(&loop_cfg)),
        vec!["loop-client".into()],
        Arc::new(spvirit_gateway::cache::negative::NegativeCache::new(
            Duration::from_secs(30),
            128,
        )),
        Arc::new(loop_guard),
        0,
        Arc::new(AccessControl::new(false, None, None)),
    );
    let loop_claim = tokio::time::timeout(Duration::from_secs(5), loop_src.claim("IT:LOOP"))
        .await
        .expect("loop claim should not time out");
    assert!(
        loop_claim.is_none(),
        "claim must refuse to bind a name that resolves into a banned own-server socket"
    );
}

/// GUID-based loop / self-connection prevention: an upstream whose search
/// response carries the gateway's OWN server GUID must be refused by
/// `claim`, even when the resolved socket itself is not banned (different
/// port than the gateway's own server) — proving the GUID ban-set, not the
/// socket ban, is what caught it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_refuses_resolution_that_carries_our_own_server_guid() {
    let Some(tcp_port) = free_tcp_port() else {
        eprintln!("Skipping test: cannot bind a free TCP port in this environment");
        return;
    };
    let Some(udp_port) = free_udp_port() else {
        eprintln!("Skipping test: cannot bind a free UDP port in this environment");
        return;
    };
    // Deliberately a DIFFERENT port than the upstream's, so the socket ban
    // (LoopGuard::is_banned) cannot be what refuses this claim.
    let Some(gateway_serverport) = free_tcp_port() else {
        eprintln!("Skipping test: cannot bind a free TCP port in this environment");
        return;
    };

    let own_guid = [0x42u8; 12];

    // The "upstream" is standing in for a search response that (somehow --
    // e.g. via an intermediate relay, or a misconfigured mesh) carries this
    // gateway's own server GUID, without being bound to this gateway's own
    // socket.
    let upstream = PvaServer::builder()
        .ai("IT:LOOPGUID", 7.0)
        .listen_ip("127.0.0.1".parse().unwrap())
        .advertise_ip("127.0.0.1".parse().unwrap())
        .port(tcp_port)
        .udp_port(udp_port)
        .guid(own_guid)
        .build();
    tokio::spawn(async move {
        let _ = upstream.run().await;
    });
    tokio::time::sleep(Duration::from_millis(600)).await;

    let client_json = format!(
        r#"{{
            "name": "loop-guid-client",
            "addrlist": "127.0.0.1",
            "bcastport": {udp_port},
            "interface": ["127.0.0.1"]
        }}"#
    );
    let cfg = GatewayConfig::from_json_str(&format!(
        r#"{{
            "version": 2,
            "clients": [{client_json}],
            "servers": [{{
                "name": "loop-guid-server",
                "clients": ["loop-guid-client"],
                "interface": ["127.0.0.1"],
                "serverport": {gateway_serverport}
            }}]
        }}"#
    ))
    .expect("parse loop-guid config");

    // Sanity control: with no GUIDs banned, the resolution succeeds (proves
    // a later `None` is attributable to the GUID ban, not to a resolution
    // failure or the socket ban).
    let control_guard = Arc::new(LoopGuard::build(
        &cfg,
        &cfg.servers[0],
        std::collections::HashSet::new(),
    ));
    let control_src = GatewaySource::new(
        Arc::new(UpstreamPool::from_config(&cfg)),
        vec!["loop-guid-client".into()],
        Arc::new(spvirit_gateway::cache::negative::NegativeCache::new(
            Duration::from_secs(30),
            128,
        )),
        control_guard,
        0,
        Arc::new(AccessControl::new(false, None, None)),
    );
    let control_claim = tokio::time::timeout(
        Duration::from_secs(5),
        control_src.claim("IT:LOOPGUID"),
    )
    .await
    .expect("control claim should not time out");
    assert!(
        control_claim.is_some(),
        "control: the upstream must resolve when its GUID is not banned"
    );

    // Now: the gateway's own server GUID set includes the upstream's GUID
    // (as it would if the upstream's server WAS one of this gateway's own
    // servers, resolved via a search-response loop). `claim` must refuse.
    let mut banned_guids = std::collections::HashSet::new();
    banned_guids.insert(own_guid);
    let guard = Arc::new(LoopGuard::build(&cfg, &cfg.servers[0], banned_guids));
    assert!(
        guard.is_guid_banned(&own_guid),
        "sanity: guard must ban the own-server GUID the upstream resolves to"
    );
    let src = GatewaySource::new(
        Arc::new(UpstreamPool::from_config(&cfg)),
        vec!["loop-guid-client".into()],
        Arc::new(spvirit_gateway::cache::negative::NegativeCache::new(
            Duration::from_secs(30),
            128,
        )),
        guard,
        0,
        Arc::new(AccessControl::new(false, None, None)),
    );
    let claim = tokio::time::timeout(Duration::from_secs(5), src.claim("IT:LOOPGUID"))
        .await
        .expect("claim should not time out");
    assert!(
        claim.is_none(),
        "claim must refuse to bind a name whose search response carries a banned own-server GUID"
    );
}
