//! p4p-compatible JSON gateway configuration schema and parser.

use serde::Deserialize;

fn default_provider() -> String {
    "pva".to_string()
}

fn default_true() -> bool {
    true
}

fn default_bcastport() -> u16 {
    5076
}

fn default_serverport() -> u16 {
    5075
}

fn default_getholdoff() -> u32 {
    0
}

/// Top-level p4p-compatible gateway configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    pub version: u32,
    #[serde(rename = "readOnly", default)]
    pub read_only: bool,
    #[serde(default)]
    pub clients: Vec<ClientCfg>,
    #[serde(default)]
    pub servers: Vec<ServerCfg>,
    /// spvirit-specific extensions (superset over p4p; see spec §5.2).
    #[serde(rename = "x-spvirit", default)]
    pub x_spvirit: Option<TopExt>,
}

/// A p4p "client" network: how the gateway searches for PVs upstream.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientCfg {
    pub name: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub addrlist: String,
    #[serde(default = "default_true")]
    pub autoaddrlist: bool,
    #[serde(default = "default_bcastport")]
    pub bcastport: u16,
    #[serde(default)]
    pub interface: Vec<String>,
}

/// A p4p "server" network: how the gateway advertises PVs downstream.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerCfg {
    pub name: String,
    #[serde(default)]
    pub clients: Vec<String>,
    #[serde(default)]
    pub interface: Vec<String>,
    #[serde(default)]
    pub addrlist: String,
    #[serde(default)]
    pub ignoreaddr: String,
    #[serde(default = "default_true")]
    pub autoaddrlist: bool,
    #[serde(default = "default_serverport")]
    pub serverport: u16,
    #[serde(default = "default_bcastport")]
    pub bcastport: u16,
    #[serde(default = "default_getholdoff")]
    pub getholdoff: u32,
    #[serde(default)]
    pub statusprefix: String,
    #[serde(default)]
    pub access: String,
    #[serde(default)]
    pub pvlist: String,
    #[serde(rename = "acf-client", default)]
    pub acf_client: Option<String>,
    /// spvirit-specific extensions (superset over p4p; see spec §5.2).
    #[serde(rename = "x-spvirit", default)]
    pub x_spvirit: Option<ServerExt>,
}

/// Top-level `x-spvirit` extension object (spec §5.2).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopExt {
    #[serde(default)]
    pub metrics: Option<MetricsExt>,
    #[serde(default)]
    pub audit: Option<AuditExt>,
    #[serde(rename = "hotReload", default)]
    pub hot_reload: Option<HotReloadExt>,
}

/// `x-spvirit.metrics` — Prometheus `/metrics` responder + status-PV mirror.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsExt {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_metrics_listen")]
    pub listen: String,
    #[serde(default = "default_metrics_path")]
    pub path: String,
}

fn default_metrics_listen() -> String {
    "0.0.0.0:9090".to_string()
}

fn default_metrics_path() -> String {
    "/metrics".to_string()
}

/// `x-spvirit.audit` — structured JSON audit sink.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditExt {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_audit_sink")]
    pub sink: AuditSink,
    #[serde(default = "default_audit_format")]
    pub format: String,
}

fn default_audit_sink() -> AuditSink {
    AuditSink::Named("stdout".to_string())
}

fn default_audit_format() -> String {
    "json".to_string()
}

/// `x-spvirit.audit.sink` — either a named sink (e.g. `"stdout"`) or a file target.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AuditSink {
    Named(String),
    File {
        #[serde(default)]
        path: String,
    },
}

/// `x-spvirit.hotReload` — ACL/pvlist hot-reload triggers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotReloadExt {
    #[serde(default)]
    pub signal: bool,
    #[serde(default)]
    pub rpc: bool,
}

/// Per-server `x-spvirit` extension object (spec §5.2).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerExt {
    #[serde(rename = "negativeCache", default)]
    pub negative_cache: Option<NegCacheExt>,
    #[serde(rename = "rateLimit", default)]
    pub rate_limit: Option<RateLimitExt>,
}

/// `x-spvirit.negativeCache` — negative-search cache TTL and capacity.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NegCacheExt {
    pub ttl_ms: u64,
    pub capacity: usize,
}

/// `x-spvirit.rateLimit` — per-downstream-client token buckets.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitExt {
    #[serde(rename = "perClient", default)]
    pub per_client: Option<PerClientLimits>,
}

/// `x-spvirit.rateLimit.perClient` — token-bucket rates and burst size.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerClientLimits {
    #[serde(default)]
    pub search_per_s: u32,
    #[serde(default)]
    pub get_per_s: u32,
    #[serde(default)]
    pub monitor_per_s: u32,
    #[serde(default)]
    pub burst: u32,
}

/// Errors that can occur while loading a gateway configuration.
#[derive(Debug, Clone)]
pub enum ConfigError {
    Json(String),
    Validation(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Json(msg) => write!(f, "invalid config JSON: {msg}"),
            ConfigError::Validation(msg) => write!(f, "invalid config: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl GatewayConfig {
    /// Parse a gateway configuration from a JSON string (p4p-compatible schema).
    pub fn from_json_str(s: &str) -> Result<Self, ConfigError> {
        serde_json::from_str(s).map_err(|e| ConfigError::Json(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const BIDI: &str = include_str!("../tests/fixtures/p4p_bidirectional.json");

    #[test]
    fn parses_p4p_bidirectional() {
        let cfg = GatewayConfig::from_json_str(BIDI).expect("parse");
        assert_eq!(cfg.version, 2);
        assert_eq!(cfg.clients.len(), 2);
        assert_eq!(cfg.servers.len(), 2);
        let dc = &cfg.clients[0];
        assert_eq!(dc.name, "docker-client-network");
        assert!(!dc.autoaddrlist);
        assert_eq!(dc.interface, vec!["172.18.0.1".to_string()]);
        assert_eq!(dc.bcastport, 5076); // default applied
        let ss = &cfg.servers[0];
        assert_eq!(ss.clients, vec!["docker-client-network".to_string()]);
        assert_eq!(ss.serverport, 5075); // default applied
        assert_eq!(cfg.servers[1].ignoreaddr, "gw.example.org 172.18.0.1");
    }

    #[test]
    fn parses_read_only_camel_case_key() {
        let cfg = GatewayConfig::from_json_str(r#"{"version": 2, "readOnly": true}"#)
            .expect("parse");
        assert!(cfg.read_only);
    }

    #[test]
    fn parses_x_spvirit_and_rejects_typos() {
        let ok = r#"{ "version":2, "clients":[], "servers":[
            { "name":"s","clients":[],
              "x-spvirit": { "negativeCache": { "ttl_ms": 5000, "capacity": 1024 } } }
        ]}"#;
        let cfg = GatewayConfig::from_json_str(ok).unwrap();
        let nc = cfg.servers[0]
            .x_spvirit
            .as_ref()
            .unwrap()
            .negative_cache
            .as_ref()
            .unwrap();
        assert_eq!(nc.ttl_ms, 5000);
        assert_eq!(nc.capacity, 1024);

        let typo = r#"{ "version":2, "clients":[], "servers":[
            { "name":"s","clients":[], "x-spvirit": { "negativeCahe": {} } }
        ]}"#;
        assert!(GatewayConfig::from_json_str(typo).is_err()); // deny_unknown_fields inside x-spvirit
    }

    #[test]
    fn unknown_p4p_key_is_ignored() {
        let s = r#"{ "version":2, "futureKey": 7, "clients":[], "servers":[] }"#;
        assert!(GatewayConfig::from_json_str(s).is_ok());
    }
}
