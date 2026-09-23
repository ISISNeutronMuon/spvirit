//! Behavioral test for `spgateway -T` (`--test-config`): the fail-closed
//! config validation path must actually reject a bad config at the CLI
//! boundary, not just inside `spvirit-gateway`'s unit tests.
//!
//! No `assert_cmd`/`tempfile` dev-dependency exists in this crate yet, so
//! this spawns the built binary directly via `std::process::Command` and
//! `env!("CARGO_BIN_EXE_spgateway")` (Cargo builds the binary before running
//! this test and hands us its path), and writes the scratch config into a
//! unique subdirectory of `std::env::temp_dir()`.

use std::path::PathBuf;
use std::process::Command;

/// A unique, self-cleaning temp directory.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("spgateway-test-config-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `spgateway -T` exits non-zero and reports the bad pvlist path when a
/// server's `pvlist` file does not exist — the fail-closed load added in
/// Task 10 (`GatewayConfig::validate`).
#[test]
fn test_config_rejects_missing_pvlist_file() {
    let dir = TempDir::new();
    let config_path = dir.path().join("gateway.json");
    std::fs::write(
        &config_path,
        r#"{ "version": 2,
             "clients": [{ "name": "c", "provider": "pva" }],
             "servers": [{ "name": "s", "clients": ["c"], "pvlist": "/no/such/pvlist.acf" }] }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_spgateway"))
        .arg("-T")
        .arg(&config_path)
        .output()
        .expect("spawn spgateway");

    assert!(
        !output.status.success(),
        "spgateway -T should fail on a missing pvlist file; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // `-T` reports failures on stderr (stdout carries `OK` alone).
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("pvlist"),
        "expected the error to name the offending pvlist file, got: {combined}"
    );
}

/// The mirror-image happy path: a config with a `pva` provider and no
/// access/pvlist files passes `-T` and exits 0.
#[test]
fn test_config_accepts_minimal_valid_config() {
    let dir = TempDir::new();
    let config_path = dir.path().join("gateway.json");
    std::fs::write(
        &config_path,
        r#"{ "version": 2,
             "clients": [{ "name": "c", "provider": "pva" }],
             "servers": [{ "name": "s", "clients": ["c"] }] }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_spgateway"))
        .arg("-T")
        .arg(&config_path)
        .output()
        .expect("spawn spgateway");

    assert!(
        output.status.success(),
        "spgateway -T should accept a minimal valid config; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A client with a non-`pva` provider must be rejected fail-closed.
#[test]
fn test_config_rejects_non_pva_provider() {
    let dir = TempDir::new();
    let config_path = dir.path().join("gateway.json");
    std::fs::write(
        &config_path,
        r#"{ "version": 2,
             "clients": [{ "name": "c", "provider": "ca" }],
             "servers": [{ "name": "s", "clients": ["c"] }] }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_spgateway"))
        .arg("-T")
        .arg(&config_path)
        .output()
        .expect("spawn spgateway");

    assert!(
        !output.status.success(),
        "spgateway -T should reject a non-pva provider; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("provider"),
        "expected the error to mention 'provider', got: {combined}"
    );
}

/// The minimal config that passes `-T`: one `pva` client, one server
/// referencing it, no access/pvlist files.
const MINIMAL_VALID_CONFIG: &str = r#"{ "version": 2,
     "clients": [{ "name": "c", "provider": "pva" }],
     "servers": [{ "name": "s", "clients": ["c"] }] }"#;

/// `-T` is the machine-readable path: stdout carries `OK` and nothing else,
/// so a caller can redirect it. Every other error path in the binary already
/// uses stderr; this one printed to stdout, so
/// `spgateway -T cfg.json > result.txt` captured the failure text as if it
/// were the result, and `2>/dev/null` swallowed nothing at all.
#[test]
fn test_config_reports_failures_on_stderr_and_keeps_stdout_clean() {
    let dir = TempDir::new();
    let cfg = dir.path().join("bad.json");
    std::fs::write(&cfg, r#"{"version": 2, "clients": [], "servers": [{"name": "s", "clients": ["nope"]}]}"#).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_spgateway"))
        .arg("-T")
        .arg(&cfg)
        .output()
        .expect("spgateway runs");

    assert!(!out.status.success(), "an invalid config must exit non-zero");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.trim().is_empty(), "stdout must stay clean on failure, got: {stdout}");
    assert!(stderr.contains("nope"), "the error must reach stderr, got: {stderr}");
}

/// The success path is unchanged and stays the contract: `OK` on stdout.
#[test]
fn test_config_success_still_prints_ok_on_stdout() {
    let dir = TempDir::new();
    let cfg = dir.path().join("good.json");
    std::fs::write(&cfg, MINIMAL_VALID_CONFIG).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_spgateway"))
        .arg("-T")
        .arg(&cfg)
        .output()
        .expect("spgateway runs");

    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "OK");
}

/// An explicitly empty `--metrics-listen` used to be indistinguishable from
/// omitting the flag — the parsed value was the empty string either way — so
/// an unset shell variable expanding to nothing silently handed the operator
/// the default listen address.
#[test]
fn an_explicitly_empty_metrics_listen_is_a_usage_error() {
    let dir = TempDir::new();
    let cfg = dir.path().join("good.json");
    std::fs::write(&cfg, MINIMAL_VALID_CONFIG).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_spgateway"))
        .arg("-T")
        .arg("--metrics-listen")
        .arg("")
        .arg(&cfg)
        .output()
        .expect("spgateway runs");

    assert!(!out.status.success(), "an empty --metrics-listen must be rejected");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--metrics-listen"),
        "the error must name the flag"
    );
}
