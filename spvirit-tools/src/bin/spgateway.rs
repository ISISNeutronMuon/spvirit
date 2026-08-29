//! `spgateway` — p4p-compatible PVAccess gateway CLI entry point.
//!
//! Usage:
//!   spgateway <config.json>
//!   spgateway -T <config.json>
//!   spgateway --test-config <config.json>
//!
//! `-T`/`--test-config` parses and validates the given configuration file,
//! printing `OK` and exiting 0 on success, or printing the error and exiting
//! 1 on failure, without serving. Without it, `spgateway` loads and validates
//! the same way, then builds a [`spvirit_gateway::runtime::Runtime`] from the
//! config and runs it until it exits (all servers stop) or Ctrl-C is
//! received.
//!
//! `-v`/`--verbose` raises the log level from the default `INFO` to `DEBUG`.
//! Without it, `spgateway` still logs a one-line-per-server startup banner
//! (which port each server listens on and which upstreams it proxies) plus
//! any warnings/errors, so a normal start is no longer silent.
//!
//! `--discovery-parity` / `--no-discovery-parity` override the JSON
//! `discovery_parity` field for **every** server: `--discovery-parity` forces
//! the broadcast/multicast UDP search parity on, `--no-discovery-parity`
//! forces it off. Passing both is an error. When neither is given, each
//! server's JSON value (default `true`) is used. The override applies on the
//! `-T` validation path too, so `-T` validates the effective config.

use std::process::ExitCode;

use tracing::Level;

use spvirit_gateway::config::GatewayConfig;
use spvirit_gateway::runtime::Runtime;

/// Determine the `discovery_parity` CLI override from the argument list.
///
/// `--no-discovery-parity` → `Some(false)`, `--discovery-parity` → `Some(true)`,
/// neither → `None`. Passing both is a usage error.
fn parse_discovery_override(args: &[String]) -> Result<Option<bool>, String> {
    let on = args.iter().any(|a| a == "--discovery-parity");
    let off = args.iter().any(|a| a == "--no-discovery-parity");
    match (on, off) {
        (true, true) => {
            Err("--discovery-parity and --no-discovery-parity are mutually exclusive".to_string())
        }
        (true, false) => Ok(Some(true)),
        (false, true) => Ok(Some(false)),
        (false, false) => Ok(None),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let test_config = args.iter().any(|a| a == "-T" || a == "--test-config");
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
    let discovery_override = match parse_discovery_override(&args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let path = args.iter().find(|a| {
        !matches!(
            a.as_str(),
            "-T" | "--test-config"
                | "-v"
                | "--verbose"
                | "--discovery-parity"
                | "--no-discovery-parity"
        )
    });

    let Some(path) = path else {
        eprintln!(
            "usage: spgateway [-T|--test-config] [-v|--verbose] \
             [--discovery-parity|--no-discovery-parity] <config.json>"
        );
        return ExitCode::FAILURE;
    };

    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to read config '{path}': {e}");
            return ExitCode::FAILURE;
        }
    };

    if test_config {
        let result = GatewayConfig::from_json_str(&contents).and_then(|mut cfg| {
            if let Some(v) = discovery_override {
                for s in &mut cfg.servers {
                    s.discovery_parity = v;
                }
            }
            cfg.validate()
        });
        return match result {
            Ok(()) => {
                println!("OK");
                ExitCode::SUCCESS
            }
            Err(e) => {
                println!("{e}");
                ExitCode::FAILURE
            }
        };
    }

    // Serving path: install a tracing subscriber so the gateway is no longer
    // silent. `PvaServer` and the runtime banner emit at INFO; `-v` adds the
    // per-module DEBUG detail. Mirrors the `spvirit_server` binary's setup.
    tracing_subscriber::fmt::fmt()
        .with_max_level(if verbose { Level::DEBUG } else { Level::INFO })
        .init();

    let mut cfg = match GatewayConfig::from_json_str(&contents) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // CLI overrides JSON for all servers.
    if let Some(v) = discovery_override {
        for s in &mut cfg.servers {
            s.discovery_parity = v;
        }
    }

    let runtime = match Runtime::from_config(cfg) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match rt.block_on(runtime.run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("spgateway: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_discovery_override;
    use spvirit_gateway::config::GatewayConfig;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_flag_yields_none() {
        assert_eq!(parse_discovery_override(&args(&["cfg.json"])).unwrap(), None);
    }

    #[test]
    fn no_discovery_parity_yields_some_false() {
        assert_eq!(
            parse_discovery_override(&args(&["--no-discovery-parity", "cfg.json"])).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn discovery_parity_yields_some_true() {
        assert_eq!(
            parse_discovery_override(&args(&["--discovery-parity", "cfg.json"])).unwrap(),
            Some(true)
        );
    }

    #[test]
    fn both_flags_error() {
        assert!(
            parse_discovery_override(&args(&["--discovery-parity", "--no-discovery-parity"]))
                .is_err()
        );
    }

    #[test]
    fn override_applies_to_all_servers() {
        let json = r#"{ "version":2, "clients":[], "servers":[
            { "name":"a","clients":[] },
            { "name":"b","clients":[], "discovery_parity": true }
        ]}"#;
        let mut cfg = GatewayConfig::from_json_str(json).unwrap();
        let override_val = parse_discovery_override(&args(&["--no-discovery-parity", "cfg.json"]))
            .unwrap();
        if let Some(v) = override_val {
            for s in &mut cfg.servers {
                s.discovery_parity = v;
            }
        }
        assert!(cfg.servers.iter().all(|s| !s.discovery_parity));
    }
}
