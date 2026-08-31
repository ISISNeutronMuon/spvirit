//! V5 LOW-1: a panicking `names()` must count as a shed.
//!
//! **One test per file, deliberately.** `pattern_enum_shed` is process-global,
//! so any sibling test in the same binary that sheds can satisfy this test's
//! delta on its behalf. That is not hypothetical — the first draft of this
//! test lived beside the paused-clock timeout test and passed against a server
//! that did not count panics at all. Cargo gives each `tests/*.rs` file its own
//! process, so alone in this file the counter moves for exactly one reason.

mod shed_common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use shed_common::{PanickingNames, UdpHarness, await_shed_count};
use spvirit_server::handler::PATTERN_ENUM_CONCURRENCY;
use spvirit_server::pvstore::Source;
use spvirit_server::search_resolve::global_stats;

/// A source whose `names()` panics leaves its pattern query unanswered — the
/// most confusing silence a server can produce, because nothing else about it
/// looks unhealthy. The permit comes back by RAII, but before this fix neither
/// arm of the `match` on `timeout` ran, so `pattern_enum_shed` stayed flat and
/// the operator's only trace of a never-answered query was missing.
///
/// More wildcards than there are permits are sent, one at a time, and every
/// one of them must be counted. That pins two things at once: each panic is
/// counted (not just the first), and the permit is genuinely returned on the
/// unwind — if it were not, the run would stall at
/// `PATTERN_ENUM_CONCURRENCY` sheds-from-panic and the rest would never enter
/// `names()` at all, which the `name_calls` assertion catches separately.
#[tokio::test]
async fn a_panicking_enumeration_counts_as_a_shed() {
    let name_calls = Arc::new(AtomicUsize::new(0));
    let src: Arc<dyn Source> = Arc::new(PanickingNames {
        name_calls: name_calls.clone(),
    });
    let h = UdpHarness::start(vec![("boom", src)]).await;

    let queries = (PATTERN_ENUM_CONCURRENCY + 2) as u64;
    let before = global_stats().pattern_enum_shed;

    for i in 0..queries {
        h.search(300 + i as u32, 300 + i as u32, "BOOM:*").await;
        // One at a time: the point is that each panic returns the permit the
        // next query needs, not that several can be in flight.
        let want = before + i + 1;
        let got = await_shed_count(want).await;
        assert!(
            got >= want,
            "wildcard #{} of {queries} panicked in names() and went unanswered \
             without moving `pattern_enum_shed` (expected >= {want}, saw \
             {got}); a panicking source is invisible to the one counter that \
             records a query nothing will ever answer",
            i + 1
        );
    }

    assert_eq!(
        name_calls.load(Ordering::SeqCst) as u64,
        queries,
        "only {} of {queries} wildcards reached names(); the permit is not \
         being returned when the enumeration unwinds, so a panicking source \
         still retires the enumeration budget",
        name_calls.load(Ordering::SeqCst)
    );

    // And the queries really are unanswered: nothing arrived on the wire.
    let seen = h.collect(Duration::from_millis(300)).await;
    let mine: Vec<_> = seen.iter().filter(|p| p.seq >= 300).collect();
    assert!(
        mine.is_empty(),
        "a panicking enumeration answered anyway: {:?}",
        mine.iter()
            .map(|p| (p.seq, p.found, p.cids.clone()))
            .collect::<Vec<_>>()
    );
}
