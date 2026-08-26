//! `spgateway` — p4p-compatible PVAccess gateway CLI entry point.
//!
//! Usage:
//!   spgateway <config.json>
//!   spgateway -T <config.json>
//!   spgateway --test-config <config.json>
//!
//! `-T`/`--test-config` parses and validates the given configuration file,
//! printing `OK` and exiting 0 on success, or printing the error and exiting
//! 1 on failure. Without it, M1 does not yet wire up the runtime (see Task
//! 14) — the gateway just reports that and exits 0.

use std::process::ExitCode;

use spvirit_gateway::config::GatewayConfig;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let test_config = args.iter().any(|a| a == "-T" || a == "--test-config");
    let path = args.iter().find(|a| *a != "-T" && *a != "--test-config");

    let Some(path) = path else {
        eprintln!("usage: spgateway [-T|--test-config] <config.json>");
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

    // M1: the runtime is not wired up yet (see Task 14); just acknowledge the
    // config path was given and exit cleanly.
    let _ = contents;
    println!("spgateway: run path not yet wired (see Task 14)");
    ExitCode::SUCCESS
}
