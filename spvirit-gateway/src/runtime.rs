//! `Runtime` — wires a validated [`GatewayConfig`] into a running gateway:
//! one shared [`UpstreamPool`] plus one [`spvirit_server::PvaServer`] (backed
//! by a [`GatewaySource`]) per `servers[]` entry.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use spvirit_server::PvaServer;

use crate::cache::negative::NegativeCache;
use crate::config::{ConfigError, GatewayConfig};
use crate::loopguard::LoopGuard;
use crate::proxy::GatewaySource;
use crate::upstream::UpstreamPool;

/// Default negative-search-cache TTL used when a server has no `x-spvirit
/// .negativeCache` override.
const DEFAULT_NEG_CACHE_TTL: Duration = Duration::from_secs(30);
/// Default negative-search-cache capacity used when a server has no
/// `x-spvirit.negativeCache` override.
const DEFAULT_NEG_CACHE_CAPACITY: usize = 128;

/// A fully wired gateway runtime: one [`PvaServer`] per configured
/// `servers[]` entry, all sharing a single upstream [`UpstreamPool`].
pub struct Runtime {
    servers: Vec<PvaServer>,
}

impl Runtime {
    /// Validate `cfg` and build a `Runtime` from it.
    ///
    /// Builds one shared [`UpstreamPool`] for the whole configuration (every
    /// `clients[]` entry becomes one upstream [`spvirit_client::PvaClient`]),
    /// then for each `servers[]` entry builds a [`LoopGuard`], a
    /// [`NegativeCache`] (from that server's `x-spvirit.negativeCache`, or
    /// the defaults above; a configured `capacity: 0` is clamped to `1` per
    /// the Task 8 deferred note since a zero-capacity cache cannot hold
    /// anything), and a [`GatewaySource`] wired to that server's
    /// `clients[]` order — then a [`PvaServer`] listening on that server's
    /// `serverport`/`bcastport`, bound (and advertised) on the first
    /// `interface` entry.
    ///
    /// If a server has no `interface` entries, it binds to `0.0.0.0`
    /// (all interfaces) — the safest default for "unspecified", and
    /// consistent with p4p's own gateway defaulting to a wildcard bind when
    /// no interface list is given.
    pub fn from_config(cfg: GatewayConfig) -> Result<Self, ConfigError> {
        cfg.validate()?;

        let pool = Arc::new(UpstreamPool::from_config(&cfg));
        let mut servers = Vec::with_capacity(cfg.servers.len());

        for server_cfg in &cfg.servers {
            let guard = Arc::new(LoopGuard::build(&cfg, server_cfg));

            let (ttl, capacity) = server_cfg
                .x_spvirit
                .as_ref()
                .and_then(|ext| ext.negative_cache.as_ref())
                .map(|nc| (Duration::from_millis(nc.ttl_ms), nc.capacity.max(1)))
                .unwrap_or((DEFAULT_NEG_CACHE_TTL, DEFAULT_NEG_CACHE_CAPACITY));
            let neg = Arc::new(NegativeCache::new(ttl, capacity));

            let src = GatewaySource::new(
                pool.clone(),
                server_cfg.clients.clone(),
                neg,
                guard,
                server_cfg.getholdoff,
            );

            let interface_ip: IpAddr = server_cfg
                .interface
                .first()
                .and_then(|s| s.parse::<IpAddr>().ok())
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));

            let server = PvaServer::builder()
                .port(server_cfg.serverport)
                .udp_port(server_cfg.bcastport)
                .listen_ip(interface_ip)
                .advertise_ip(interface_ip)
                .source("gateway", 0, Arc::new(src))
                .build();

            servers.push(server);

            tracing::info!(
                "spgateway: server '{}' -> {}:{} (udp {}), upstreams [{}]",
                server_cfg.name,
                interface_ip,
                server_cfg.serverport,
                server_cfg.bcastport,
                server_cfg.clients.join(", "),
            );
        }

        tracing::info!(
            "spgateway: {} server(s), {} upstream(s) configured",
            cfg.servers.len(),
            cfg.clients.len(),
        );

        Ok(Runtime { servers })
    }

    /// Run every server's serve loop concurrently until either all of them
    /// exit, one of them errors, or Ctrl-C is received (in which case `run`
    /// returns `Ok(())` — a graceful stop, not a failure).
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        tracing::info!(
            "spgateway: starting {} server(s); press Ctrl-C to stop",
            self.servers.len()
        );

        let mut set = tokio::task::JoinSet::new();
        for server in self.servers {
            // `PvaServer::run`'s error type (`Box<dyn std::error::Error>`,
            // without a `Send` bound) can't cross a `JoinSet`'s task
            // boundary directly, so it is stringified inside the spawned
            // task, before it would otherwise need to be held across an
            // await point in the joiner.
            set.spawn(async move { server.run().await.map_err(|e| e.to_string()) });
        }

        tokio::select! {
            result = join_all(&mut set) => result,
            _ = tokio::signal::ctrl_c() => Ok(()),
        }
    }
}

/// Drain `set`, returning the first error encountered (from either a failed
/// server or a panicked/cancelled task), or `Ok(())` once every server's
/// `run()` has returned successfully.
async fn join_all(
    set: &mut tokio::task::JoinSet<Result<(), String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    while let Some(res) = set.join_next().await {
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e.into()),
            Err(join_err) => return Err(join_err.to_string().into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIDI: &str = include_str!("../tests/fixtures/p4p_bidirectional.json");

    #[test]
    fn from_config_rejects_invalid_config() {
        let cfg = GatewayConfig::from_json_str(
            r#"{ "version":2, "clients":[{"name":"a"}], "servers":[{"name":"s","clients":["nope"]}] }"#,
        )
        .unwrap();
        match Runtime::from_config(cfg) {
            Err(ConfigError::Validation(_)) => {}
            Err(other) => panic!("expected a validation error, got {other:?}"),
            Ok(_) => panic!("expected a validation error, got Ok"),
        }
    }

    #[test]
    fn from_config_builds_one_server_per_entry() {
        let cfg = GatewayConfig::from_json_str(BIDI).unwrap();
        let n_servers = cfg.servers.len();
        let rt = Runtime::from_config(cfg).expect("valid config builds");
        assert_eq!(rt.servers.len(), n_servers);
    }
}
