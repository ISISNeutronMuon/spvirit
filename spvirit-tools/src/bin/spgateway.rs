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

use std::process::ExitCode;

use tracing::Level;

use spvirit_gateway::config::GatewayConfig;
use spvirit_gateway::runtime::Runtime;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let test_config = args.iter().any(|a| a == "-T" || a == "--test-config");
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
    let path = args
        .iter()
        .find(|a| !matches!(a.as_str(), "-T" | "--test-config" | "-v" | "--verbose"));

    let Some(path) = path else {
        eprintln!("usage: spgateway [-T|--test-config] [-v|--verbose] <config.json>");
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
        return match GatewayConfig::from_json_str(&contents).and_then(|cfg| cfg.validate()) {
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

    let cfg = match GatewayConfig::from_json_str(&contents) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

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
