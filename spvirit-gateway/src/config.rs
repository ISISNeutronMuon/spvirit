//! p4p-compatible JSON gateway configuration schema and parser.

use serde::Deserialize;

use crate::access::acf::parse_acf;
use crate::access::pvlist::parse_pvlist;
use crate::access::AccessControl;

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
    /// When true (default), the server's UDP search socket binds `0.0.0.0` and
    /// joins the PVA multicast group (224.0.0.128) so broadcast/multicast
    /// searches are heard (p4p parity). When false, it binds only the
    /// configured `interface` IP and does not join multicast.
    #[serde(default = "default_true")]
    pub discovery_parity: bool,
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

    /// Semantic validation beyond what serde can express: uniqueness of
    /// names and cross-references between clients/servers.
    pub fn validate(&self) -> Result<(), ConfigError> {
        use std::collections::HashSet;

        let mut client_names: HashSet<&str> = HashSet::new();
        for c in &self.clients {
            if !client_names.insert(c.name.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate client name: {}",
                    c.name
                )));
            }
        }

        let mut server_names: HashSet<&str> = HashSet::new();
        for s in &self.servers {
            if !server_names.insert(s.name.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate server name: {}",
                    s.name
                )));
            }

            if s.clients.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "server '{}' has no clients (no upstream source)",
                    s.name
                )));
            }

            for c in &s.clients {
                if !client_names.contains(c.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "server '{}' references unknown client '{}'",
                        s.name, c
                    )));
                }
            }

            if let Some(acf) = &s.acf_client
                && !client_names.contains(acf.as_str())
            {
                return Err(ConfigError::Validation(format!(
                    "server '{}' references unknown acf-client '{}'",
                    s.name, acf
                )));
            }
        }

        // Fail-closed: reject unsupported providers. spvirit only speaks
        // PVAccess upstream; a `ca` (or any other) provider silently
        // dropping requests is worse than refusing to start.
        for c in &self.clients {
            if c.provider != "pva" {
                return Err(ConfigError::Validation(format!(
                    "client '{}' uses unsupported provider '{}' (only 'pva')",
                    c.name, c.provider
                )));
            }
        }

        // Fail-closed: load+parse every access/pvlist file now, so
        // `spgateway -T` catches a missing/unreadable/unparseable file
        // before the gateway ever serves a request. The built
        // `AccessControl` is discarded here; the live Runtime (Task 11)
        // calls `build_access_control` again to get one it keeps.
        for s in &self.servers {
            let _ = self.build_access_control(s)?;
        }

        Ok(())
    }

    /// Loads and parses `server`'s `access` (ACF) and `pvlist` files into a
    /// built [`AccessControl`].
    ///
    /// Paths are resolved relative to the process's current working
    /// directory (matching p4p/pvagw, which is launched from the config
    /// file's own directory) — *not* relative to the config file's path.
    /// An empty path means "no file", producing the drop-in-default `None`
    /// half of the `AccessControl`.
    pub fn build_access_control(&self, server: &ServerCfg) -> Result<AccessControl, ConfigError> {
        let pvlist = if server.pvlist.is_empty() {
            None
        } else {
            let text = std::fs::read_to_string(&server.pvlist).map_err(|e| {
                ConfigError::Validation(format!("pvlist '{}': {e}", server.pvlist))
            })?;
            Some(parse_pvlist(&text).map_err(|e| {
                ConfigError::Validation(format!("pvlist '{}': {e}", server.pvlist))
            })?)
        };
        let acf = if server.access.is_empty() {
            None
        } else {
            let text = std::fs::read_to_string(&server.access).map_err(|e| {
                ConfigError::Validation(format!("access '{}': {e}", server.access))
            })?;
            Some(parse_acf(&text).map_err(|e| {
                ConfigError::Validation(format!("access '{}': {e}", server.access))
            })?)
        };
        Ok(AccessControl::new(self.read_only, pvlist, acf))
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
    fn discovery_parity_defaults_true_and_parses_explicit_false() {
        let default = r#"{ "version":2, "clients":[], "servers":[
            { "name":"s","clients":[] }
        ]}"#;
        let cfg = GatewayConfig::from_json_str(default).unwrap();
        assert!(cfg.servers[0].discovery_parity);

        let off = r#"{ "version":2, "clients":[], "servers":[
            { "name":"s","clients":[], "discovery_parity": false }
        ]}"#;
        let cfg = GatewayConfig::from_json_str(off).unwrap();
        assert!(!cfg.servers[0].discovery_parity);
    }

    #[test]
    fn unknown_p4p_key_is_ignored() {
        let s = r#"{ "version":2, "futureKey": 7, "clients":[], "servers":[] }"#;
        assert!(GatewayConfig::from_json_str(s).is_ok());
    }

    #[test]
    fn validate_rejects_dangling_client_ref() {
        let s = r#"{ "version":2, "clients":[{"name":"a"}],
            "servers":[{"name":"s","clients":["nope"]}] }"#;
        let cfg = GatewayConfig::from_json_str(s).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn validate_accepts_valid_config() {
        // BIDI's `pvlist` path (`/etc/pvagw/pvacl.conf`) is illustrative of a
        // real p4p deployment, not a file present in this checkout; clear it
        // so this test exercises only the referential checks it's named for
        // (duplicate/dangling names), not fail-closed file loading (which
        // has its own tests below).
        let mut cfg = GatewayConfig::from_json_str(BIDI).expect("parse");
        for s in &mut cfg.servers {
            s.pvlist.clear();
            s.access.clear();
        }
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_duplicate_client_names() {
        let s = r#"{ "version":2, "clients":[{"name":"a"},{"name":"a"}],
            "servers":[] }"#;
        let cfg = GatewayConfig::from_json_str(s).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn validate_rejects_duplicate_server_names() {
        let s = r#"{ "version":2, "clients":[{"name":"a"}],
            "servers":[{"name":"s","clients":["a"]},{"name":"s","clients":["a"]}] }"#;
        let cfg = GatewayConfig::from_json_str(s).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn validate_rejects_server_with_no_clients() {
        let s = r#"{ "version":2, "clients":[{"name":"a"}],
            "servers":[{"name":"s","clients":[]}] }"#;
        let cfg = GatewayConfig::from_json_str(s).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn validate_rejects_unknown_acf_client() {
        let s = r#"{ "version":2, "clients":[{"name":"a"}],
            "servers":[{"name":"s","clients":["a"],"acf-client":"nope"}] }"#;
        let cfg = GatewayConfig::from_json_str(s).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    // ─── fail-closed: provider check + access/pvlist file loading ──────────

    /// A minimal config that currently passes `validate()`: one `pva` client,
    /// one server with empty `access`/`pvlist` (drop-in defaults).
    fn minimal_valid_config() -> GatewayConfig {
        let s = r#"{ "version":2, "clients":[{"name":"c","provider":"pva"}],
            "servers":[{"name":"s","clients":["c"],"access":"","pvlist":""}] }"#;
        GatewayConfig::from_json_str(s).unwrap()
    }

    /// A unique, self-cleaning temp directory (no `tempfile` dev-dependency
    /// exists in this crate; both `std::env::temp_dir` + a unique suffix and
    /// `Drop`-based cleanup are all that's needed).
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("spvirit-gateway-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    #[test]
    fn validate_rejects_non_pva_provider() {
        let mut cfg = minimal_valid_config();
        cfg.clients[0].provider = "ca".into();
        let e = cfg.validate().unwrap_err();
        assert!(format!("{e}").contains("provider"));
    }

    #[test]
    fn validate_rejects_missing_pvlist_file() {
        let mut cfg = minimal_valid_config();
        cfg.servers[0].pvlist = "/no/such/pvlist.acf".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_accepts_empty_access_and_pvlist() {
        let cfg = minimal_valid_config(); // access="" pvlist="" provider="pva"
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_unparseable_pvlist_contents() {
        let dir = tempdir();
        let path = dir.path().join("bad.pvlist");
        std::fs::write(&path, "X:.* FROBNICATE").unwrap();
        let mut cfg = minimal_valid_config();
        cfg.servers[0].pvlist = path.to_string_lossy().into();
        assert!(cfg.validate().is_err());
    }
}
