//! Top-level PVA server orchestration.
//!
//! [`run_pva_server`] binds UDP + TCP + beacon and runs until cancelled.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use tracing::{error, info};

use crate::beacon::{BeaconConfig, run_beacon};
use tokio::net::TcpListener;

use crate::handler::{PvListMode, ServerState, rand_guid, run_tcp_server, run_udp_search};
use crate::monitor::MonitorRegistry;
use crate::pvstore::SourceRegistry;

/// Configuration for the PVA server.
pub struct PvaServerConfig {
    /// IP address to listen on (default: 0.0.0.0).
    pub listen_ip: IpAddr,
    /// TCP port (default: 5075).
    pub tcp_port: u16,
    /// UDP port (default: 5076).
    pub udp_port: u16,
    /// Address to advertise in search responses (None = auto).
    pub advertise_ip: Option<IpAddr>,
    /// Beacon target address (default: 224.0.0.128:5076).
    pub beacon_target: SocketAddr,
    /// Beacon period in seconds.
    pub beacon_period_secs: u64,
    /// Idle connection timeout.
    pub conn_timeout: Duration,
    /// Whether to compute alarms from limits.
    pub compute_alarms: bool,
    /// PV list mode.
    pub pvlist_mode: PvListMode,
    /// Maximum PV names in pvlist responses.
    pub pvlist_max: usize,
    /// Optional regex filter for pvlist.
    pub pvlist_allow_pattern: Option<Regex>,
    /// Override for the server's GUID (default: `None`, mints a random one).
    pub guid: Option<[u8; 12]>,
    /// When true, bind the UDP search socket to `0.0.0.0` and join the PVA
    /// multicast group (224.0.0.128) so broadcast/multicast searches are heard
    /// (p4p parity). When false, bind only `listen_ip` and do not join
    /// multicast (today's behavior).
    pub discovery_parity: bool,
}

impl Default for PvaServerConfig {
    fn default() -> Self {
        Self {
            listen_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            tcp_port: 5075,
            udp_port: 5076,
            advertise_ip: None,
            beacon_target: "224.0.0.128:5076".parse().unwrap(),
            beacon_period_secs: 15,
            conn_timeout: Duration::from_secs(64000),
            compute_alarms: false,
            pvlist_mode: PvListMode::List,
            pvlist_max: 1024,
            pvlist_allow_pattern: None,
            guid: None,
            discovery_parity: true,
        }
    }
}

/// Decide how the UDP search-receive socket should bind, given the parity mode
/// and the configured listen IP.
///
/// Returns the socket address to bind and, when parity is on, the IPv4
/// interface to join the PVA multicast group on (`None` disables the join).
/// Under parity the socket binds `0.0.0.0` (all interfaces) so broadcast and
/// multicast searches are heard even when an interface is pinned; the join
/// interface is the pinned IPv4 address (or `0.0.0.0` when unset / IPv6). When
/// parity is off, the socket binds `listen_ip` and no multicast join occurs.
pub fn udp_search_bind(
    discovery_parity: bool,
    listen_ip: IpAddr,
    udp_port: u16,
) -> (SocketAddr, Option<Ipv4Addr>) {
    if discovery_parity {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), udp_port);
        let iface = match listen_ip {
            IpAddr::V4(v4) => v4, // pinned interface, or 0.0.0.0 if unset
            IpAddr::V6(_) => Ipv4Addr::UNSPECIFIED,
        };
        (bind, Some(iface))
    } else {
        (SocketAddr::new(listen_ip, udp_port), None)
    }
}

/// Shared server state wrapping a [`SourceRegistry`].
///
/// Consumers can hold an `Arc<PvaServerState>` to inspect or mutate the
/// underlying sources while the server tasks are running.
pub struct PvaServerState {
    pub inner: Arc<ServerState>,
    pub registry: Arc<MonitorRegistry>,
}

impl PvaServerState {
    pub fn new(sources: Arc<SourceRegistry>, config: &PvaServerConfig) -> Self {
        Self::with_registry(sources, config, Arc::new(MonitorRegistry::new()))
    }

    pub fn with_registry(
        sources: Arc<SourceRegistry>,
        config: &PvaServerConfig,
        registry: Arc<MonitorRegistry>,
    ) -> Self {
        let guid = config.guid.unwrap_or_else(rand_guid);
        let inner = Arc::new(ServerState::new(
            sources,
            registry.clone(),
            config.compute_alarms,
            config.pvlist_mode,
            config.pvlist_max,
            config.pvlist_allow_pattern.clone(),
            guid,
            config.tcp_port,
            config.advertise_ip,
            config.listen_ip,
        ));
        Self { inner, registry }
    }
}

/// Run a PVA server (UDP search + TCP handler + beacon).
///
/// This function drives the three server tasks in a `tokio::select!` loop and
/// returns when any task errors or the future is dropped.
pub async fn run_pva_server(
    sources: Arc<SourceRegistry>,
    config: PvaServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let registry = Arc::new(MonitorRegistry::new());
    run_pva_server_with_registry(sources, config, registry).await
}

/// Like [`run_pva_server`] but re-uses an existing [`MonitorRegistry`].
pub async fn run_pva_server_with_registry(
    sources: Arc<SourceRegistry>,
    config: PvaServerConfig,
    registry: Arc<MonitorRegistry>,
) -> Result<(), Box<dyn std::error::Error>> {
    let server_state = PvaServerState::with_registry(sources, &config, registry);
    let state = server_state.inner;

    let guid = state.guid;
    let tcp_addr = SocketAddr::new(config.listen_ip, config.tcp_port);
    // TCP always binds the configured interface; the UDP search socket binds
    // by parity mode (0.0.0.0 + multicast join when on).
    let (udp_addr, multicast_iface) =
        udp_search_bind(config.discovery_parity, config.listen_ip, config.udp_port);

    info!(
        "Starting PVA server: udp={} (discovery_parity={}) tcp={} pvlist_mode={:?} pvlist_max={} filter={}",
        udp_addr,
        config.discovery_parity,
        tcp_addr,
        config.pvlist_mode,
        config.pvlist_max,
        config
            .pvlist_allow_pattern
            .as_ref()
            .map(|r| r.as_str())
            .unwrap_or("<none>")
    );

    // Bind TCP before spawning anything.  If the port is already in use we
    // return an error here and the beacon is never started, preventing a
    // ghost-beacon that advertises a port this process does not serve.
    let tcp_listener = TcpListener::bind(tcp_addr).await?;
    info!("TCP listener bound on {}", tcp_addr);

    let beacon_config = BeaconConfig {
        target: config.beacon_target,
        guid,
        tcp_port: config.tcp_port,
        advertise_ip: config.advertise_ip,
        listen_ip: config.listen_ip,
        period_secs: config.beacon_period_secs,
    };

    let udp_state = state.clone();
    let udp_task = tokio::spawn(async move {
        if let Err(e) = run_udp_search(
            udp_state,
            udp_addr,
            config.tcp_port,
            guid,
            config.advertise_ip,
            multicast_iface,
        )
        .await
        {
            error!("UDP search server error: {}", e);
        }
    });

    let tcp_state = state.clone();
    let tcp_task = tokio::spawn(async move {
        if let Err(e) = run_tcp_server(tcp_state, tcp_listener, config.conn_timeout).await {
            error!("TCP server error: {}", e);
        }
    });

    let beacon_change = state.beacon_change.clone();
    let beacon_task = tokio::spawn(async move {
        if let Err(e) = run_beacon(beacon_config, beacon_change).await {
            error!("Beacon task error: {}", e);
        }
    });

    let _ = tokio::join!(udp_task, tcp_task, beacon_task);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_discovery_parity_on() {
        assert!(PvaServerConfig::default().discovery_parity);
    }

    #[test]
    fn udp_search_bind_parity_on_pinned_interface() {
        let listen = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
        let (addr, iface) = udp_search_bind(true, listen, 5076);
        assert_eq!(addr, SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 5076));
        assert_eq!(iface, Some(Ipv4Addr::new(10, 0, 0, 5)));
    }

    #[test]
    fn udp_search_bind_parity_on_unset_interface() {
        let listen = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let (addr, iface) = udp_search_bind(true, listen, 5076);
        assert_eq!(addr, SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 5076));
        assert_eq!(iface, Some(Ipv4Addr::UNSPECIFIED));
    }

    #[test]
    fn udp_search_bind_parity_off_binds_listen_ip() {
        let listen = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
        let (addr, iface) = udp_search_bind(false, listen, 5076);
        assert_eq!(addr, SocketAddr::new(listen, 5076));
        assert_eq!(iface, None);
    }
}
