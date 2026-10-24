//! Distance between two source fingerprints.
//!
//! Weighting follows the measured discrimination in spec §10: phase separates
//! the two reference channels by 434 ms against a within-channel spread of at
//! most 0.197 ms — a ratio of about 2200 — so it carries half the weight. The
//! lattice is next, being a hardware property; cadence and arrival delta are
//! configuration- and topology-dependent and carry least.
//!
//! Every component is skipped when either side lacks it, and the surviving
//! weights are renormalised. A missing observable is therefore never counted
//! as a difference (spec §6.4) — but note what renormalisation does to the
//! components that remain: they carry the whole decision, so the effective
//! tolerance on each of them *tightens*. With all four present, threshold
//! 0.05 admits an arrival-delta gap of up to 333 ms; with phase skipped only
//! 0.5 of the weight survives and the same threshold admits 167 ms. Fewer
//! observables means a stricter test on each survivor, not a looser one.
//!
//! When *no* component survives there is no evidence in either direction, and
//! spec §6.1 forbids letting that read as "computed and unremarkable". A
//! scalar cannot carry "unknown", so [`fingerprint_distance_opt`] returns
//! `None` and [`fingerprint_distance`] returns `f64::NAN` — which compares
//! false against any threshold, so an unknown distance can be mistaken
//! neither for a match nor for a difference.

use super::circular::circ_diff_ms;
use super::types::SourceFingerprint;

/// Normalisation scales. Each converts a raw difference into `0..=1` by
/// dividing by the difference at which two sources are unambiguously
/// distinct, then clamping.
const PHASE_SCALE_MS: f64 = 500.0;
const CADENCE_SCALE_LOG2: f64 = 4.0;
const ARRIVAL_SCALE_MS: f64 = 1000.0;

/// Default distance below which two fingerprints are considered the same
/// source.
pub const DEFAULT_MATCH_THRESHOLD: f64 = 0.05;

/// Relative weights of the four observables.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimilarityWeights {
    pub phase: f64,
    pub lsb: f64,
    pub cadence: f64,
    pub arrival: f64,
}

impl Default for SimilarityWeights {
    fn default() -> Self {
        Self { phase: 0.5, lsb: 0.2, cadence: 0.15, arrival: 0.15 }
    }
}

/// Weighted distance in `0..=1`, or `None` when nothing was comparable.
///
/// `None` is not zero: it means the two fingerprints share no observable that
/// could be measured against each other, so neither sameness nor difference
/// has been established (spec §6.1, §6.4).
#[must_use]
pub fn fingerprint_distance_opt(
    a: &SourceFingerprint,
    b: &SourceFingerprint,
    w: &SimilarityWeights,
) -> Option<f64> {
    let mut total = 0.0;
    let mut weight = 0.0;

    let mut add = |component_weight: f64, distance: f64| {
        if component_weight > 0.0 {
            total += component_weight * distance;
            weight += component_weight;
        }
    };

    // O1 — phase. Skipped unless both sides are locked: spec §2.1 says a wide
    // phase is absence of evidence, not evidence of a different source.
    if a.phase_is_locked() && b.phase_is_locked() {
        let d = circ_diff_ms(a.phase_mean_ms, b.phase_mean_ms) / PHASE_SCALE_MS;
        add(w.phase, d.clamp(0.0, 1.0));
    }

    // O2 — lattice. Compared as a ratio: a lattice is a scale, so 0.01 vs
    // 0.0175 matters as much at millivolts as at kilovolts. A non-positive
    // lattice is not a lattice, so the component is skipped rather than
    // scored as agreement.
    if let (Some(la), Some(lb)) = (a.value_lsb, b.value_lsb)
        && la > 0.0
        && lb > 0.0
    {
        let d = (la.max(lb) / la.min(lb) - 1.0).clamp(0.0, 1.0);
        add(w.lsb, d);
    }

    // O3 — cadence, in octaves, so 0.1 s vs 1 s counts the same as 1 s vs
    // 10 s.
    if a.cadence_modal_s > 0.0 && b.cadence_modal_s > 0.0 {
        let d = ((a.cadence_modal_s / b.cadence_modal_s).log2().abs() / CADENCE_SCALE_LOG2)
            .clamp(0.0, 1.0);
        add(w.cadence, d);
    }

    // O4 — arrival minus stamp. Always present on a wire observation, so the
    // component is never skipped for absence.
    let arrival = ((a.arrival_delta_mean_ms - b.arrival_delta_mean_ms).abs() / ARRIVAL_SCALE_MS)
        .clamp(0.0, 1.0);
    add(w.arrival, arrival);

    if weight > 0.0 { Some(total / weight) } else { None }
}

/// Weighted distance in `0..=1`. Zero means indistinguishable.
///
/// **Prefer [`fingerprint_distance_opt`].** This form returns `f64::NAN` when
/// no component is comparable, and NaN does not obey the comparison algebra
/// callers expect: `!(d <= threshold)` is *not* the negation of a match, it
/// is true for both a genuine difference and an unknown. Only
/// `fingerprint_distance_opt` makes "nothing was comparable" handleable; this
/// signature exists because the task interface mandates an `f64`.
#[must_use]
pub fn fingerprint_distance(
    a: &SourceFingerprint,
    b: &SourceFingerprint,
    w: &SimilarityWeights,
) -> f64 {
    fingerprint_distance_opt(a, b, w).unwrap_or(f64::NAN)
}

/// Whether two fingerprints are close enough to be treated as one source.
///
/// An incomparable pair is never a match: `NAN <= threshold` is false.
#[must_use]
pub fn fingerprints_match(a: &SourceFingerprint, b: &SourceFingerprint, threshold: f64) -> bool {
    fingerprint_distance(a, b, &SimilarityWeights::default()) <= threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scry::common::fingerprint::types::{SamplingRegime, SourceFingerprint};
    use std::time::UNIX_EPOCH;

    fn fp(
        channel: &str,
        phase_ms: f64,
        spread_us: f64,
        lsb: Option<f64>,
        modal: f64,
        arrival: f64,
    ) -> SourceFingerprint {
        SourceFingerprint {
            channel: channel.to_string(),
            server_addr: "10.0.0.11:5075".parse().unwrap(),
            phase_mean_ms: phase_ms,
            phase_spread_us: spread_us,
            phase_circular_var: 3e-7,
            value_lsb: lsb,
            lsb_confidence: if lsb.is_some() { 1.0 } else { 0.0 },
            cadence_modal_s: modal,
            cadence_p50_s: modal,
            cadence_p95_s: modal * 25.0,
            arrival_delta_mean_ms: arrival,
            arrival_delta_var_ms: Some(0.1),
            sample_count: 40,
            window_start: UNIX_EPOCH,
            window_end: UNIX_EPOCH,
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            struct_id_changed: false,
            has_nan_limits: false,
            observed_count: 40,
            sampling: SamplingRegime::Full,
        }
    }

    /// Spec §10: the two reference channels MUST be classified as distinct
    /// sources. This is the headline requirement of the whole exercise.
    #[test]
    fn the_reference_channels_are_distinct_sources() {
        let a = fp("R55IWATER::AMBIENT:TEMPERATURE", 262.016, 85.6, Some(0.0175), 1.0, 4.0);
        let b = fp("R4WATER::DEMINWAT:PH", 695.973, 196.6, Some(0.01), 1.0, 4.0);
        let d = fingerprint_distance(&a, &b, &SimilarityWeights::default());
        assert!(d > DEFAULT_MATCH_THRESHOLD * 4.0, "distance was only {}", d);
        assert!(!fingerprints_match(&a, &b, DEFAULT_MATCH_THRESHOLD));
    }

    /// Two channels from the same IOC differ only by within-window jitter and
    /// must be classified as one source.
    #[test]
    fn two_channels_from_one_source_match() {
        let a = fp("IOC1::A", 262.016, 85.6, Some(0.0175), 1.0, 4.0);
        let b = fp("IOC1::B", 262.020, 90.1, Some(0.0175), 1.0, 4.1);
        assert!(fingerprints_match(&a, &b, DEFAULT_MATCH_THRESHOLD));
    }

    #[test]
    fn a_fingerprint_matches_itself_at_zero_distance() {
        let a = fp("SELF", 262.016, 85.6, Some(0.0175), 1.0, 4.0);
        assert!(fingerprint_distance(&a, &a, &SimilarityWeights::default()) < 1e-12);
    }

    /// Phase distance must be modular, or a source stamping near the second
    /// boundary looks maximally distant from itself.
    #[test]
    fn phase_distance_wraps() {
        let a = fp("A", 999.9, 50.0, Some(0.01), 1.0, 4.0);
        let b = fp("B", 0.1, 50.0, Some(0.01), 1.0, 4.0);
        assert!(fingerprints_match(&a, &b, DEFAULT_MATCH_THRESHOLD));
    }

    /// Spec §2.1: a wide phase is absence of evidence. Two unlocked sources
    /// must not be pushed apart by phase they never had.
    #[test]
    fn unlocked_phase_is_excluded_rather_than_counted() {
        let mut a = fp("EVENT:A", 100.0, 400_000.0, Some(0.01), 1.0, 4.0);
        let mut b = fp("EVENT:B", 800.0, 400_000.0, Some(0.01), 1.0, 4.0);
        a.phase_circular_var = 0.9;
        b.phase_circular_var = 0.9;
        assert!(
            fingerprints_match(&a, &b, DEFAULT_MATCH_THRESHOLD),
            "distance {}",
            fingerprint_distance(&a, &b, &SimilarityWeights::default())
        );
    }

    /// One unlocked side is enough to disqualify the component: an unlocked
    /// phase has no mean worth comparing against a locked one.
    #[test]
    fn one_unlocked_side_disqualifies_the_phase_component() {
        let a = fp("LOCKED", 100.0, 85.6, Some(0.01), 1.0, 4.0);
        let mut b = fp("UNLOCKED", 800.0, 400_000.0, Some(0.01), 1.0, 4.0);
        b.phase_circular_var = 0.9;
        assert!(fingerprints_match(&a, &b, DEFAULT_MATCH_THRESHOLD));
    }

    /// A different digitiser is a different source even at the same phase.
    #[test]
    fn a_different_lattice_separates_otherwise_identical_fingerprints() {
        let a = fp("A", 262.016, 85.6, Some(0.0175), 1.0, 4.0);
        let b = fp("B", 262.016, 85.6, Some(0.01), 1.0, 4.0);
        assert!(fingerprint_distance(&a, &b, &SimilarityWeights::default()) > 0.0);
    }

    /// A missing lattice on one side must widen uncertainty, not manufacture
    /// a difference.
    #[test]
    fn a_missing_component_is_skipped_not_penalised() {
        let a = fp("A", 262.016, 85.6, Some(0.0175), 1.0, 4.0);
        let b = fp("B", 262.016, 85.6, None, 1.0, 4.0);
        assert!(fingerprints_match(&a, &b, DEFAULT_MATCH_THRESHOLD));
    }

    /// Skipping renormalises: the components that survive keep their relative
    /// influence rather than being diluted by the absent one.
    #[test]
    fn skipping_renormalises_the_surviving_weights() {
        let a = fp("A", 262.016, 85.6, Some(0.0175), 1.0, 4.0);
        let b = fp("B", 362.016, 85.6, Some(0.0175), 1.0, 4.0);
        let with_lsb = fingerprint_distance(&a, &b, &SimilarityWeights::default());

        let mut a2 = a.clone();
        let mut b2 = b.clone();
        a2.value_lsb = None;
        a2.lsb_confidence = 0.0;
        b2.value_lsb = None;
        b2.lsb_confidence = 0.0;
        let without_lsb = fingerprint_distance(&a2, &b2, &SimilarityWeights::default());

        // Same raw phase difference, one fewer agreeing component: dropping
        // it must raise the normalised distance, never lower it.
        assert!(without_lsb > with_lsb, "with {} without {}", with_lsb, without_lsb);
        // 0.8 of the weight survives, so the phase term is scaled by 1/0.8.
        assert!((without_lsb - with_lsb / 0.8).abs() < 1e-12);
    }

    /// Spec §2.4: a message-bus hop shows up as a large arrival delta, which
    /// must separate an otherwise identical-looking pair.
    #[test]
    fn a_bridge_hop_separates_by_arrival_delta() {
        let a = fp("DIRECT", 262.016, 85.6, Some(0.0175), 1.0, 4.0);
        let b = fp("BRIDGED", 262.016, 85.6, Some(0.0175), 1.0, 900.0);
        assert!(!fingerprints_match(&a, &b, DEFAULT_MATCH_THRESHOLD));
    }

    /// A 10 Hz source and a 1 Hz source are different sources.
    #[test]
    fn cadence_separates_scan_rates() {
        let a = fp("FAST", 262.016, 85.6, Some(0.0175), 0.1, 4.0);
        let b = fp("SLOW", 262.016, 85.6, Some(0.0175), 1.0, 4.0);
        assert!(!fingerprints_match(&a, &b, DEFAULT_MATCH_THRESHOLD));
    }

    /// Distance is a metric-like quantity: symmetric and bounded.
    #[test]
    fn distance_is_symmetric_and_bounded() {
        let a = fp("A", 262.016, 85.6, Some(0.0175), 1.0, 4.0);
        let b = fp("B", 695.973, 196.6, Some(0.01), 10.0, 900.0);
        let w = SimilarityWeights::default();
        let ab = fingerprint_distance(&a, &b, &w);
        let ba = fingerprint_distance(&b, &a, &w);
        assert!((ab - ba).abs() < 1e-12);
        assert!((0.0..=1.0).contains(&ab), "distance {} out of range", ab);
    }

    /// Spec §6.1: "nothing to compare" is not "identical". With no component
    /// surviving there is no evidence either way, and the distance must say
    /// so rather than collapse to zero.
    #[test]
    fn no_comparable_component_is_undefined_not_zero() {
        let a = fp("A", 262.016, 85.6, Some(0.0175), 1.0, 4.0);
        let b = fp("B", 695.973, 196.6, Some(0.01), 10.0, 900.0);
        let none = SimilarityWeights { phase: 0.0, lsb: 0.0, cadence: 0.0, arrival: 0.0 };

        assert_eq!(fingerprint_distance_opt(&a, &b, &none), None);
        let d = fingerprint_distance(&a, &b, &none);
        assert!(d.is_nan(), "distance was {}, which would read as identical", d);
        // An undefined distance must not be mistaken for a match. Written
        // as the negation of the real predicate `fingerprints_match` uses,
        // so it stays true to the code under test; the lint's suggested
        // rewrite `d > t` is a different (and weaker) claim about NaN.
        #[allow(clippy::neg_cmp_op_on_partial_ord)]
        {
            assert!(!(d <= DEFAULT_MATCH_THRESHOLD));
        }
    }

    /// A non-positive lattice is not a lattice; comparing against it would
    /// fabricate agreement.
    #[test]
    fn a_non_positive_lattice_is_skipped() {
        let a = fp("A", 262.016, 85.6, Some(0.0), 1.0, 4.0);
        let b = fp("B", 262.016, 85.6, Some(0.0175), 1.0, 4.0);
        let w = SimilarityWeights::default();
        // Only phase, cadence and arrival survive, and all three agree.
        assert_eq!(fingerprint_distance(&a, &b, &w), 0.0);
    }
}
