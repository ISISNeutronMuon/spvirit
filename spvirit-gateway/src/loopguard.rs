//! Loop / self-connection prevention.
//!
//! A bidirectional gateway must never resolve a PV search back into one of
//! its own downstream servers. `LoopGuard` bans the gateway's own server
//! *sockets* (each `servers[].interface` IP paired with that server's
//! `serverport`) plus any per-server `ignoreaddr` hosts (hostnames
//! forward-resolved to IPs, matching spec §7.1.1).
//!
//! The distinction matters: an own-server ban is socket-specific (a given IP
//! *and* the gateway's TCP `serverport`), so a legitimate upstream IOC that
//! happens to share the gateway's IP but listens on a different port — the
//! everyday case on loopback, and possible on shared hosts — still resolves.
//! An `ignoreaddr` ban is IP-wide (every port on that host), because
//! `ignoreaddr` is an operator's blanket "never talk to this host" list.

use crate::config::{GatewayConfig, ServerCfg};
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

/// Set of addresses that must never be treated as valid resolution targets
/// for PV searches, to prevent self-connection loops. Own-server addresses
/// are banned at socket granularity (IP + `serverport`); `ignoreaddr` hosts
/// are banned at every port.
pub struct LoopGuard {
    /// Own-server sockets: `(interface IP, serverport)` for every server of
    /// this gateway instance. Banned only at that exact port.
    banned_sockets: HashSet<SocketAddr>,
    /// `ignoreaddr` hosts for this server, banned at every port.
    banned_ips: HashSet<IpAddr>,
    /// GUIDs of every server this gateway instance runs, generated up front
    /// (see `runtime.rs::from_config`). A search response carrying one of
    /// these GUIDs is a self-reference regardless of which socket it claims
    /// to be from, so it closes the gap a socket-only ban leaves open when a
    /// server rebinds or is reachable via an address the socket ban does not
    /// cover.
    banned_guids: HashSet<[u8; 12]>,
}

impl LoopGuard {
    /// Build a `LoopGuard` for `server`, banning the socket (interface IP +
    /// `serverport`) of every server of this gateway instance (`cfg`), plus
    /// this server's forward-resolved `ignoreaddr` hosts/IPs (IP-wide), plus
    /// `banned_guids` (this gateway instance's up-front-generated per-server
    /// GUIDs; see `runtime.rs::from_config`).
    ///
    /// A server with an empty `interface` list binds `0.0.0.0` (all
    /// interfaces), so its own-server ban is expanded to every local
    /// interface IP on its `serverport` (see `local_interface_ips`) rather
    /// than being skipped, which would leave the wildcard bind unprotected.
    pub fn build(cfg: &GatewayConfig, server: &ServerCfg, banned_guids: HashSet<[u8; 12]>) -> Self {
        let mut banned_sockets: HashSet<SocketAddr> = HashSet::new();

        for s in &cfg.servers {
            if s.interface.is_empty() {
                // Binds 0.0.0.0 -> ban every local interface address on its port.
                for ip in local_interface_ips() {
                    banned_sockets.insert(SocketAddr::new(ip, s.serverport));
                }
            } else {
                for iface in &s.interface {
                    if let Ok(ip) = iface.parse::<IpAddr>() {
                        banned_sockets.insert(SocketAddr::new(ip, s.serverport));
                    } else {
                        for ip in resolve_hosts(iface) {
                            banned_sockets.insert(SocketAddr::new(ip, s.serverport));
                        }
                    }
                }
            }
        }

        let banned_ips: HashSet<IpAddr> = resolve_hosts(&server.ignoreaddr).into_iter().collect();

        LoopGuard {
            banned_sockets,
            banned_ips,
            banned_guids,
        }
    }

    /// Returns true if `addr` is banned (would create a self-connection loop):
    /// its host is in the `ignoreaddr` set (any port), or the exact
    /// `IP:port` socket is one of this gateway's own downstream servers.
    pub fn is_banned(&self, addr: SocketAddr) -> bool {
        self.banned_ips.contains(&addr.ip()) || self.banned_sockets.contains(&addr)
    }

    /// Returns true if `guid` matches one of this gateway instance's own
    /// server GUIDs (a self-reference). The all-zero sentinel (used by
    /// `pvinfo_full` for unicast resolutions with no search response) is
    /// never treated as a match, so it can never collide with an
    /// accidentally-zero server GUID.
    pub fn is_guid_banned(&self, guid: &[u8; 12]) -> bool {
        *guid != [0u8; 12] && self.banned_guids.contains(guid)
    }

    /// Test-only constructor for exercising `is_guid_banned` in isolation,
    /// without needing a full `GatewayConfig`.
    #[cfg(test)]
    pub fn with_banned_guids(banned_guids: HashSet<[u8; 12]>) -> Self {
        Self {
            banned_sockets: HashSet::new(),
            banned_ips: HashSet::new(),
            banned_guids,
        }
    }
}

/// Every local interface IP address, always including `127.0.0.1`/`::1`.
/// Used to expand a `0.0.0.0` (wildcard) server bind into the concrete set
/// of sockets it actually listens on, for the loop-guard's own-server ban.
///
/// Enumeration failure is non-fatal: fails closed to loopback-only (still
/// banning the safest, most common self-connection case) plus a logged
/// warning, rather than panicking.
fn local_interface_ips() -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = vec![
        IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    ];
    match get_if_addrs::get_if_addrs() {
        Ok(ifaces) => {
            for iface in ifaces {
                let ip = iface.ip();
                if !ips.contains(&ip) {
                    ips.push(ip);
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                "loopguard: failed to enumerate local interfaces: {e}; \
                 falling back to loopback-only for the 0.0.0.0 backstop"
            );
        }
    }
    ips
}

/// Whitespace-split `list`; each token is parsed as an IP literal, else
/// forward-resolved via DNS. Resolution failures are non-fatal: they are
/// skipped and logged with `tracing::warn!`.
fn resolve_hosts(list: &str) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for token in list.split_whitespace() {
        if let Ok(ip) = token.parse::<IpAddr>() {
            out.push(ip);
            continue;
        }
        match (token, 0u16).to_socket_addrs() {
            Ok(addrs) => {
                for addr in addrs {
                    out.push(addr.ip());
                }
            }
            Err(e) => {
                tracing::warn!("loopguard: failed to resolve host '{token}': {e}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIDI: &str = include_str!("../tests/fixtures/p4p_bidirectional.json");

    fn sock(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn bans_own_server_sockets_and_ignoreaddr_ips() {
        let cfg = GatewayConfig::from_json_str(BIDI).unwrap();
        // servers[1] = docker-server-network; ignoreaddr lists 172.18.0.1.
        // Both servers default serverport to 5075.
        let guard = LoopGuard::build(&cfg, &cfg.servers[1], HashSet::new());

        // ignoreaddr host is banned at EVERY port (IP-wide).
        assert!(guard.is_banned(sock("172.18.0.1:5075")));
        assert!(guard.is_banned(sock("172.18.0.1:9999")));

        // The other own server's interface is banned only at its serverport.
        assert!(guard.is_banned(sock("10.0.90.203:5075"))); // own server socket
        assert!(!guard.is_banned(sock("10.0.90.203:9999"))); // same IP, different port → allowed

        // A real IOC sharing neither an ignoreaddr host nor an own socket.
        assert!(!guard.is_banned(sock("172.18.5.9:5075")));
    }

    #[test]
    fn guid_ban_matches_self_guid() {
        let mut guids = HashSet::new();
        guids.insert([1u8; 12]);
        let guard = LoopGuard::with_banned_guids(guids); // test constructor
        assert!(guard.is_guid_banned(&[1u8; 12]));
        assert!(!guard.is_guid_banned(&[2u8; 12]));
    }

    #[test]
    fn zero_guid_sentinel_is_never_banned() {
        let mut guids = HashSet::new();
        guids.insert([0u8; 12]);
        let guard = LoopGuard::with_banned_guids(guids);
        assert!(!guard.is_guid_banned(&[0u8; 12]));
    }

    #[test]
    fn empty_interface_bans_local_ips_on_server_port() {
        // A server with empty `interface` (binds 0.0.0.0) must still ban
        // (each local interface IP : serverport).
        let cfg = GatewayConfig::from_json_str(
            r#"{
                "version": 2,
                "clients": [{"name": "c"}],
                "servers": [{"name": "s", "clients": ["c"], "serverport": 5099}]
            }"#,
        )
        .unwrap();
        let guard = LoopGuard::build(&cfg, &cfg.servers[0], HashSet::new());
        for ip in local_interface_ips() {
            // same helper build() uses
            assert!(guard.is_banned(SocketAddr::new(ip, 5099)));
        }
    }
}
