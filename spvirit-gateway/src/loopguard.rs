//! Loop / self-connection prevention.
//!
//! A bidirectional gateway must never resolve a PV search back into one of
//! its own downstream servers. `LoopGuard` bans the gateway's own server
//! interface IPs plus any per-server `ignoreaddr` hosts (hostnames
//! forward-resolved to IPs, matching spec §7.1.1).

use crate::config::{GatewayConfig, ServerCfg};
use std::collections::HashSet;
use std::net::{IpAddr, ToSocketAddrs};

/// Set of IP addresses that must never be treated as valid resolution
/// targets for PV searches, to prevent self-connection loops.
pub struct LoopGuard {
    banned: HashSet<IpAddr>,
}

impl LoopGuard {
    /// Build a `LoopGuard` for `server`, banning every IP appearing as any
    /// `servers[].interface` of this gateway instance (`cfg`), plus this
    /// server's forward-resolved `ignoreaddr` hosts/IPs.
    pub fn build(cfg: &GatewayConfig, server: &ServerCfg) -> Self {
        let mut banned: HashSet<IpAddr> = HashSet::new();

        for s in &cfg.servers {
            for iface in &s.interface {
                if let Ok(ip) = iface.parse::<IpAddr>() {
                    banned.insert(ip);
                } else {
                    for ip in resolve_hosts(iface) {
                        banned.insert(ip);
                    }
                }
            }
        }

        for ip in resolve_hosts(&server.ignoreaddr) {
            banned.insert(ip);
        }

        LoopGuard { banned }
    }

    /// Returns true if `ip` is banned (would create a self-connection loop).
    pub fn is_banned(&self, ip: IpAddr) -> bool {
        self.banned.contains(&ip)
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

    #[test]
    fn bans_own_servers_and_ignoreaddr_ips() {
        let cfg = GatewayConfig::from_json_str(BIDI).unwrap();
        let guard = LoopGuard::build(&cfg, &cfg.servers[1]); // docker-server, ignoreaddr has 172.18.0.1
        assert!(guard.is_banned("172.18.0.1".parse().unwrap())); // own interface + ignoreaddr
        assert!(guard.is_banned("10.0.90.203".parse().unwrap())); // other own server interface
        assert!(!guard.is_banned("172.18.5.9".parse().unwrap())); // a real IOC
    }
}
