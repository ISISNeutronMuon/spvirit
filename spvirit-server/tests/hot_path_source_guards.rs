//! Source-level guards for hot-path invariants that have no runtime
//! observable. Precedent: `tests/example_sources_keep_full_senders.rs`.

use std::fs;
use std::path::Path;

fn handler_src() -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/handler.rs"))
        .expect("handler.rs must be readable")
}

/// Every one-shot reply used to call `registry.send_msg(conn_id, ..)`, which
/// locks the process-global `conns` map to look up a writer the connection
/// task created itself and could simply have kept. That is a global lock
/// acquisition per reply, serializing every connection on the server against
/// every other.
#[test]
fn handler_does_not_reply_through_the_global_conns_map() {
    let src = handler_src();
    let offenders: Vec<String> = src
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains("registry.send_msg(conn_id"))
        .map(|(i, l)| format!("handler.rs:{}: {}", i + 1, l.trim()))
        .collect();

    assert!(
        offenders.is_empty(),
        "these replies still go through MonitorRegistry::send_msg, which locks \
         the global conns map for a writer the connection already holds. Use \
         the connection's Replier instead.\n  {}",
        offenders.join("\n  ")
    );
}
