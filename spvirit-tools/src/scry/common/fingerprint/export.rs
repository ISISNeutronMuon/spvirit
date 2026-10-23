//! InfluxDB line-protocol export for fingerprints and coverage (spec §7).
//!
//! The measurement name and field set are fixed by the spec so that
//! fingerprints join the existing export rather than forming a parallel
//! pipeline. Deliberate additions beyond the spec's example line:
//!
//! * `observed_count`, without which a reservoir-sampled window understates
//!   the channel's real update rate — `sample_count` is only what was
//!   *retained*.
//! * the `sampling` tag, which spec §4.3 requires so consumers can reason
//!   about detection latency instead of guessing at it.
//! * `struct_id_changed`, the sticky marker that the window saw more than one
//!   struct id. It is a D1 source-change signal in its own right and would be
//!   swallowed entirely if only the first `struct_id` were exported.
//!
//! Both functions take the timestamp as an argument and never read the clock,
//! so a line is a pure function of the record it describes.

use super::detect::DetectorCounts;
use super::types::{FingerprintCoverage, SourceFingerprint};

/// Escape a tag key or value for line protocol.
///
/// PV names and struct ids routinely contain commas, spaces and equals signs,
/// any of which silently corrupts a line if left unescaped.
pub fn escape_tag(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            ',' | ' ' | '=' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// One `pv_fingerprint` line (spec §7).
///
/// `value_lsb` and `arrival_delta_var_ms` are omitted rather than zeroed when
/// absent: spec §6.1 requires "absent" to be distinguishable from "computed
/// and unremarkable", and a lattice of zero or a transport variance of zero
/// are both real, meaningful values. An absent InfluxDB field is exactly the
/// "no value" the record's `None` means.
pub fn fingerprint_line_protocol(
    fp: &SourceFingerprint,
    host: &str,
    timestamp_ns: u128,
) -> String {
    // A non-NT channel often has no struct id at all. `struct_id=` is not a
    // legal tag value, so the tag would vanish and the series would silently
    // merge with a differently-shaped one; a stable placeholder keeps the
    // series distinct and the absence visible.
    let struct_id = if fp.struct_id.is_empty() { "unknown" } else { fp.struct_id.as_str() };

    // Field order is fixed rather than derived from a map, so that two runs
    // over the same record produce byte-identical lines.
    let mut fields = format!(
        "phase_mean_ms={},phase_spread_us={},phase_circular_var={}",
        fp.phase_mean_ms, fp.phase_spread_us, fp.phase_circular_var
    );
    if let Some(lsb) = fp.value_lsb {
        fields.push_str(&format!(",value_lsb={}", lsb));
    }
    fields.push_str(&format!(
        ",lsb_confidence={},cadence_modal_s={},cadence_p50_s={},cadence_p95_s={}",
        fp.lsb_confidence, fp.cadence_modal_s, fp.cadence_p50_s, fp.cadence_p95_s
    ));
    fields.push_str(&format!(",arrival_delta_mean_ms={}", fp.arrival_delta_mean_ms));
    if let Some(var) = fp.arrival_delta_var_ms {
        fields.push_str(&format!(",arrival_delta_var_ms={}", var));
    }
    fields.push_str(&format!(
        ",sample_count={}i,observed_count={}i,has_nan_limits={},struct_id_changed={}",
        fp.sample_count, fp.observed_count, fp.has_nan_limits, fp.struct_id_changed
    ));

    format!(
        "pv_fingerprint,host={},channel={},server={},struct_id={},sampling={} {} {}\n",
        escape_tag(host),
        escape_tag(&fp.channel),
        escape_tag(&fp.server_addr.to_string()),
        escape_tag(struct_id),
        fp.sampling.label(),
        fields,
        timestamp_ns
    )
}

/// One `pv_fingerprint_detect` line: everything the detector declined to
/// emit (spec §5, §6.1).
///
/// A detection pipeline that deduplicated, bounded or suppressed firings
/// without saying so would be indistinguishable from a quiet network. These
/// counters are cumulative over the life of the detector, so a restart is
/// visible as a reset rather than as a gap.
/// `event_buffer_dropped` is not a detector counter: it is the number of
/// events discarded by the in-memory cap on `IfStats::fingerprint_events`,
/// carried on this line because a collector-only deployment has no screen to
/// read it from. It is deliberately named apart from
/// `events_dropped_by_cap`, the detector's own per-ingest cap, so the two
/// cannot be confused.
pub fn detect_counts_line_protocol(
    counts: &DetectorCounts,
    event_buffer_dropped: u64,
    host: &str,
    timestamp_ns: u128,
) -> String {
    format!(
        "pv_fingerprint_detect,host={} d1_suppressed={}i,d2_repeat_suppressed={}i,d2_incomparable_pairs={}i,d2_channels_skipped={}i,d2_channels_carried_over={}i,d2_pairs_evicted={}i,d3_repeat_suppressed={}i,d4_population_too_small={}i,d4_no_spread={}i,events_dropped_by_cap={}i,event_buffer_dropped={}i {}
",
        escape_tag(host),
        counts.d1_suppressed,
        counts.d2_repeat_suppressed,
        counts.d2_incomparable_pairs,
        counts.d2_channels_skipped,
        counts.d2_channels_carried_over,
        counts.d2_pairs_evicted,
        counts.d3_repeat_suppressed,
        counts.d4_population_too_small,
        counts.d4_no_spread,
        counts.events_dropped_by_cap,
        event_buffer_dropped,
        timestamp_ns
    )
}

/// One `pv_fingerprint_coverage` line.
///
/// Spec §6.3: an observer sees only what its placement lets it see, and the
/// resulting coverage must be measured and reported rather than assumed.
///
/// Every field of `FingerprintCoverage` is exported, including the deferral
/// and rotation totals, both saturation flags, and the count of updates
/// the probe could not read at all. Those fields exist so that
/// a capacity cap or a round-robin slot cannot hide from the operator;
/// dropping them from the line would put the hiding place back.
pub fn coverage_line_protocol(
    cov: &FingerprintCoverage,
    host: &str,
    timestamp_ns: u128,
) -> String {
    format!(
        "pv_fingerprint_coverage,host={},interface={} \
         tracked_channels={}i,admitted_channels={}i,deferred_channels={}i,\
deferred_total={}i,deferred_saturated={},sampled_channels={}i,\
rotated_out_channels={}i,rotated_out_total={}i,rotated_out_saturated={},\
unprobeable_updates={}i,\
min_samples={}i {}\n",
        escape_tag(host),
        escape_tag(&cov.interface),
        cov.tracked_channels,
        cov.admitted_channels,
        cov.deferred_channels,
        cov.deferred_total,
        cov.deferred_saturated,
        cov.sampled_channels,
        cov.rotated_out_channels,
        cov.rotated_out_total,
        cov.rotated_out_saturated,
        cov.unprobeable_updates,
        cov.min_samples,
        timestamp_ns
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scry::common::fingerprint::types::{
        FingerprintCoverage, SamplingRegime, SourceFingerprint,
    };
    use std::time::{Duration, UNIX_EPOCH};

    fn reference_fingerprint() -> SourceFingerprint {
        SourceFingerprint {
            channel: "R55IWATER::AMBIENT:TEMPERATURE".to_string(),
            server_addr: "10.0.0.11:5075".parse().unwrap(),
            phase_mean_ms: 262.016,
            phase_spread_us: 85.6,
            phase_circular_var: 0.0000003,
            value_lsb: Some(0.0175),
            lsb_confidence: 1.0,
            cadence_modal_s: 1.0,
            cadence_p50_s: 1.0,
            cadence_p95_s: 25.0,
            arrival_delta_mean_ms: 4.25,
            arrival_delta_var_ms: Some(0.5),
            sample_count: 40,
            window_start: UNIX_EPOCH + Duration::from_secs(1_755_756_000),
            window_end: UNIX_EPOCH + Duration::from_secs(1_755_756_900),
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            struct_id_changed: false,
            has_nan_limits: true,
            observed_count: 40,
            sampling: SamplingRegime::Full,
        }
    }

    fn reference_coverage() -> FingerprintCoverage {
        FingerprintCoverage {
            interface: "bond10gbe".to_string(),
            tracked_channels: 1200,
            admitted_channels: 1000,
            deferred_channels: 200,
            deferred_total: 4096,
            deferred_saturated: true,
            sampled_channels: 37,
            rotated_out_channels: 0,
            rotated_out_total: 0,
            rotated_out_saturated: false,
            unprobeable_updates: 17,
            min_samples: 30,
        }
    }

    /// Spec §7: measurement name, tag set and field set are fixed.
    #[test]
    fn emits_the_spec_line_protocol() {
        let line =
            fingerprint_line_protocol(&reference_fingerprint(), "scry01", 1_755_756_900_000_000_000);
        assert!(line.starts_with("pv_fingerprint,"), "{}", line);
        assert!(line.contains("channel=R55IWATER::AMBIENT:TEMPERATURE"), "{}", line);
        assert!(line.contains("server=10.0.0.11:5075"), "{}", line);
        assert!(line.contains("struct_id=epics:nt/NTScalar:1.0"), "{}", line);
        assert!(line.contains("host=scry01"), "{}", line);
        assert!(line.contains("sampling=full"), "{}", line);
        for field in [
            "phase_mean_ms=262.016",
            "phase_spread_us=85.6",
            "value_lsb=0.0175",
            "lsb_confidence=1",
            "cadence_modal_s=1",
            "cadence_p95_s=25",
            "arrival_delta_mean_ms=4.25",
            "arrival_delta_var_ms=0.5",
            "sample_count=40i",
            "observed_count=40i",
            "has_nan_limits=true",
        ] {
            assert!(line.contains(field), "missing {} in {}", field, line);
        }
        assert!(line.ends_with("1755756900000000000\n"), "{}", line);
    }

    /// The line must be well-formed as a whole, not merely contain the right
    /// substrings: exactly one unescaped space between tag set, field set and
    /// timestamp.
    #[test]
    fn is_a_single_well_formed_line() {
        let line =
            fingerprint_line_protocol(&reference_fingerprint(), "scry 01", 1_755_756_900_000_000_000);
        let body = line.strip_suffix('\n').expect("trailing newline");
        assert!(!body.contains('\n'), "{}", line);

        // Split on spaces that are not backslash-escaped.
        let mut parts: Vec<String> = vec![String::new()];
        let mut escaped = false;
        for c in body.chars() {
            if escaped {
                parts.last_mut().unwrap().push(c);
                escaped = false;
            } else if c == '\\' {
                parts.last_mut().unwrap().push(c);
                escaped = true;
            } else if c == ' ' {
                parts.push(String::new());
            } else {
                parts.last_mut().unwrap().push(c);
            }
        }
        assert_eq!(parts.len(), 3, "expected key/fields/timestamp, got {:?}", parts);
        assert_eq!(parts[2], "1755756900000000000");
        for tag in parts[0].split(',').skip(1) {
            assert!(tag.contains('='), "malformed tag {:?} in {}", tag, line);
        }
    }

    /// Spec §6.1: "no lattice observed" must not be exported as a lattice of
    /// zero, so the field is omitted rather than zeroed.
    #[test]
    fn omits_the_lattice_field_when_there_is_no_lattice() {
        let mut fp = reference_fingerprint();
        fp.value_lsb = None;
        fp.lsb_confidence = 0.0;
        let line = fingerprint_line_protocol(&fp, "scry01", 1);
        assert!(!line.contains("value_lsb="), "{}", line);
        assert!(line.contains("lsb_confidence=0"), "{}", line);
    }

    /// Spec §6.1 again: a rock-steady transport really does have an arrival
    /// variance of 0.0, so "fewer than two samples" must not be exported as
    /// that same zero.
    #[test]
    fn omits_the_arrival_variance_when_it_is_undefined() {
        let mut fp = reference_fingerprint();
        fp.arrival_delta_var_ms = None;
        let line = fingerprint_line_protocol(&fp, "scry01", 1);
        assert!(!line.contains("arrival_delta_var_ms="), "{}", line);
        assert!(line.contains("arrival_delta_mean_ms="), "{}", line);

        fp.arrival_delta_var_ms = Some(0.0);
        let line = fingerprint_line_protocol(&fp, "scry01", 1);
        assert!(line.contains("arrival_delta_var_ms=0"), "{}", line);
    }

    /// A struct id that changed mid-window is a source-change signal; the
    /// export must carry it rather than only the first id seen.
    #[test]
    fn reports_a_struct_id_that_changed_mid_window() {
        let mut fp = reference_fingerprint();
        assert!(
            fingerprint_line_protocol(&fp, "h", 1).contains("struct_id_changed=false"),
            "steady struct id must still be stated, not inferred from silence"
        );
        fp.struct_id_changed = true;
        assert!(fingerprint_line_protocol(&fp, "h", 1).contains("struct_id_changed=true"));
    }

    /// PV names and struct ids contain characters that break line protocol if
    /// they are not escaped.
    #[test]
    fn escapes_tag_values() {
        assert_eq!(escape_tag("a,b c=d"), "a\\,b\\ c\\=d");
        let mut fp = reference_fingerprint();
        fp.channel = "WEIRD PV,NAME=1".to_string();
        let line = fingerprint_line_protocol(&fp, "scry01", 1);
        assert!(line.contains("channel=WEIRD\\ PV\\,NAME\\=1"), "{}", line);
    }

    /// A struct id is often unknown for non-NT channels; the tag must stay
    /// present with a stable placeholder rather than emitting `struct_id=`.
    #[test]
    fn substitutes_a_placeholder_for_an_empty_struct_id() {
        let mut fp = reference_fingerprint();
        fp.struct_id = String::new();
        let line = fingerprint_line_protocol(&fp, "scry01", 1);
        assert!(line.contains("struct_id=unknown"), "{}", line);
    }

    /// Spec §4.3: the regime tag distinguishes a full window from a sampled
    /// or rotated one.
    #[test]
    fn tags_the_sampling_regime() {
        let mut fp = reference_fingerprint();
        fp.sampling = SamplingRegime::Reservoir { cap: 200 };
        assert!(fingerprint_line_protocol(&fp, "h", 1).contains("sampling=reservoir"));
        fp.sampling = SamplingRegime::RoundRobin { cap: 200, slots: 4 };
        assert!(fingerprint_line_protocol(&fp, "h", 1).contains("sampling=round_robin"));
    }

    /// The same record must produce the same bytes every time — no map
    /// iteration order, no clock read.
    #[test]
    fn is_deterministic() {
        let fp = reference_fingerprint();
        assert_eq!(
            fingerprint_line_protocol(&fp, "scry01", 7),
            fingerprint_line_protocol(&fp, "scry01", 7)
        );
        let cov = reference_coverage();
        assert_eq!(
            coverage_line_protocol(&cov, "scry01", 7),
            coverage_line_protocol(&cov, "scry01", 7)
        );
    }

    /// Spec §6.3: coverage is measured and reported, not assumed.
    #[test]
    fn emits_a_coverage_line() {
        let line = coverage_line_protocol(&reference_coverage(), "scry01", 42);
        assert!(line.starts_with("pv_fingerprint_coverage,"), "{}", line);
        assert!(line.contains("interface=bond10gbe"), "{}", line);
        assert!(line.contains("tracked_channels=1200i"), "{}", line);
        assert!(line.contains("admitted_channels=1000i"), "{}", line);
        assert!(line.contains("deferred_channels=200i"), "{}", line);
        assert!(line.contains("sampled_channels=37i"), "{}", line);
        assert!(line.ends_with(" 42\n"), "{}", line);
    }

    /// The cap and rotation accounting exists so that loss cannot hide; if it
    /// is not exported, it hides.
    #[test]
    fn coverage_carries_every_loss_counter() {
        let mut cov = reference_coverage();
        cov.rotated_out_channels = 512;
        cov.rotated_out_total = 90_000;
        cov.rotated_out_saturated = true;
        let line = coverage_line_protocol(&cov, "scry01", 42);
        for field in [
            "deferred_total=4096i",
            "deferred_saturated=true",
            "rotated_out_channels=512i",
            "rotated_out_total=90000i",
            "rotated_out_saturated=true",
            "unprobeable_updates=17i",
            "min_samples=30i",
        ] {
            assert!(line.contains(field), "missing {} in {}", field, line);
        }
        // Exactly one space between the tag set, the field set and the
        // timestamp — a stray space in the continuation string would break
        // the line silently.
        assert_eq!(line.trim_end().matches(' ').count(), 2, "{}", line);
    }
}
