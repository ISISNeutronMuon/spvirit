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
///
/// The call can also be spread across several lines (e.g. `.send_msg(\n  ...`
/// or `.\n send_msg(`), so this collapses all whitespace out of the source
/// before matching rather than scanning line-by-line — a line-oriented scan
/// would silently miss a wrapped call.
#[test]
fn handler_does_not_reply_through_the_global_conns_map() {
    let src = handler_src();
    let collapsed: String = src.chars().filter(|c| !c.is_whitespace()).collect();

    assert!(
        !collapsed.contains(".send_msg("),
        "handler.rs still contains a `.send_msg(` call (possibly spanning \
         multiple lines). One-shot replies must go through the connection's \
         Replier, not MonitorRegistry::send_msg, which locks the global conns \
         map for a writer the connection already holds."
    );
}
