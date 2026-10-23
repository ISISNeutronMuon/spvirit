//! Guards on the crate's public surface.

/// `PvMonitorEvent` was never constructed anywhere and its doc comment
/// described behaviour `pvmonitor` does not have. It is gone; this test
/// documents that a re-add would be deliberate.
#[test]
fn pv_monitor_event_is_not_part_of_the_public_surface() {
    // Built with `concat!` so this file never contains the name itself.
    const NAME: &str = concat!("PvMonitor", "Event");
    let src = include_str!("../src/lib.rs");
    assert!(
        !src.contains(NAME),
        "PvMonitorEvent was re-exported from lib.rs"
    );
    let types = include_str!("../src/types.rs");
    assert!(
        !types.contains(NAME),
        "PvMonitorEvent was re-added to types.rs"
    );
}
