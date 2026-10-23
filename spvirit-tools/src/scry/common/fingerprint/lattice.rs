//! Value quantisation lattice estimation — observable O2 of the source
//! fingerprint.
//!
//! Estimates the smallest value step a source is capable of emitting, which
//! is a property of its digitiser rather than of its configuration.
//!
//! The estimator works on *successive differences* between distinct observed
//! values, never on the raw values. A GCD over raw values is offset-dependent:
//! on the real R55IWATER sample, whose values sit on a 0.0175 degC lattice
//! with a non-integral offset, Euclid over the raw values divides past the
//! true quantum and reports 0.0025 degC (spec §2.2).

/// Differences below this are treated as IEEE-754 representation noise and
/// discarded rather than mistaken for a very fine lattice.
///
/// `eps_abs` serves three distinct roles across this module: the dedup
/// threshold for collapsing repeated raw values, the noise floor below which
/// a difference is discarded as representation error rather than a real
/// step, and the absolute floor under [`is_multiple`]'s relative tolerance
/// so that near-zero differences and quanta cannot pass by relative
/// comparison alone.
pub const DEFAULT_EPS_ABS: f64 = 1e-9;

/// Relative tolerance used by [`is_multiple`] when deciding whether a
/// difference is an integer multiple of a candidate quantum. The tolerance
/// is relative to the *difference* being tested, not to the quantum itself:
/// a difference `d` counts as a multiple of `quantum` when the nearest
/// multiple of `quantum` is within `rel_tol * d` (or `eps_abs`, whichever is
/// larger) of `d`.
pub const DEFAULT_REL_TOL: f64 = 1e-6;

/// Minimum number of surviving differences before a lattice may be reported.
pub const MIN_DIFFS: usize = 3;

/// Upper bound on how finely a single surviving difference may be
/// subdivided when generating candidate quanta (see [`estimate_lattice`]).
/// Bounds candidate generation and scoring at `O(MAX_SUBDIVISION * n^2)` for
/// `n` surviving differences, which is why candidates are deduplicated
/// within `rel_tol` before scoring.
const MAX_SUBDIVISION: usize = 8;

/// An estimated value lattice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatticeEstimate {
    /// The smallest step the source appears able to emit.
    pub lsb: f64,
    /// Fraction of observed differences that are integer multiples of `lsb`.
    /// Below 1.0 means some updates did not land on the lattice — expect this
    /// on computed channels (spec §6.5).
    pub confidence: f64,
}

/// Estimate the value lattice from a set of observed values.
///
/// Returns `None` when there is not enough evidence: fewer than [`MIN_DIFFS`]
/// distinct successive differences survive, or the differences have no common
/// divisor above `eps_abs`. `None` means "no lattice found", which is not the
/// same as "the source is unquantised" (spec §6.1).
pub fn estimate_lattice(values: &[f64], eps_abs: f64, rel_tol: f64) -> Option<LatticeEstimate> {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("non-finite values filtered above"));
    // Collapse repeats: identical values contribute no step information.
    sorted.dedup_by(|a, b| (*a - *b).abs() < eps_abs);

    // `*d >= eps_abs` is redundant with the dedup above in the common case
    // (a surviving pair of distinct, deduped values differs by more than
    // `eps_abs`), but is kept as a defensive floor: `dedup_by`'s `<` compares
    // adjacent pairs only, so a chain of values each just under `eps_abs`
    // apart from its neighbour can still leave a below-threshold gap between
    // non-adjacent survivors after collapsing.
    let diffs: Vec<f64> = sorted
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|d| *d >= eps_abs)
        .collect();

    if diffs.len() < MIN_DIFFS {
        return None;
    }

    // Candidate quantum: bounded subdivision-and-score, not a plain min-diff
    // or a full tolerance-based Euclidean GCD.
    //
    // A full Euclidean GCD (repeatedly reducing (a, b) via a nearest-multiple
    // remainder) was tried first and rejected: two essentially unrelated
    // reals still have excellent rational approximants a few
    // continued-fraction steps in, so recursive Euclid over a genuine
    // outlier diff "discovers" a spurious, arbitrarily fine common divisor
    // that then appears to divide every other diff too — silently erasing
    // the outlier's evidence instead of scoring it as inconsistent.
    // Concretely, Euclid over {0.01, 0.0042857} converges to ~1e-7, which
    // every other 0.01-scale diff in the sample trivially "divides",
    // producing confidence 1.0 on data that is supposed to show an
    // off-lattice step.
    //
    // Plain min-diff (the first fix) over-corrects in the other direction in
    // two realistic regimes: a deadbanded channel where the single-LSB step
    // never appears (diffs {0.0525, 0.07} from a true LSB of 0.0175 — the
    // minimum diff is 3 quanta, not 1), and a channel with one sub-quantum
    // noise step, where the outlier diff itself becomes the reported "lsb"
    // instead of just lowering confidence.
    //
    // Bounded subdivision fixes both: every surviving diff is divided by
    // 1..=MAX_SUBDIVISION to propose candidate quanta, each candidate is
    // scored by how many diffs it evenly divides within tolerance, and the
    // *coarsest* candidate attaining the best score wins. A deadbanded
    // diff of 0.0525 proposes 0.0175 (= d/3) as a candidate, which then
    // scores as well as or better than any finer candidate; ties prefer the
    // coarsest so 0.0175 beats spurious finer subdivisions of the same diffs.
    let mut cands: Vec<f64> = diffs
        .iter()
        .flat_map(|d| (1..=MAX_SUBDIVISION).map(move |k| d / k as f64))
        .filter(|q| *q > eps_abs)
        .collect();
    cands.sort_by(|a, b| b.partial_cmp(a).unwrap());
    // Dedup within rel_tol so near-identical candidates (e.g. d/2 from one
    // diff and a close neighbour's d'/2) are scored once rather than
    // repeatedly; keeps the O(MAX_SUBDIVISION * n) candidate scoring against
    // n diffs from blowing up on large diff sets. Candidate scoring overall
    // is O(MAX_SUBDIVISION * n^2).
    cands.dedup_by(|a, b| (*a - *b).abs() <= (rel_tol * *b).max(eps_abs));

    let (lsb, consistent) = cands
        .iter()
        .map(|q| {
            (
                *q,
                diffs
                    .iter()
                    .filter(|d| is_multiple(**d, *q, rel_tol, eps_abs))
                    .count(),
            )
        })
        .fold((f64::NAN, 0usize), |best, c| if c.1 > best.1 { c } else { best });

    if consistent == 0 {
        return None;
    }

    Some(LatticeEstimate {
        lsb,
        confidence: consistent as f64 / diffs.len() as f64,
    })
}

/// True when `d` is an integer multiple of `quantum` within tolerance.
fn is_multiple(d: f64, quantum: f64, rel_tol: f64, eps_abs: f64) -> bool {
    if quantum <= eps_abs {
        return false;
    }
    let k = (d / quantum).round();
    (d - k * quantum).abs() <= (rel_tol * d).max(eps_abs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn est(values: &[f64]) -> Option<LatticeEstimate> {
        estimate_lattice(values, DEFAULT_EPS_ABS, DEFAULT_REL_TOL)
    }

    /// Spec §10 reference: R55IWATER::AMBIENT:TEMPERATURE, LSB 0.0175 degC.
    /// The base 13.1625 is deliberately *not* a whole number of steps, which
    /// is exactly the case where a raw-value GCD reports 0.0025 (spec §2.2).
    #[test]
    fn recovers_r55iwater_reference_lsb() {
        let values: Vec<f64> = (0..12).map(|k| 13.1625 + k as f64 * 0.0175).collect();
        let e = est(&values).expect("lattice should be found");
        assert!((e.lsb - 0.0175).abs() < 1e-9, "lsb was {}", e.lsb);
        assert!((e.confidence - 1.0).abs() < 1e-9);
    }

    /// The offset-dependence guard from spec §2.2, stated directly: Euclid on
    /// the raw values of the sample above yields 0.0025, seven times too fine.
    /// The estimator must not reproduce that.
    #[test]
    fn does_not_report_the_offset_dependent_lattice() {
        let values: Vec<f64> = (0..12).map(|k| 13.1625 + k as f64 * 0.0175).collect();
        let e = est(&values).unwrap();
        assert!(
            (e.lsb - 0.0025).abs() > 1e-6,
            "estimator fell back to the raw-value GCD: {}",
            e.lsb
        );
    }

    /// Spec §10 reference: R4WATER::DEMINWAT:PH, LSB 0.01 pH.
    #[test]
    fn recovers_r4water_reference_lsb() {
        let values = [7.32, 7.33, 7.35, 7.36, 7.40, 7.41, 7.44];
        let e = est(&values).unwrap();
        assert!((e.lsb - 0.01).abs() < 1e-9, "lsb was {}", e.lsb);
    }

    /// Real traffic skips lattice steps whenever the input moves by more than
    /// one quantum between updates.
    #[test]
    fn tolerates_skipped_lattice_steps() {
        let values = [0.0, 0.0175, 0.0525, 0.07, 0.14, 0.1575];
        let e = est(&values).unwrap();
        assert!((e.lsb - 0.0175).abs() < 1e-9, "lsb was {}", e.lsb);
    }

    /// Spec §2.2: values arrive as IEEE-754 doubles carrying representation
    /// error, so the GCD has to be tolerance-based rather than exact.
    #[test]
    fn absorbs_ieee754_representation_error() {
        let values = [
            13.162500000000001,
            13.180000000000003,
            13.197499999999998,
            13.215000000000002,
            13.232500000000000,
        ];
        let e = est(&values).unwrap();
        assert!((e.lsb - 0.0175).abs() < 1e-7, "lsb was {}", e.lsb);
    }

    /// Spec §2.2 step 5: fewer than three distinct differences is not enough
    /// evidence to claim a lattice.
    #[test]
    fn refuses_to_report_a_lattice_from_too_few_differences() {
        assert!(est(&[1.0, 1.5, 2.0]).is_none());
        assert!(est(&[1.0]).is_none());
        assert!(est(&[]).is_none());
    }

    /// Spec §6.5: computed channels may show no coherent lattice. Continuous
    /// values must not yield a confident fine-grained quantum.
    #[test]
    fn continuous_values_yield_low_confidence_or_nothing() {
        let values: Vec<f64> = (0..40)
            .map(|k| 3.0 + (k as f64 * 0.7919).sin() * 1.2345)
            .collect();
        match est(&values) {
            None => {}
            Some(e) => assert!(
                e.lsb < 1e-6 || e.confidence < 0.99,
                "claimed a confident lattice of {} on continuous data",
                e.lsb
            ),
        }
    }

    /// Repeated values carry no lattice information and must be collapsed
    /// before differencing, not treated as zero-width steps. Non-finite
    /// values are dropped rather than poisoning the sort.
    #[test]
    fn ignores_repeated_and_nonfinite_values() {
        let values = [
            0.0,
            0.0,
            f64::NAN,
            0.0175,
            0.0175,
            f64::INFINITY,
            0.035,
            0.0525,
        ];
        let e = est(&values).unwrap();
        assert!((e.lsb - 0.0175).abs() < 1e-9, "lsb was {}", e.lsb);
    }

    /// A deadbanded channel may never emit the single-quantum step: every
    /// observed move is 3 or 4 quanta. The estimator must still recover the
    /// underlying 0.0175 lattice, not report 0.0525 (the smallest diff
    /// actually present) as the LSB.
    #[test]
    fn recovers_the_quantum_when_only_multiples_appear() {
        let steps = [3, 4, 3, 4, 3, 4];
        let mut v = 0.0;
        let mut values = vec![v];
        for k in steps {
            v += k as f64 * 0.0175;
            values.push(v);
        }
        let e = est(&values).unwrap();
        assert!((e.lsb - 0.0175).abs() < 1e-9, "lsb was {}", e.lsb);
        assert!((e.confidence - 1.0).abs() < 1e-9, "confidence was {}", e.confidence);
    }

    /// Confidence is the fraction of differences that are integer multiples
    /// of the reported quantum (spec §3, `lsb_confidence`).
    #[test]
    fn confidence_falls_when_a_difference_is_off_lattice() {
        let values = [0.0, 0.01, 0.02, 0.03, 0.0342857, 0.0442857, 0.0542857];
        let e = est(&values).unwrap();
        assert!(e.confidence < 1.0, "confidence was {}", e.confidence);
        assert!(e.confidence > 0.0);
    }
}
