use std::io::BufRead;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::harness::{
    LocalServerFixture, ProcessGuard, contains_any, env_enabled, run_command_success,
    tool_path_from_base,
};

#[test]
fn epics_base_cli_interop_smoke_optional() {
    if !env_enabled("RUN_EPICS_INTEROP") {
        eprintln!("Skipping interop test: set RUN_EPICS_INTEROP=1");
        return;
    }

    let Some(pvget_bin) = tool_path_from_base("EPICS_BASE_BIN", "pvget") else {
        eprintln!("Skipping interop test: EPICS_BASE_BIN/pvget not found");
        return;
    };
    let Some(pvput_bin) = tool_path_from_base("EPICS_BASE_BIN", "pvput") else {
        eprintln!("Skipping interop test: EPICS_BASE_BIN/pvput not found");
        return;
    };
    let Some(pvmonitor_bin) = tool_path_from_base("EPICS_BASE_BIN", "pvmonitor") else {
        eprintln!("Skipping interop test: EPICS_BASE_BIN/pvmonitor not found");
        return;
    };
    let Some(pvinfo_bin) = tool_path_from_base("EPICS_BASE_BIN", "pvinfo") else {
        eprintln!("Skipping interop test: EPICS_BASE_BIN/pvinfo not found");
        return;
    };
    let Some(pvlist_bin) = tool_path_from_base("EPICS_BASE_BIN", "pvlist") else {
        eprintln!("Skipping interop test: EPICS_BASE_BIN/pvlist not found");
        return;
    };

    let prefix = format!("INT:{}", std::process::id());
    let ai_pv = format!("{}:AI", prefix);
    let ao_pv = format!("{}:AO", prefix);
    let db_contents = format!(
        r#"
record(ai, "{}") {{
    field(VAL, "1.25")
}}
record(ao, "{}") {{
    field(VAL, "2.50")
}}
"#,
        ai_pv, ao_pv
    );

    let server = match LocalServerFixture::spawn(&db_contents, &["--pvlist-mode", "list"]) {
        Ok(fixture) => fixture,
        Err(message) => {
            eprintln!("Skipping interop test: {}", message);
            return;
        }
    };
    let common_env = server.epics_env();

    let mut get_before = Command::new(&pvget_bin);
    get_before
        .arg(&ao_pv)
        .envs(common_env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let get_before = run_command_success(&mut get_before, "epics pvget before")
        .expect("pvget before should succeed");
    let before_text = String::from_utf8_lossy(&get_before.stdout);
    assert!(
        contains_any(&before_text, &["2.5", "2.50"]),
        "unexpected initial pvget output: {}",
        before_text
    );

    let mut put = Command::new(&pvput_bin);
    put.arg(&ao_pv)
        .arg("6.78")
        .envs(common_env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_command_success(&mut put, "epics pvput").expect("pvput should succeed");

    let mut get_after = Command::new(&pvget_bin);
    get_after
        .arg(&ao_pv)
        .envs(common_env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let get_after =
        run_command_success(&mut get_after, "epics pvget after").expect("pvget after ok");
    let after_text = String::from_utf8_lossy(&get_after.stdout);
    assert!(
        after_text.contains("6.78"),
        "unexpected updated pvget output: {}",
        after_text
    );

    let mut pvinfo = Command::new(&pvinfo_bin);
    pvinfo
        .arg(&ai_pv)
        .envs(common_env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let pvinfo_output = run_command_success(&mut pvinfo, "epics pvinfo").expect("pvinfo ok");
    let pvinfo_text = String::from_utf8_lossy(&pvinfo_output.stdout);
    assert!(
        contains_any(&pvinfo_text, &[&ai_pv, "value"]),
        "unexpected pvinfo output: {}",
        pvinfo_text
    );

    let mut pvlist = Command::new(&pvlist_bin);
    let list_pattern = format!("{}:*", prefix);
    pvlist
        .arg(&list_pattern)
        .envs(common_env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let pvlist_output = run_command_success(&mut pvlist, "epics pvlist").expect("pvlist ok");
    let pvlist_text = String::from_utf8_lossy(&pvlist_output.stdout);
    assert!(
        pvlist_text.contains(&ai_pv) || pvlist_text.contains(&ao_pv),
        "unexpected pvlist output: {}",
        pvlist_text
    );

    let mut monitor_cmd = Command::new(&pvmonitor_bin);
    monitor_cmd
        .arg(&ao_pv)
        .envs(common_env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut monitor =
        ProcessGuard::spawn(&mut monitor_cmd, "epics pvmonitor").expect("pvmonitor should spawn");
    std::thread::sleep(Duration::from_millis(250));

    let mut bump = Command::new(&pvput_bin);
    bump.arg(&ao_pv)
        .arg("7.89")
        .envs(common_env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_command_success(&mut bump, "epics pvput bump").expect("pvput bump should succeed");

    std::thread::sleep(Duration::from_millis(450));
    monitor.kill_and_wait();
    let monitor_output = monitor
        .child_mut()
        .stdout
        .take()
        .map(|stdout| {
            let reader = std::io::BufReader::new(stdout);
            let lines: Vec<String> = reader.lines().flatten().collect();
            lines.join("\n")
        })
        .unwrap_or_default();
    assert!(
        contains_any(&monitor_output, &[&ao_pv, "7.89", "6.78"]),
        "unexpected pvmonitor output: {}",
        monitor_output
    );
}

#[test]
fn epics_base_pvmonitor_nested_pvrequest_filters_fields_optional() {
    if !env_enabled("RUN_EPICS_INTEROP") {
        eprintln!("Skipping interop test: set RUN_EPICS_INTEROP=1");
        return;
    }

    let Some(pvget_bin) = tool_path_from_base("EPICS_BASE_BIN", "pvget") else {
        eprintln!("Skipping interop test: EPICS_BASE_BIN/pvget not found");
        return;
    };
    let Some(pvput_bin) = tool_path_from_base("EPICS_BASE_BIN", "pvput") else {
        eprintln!("Skipping interop test: EPICS_BASE_BIN/pvput not found");
        return;
    };
    let Some(pvmonitor_bin) = tool_path_from_base("EPICS_BASE_BIN", "pvmonitor") else {
        eprintln!("Skipping interop test: EPICS_BASE_BIN/pvmonitor not found");
        return;
    };

    let prefix = format!("INT:FLD:{}", std::process::id());
    let ao_pv = format!("{}:AO", prefix);
    let db_contents = format!(
        r#"
record(ao, "{}") {{
    field(VAL, "0.0")
}}
"#,
        ao_pv
    );

    let server = match LocalServerFixture::spawn(&db_contents, &["--pvlist-mode", "list"]) {
        Ok(fixture) => fixture,
        Err(message) => {
            eprintln!("Skipping interop test: {}", message);
            return;
        }
    };
    let common_env = server.epics_env();

    // Filtered GET: request only alarm.severity via `-r field(alarm.severity)`.
    let mut get_filtered = Command::new(&pvget_bin);
    get_filtered
        .args(["-r", "field(alarm.severity)"])
        .arg(&ao_pv)
        .envs(common_env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let get_filtered =
        run_command_success(&mut get_filtered, "epics pvget filtered").expect("pvget filtered");
    let get_filtered_text = String::from_utf8_lossy(&get_filtered.stdout);
    assert!(
        get_filtered_text.contains("severity"),
        "pvget -r field(alarm.severity) output missing severity: {}",
        get_filtered_text
    );
    assert!(
        !get_filtered_text.contains("timeStamp"),
        "pvget -r field(alarm.severity) unexpectedly returned timeStamp: {}",
        get_filtered_text
    );

    // Filtered MONITOR: updates to value should NOT produce frames (our server
    // suppresses deltas whose filtered view is unchanged).
    let mut monitor_cmd = Command::new(&pvmonitor_bin);
    monitor_cmd
        .args(["-r", "field(alarm.severity)"])
        .arg(&ao_pv)
        .envs(common_env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut monitor = ProcessGuard::spawn(&mut monitor_cmd, "epics pvmonitor filtered")
        .expect("pvmonitor filtered should spawn");
    std::thread::sleep(Duration::from_millis(300));

    for val in ["1.0", "2.0", "3.0"] {
        let mut put = Command::new(&pvput_bin);
        put.arg(&ao_pv)
            .arg(val)
            .envs(common_env.iter().map(|(key, value)| (key, value)))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        run_command_success(&mut put, "epics pvput (filtered monitor)").expect("pvput ok");
    }

    std::thread::sleep(Duration::from_millis(500));
    monitor.kill_and_wait();
    let monitor_output = monitor
        .child_mut()
        .stdout
        .take()
        .map(|stdout| {
            let reader = std::io::BufReader::new(stdout);
            let lines: Vec<String> = reader.lines().flatten().collect();
            lines.join("\n")
        })
        .unwrap_or_default();
    // First frame is the full filtered projection (contains severity). Any
    // subsequent frame must not leak the `value` field, since the client
    // only requested alarm.severity.
    assert!(
        !monitor_output.contains("timeStamp"),
        "pvmonitor -r field(alarm.severity) leaked timeStamp: {}",
        monitor_output
    );
}
