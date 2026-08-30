//! `spgateway` — p4p-compatible PVAccess gateway CLI entry point.
//!
//! Usage:
//!   spgateway <config.json>
//!   spgateway -T <config.json>
//!   spgateway --test-config <config.json>
//!
//! Argument parsing uses the `argparse` crate (as every other `spvirit-tools`
//! binary does), so `spgateway --help` prints a generated usage summary.
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
//!
//! `--metrics` enables the Prometheus `/metrics` endpoint regardless of the
//! config's `x-spvirit.metrics.enabled` field (if the flag is present, metrics
//! serve even when the block is absent or `enabled:false`).
//! `--metrics-listen <ADDR>` overrides the metrics listen address (e.g.
//! `127.0.0.1:9110`); the CLI value takes precedence over the config `listen`.
//! Both apply on the `-T` validation path too, so `-T` validates the effective
//! config.

use std::io;
use std::process::ExitCode;

use argparse::{ArgumentParser, Store, StoreTrue};
use tracing::Level;

use spvirit_gateway::config::{GatewayConfig, MetricsExt, TopExt};
use spvirit_gateway::runtime::Runtime;

/// The parsed CLI options. Filled by [`parse_cli`] so flag handling is
/// unit-testable without spawning a process.
#[derive(Debug, Default, PartialEq, Eq)]
struct CliOptions {
    path: String,
    test_config: bool,
    verbose: bool,
    discovery_parity: bool,
    no_discovery_parity: bool,
    metrics: bool,
    metrics_listen: String,
}

/// Parse `args` (the arguments *after* the program name) into [`CliOptions`]
/// using `argparse`. Returns `Err(code)` when `argparse` would exit — `0` for
/// `--help` (already printed), non-zero for a usage error (already reported to
/// stderr).
fn parse_cli(args: &[String]) -> Result<CliOptions, i32> {
    let mut opts = CliOptions::default();
    {
        let mut ap = ArgumentParser::new();
        ap.set_description("p4p-compatible PVAccess gateway.");
        ap.refer(&mut opts.test_config).add_option(
            &["-T", "--test-config"],
            StoreTrue,
            "Validate the config and exit (prints OK / the error)",
        );
        ap.refer(&mut opts.verbose).add_option(
            &["-v", "--verbose"],
            StoreTrue,
            "Raise the log level from INFO to DEBUG",
        );
        ap.refer(&mut opts.discovery_parity).add_option(
            &["--discovery-parity"],
            StoreTrue,
            "Force broadcast/multicast UDP search parity ON for every server",
        );
        ap.refer(&mut opts.no_discovery_parity).add_option(
            &["--no-discovery-parity"],
            StoreTrue,
            "Force broadcast/multicast UDP search parity OFF for every server",
        );
        ap.refer(&mut opts.metrics).add_option(
            &["--metrics"],
            StoreTrue,
            "Serve the Prometheus /metrics endpoint (overrides config enabled)",
        );
        ap.refer(&mut opts.metrics_listen).add_option(
            &["--metrics-listen"],
            Store,
            "Metrics listen address ip:port (overrides config listen)",
        );
        ap.refer(&mut opts.path).add_argument(
            "config",
            Store,
            "Gateway configuration JSON file",
        );
        // argparse expects argv[0] to be the program name.
        let argv: Vec<String> = std::iter::once("spgateway".to_string())
            .chain(args.iter().cloned())
            .collect();
        ap.parse(argv, &mut io::stdout(), &mut io::stderr())?;
    }
    Ok(opts)
}

/// Map the two discovery-parity flags to an override, reproducing the original
/// mutual-exclusion error. `--no-discovery-parity` → `Some(false)`,
/// `--discovery-parity` → `Some(true)`, neither → `None`. Passing both is a
/// usage error. Kept as a pure helper so it stays unit-testable.
fn discovery_override_from_flags(on: bool, off: bool) -> Result<Option<bool>, String> {
    match (on, off) {
        (true, true) => {
            Err("--discovery-parity and --no-discovery-parity are mutually exclusive".to_string())
        }
        (true, false) => Ok(Some(true)),
        (false, true) => Ok(Some(false)),
        (false, false) => Ok(None),
    }
}

/// Apply a `discovery_parity` CLI override to every server in `cfg`. A `None`
/// override leaves the config untouched (each server keeps its JSON value);
/// `Some(v)` sets every server's `discovery_parity` to `v` (CLI overrides JSON
/// for all servers).
fn apply_discovery_override(cfg: &mut GatewayConfig, override_val: Option<bool>) {
    if let Some(v) = override_val {
        for s in &mut cfg.servers {
            s.discovery_parity = v;
        }
    }
}

/// Apply the metrics CLI overrides to `cfg`'s `x-spvirit.metrics` block,
/// synthesizing the `x-spvirit`/`metrics` objects if the config omits them.
///
/// - `force_on` (from `--metrics`) forces `enabled = true` regardless of the
///   config's own value.
/// - `listen` (from `--metrics-listen`) overrides the listen address.
///
/// When neither is set the config is left untouched.
fn apply_metrics_override(cfg: &mut GatewayConfig, force_on: bool, listen: Option<&str>) {
    if !force_on && listen.is_none() {
        return;
    }
    let top = cfg.x_spvirit.get_or_insert_with(TopExt::default);
    let m = top.metrics.get_or_insert_with(MetricsExt::default);
    if force_on {
        m.enabled = true;
    }
    if let Some(l) = listen {
        m.listen = l.to_string();
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let opts = match parse_cli(&args) {
        Ok(o) => o,
        // argparse already printed usage/help/error; `0` means `--help`.
        Err(0) => return ExitCode::SUCCESS,
        Err(_) => return ExitCode::FAILURE,
    };

    let discovery_override =
        match discovery_override_from_flags(opts.discovery_parity, opts.no_discovery_parity) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        };

    if opts.path.is_empty() {
        eprintln!(
            "usage: spgateway [-T|--test-config] [-v|--verbose] \
             [--discovery-parity|--no-discovery-parity] \
             [--metrics] [--metrics-listen ADDR] <config.json>"
        );
        return ExitCode::FAILURE;
    }

    let metrics_listen = if opts.metrics_listen.is_empty() {
        None
    } else {
        Some(opts.metrics_listen.as_str())
    };

    let contents = match std::fs::read_to_string(&opts.path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to read config '{}': {e}", opts.path);
            return ExitCode::FAILURE;
        }
    };

    if opts.test_config {
        let result = GatewayConfig::from_json_str(&contents).and_then(|mut cfg| {
            apply_discovery_override(&mut cfg, discovery_override);
            apply_metrics_override(&mut cfg, opts.metrics, metrics_listen);
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
        .with_max_level(if opts.verbose {
            Level::DEBUG
        } else {
            Level::INFO
        })
        .init();

    let mut cfg = match GatewayConfig::from_json_str(&contents) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // CLI overrides JSON for all servers.
    apply_discovery_override(&mut cfg, discovery_override);
    apply_metrics_override(&mut cfg, opts.metrics, metrics_listen);

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
    use super::{
        apply_discovery_override, apply_metrics_override, discovery_override_from_flags, parse_cli,
    };
    use spvirit_gateway::config::GatewayConfig;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── discovery-parity mapping (pure) ────────────────────────────────────

    #[test]
    fn no_flag_yields_none() {
        assert_eq!(discovery_override_from_flags(false, false).unwrap(), None);
    }

    #[test]
    fn no_discovery_parity_yields_some_false() {
        assert_eq!(
            discovery_override_from_flags(false, true).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn discovery_parity_yields_some_true() {
        assert_eq!(
            discovery_override_from_flags(true, false).unwrap(),
            Some(true)
        );
    }

    #[test]
    fn both_flags_error() {
        assert!(discovery_override_from_flags(true, true).is_err());
    }

    #[test]
    fn override_applies_to_all_servers() {
        let json = r#"{ "version":2, "clients":[], "servers":[
            { "name":"a","clients":[] },
            { "name":"b","clients":[], "discovery_parity": true }
        ]}"#;
        let mut cfg = GatewayConfig::from_json_str(json).unwrap();
        apply_discovery_override(&mut cfg, Some(false));
        assert!(cfg.servers.iter().all(|s| !s.discovery_parity));
    }

    // ── argparse CLI parsing ───────────────────────────────────────────────

    #[test]
    fn parses_positional_config_only() {
        let o = parse_cli(&args(&["cfg.json"])).unwrap();
        assert_eq!(o.path, "cfg.json");
        assert!(!o.test_config);
        assert!(!o.metrics);
        assert!(o.metrics_listen.is_empty());
    }

    #[test]
    fn parses_test_config_flag() {
        let o = parse_cli(&args(&["-T", "cfg.json"])).unwrap();
        assert!(o.test_config);
        assert_eq!(o.path, "cfg.json");
    }

    #[test]
    fn metrics_flag_sets_bool() {
        let o = parse_cli(&args(&["--metrics", "cfg.json"])).unwrap();
        assert!(o.metrics);
        assert_eq!(o.path, "cfg.json");
    }

    #[test]
    fn metrics_listen_sets_addr() {
        let o = parse_cli(&args(&["--metrics-listen", "127.0.0.1:9110", "cfg.json"])).unwrap();
        assert_eq!(o.metrics_listen, "127.0.0.1:9110");
        assert!(!o.metrics); // listen alone does not force enable
        assert_eq!(o.path, "cfg.json");
    }

    #[test]
    fn metrics_flags_combine_with_test_config() {
        let o = parse_cli(&args(&[
            "-T",
            "--metrics",
            "--metrics-listen",
            "127.0.0.1:0",
            "cfg.json",
        ]))
        .unwrap();
        assert!(o.test_config);
        assert!(o.metrics);
        assert_eq!(o.metrics_listen, "127.0.0.1:0");
        assert_eq!(o.path, "cfg.json");
    }

    #[test]
    fn metrics_listen_without_value_is_usage_error() {
        // argparse reports the missing value as a non-zero exit code.
        assert!(parse_cli(&args(&["--metrics-listen"])).is_err());
    }

    // ── metrics override application ────────────────────────────────────────

    #[test]
    fn metrics_override_synthesizes_block_when_absent() {
        let mut cfg =
            GatewayConfig::from_json_str(r#"{ "version":2, "clients":[], "servers":[] }"#).unwrap();
        assert!(cfg.x_spvirit.is_none());
        apply_metrics_override(&mut cfg, true, Some("127.0.0.1:9110"));
        let m = cfg.x_spvirit.unwrap().metrics.unwrap();
        assert!(m.enabled);
        assert_eq!(m.listen, "127.0.0.1:9110");
        assert_eq!(m.path, "/metrics"); // default preserved
    }

    #[test]
    fn metrics_flag_forces_enabled_over_config_false() {
        let json = r#"{ "version":2, "clients":[], "servers":[],
            "x-spvirit": { "metrics": { "enabled": false, "listen": "0.0.0.0:9090" } } }"#;
        let mut cfg = GatewayConfig::from_json_str(json).unwrap();
        apply_metrics_override(&mut cfg, true, None);
        let m = cfg.x_spvirit.unwrap().metrics.unwrap();
        assert!(m.enabled);
        assert_eq!(m.listen, "0.0.0.0:9090"); // config listen kept
    }

    #[test]
    fn metrics_listen_overrides_config_listen_without_forcing_enable() {
        let json = r#"{ "version":2, "clients":[], "servers":[],
            "x-spvirit": { "metrics": { "enabled": true, "listen": "0.0.0.0:9090" } } }"#;
        let mut cfg = GatewayConfig::from_json_str(json).unwrap();
        apply_metrics_override(&mut cfg, false, Some("127.0.0.1:9110"));
        let m = cfg.x_spvirit.unwrap().metrics.unwrap();
        assert!(m.enabled); // untouched
        assert_eq!(m.listen, "127.0.0.1:9110");
    }

    #[test]
    fn no_metrics_flags_leaves_config_untouched() {
        let mut cfg =
            GatewayConfig::from_json_str(r#"{ "version":2, "clients":[], "servers":[] }"#).unwrap();
        apply_metrics_override(&mut cfg, false, None);
        assert!(cfg.x_spvirit.is_none());
    }
}
