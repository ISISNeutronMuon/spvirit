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
#[derive(Debug, Clone, Default, Deserialize)]
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

impl Default for MetricsExt {
    /// Matches the serde field defaults (`enabled:false`, the default listen
    /// address and path), so the CLI can synthesize a `MetricsExt` when the
    /// config omits the block but `--metrics`/`--metrics-listen` is given.
    fn default() -> Self {
        Self {
            enabled: false,
            listen: default_metrics_listen(),
            path: default_metrics_path(),
        }
    }
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

/// The only gateway config schema revision that exists.
///
/// This is the `version` field of the JSON config **file** — p4p's schema
/// marker, which every example, doc page and test in this repo sets to 2. It
/// is deliberately a hard-coded constant and has nothing whatsoever to do with
/// the workspace crate version: a crate release must never imply a config
/// schema change, and bumping one must never require bumping the other.
pub const SUPPORTED_CONFIG_VERSION: u32 = 2;

/// Keys that are `discovery_parity` misspelled.
///
/// `discovery_parity` is spvirit-specific but lives at the top level of a
/// `server` block (documented in `docs/book/src/04-tools/spgateway.md`, placed
/// there deliberately per the feature brief), and `ServerCfg` cannot use
/// `deny_unknown_fields` because p4p forward compatibility requires tolerating
/// keys we have never heard of. Combined with the field defaulting to `true`,
/// that made a misspelling silent: the operator writes
/// `"discoveryParity": false`, serde ignores it, and discovery parity stays
/// ON.
///
/// So: near-misses are rejected by name, and only near-misses. A key is a
/// near-miss when lower-casing it and reducing every non-alphanumeric run to
/// nothing yields `discoveryparity` — that catches case, separator and
/// `x-spvirit`-prefix variants while leaving any genuinely unrelated future
/// p4p key alone.
fn reject_discovery_parity_near_misses(raw: &serde_json::Value) -> Result<(), ConfigError> {
    fn squash(key: &str) -> String {
        key.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    }

    let Some(servers) = raw.get("servers").and_then(|s| s.as_array()) else {
        return Ok(());
    };
    for server in servers {
        let Some(obj) = server.as_object() else {
            continue;
        };
        for key in obj.keys() {
            if key != "discovery_parity" && squash(key).ends_with("discoveryparity") {
                return Err(ConfigError::Validation(format!(
                    "unknown server key `{key}`: did you mean `discovery_parity`? \
                     (spvirit reads this key at the top level of a server block; \
                     a misspelling would silently leave discovery parity at its default)"
                )));
            }
        }
    }
    Ok(())
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
    /// Parses a p4p-compatible gateway config from JSON.
    ///
    /// Unknown top-level and unknown `server`/`client` keys are tolerated for
    /// p4p forward compatibility, with one exception: a key that is
    /// [`discovery_parity`](reject_discovery_parity_near_misses) misspelled is
    /// a hard error rather than a silent no-op.
    pub fn from_json_str(s: &str) -> Result<Self, ConfigError> {
        let raw: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ConfigError::Json(e.to_string()))?;
        reject_discovery_parity_near_misses(&raw)?;
        serde_json::from_value(raw).map_err(|e| ConfigError::Json(e.to_string()))
    }

    /// Runs every semantic check and builds one [`AccessControl`] per server,
    /// in `self.servers` order, reading each pvlist/`.acf` file **exactly
    /// once**.
    ///
    /// The two used to be separate passes: `validate` built a control per
    /// server to prove the files parsed and dropped it, and the runtime then
    /// built them all over again. That doubled the startup file I/O and, worse,
    /// opened a TOCTOU window — a file edited between the two reads is
    /// validated in one state and enforced in another, silently.
    pub fn validate_and_build(&self) -> Result<Vec<AccessControl>, ConfigError> {
        use std::collections::HashSet;

        if self.version != SUPPORTED_CONFIG_VERSION {
            return Err(ConfigError::Validation(format!(
                "unsupported config version {}: only version {SUPPORTED_CONFIG_VERSION} exists",
                self.version
            )));
        }

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
        // `AccessControl`s are handed back to the caller so this is the
        // only time each file is read.
        self.servers
            .iter()
            .map(|s| self.build_access_control(s))
            .collect()
    }

    /// Runs every semantic check, discarding the [`AccessControl`]s built
    /// along the way. For callers that only want a yes/no — `spgateway -T`,
    /// principally. A caller that is about to *use* the controls should call
    /// [`Self::validate_and_build`] instead of validating and then rebuilding.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_and_build().map(|_| ())
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

    /// A minimal valid config with every key these tests mutate present and
    /// explicit, so a `str::replace` in a test cannot silently no-op.
    ///
    /// Same shape as `minimal_valid_config()` (one `pva` client, one server
    /// referencing it, no `access`/`pvlist` files) so it passes `validate` for
    /// reasons unrelated to this task, plus an explicit `discovery_parity`.
    fn minimal_config_json() -> String {
        r#"{
            "version": 2,
            "clients": [{"name": "cli", "provider": "pva"}],
            "servers": [{"name": "srv", "clients": ["cli"], "access": "", "pvlist": "", "discovery_parity": true}]
        }"#
        .to_string()
    }

    /// `version` is required by serde and then never looked at, so a config
    /// written for a different schema revision is accepted in full and
    /// silently reinterpreted under today's rules. Only 2 exists.
    ///
    /// NB: this is the CONFIG FILE's schema version. It has nothing to do with
    /// the workspace crate version and must never be derived from it.
    #[test]
    fn validate_rejects_an_unsupported_config_version() {
        let base = minimal_config_json();
        let ok = GatewayConfig::from_json_str(&base).unwrap();
        assert_eq!(ok.version, SUPPORTED_CONFIG_VERSION);
        ok.validate().expect("version 2 is the supported schema");

        for bad in [0u32, 1, 3, 42] {
            let cfg = GatewayConfig::from_json_str(&base.replace("\"version\": 2", &format!("\"version\": {bad}")))
                .unwrap();
            let err = cfg
                .validate()
                .expect_err(&format!("version {bad} must be rejected"));
            let msg = format!("{err}");
            assert!(msg.contains("version"), "error must name the offending key: {msg}");
            assert!(msg.contains(&bad.to_string()), "error must quote the bad value: {msg}");
        }
    }

    /// `discovery_parity` defaults to true and lives in a struct that must
    /// tolerate unknown keys (p4p forward compatibility), so a misspelling is
    /// silently ignored and the operator gets parity ON while their file says
    /// OFF. Near-misses are rejected by name; genuinely unknown p4p keys are
    /// still accepted.
    #[test]
    fn a_misspelled_discovery_parity_is_a_hard_error() {
        for typo in [
            "discoveryParity",
            "discovery-parity",
            "Discovery_Parity",
            "x-spvirit-discovery_parity",
        ] {
            let json = minimal_config_json().replace(
                "\"discovery_parity\": true",
                &format!("\"{typo}\": false"),
            );
            let err = GatewayConfig::from_json_str(&json)
                .expect_err(&format!("{typo} must be rejected, not silently ignored"));
            let msg = format!("{err}");
            assert!(msg.contains(typo), "error must quote what was written: {msg}");
            assert!(
                msg.contains("discovery_parity"),
                "error must name the correct spelling: {msg}"
            );
        }
    }

    /// The scan must stay narrow: an unrelated future p4p key in a server
    /// block is not a near-miss and must still be tolerated, exactly as
    /// `unknown_p4p_key_is_ignored` requires at the top level.
    #[test]
    fn an_unrelated_unknown_server_key_is_still_tolerated() {
        let json = minimal_config_json()
            .replace("\"discovery_parity\": true", "\"someFutureP4pKey\": 7");
        let cfg = GatewayConfig::from_json_str(&json).expect("unknown p4p keys stay tolerated");
        assert!(cfg.servers[0].discovery_parity, "and the default still applies");
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

    /// Validation and construction must be one pass over the ACL files, not
    /// two. Proven by rewriting every pvlist file to its opposite *after* the
    /// call returns: if the returned controls still enforce what was
    /// validated, the files were read once and the result kept. Two reads
    /// would leave a TOCTOU window in which the config that got validated is
    /// not the config that gets enforced.
    ///
    /// Also pins the ordering contract runtime.rs depends on: one control per
    /// server, in `self.servers` order.
    #[test]
    fn validate_and_build_reads_each_acl_file_once_and_returns_them_in_server_order() {
        use crate::access::{Decision, Identity, Op};

        // `tempdir()` is this module's existing helper (config.rs:516-525);
        // its `TempDir` has no `new()`.
        let dir = tempdir();
        let allow = dir.path().join("allow.pvlist");
        let deny = dir.path().join("deny.pvlist");
        std::fs::write(&allow, ".* ALLOW\n").unwrap();
        std::fs::write(&deny, "SECRET:.* DENY\n.* ALLOW\n").unwrap();

        let json = format!(
            r#"{{
                "version": 2,
                "clients": [{{"name": "cli", "provider": "pva", "addrlist": "127.0.0.1"}}],
                "servers": [
                  {{"name": "open",   "clients": ["cli"], "pvlist": "{}"}},
                  {{"name": "closed", "clients": ["cli"], "pvlist": "{}"}}
                ]
            }}"#,
            allow.display().to_string().replace('\\', "\\\\"),
            deny.display().to_string().replace('\\', "\\\\"),
        );
        let cfg = GatewayConfig::from_json_str(&json).unwrap();

        let controls = cfg.validate_and_build().expect("a valid config builds");
        assert_eq!(controls.len(), cfg.servers.len(), "one control per server");

        // Swap the files out from under the returned controls.
        std::fs::write(&allow, "SECRET:.* DENY\n.* ALLOW\n").unwrap();
        std::fs::write(&deny, ".* ALLOW\n").unwrap();

        let who = Identity::default();
        assert_eq!(
            controls[0].decide(Op::Get, "SECRET:X", &who),
            Decision::Allow,
            "servers[0] must still enforce the ruleset that was validated"
        );
        assert_eq!(
            controls[1].decide(Op::Get, "SECRET:X", &who),
            Decision::Deny,
            "servers[1] must still enforce the ruleset that was validated, in order"
        );
    }
}
