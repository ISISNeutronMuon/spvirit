//! The Fingerprints page: list, clusters, and the discontinuity log
//! (spec §8).

use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Row, Table};
use ratatui::Frame;

use super::super::state::{format_lsb, format_phase_ms, AppState, FingerprintsView};
use super::visible_window;

pub fn draw_fingerprints(frame: &mut Frame, state: &mut AppState, area: Rect) {
    match state.fingerprints_view {
        FingerprintsView::List => draw_list(frame, state, area),
        FingerprintsView::Clusters => draw_clusters(frame, state, area),
        FingerprintsView::Events => draw_events(frame, state, area),
    }
}

/// Coverage is stated on every sub-view's title. Spec §6.3: coverage must be
/// measured and reported, not assumed.
///
/// `deferred_channels` and `rotated_out_channels` are themselves capped
/// counters (see `FingerprintCoverage`'s field docs): once saturated they are
/// a floor, not an exact figure, so they are rendered with a `≥` marker
/// whenever `*_saturated` is set. A saturated counter silently read as exact
/// is the failure this guards against.
///
/// `unprobeable` is a different kind of loss again: monitor-data updates
/// that never reached the engine because nothing timestamped could be read
/// out of them. Without it, `tracked` reads as the whole of the wire.
fn coverage_suffix(state: &AppState) -> String {
    let c = &state.if_stats_local.fingerprint_coverage;
    format!(
        "if {} | tracked {} | admitted {} | deferred {} (total {}) | rotated_out {} (total {}) | unprobeable {} | sampled {} | sampling {}",
        c.interface,
        c.tracked_channels,
        c.admitted_channels,
        floor_or_exact(c.deferred_channels, c.deferred_saturated),
        c.deferred_total,
        floor_or_exact(c.rotated_out_channels, c.rotated_out_saturated),
        c.rotated_out_total,
        c.unprobeable_updates,
        c.sampled_channels,
        sampling_regimes_present(state),
    )
}

/// Renders a capped counter as `≥N` when it has saturated its cap (a floor,
/// not an exact figure) or as a plain `N` otherwise.
fn floor_or_exact(count: usize, saturated: bool) -> String {
    if saturated {
        format!("\u{2265}{}", count)
    } else {
        count.to_string()
    }
}

/// The sampling regime is a per-fingerprint fact (spec §4.3), not a
/// page-global one: reporting a single arbitrary row's regime (e.g. the
/// first, sorted or not) as if it applied to the whole page would be false
/// whenever the fingerprints on this page were sampled under more than one
/// regime. Report the distinct set actually present instead.
fn sampling_regimes_present(state: &AppState) -> String {
    let mut labels: Vec<&'static str> =
        state.if_stats_local.fingerprints.iter().map(|fp| fp.sampling.label()).collect();
    labels.sort_unstable();
    labels.dedup();
    if labels.is_empty() {
        "n/a".to_string()
    } else {
        labels.join(",")
    }
}

fn draw_list(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let rows_data = state.fingerprint_rows();
    let view_height = area.height.saturating_sub(3).max(1);
    state.fingerprints_view_height = view_height;
    let (start, end) =
        visible_window(rows_data.len(), &mut state.fingerprints_scroll, view_height);

    let header = Row::new(vec![
        Cell::from("Channel"),
        Cell::from("Server"),
        Cell::from("Phase ms"),
        Cell::from("LSB"),
        Cell::from("Cadence s"),
        Cell::from("Arrival ms"),
        Cell::from("N"),
        Cell::from("Flags"),
    ])
    .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = if rows_data.is_empty() {
        vec![Row::new(vec![Cell::from(
            "No fingerprints yet — start with --fingerprint, then wait one window",
        )])]
    } else {
        rows_data[start..end]
            .iter()
            .map(|r| {
                let style = if r.flags.contains("NaN-limits") {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Row::new(vec![
                    Cell::from(r.channel.clone()),
                    Cell::from(r.server.clone()),
                    Cell::from(r.phase.clone()),
                    Cell::from(r.lsb.clone()),
                    Cell::from(r.cadence.clone()),
                    Cell::from(r.arrival.clone()),
                    Cell::from(r.samples.clone()),
                    Cell::from(r.flags.clone()),
                ])
                .style(style)
            })
            .collect()
    };

    let widths = [
        Constraint::Min(24),
        Constraint::Length(21),
        Constraint::Length(18),
        Constraint::Length(16),
        Constraint::Length(12),
        Constraint::Length(14),
        Constraint::Length(9),
        Constraint::Length(18),
    ];
    let table = Table::new(rows, widths).header(header).block(
        Block::default()
            .title(format!(
                "Fingerprints ({}) | {}",
                rows_data.len(),
                coverage_suffix(state)
            ))
            .borders(Borders::ALL),
    );
    frame.render_widget(table, area);
}

fn draw_clusters(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let (clusters, dropped) = state.fingerprint_clusters();
    let view_height = area.height.saturating_sub(3).max(1);
    state.fingerprints_view_height = view_height;
    let (start, end) = visible_window(clusters.len(), &mut state.fingerprints_scroll, view_height);

    let header = Row::new(vec![
        Cell::from("#"),
        Cell::from("Members"),
        Cell::from("Servers"),
        Cell::from("Phase ms"),
        Cell::from("LSB"),
        Cell::from("Exemplar"),
    ])
    .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = clusters[start..end]
        .iter()
        .map(|c| {
            // Spec §8: a cluster crossing expected IOC boundaries should be
            // immediately visible.
            let style = if c.servers.len() > 1 {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            Row::new(vec![
                Cell::from(c.id.to_string()),
                Cell::from(c.members.len().to_string()),
                Cell::from(
                    c.servers.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", "),
                ),
                Cell::from(format_phase_ms(c.exemplar.phase_mean_ms, 3)),
                Cell::from(
                    c.exemplar.value_lsb.map(format_lsb).unwrap_or_else(|| "—".to_string()),
                ),
                Cell::from(c.exemplar.channel.clone()),
            ])
            .style(style)
        })
        .collect();

    // Spec §4.3 / §6.3: a truncated view must say so — and coverage
    // information must not disappear just because a *different* truncation
    // (the cluster-input cap) also occurred; the two facts are independent
    // and both belong in the title.
    let title = if dropped > 0 {
        format!(
            "Clusters ({}) | {} NOT CLUSTERED (input cap) | {}",
            clusters.len(),
            dropped,
            coverage_suffix(state)
        )
    } else {
        format!("Clusters ({}) | {}", clusters.len(), coverage_suffix(state))
    };

    let widths = [
        Constraint::Length(4),
        Constraint::Length(9),
        Constraint::Min(24),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Min(20),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().title(title).borders(Borders::ALL));
    frame.render_widget(table, area);
}

fn draw_events(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let events = &state.if_stats_local.fingerprint_events;
    let dropped = state.if_stats_local.fingerprint_events_dropped;
    let view_height = area.height.saturating_sub(3).max(1);
    state.fingerprints_view_height = view_height;
    let (start, end) = visible_window(events.len(), &mut state.fingerprints_scroll, view_height);

    let header = Row::new(vec![
        Cell::from("Time"),
        Cell::from("Rule"),
        Cell::from("Channel"),
        Cell::from("Server"),
        Cell::from("Detail"),
    ])
    .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD));

    // Newest first: the discontinuity log is read from the top.
    let rows: Vec<Row> = events
        .iter()
        .rev()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|e| {
            let ts = e
                .at
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| {
                    chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                        .map(|t| t.format("%H:%M:%S").to_string())
                        .unwrap_or_else(|| "-".to_string())
                })
                .unwrap_or_else(|_| "-".to_string());
            Row::new(vec![
                Cell::from(ts),
                Cell::from(e.rule.code()),
                Cell::from(e.channel.clone()),
                Cell::from(e.server_addr.to_string()),
                Cell::from(e.detail.clone()),
            ])
            .style(Style::default().fg(Color::Yellow))
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Min(20),
        Constraint::Length(21),
        Constraint::Min(30),
    ];
    let title = events_title(events.len(), dropped);
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().title(title).borders(Borders::ALL));
    frame.render_widget(table, area);
}

/// Title for the Events sub-view. Spec §6.1: a capped log must never look
/// like a complete one. When the in-memory cap has discarded anything, say
/// so above the rows rather than letting the row count alone imply
/// completeness — a reader must never mistake a truncated log for the whole
/// one.
fn events_title(count: usize, dropped: u64) -> String {
    if dropped > 0 {
        format!(
            "Discontinuity Log ({}) | {} earlier event(s) dropped by the 500-event cap",
            count, dropped
        )
    } else {
        format!("Discontinuity Log ({})", count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scry::common::fingerprint::types::{FingerprintCoverage, SamplingRegime, SourceFingerprint};
    use std::time::UNIX_EPOCH;

    fn mk_fp(channel: &str, sampling: SamplingRegime) -> SourceFingerprint {
        SourceFingerprint {
            channel: channel.to_string(),
            server_addr: "10.0.0.11:5075".parse().unwrap(),
            phase_mean_ms: 1.0,
            phase_spread_us: 1.0,
            phase_circular_var: 0.0,
            value_lsb: None,
            lsb_confidence: 0.0,
            cadence_modal_s: 1.0,
            cadence_p50_s: 1.0,
            cadence_p95_s: 1.0,
            arrival_delta_mean_ms: 1.0,
            arrival_delta_var_ms: None,
            sample_count: 1,
            window_start: UNIX_EPOCH,
            window_end: UNIX_EPOCH,
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            struct_id_changed: false,
            has_nan_limits: false,
            observed_count: 1,
            sampling,
        }
    }

    /// Finding 3 (fix round 1): `deferred_channels` and `rotated_out_channels`
    /// are capped counters — once saturated they are a floor, never an exact
    /// figure. If the `≥` marker were ever dropped from a saturated count,
    /// this test must fail.
    #[test]
    fn coverage_suffix_marks_saturated_counts_as_a_floor() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprint_coverage = FingerprintCoverage {
            interface: "eth0".to_string(),
            tracked_channels: 2000,
            admitted_channels: 1024,
            deferred_channels: 1024,
            deferred_total: 5000,
            deferred_saturated: true,
            sampled_channels: 900,
            rotated_out_channels: 256,
            rotated_out_total: 900,
            rotated_out_saturated: true,
            unprobeable_updates: 0,
            min_samples: 30,
        };
        let suffix = coverage_suffix(&state);
        assert!(suffix.contains("\u{2265}1024"), "{}", suffix);
        assert!(suffix.contains("total 5000"), "{}", suffix);
        assert!(suffix.contains("\u{2265}256"), "{}", suffix);
        assert!(suffix.contains("total 900"), "{}", suffix);
    }

    /// Spec §6.3: updates the probe could not read never reach the engine,
    /// so the operator must be told they happened — otherwise a page showing
    /// only the NT-scalar subset of the traffic reads as full coverage.
    #[test]
    fn coverage_suffix_reports_unprobeable_updates() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprint_coverage = FingerprintCoverage {
            interface: "eth0".to_string(),
            tracked_channels: 400,
            unprobeable_updates: 12_345,
            ..Default::default()
        };
        let suffix = coverage_suffix(&state);
        assert!(suffix.contains("unprobeable 12345"), "{}", suffix);
    }

    /// Companion to the saturated case: when the caps were not hit, the
    /// counts are exact and must render as plain numbers, not floors.
    #[test]
    fn coverage_suffix_reports_exact_counts_when_not_saturated() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprint_coverage = FingerprintCoverage {
            interface: "eth0".to_string(),
            tracked_channels: 10,
            admitted_channels: 7,
            deferred_channels: 3,
            deferred_total: 3,
            deferred_saturated: false,
            sampled_channels: 2,
            rotated_out_channels: 0,
            rotated_out_total: 0,
            rotated_out_saturated: false,
            unprobeable_updates: 0,
            min_samples: 30,
        };
        let suffix = coverage_suffix(&state);
        assert!(!suffix.contains('\u{2265}'), "{}", suffix);
        assert!(suffix.contains("deferred 3"), "{}", suffix);
        assert!(suffix.contains("rotated_out 0"), "{}", suffix);
    }

    /// Finding 4 (fix round 1): the sampling regime is a per-fingerprint
    /// fact, not a page-global one. Before this fix, the suffix reported
    /// only `fingerprints.first()`'s regime; with a Full-regime channel
    /// sorted first among reservoir-sampled channels, the page header would
    /// falsely claim "sampling full" for the whole page. This test fails if
    /// the suffix ever again reports just one arbitrary row's regime while
    /// other regimes are present.
    #[test]
    fn coverage_suffix_reports_the_distinct_set_of_regimes_present() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprints = vec![
            mk_fp("PV:A", SamplingRegime::Full),
            mk_fp("PV:B", SamplingRegime::Reservoir { cap: 64 }),
        ];
        let suffix = coverage_suffix(&state);
        assert!(suffix.contains("full"), "{}", suffix);
        assert!(suffix.contains("reservoir"), "{}", suffix);
    }

    #[test]
    fn events_title_states_completeness_when_nothing_dropped() {
        let title = events_title(12, 0);
        assert_eq!(title, "Discontinuity Log (12)");
    }

    /// Spec §6.1: a reader must never see a truncated log presented as
    /// complete, so a non-zero drop count must appear in the title itself.
    #[test]
    fn events_title_surfaces_dropped_count_when_capped() {
        let title = events_title(500, 37);
        assert!(title.contains("500"), "{}", title);
        assert!(
            title.contains("37 earlier event(s) dropped by the 500-event cap"),
            "{}",
            title
        );
    }
}
