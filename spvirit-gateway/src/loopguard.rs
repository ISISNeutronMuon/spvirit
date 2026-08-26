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
}

impl LoopGuard {
    /// Build a `LoopGuard` for `server`, banning the socket (interface IP +
    /// `serverport`) of every server of this gateway instance (`cfg`), plus
    /// this server's forward-resolved `ignoreaddr` hosts/IPs (IP-wide).
    pub fn build(cfg: &GatewayConfig, server: &ServerCfg) -> Self {
        let mut banned_sockets: HashSet<SocketAddr> = HashSet::new();

        for s in &cfg.servers {
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

        let banned_ips: HashSet<IpAddr> = resolve_hosts(&server.ignoreaddr).into_iter().collect();

        LoopGuard {
            banned_sockets,
            banned_ips,
        }
    }

    /// Returns true if `addr` is banned (would create a self-connection loop):
    /// its host is in the `ignoreaddr` set (any port), or the exact
    /// `IP:port` socket is one of this gateway's own downstream servers.
    pub fn is_banned(&self, addr: SocketAddr) -> bool {
        self.banned_ips.contains(&addr.ip()) || self.banned_sockets.contains(&addr)
    }
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
        let guard = LoopGuard::build(&cfg, &cfg.servers[1]);

        // ignoreaddr host is banned at EVERY port (IP-wide).
        assert!(guard.is_banned(sock("172.18.0.1:5075")));
        assert!(guard.is_banned(sock("172.18.0.1:9999")));

        // The other own server's interface is banned only at its serverport.
        assert!(guard.is_banned(sock("10.0.90.203:5075"))); // own server socket
        assert!(!guard.is_banned(sock("10.0.90.203:9999"))); // same IP, different port → allowed

        // A real IOC sharing neither an ignoreaddr host nor an own socket.
        assert!(!guard.is_banned(sock("172.18.5.9:5075")));
    }
}
