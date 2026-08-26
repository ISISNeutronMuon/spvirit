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
    #[serde(rename = "read-only", default)]
    pub read_only: bool,
    #[serde(default)]
    pub clients: Vec<ClientCfg>,
    #[serde(default)]
    pub servers: Vec<ServerCfg>,
    /// Reserved for spvirit-specific extensions (typed in a later task).
    #[serde(rename = "x-spvirit", default)]
    pub x_spvirit: Option<serde_json::Value>,
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
    /// Reserved for spvirit-specific extensions (typed in a later task).
    #[serde(rename = "x-spvirit", default)]
    pub x_spvirit: Option<serde_json::Value>,
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
}
