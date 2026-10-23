//! Circular (modular) statistics over sub-second timestamp phase — observable
//! O1 of the source fingerprint.
//!
//! Phase is an angle, not a scalar: 0.999 s and 0.001 s are 2 ms apart, not
//! 998 ms. Spec §4.1 requires circular statistics throughout, so that a source
//! phase-locked near the second boundary is reported as locked rather than as
//! maximally spread.

use std::f64::consts::TAU;

/// Nanoseconds in one second, as a float.
pub const NANOS_PER_SEC: f64 = 1_000_000_000.0;

/// Running circular statistics over sub-second phase samples.
///
/// Constant memory: only the resultant vector and a count are retained, so
/// this is safe to keep per channel at high channel counts.
#[derive(Debug, Clone, Default)]
pub struct CircularPhase {
    sum_cos: f64,
    sum_sin: f64,
    n: u64,
}

impl CircularPhase {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one sub-second phase sample, in nanoseconds.
    ///
    /// Values at or above one second are reduced modulo one second, so a
    /// caller that accidentally passes a full nanosecond-of-day still gets
    /// meaningful phase rather than nonsense.
    pub fn push(&mut self, nanos: u32) {
        let wrapped = (nanos as f64) % NANOS_PER_SEC;
        let theta = TAU * wrapped / NANOS_PER_SEC;
        self.sum_cos += theta.cos();
        self.sum_sin += theta.sin();
        self.n += 1;
    }

    /// Number of samples accumulated.
    pub fn count(&self) -> u64 {
        self.n
    }

    /// Resultant length `R`, in `0..=1`.
    ///
    /// `R` near 1 means tightly phase-locked; `R` near 0 means uniformly
    /// spread, i.e. no phase signature at all (spec §4.1).
    pub fn resultant(&self) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let c = self.sum_cos / self.n as f64;
        let s = self.sum_sin / self.n as f64;
        (c * c + s * s).sqrt().min(1.0)
    }

    /// Mean phase in milliseconds, normalised to `0.0..1000.0`.
    ///
    /// The half-open range is enforced, not merely intended. Samples
    /// straddling the second boundary cancel in `sum_sin` to floating-point
    /// residue; `atan2` of that residue is a tiny negative angle, and adding
    /// `NANOS_PER_SEC` rounds to exactly `1e9`. That escaped as `1000.0` --
    /// a value the observable cannot hold -- into both the exporters and the
    /// TUI. The `+ 0.0` is belt-and-braces: no input reaching `push` today
    /// produces a `sum_sin` of `-0.0`, so unlike the range clamp it is not
    /// covered by a test, and it costs one add to stay safe if that changes.
    pub fn mean_ms(&self) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let c = self.sum_cos / self.n as f64;
        let s = self.sum_sin / self.n as f64;
        let mut nanos = s.atan2(c) * NANOS_PER_SEC / TAU;
        if nanos < 0.0 {
            nanos += NANOS_PER_SEC;
        }
        let ms = nanos / 1e6;
        // The `is_finite` arm also stops a NaN propagating into every
        // downstream comparison; it is not reachable from `push` today, so
        // only the `>= 1000.0` arm carries a test.
        if !ms.is_finite() || ms >= 1000.0 {
            return 0.0;
        }
        // `-0.0 + 0.0` is `0.0`; every other value is unchanged.
        ms + 0.0
    }

    /// Circular variance, `1 - R`. Zero means locked, one means uniform.
    ///
    /// Consumers use this to tell "locked at 262 ms" from "no signal"; a
    /// phase mean without it is not interpretable.
    pub fn circular_var(&self) -> f64 {
        1.0 - self.resultant()
    }

    /// Circular standard deviation, expressed in microseconds of phase.
    ///
    /// For the narrow spreads this fingerprint cares about (tens to hundreds
    /// of microseconds) this agrees with the linear standard deviation to
    /// better than a part in a million, while remaining correct at the wrap.
    pub fn spread_us(&self) -> f64 {
        let r = self.resultant();
        if r <= 0.0 {
            return f64::INFINITY;
        }
        if r >= 1.0 {
            return 0.0;
        }
        (-2.0 * r.ln()).sqrt() * NANOS_PER_SEC / TAU / 1e3
    }
}

/// Shortest modular distance between two phase means, in milliseconds.
///
/// Both arguments are taken modulo 1000 ms; the result is in `0.0..=500.0`.
pub fn circ_diff_ms(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(1000.0);
    d.min(1000.0 - d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec §10 reference: R55IWATER::AMBIENT:TEMPERATURE sits at 262.016 ms
    /// with 85.6 us of spread. Forty samples split evenly either side of the
    /// mean at exactly +/- 85_600 ns have a population standard deviation of
    /// exactly 85.6 us, so the estimator must return both numbers.
    #[test]
    fn recovers_r55iwater_reference_phase() {
        let mut phase = CircularPhase::new();
        for i in 0..40u32 {
            let offset: i64 = if i % 2 == 0 { 85_600 } else { -85_600 };
            phase.push((262_016_000i64 + offset) as u32);
        }
        assert_eq!(phase.count(), 40);
        assert!(
            (phase.mean_ms() - 262.016).abs() < 1e-6,
            "mean_ms was {}",
            phase.mean_ms()
        );
        assert!(
            (phase.spread_us() - 85.6).abs() < 0.01,
            "spread_us was {}",
            phase.spread_us()
        );
        assert!(
            phase.circular_var() < 1e-6,
            "circular_var was {}",
            phase.circular_var()
        );
    }

    /// Spec §10 reference: R4WATER::DEMINWAT:PH at 695.973 ms / 196.6 us.
    #[test]
    fn recovers_r4water_reference_phase() {
        let mut phase = CircularPhase::new();
        for i in 0..30u32 {
            let offset: i64 = if i % 2 == 0 { 196_600 } else { -196_600 };
            phase.push((695_973_000i64 + offset) as u32);
        }
        assert!((phase.mean_ms() - 695.973).abs() < 1e-6);
        assert!((phase.spread_us() - 196.6).abs() < 0.01);
    }

    /// Spec §4.1: a source stamping either side of the second boundary is
    /// tightly locked, not maximally spread. A naive mean would report
    /// 500.0 ms and a naive stddev about 499.9 ms; both are wrong.
    #[test]
    fn phase_wraps_across_the_second_boundary() {
        let mut phase = CircularPhase::new();
        for _ in 0..10 {
            phase.push(999_900_000);
            phase.push(100_000);
        }
        assert!(phase.mean_ms() < 0.001 || phase.mean_ms() > 999.999);
        assert!(
            (phase.spread_us() - 100.0).abs() < 0.01,
            "spread_us was {}",
            phase.spread_us()
        );
    }

    /// Spec §2.1 caution / §4.1: uniform phase must be reported as
    /// "no signal", i.e. circular variance approaching 1.
    #[test]
    fn uniform_phase_has_no_signature() {
        let mut phase = CircularPhase::new();
        for i in 0..1000u32 {
            phase.push(i * 1_000_000);
        }
        assert!(
            phase.circular_var() > 0.99,
            "circular_var was {}",
            phase.circular_var()
        );
    }

    #[test]
    fn empty_accumulator_reports_no_signal() {
        let phase = CircularPhase::new();
        assert_eq!(phase.count(), 0);
        assert_eq!(phase.circular_var(), 1.0);
    }

    /// Spec §5.1 compares two phase means, which must be a modular
    /// difference.
    #[test]
    fn circular_difference_takes_the_short_way_round() {
        assert!((circ_diff_ms(999.9, 0.1) - 0.2).abs() < 1e-9);
        assert!((circ_diff_ms(0.1, 999.9) - 0.2).abs() < 1e-9);
        assert!((circ_diff_ms(262.016, 695.973) - 433.957).abs() < 1e-9);
        assert!((circ_diff_ms(10.0, 10.0)).abs() < 1e-12);
    }

    /// The documented range is `0.0..1000.0`, half-open, and it leaked.
    ///
    /// Two samples placed symmetrically either side of the second boundary
    /// cancel in `sum_sin` down to floating-point residue. `atan2` of that
    /// residue is a tiny negative angle, and adding `NANOS_PER_SEC` to the
    /// resulting nanosecond offset rounds to exactly `1e9` — one ulp near
    /// 1e9 is about 2.4e-7 ns, far coarser than the residue. `mean_ms` then
    /// returned `1000.0`, a value the observable cannot hold, and it reached
    /// both the InfluxDB export and the TUI.
    ///
    /// The straddling pair is the case that matters in the field: a source
    /// phase-locked on the second boundary produces exactly this input.
    #[test]
    fn the_mean_never_leaves_its_documented_range() {
        // Pairs straddling the wrap: the residue case. Every offset here
        // escaped the range before `mean_ms` enforced it.
        for k in 1..4000u32 {
            let mut acc = CircularPhase::new();
            acc.push(k);
            acc.push(1_000_000_000 - k);
            let m = acc.mean_ms();
            assert!(
                (0.0..1000.0).contains(&m),
                "mean_ms for the pair +/-{}ns escaped 0.0..1000.0: {:?}",
                k,
                m
            );
            assert!(
                m.is_sign_positive(),
                "mean_ms for the pair +/-{}ns carried a negative sign: {:?}",
                k,
                m
            );
        }

        // Single samples across the top of the range: no residue, but the
        // clamp must not distort them either.
        for nanos in (999_000_000u32..1_000_000_000).step_by(37) {
            let mut acc = CircularPhase::new();
            acc.push(nanos);
            let m = acc.mean_ms();
            assert!(
                (0.0..1000.0).contains(&m) && m.is_sign_positive(),
                "mean_ms({}) escaped 0.0..1000.0: {:?}",
                nanos,
                m
            );
        }
    }

    /// The clamp must not be a blanket "return 0.0 if anything looks odd":
    /// it has to leave a legitimate near-boundary mean alone.
    #[test]
    fn the_range_clamp_does_not_flatten_a_real_phase() {
        let mut acc = CircularPhase::new();
        for _ in 0..20 {
            acc.push(999_500_000);
        }
        let m = acc.mean_ms();
        assert!(
            (m - 999.5).abs() < 1e-6,
            "a phase at 999.5 ms was distorted to {:?}",
            m
        );
    }
}
