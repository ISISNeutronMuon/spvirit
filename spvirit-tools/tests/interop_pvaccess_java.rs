#[path = "interop/harness.rs"]
mod harness;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use harness::{
    LocalServerFixture, ProcessGuard, env_enabled, free_tcp_port, free_udp_port,
    run_command_success_with_timeout, workspace_bin,
};

/// Locate the java-interop Gradle project root (relative to the workspace).
fn java_interop_dir() -> PathBuf {
    let test_exe = std::env::current_exe().expect("cannot locate test executable");
    // test exe lives in target/debug/deps/ — walk up to workspace root
    let workspace = test_exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    workspace.join("tests").join("java-interop")
}

/// Check that Gradle wrapper or system gradle is available and return the
/// command name to invoke.
fn gradle_cmd(project_dir: &Path) -> Option<String> {
    // Prefer gradlew in the project dir
    let gradlew = if cfg!(windows) {
        project_dir.join("gradlew.bat")
    } else {
        project_dir.join("gradlew")
    };
    if gradlew.exists() {
        return Some(gradlew.to_string_lossy().to_string());
    }

    // Fall back to system gradle
    let name = if cfg!(windows) {
        "gradle.bat"
    } else {
        "gradle"
    };
    let check = Command::new(name).arg("--version").output();
    if let Ok(output) = check
        && output.status.success() {
            return Some(name.to_string());
        }
    None
}

/// Build the Java interop project (once). Returns the gradle command name on success.
fn ensure_java_built(project_dir: &Path) -> Result<String, String> {
    let gradle = gradle_cmd(project_dir).ok_or_else(|| {
        "gradle not found (no gradlew in tests/java-interop and no system gradle)".to_string()
    })?;

    let mut build_cmd = Command::new(&gradle);
    build_cmd
        .arg("--project-dir")
        .arg(project_dir)
        .arg("classes")
        .arg("--no-daemon")
        .arg("-q")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    run_command_success_with_timeout(&mut build_cmd, "gradle classes", Duration::from_secs(120))?;
    Ok(gradle)
}

/// Run the Java interop CLI with the given arguments and return stdout.
fn run_java_interop(
    gradle: &str,
    project_dir: &Path,
    server_addr: &str,
    pv_name: &str,
    tests: &[&str],
    env: &[(String, String)],
) -> Result<String, String> {
    let mut args_str = format!("{} {}", server_addr, pv_name);
    for t in tests {
        args_str.push(' ');
        args_str.push_str(t);
    }

    let mut cmd = Command::new(gradle);
    cmd.arg("--project-dir")
        .arg(project_dir)
        .arg("run")
        .arg("--no-daemon")
        .arg("-q")
        .arg(format!("--args={}", args_str))
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output =
        run_command_success_with_timeout(&mut cmd, "java interop", Duration::from_secs(60))?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn java_pvaccess_scalar_interop() {
    if !env_enabled("RUN_JAVA_INTEROP") {
        eprintln!("Skipping Java interop test: set RUN_JAVA_INTEROP=1");
        return;
    }

    let project_dir = java_interop_dir();
    if !project_dir.join("build.gradle").exists() {
        eprintln!(
            "Skipping Java interop: {} does not exist",
            project_dir.display()
        );
        return;
    }

    let gradle = match ensure_java_built(&project_dir) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("Skipping Java interop: {}", e);
            return;
        }
    };

    let prefix = format!("JINT:{}", std::process::id());
    let ao_pv = format!("{}:AO", prefix);
    let db = format!(
        r#"
record(ao, "{}") {{
    field(VAL, "3.14")
}}
"#,
        ao_pv
    );

    let server =
        LocalServerFixture::spawn(&db, &["--pvlist-mode", "list"]).expect("spawn spvirit_server");
    let env = server.epics_env();

    // beacon_connect
    let out = run_java_interop(
        &gradle,
        &project_dir,
        &server.server_addr(),
        &ao_pv,
        &["beacon_connect"],
        &env,
    )
    .expect("beacon_connect should succeed");
    eprintln!("[java beacon_connect] {}", out);
    assert!(out.contains("PASS"), "beacon_connect did not PASS: {}", out);

    // scalar_get
    let out = run_java_interop(
        &gradle,
        &project_dir,
        &server.server_addr(),
        &ao_pv,
        &["scalar_get"],
        &env,
    )
    .expect("scalar_get should succeed");
    eprintln!("[java scalar_get] {}", out);
    assert!(out.contains("PASS"), "scalar_get did not PASS: {}", out);

    // scalar_put_get
    let out = run_java_interop(
        &gradle,
        &project_dir,
        &server.server_addr(),
        &ao_pv,
        &["scalar_put_get"],
        &env,
    )
    .expect("scalar_put_get should succeed");
    eprintln!("[java scalar_put_get] {}", out);
    assert!(out.contains("PASS"), "scalar_put_get did not PASS: {}", out);

    // scalar_monitor
    let out = run_java_interop(
        &gradle,
        &project_dir,
        &server.server_addr(),
        &ao_pv,
        &["scalar_monitor"],
        &env,
    )
    .expect("scalar_monitor should succeed");
    eprintln!("[java scalar_monitor] {}", out);
    assert!(out.contains("PASS"), "scalar_monitor did not PASS: {}", out);
}

#[test]
fn java_pvaccess_ndarray_interop() {
    if !env_enabled("RUN_JAVA_INTEROP") {
        eprintln!("Skipping Java NTNDArray interop test: set RUN_JAVA_INTEROP=1");
        return;
    }

    let project_dir = java_interop_dir();
    if !project_dir.join("build.gradle").exists() {
        eprintln!(
            "Skipping Java interop: {} does not exist",
            project_dir.display()
        );
        return;
    }

    let gradle = match ensure_java_built(&project_dir) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("Skipping Java interop (ndarray): {}", e);
            return;
        }
    };

    // Spawn pvdodeca on its own ports
    let tcp_port = free_tcp_port().expect("free TCP port for pvdodeca");
    let udp_port = free_udp_port().expect("free UDP port for pvdodeca");
    let dodeca_bin = workspace_bin("spdodeca");
    let pv_name = format!("DODECA:JTEST:{}", std::process::id());

    let mut dodeca_cmd = Command::new(&dodeca_bin);
    dodeca_cmd
        .arg("--pv")
        .arg(&pv_name)
        .arg("--listen-addr")
        .arg("127.0.0.1")
        .arg("--tcp-port")
        .arg(tcp_port.to_string())
        .arg("--udp-port")
        .arg(udp_port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let _dodeca =
        ProcessGuard::spawn(&mut dodeca_cmd, "spawn pvdodeca").expect("pvdodeca should spawn");
    std::thread::sleep(std::time::Duration::from_millis(600));

    let env = vec![
        ("EPICS_PVA_ADDR_LIST".to_string(), "127.0.0.1".to_string()),
        ("EPICS_PVA_AUTO_ADDR_LIST".to_string(), "NO".to_string()),
        ("EPICS_PVA_BROADCAST_PORT".to_string(), udp_port.to_string()),
        ("EPICS_PVA_CONN_TMO".to_string(), "5".to_string()),
    ];

    let server_addr = format!("127.0.0.1:{}", tcp_port);

    // ndarray_get
    let out = run_java_interop(
        &gradle,
        &project_dir,
        &server_addr,
        &pv_name,
        &["ndarray_get"],
        &env,
    )
    .expect("ndarray_get should succeed");
    eprintln!("[java ndarray_get] {}", out);
    assert!(out.contains("PASS"), "ndarray_get did not PASS: {}", out);

    // ndarray_monitor
    let out = run_java_interop(
        &gradle,
        &project_dir,
        &server_addr,
        &pv_name,
        &["ndarray_monitor"],
        &env,
    )
    .expect("ndarray_monitor should succeed");
    eprintln!("[java ndarray_monitor] {}", out);
    assert!(
        out.contains("PASS"),
        "ndarray_monitor did not PASS: {}",
        out
    );
}
