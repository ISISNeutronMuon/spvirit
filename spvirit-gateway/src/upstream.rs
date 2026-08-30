//! Upstream client pool — one [`spvirit_client::PvaClient`] per `clients[]`
//! entry in the gateway configuration, honouring `interface` binding (a
//! spvirit feature p4p itself ignores).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use spvirit_client::{ByteSink, PvaClient};
use spvirit_server::diag::BandwidthCounters;

use crate::config::{ClientCfg, GatewayConfig};

/// A pool of upstream [`PvaClient`]s, keyed by the `clients[].name` from the
/// gateway configuration.
#[derive(Debug, Clone, Default)]
pub struct UpstreamPool {
    clients: HashMap<String, Arc<PvaClient>>,
}

/// Adapts a shared [`BandwidthCounters`] to the [`ByteSink`] seam
/// `spvirit-client` exposes on every [`PvaClient`] — installed identically
/// on every upstream client so a gateway's whole `clients[]` fleet feeds the
/// SAME upstream (`us_*`) counters that [`spvirit_server::PvaServer`]'s
/// downstream (`ds_*`) accounting shares via
/// `PvaServer::bandwidth_counters` (see `runtime.rs`).
struct CountersSink(Arc<BandwidthCounters>);

impl ByteSink for CountersSink {
    fn on_tx(&self, pv: &str, host: &str, n: u64) {
        self.0.us_bypv_tx.add(pv, n);
        self.0.us_byhost_tx.add(host, n);
    }

    fn on_rx(&self, pv: &str, host: &str, n: u64) {
        self.0.us_bypv_rx.add(pv, n);
        self.0.us_byhost_rx.add(host, n);
    }
}

impl UpstreamPool {
    /// Build a pool with one client per `cfg.clients[]` entry, with no
    /// upstream byte accounting installed. Prefer
    /// [`from_config_with_counters`](Self::from_config_with_counters) in
    /// production wiring (`Runtime::from_config`) so upstream traffic is
    /// counted; this bare constructor stays available for existing
    /// call sites/tests that don't care about bandwidth accounting.
    pub fn from_config(cfg: &GatewayConfig) -> Self {
        Self::from_config_with_counters(cfg, None)
    }

    /// Build a pool with one client per `cfg.clients[]` entry, installing a
    /// [`CountersSink`] wrapping `counters` on EVERY client when `counters`
    /// is `Some` — the same `Arc` passed by the caller, never a
    /// freshly-built `BandwidthCounters`, so every upstream client's
    /// `us_*` accounting lands in the one instance the caller also threads
    /// into `PvaServer::bandwidth_counters`.
    pub fn from_config_with_counters(
        cfg: &GatewayConfig,
        counters: Option<&Arc<BandwidthCounters>>,
    ) -> Self {
        let mut clients = HashMap::with_capacity(cfg.clients.len());
        for c in &cfg.clients {
            let mut client = build_client(c);
            if let Some(counters) = counters {
                client.set_byte_sink(Arc::new(CountersSink(counters.clone())) as Arc<dyn ByteSink>);
            }
            clients.insert(c.name.clone(), Arc::new(client));
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
/// - `autoaddrlist == false` with no usable `addrlist` target -> disable UDP
///   search entirely (see the `has_search_target` note below for why this is
///   narrower than "just `autoaddrlist == false`").
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

    // Only fully disable UDP search when the user opted out of automatic
    // address-list discovery AND gave no explicit search target. spvirit-client's
    // no_broadcast() suppresses the WHOLE UDP search branch — including the
    // unicast search_addr — so calling it while a search target is set would
    // silently resolve nothing (a standard p4p `{autoaddrlist:false, addrlist:"<ip>"}`
    // unicast config). When an explicit addrlist target is present we therefore
    // keep UDP search enabled so search_addr is honored. Residual (documented
    // §14 gap): a subnet broadcast is still emitted alongside the unicast search
    // in that case — spvirit-client offers no "unicast-only UDP search" mode
    // without modifying that crate.
    let has_search_target = c
        .addrlist
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<IpAddr>().ok())
        .is_some();
    if !c.autoaddrlist && !has_search_target {
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
