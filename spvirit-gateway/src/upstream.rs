//! Upstream client pool — one [`spvirit_client::PvaClient`] per `clients[]`
//! entry in the gateway configuration, honouring `interface` binding (a
//! spvirit feature p4p itself ignores).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use spvirit_client::PvaClient;

use crate::config::{ClientCfg, GatewayConfig};

/// A pool of upstream [`PvaClient`]s, keyed by the `clients[].name` from the
/// gateway configuration.
#[derive(Debug, Clone, Default)]
pub struct UpstreamPool {
    clients: HashMap<String, Arc<PvaClient>>,
}

impl UpstreamPool {
    /// Build a pool with one client per `cfg.clients[]` entry.
    pub fn from_config(cfg: &GatewayConfig) -> Self {
        let mut clients = HashMap::with_capacity(cfg.clients.len());
        for c in &cfg.clients {
            clients.insert(c.name.clone(), Arc::new(build_client(c)));
        }
        Self { clients }
    }

    /// Look up the upstream client for a given `clients[].name`.
    pub fn client(&self, name: &str) -> Option<Arc<PvaClient>> {
        self.clients.get(name).cloned()
    }

    /// Names of all upstream clients in the pool (unsorted).
    pub fn names(&self) -> Vec<String> {
        self.clients.keys().cloned().collect()
    }
}

/// Map a p4p-compatible [`ClientCfg`] onto a [`PvaClient`] via its builder.
///
/// - `bcastport` -> UDP search port.
/// - `autoaddrlist == false` -> disable UDP broadcast search.
/// - first whitespace-separated `addrlist` entry (parsed as [`IpAddr`]) ->
///   search target address.
/// - first `interface` entry (parsed as [`IpAddr`]) -> local bind address for
///   UDP search (a spvirit extension; p4p's own gateway ignores `interface`
///   for client networks).
///
/// `addrlist` may in principle carry multiple space-separated addresses (p4p
/// convention); M1 only wires up the first one.
// TODO(M1+): multi-addr search list — `addrlist` can contain several
// space-separated addresses; only the first is honoured for now.
fn build_client(c: &ClientCfg) -> PvaClient {
    let mut b = PvaClient::builder().udp_port(c.bcastport);

    if !c.autoaddrlist {
        b = b.no_broadcast();
    }

    if let Some(first) = c.addrlist.split_whitespace().next()
        && let Ok(addr) = first.parse::<IpAddr>()
    {
        b = b.search_addr(addr);
    }

    if let Some(first) = c.interface.first()
        && let Ok(addr) = first.parse::<IpAddr>()
    {
        b = b.bind_addr(addr);
    }

    b.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_client_per_entry() {
        let cfg = GatewayConfig::from_json_str(include_str!(
            "../tests/fixtures/p4p_bidirectional.json"
        ))
        .unwrap();
        let pool = UpstreamPool::from_config(&cfg);
        assert!(pool.client("docker-client-network").is_some());
        assert!(pool.client("external-client-network").is_some());
        assert!(pool.client("nope").is_none());
        let mut ns = pool.names();
        ns.sort();
        assert_eq!(ns, vec!["docker-client-network", "external-client-network"]);
    }
}
