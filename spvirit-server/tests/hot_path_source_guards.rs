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

/// The spawned pattern-query-reply task (`spawn_pattern_query_reply`, driven
/// from the TCP search handler) can run for up to 30s after the connection
/// that asked for it has torn down. Its closure must capture the deferred
/// reply through `replier.downgrade()` (a `Weak<ConnWriter>`) rather than
/// `replier.clone()` (a strong `Replier`, which keeps the connection's
/// `ConnWriter` — and its buffers — resident for the closure's whole
/// lifetime regardless of teardown).
///
/// This is a wiring assertion, not a behavioural one: `WeakReplier` itself
/// is exercised elsewhere, but nothing short of reading the source proves
/// the *search-reply closure specifically* uses it, since any correctly
/// behaving `Weak` would pass a generic "does downgrade work" test even if
/// the production call site had been reverted to a strong clone.
#[test]
fn deferred_search_reply_captures_a_weak_replier_not_a_strong_one() {
    let src = handler_src();
    let collapsed: String = src.chars().filter(|c| !c.is_whitespace()).collect();

    assert!(
        collapsed.contains("letreplier_for_task=replier.downgrade();"),
        "handler.rs no longer wires the deferred search-reply closure through \
         `replier.downgrade()`. The spawned task must capture a weak handle, \
         not a strong one, or it will keep the connection's ConnWriter alive \
         for up to 30s after teardown."
    );
    assert!(
        !collapsed.contains("letreplier_for_task=replier.clone();"),
        "handler.rs wires the deferred search-reply closure through \
         `replier.clone()`, a strong Replier. This keeps the connection's \
         ConnWriter (and its buffers) alive for as long as the pattern-query \
         enumeration runs (up to 30s), even after the connection has torn \
         down. Use `replier.downgrade()` instead."
    );
}

/// `Events::drain` polled `inflight` in a yield_now loop, rescheduling itself
/// on every tick for up to ten seconds. It must wait on a notification.
#[test]
fn events_drain_does_not_busy_spin() {
    let src = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/events.rs"))
        .expect("events.rs must be readable");
    let start = src
        .find("pub async fn drain(&self)")
        .expect("Events::drain must exist");
    let end = src[start..]
        .find("\n    }\n")
        .map(|e| start + e)
        .unwrap_or(src.len());
    let body = &src[start..end];

    assert!(
        !body.contains("yield_now"),
        "Events::drain must wait on a Notify, not spin with yield_now"
    );
    assert!(
        body.contains("notified()"),
        "Events::drain must await the idle notification"
    );
}
