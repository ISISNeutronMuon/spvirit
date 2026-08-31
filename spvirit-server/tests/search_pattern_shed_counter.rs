//! V6 LOW-3: shedding must move `pattern_enum_shed`, once per shed query.
//!
//! **One test per file, deliberately.** The counter is process-global and
//! monotonic. In its previous home (`search_pattern_shed.rs`) this assertion
//! was a `>=` delta sitting beside two sibling tests that also shed, so a
//! concurrent sibling could satisfy it on this test's behalf. It passed alone,
//! which is why it survived review — but this branch has already shipped one
//! test that passed only because a sibling moved a global, so "passes alone
//! today" is not the standard.
//!
//! Cargo gives each `tests/*.rs` file its own binary and therefore its own
//! process. Alone in this file, nothing else in the process can touch the
//! counter, which is what lets the assertion below be an **exact equality**
//! rather than a lower bound: not just "at least three sheds happened
//! somewhere", but "these three queries were shed, and nothing else was".

mod shed_common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use shed_common::{HangingNames, OneName, UdpHarness, await_shed_count};
use spvirit_server::handler::PATTERN_ENUM_CONCURRENCY;
use spvirit_server::pvstore::Source;
use spvirit_server::search_resolve::global_stats;

const SERVED: &str = "SHED:SERVED";

/// Shedding is silent on the wire by design, so the counter is the only trace
/// it leaves. Without it a sustained wildcard flood — or one upstream hung in
/// `names()` holding every permit — is undetectable in production.
#[tokio::test]
async fn shedding_increments_the_counter_once_per_shed_query() {
    let name_calls = Arc::new(AtomicUsize::new(0));
    let one: Arc<dyn Source> = Arc::new(OneName(SERVED));
    let hanging: Arc<dyn Source> = Arc::new(HangingNames {
        name_calls: name_calls.clone(),
    });
    let h = UdpHarness::start(vec![("one", one), ("hanging", hanging)]).await;

    let baseline = global_stats().pattern_enum_shed;

    // Pin every permit with wildcards whose `names()` never returns.
    for i in 0..PATTERN_ENUM_CONCURRENCY {
        h.search(500 + i as u32, 500 + i as u32, "SHED:*").await;
    }
    for _ in 0..200 {
        if name_calls.load(Ordering::SeqCst) >= PATTERN_ENUM_CONCURRENCY {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        name_calls.load(Ordering::SeqCst),
        PATTERN_ENUM_CONCURRENCY,
        "the permits were never saturated, so nothing below would be shed and \
         the test would prove nothing"
    );
    assert_eq!(
        global_stats().pattern_enum_shed,
        baseline,
        "saturating the permits shed something by itself; the count below \
         would then not be attributable to the queries that follow"
    );

    // Every one of these must now find the cap saturated.
    const EXTRA: u64 = 3;
    for i in 0..EXTRA {
        h.search(700 + i as u32, 700 + i as u32, "SHED:*").await;
    }

    let want = baseline + EXTRA;
    let got = await_shed_count(want).await;
    assert_eq!(
        got, want,
        "{EXTRA} pattern queries were shed but `pattern_enum_shed` went from \
         {baseline} to {got}; this process sheds for exactly one reason, so \
         any other value means shedding is either not counted or double-counted \
         — and shedding is invisible to /metrics without it"
    );

    // Give any stray reply time to arrive, then confirm the counter did not
    // drift: nothing else in this process can move it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        global_stats().pattern_enum_shed,
        want,
        "the counter kept moving after the shed queries were accounted for"
    );
}
