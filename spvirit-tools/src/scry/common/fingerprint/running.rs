//! Online mean and variance (Welford), used for the arrival-minus-stamp delta
//! of observable O4.
//!
//! Arrival deltas carry a large constant clock offset with small jitter on
//! top, which is the regime where a naive sum-of-squares estimator loses
//! most of its precision.

/// Streaming first and second moments in constant memory.
#[derive(Debug, Clone, Default)]
pub struct Welford {
    n: u64,
    mean: f64,
    m2: f64,
}

impl Welford {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, x: f64) {
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        self.m2 += delta * (x - self.mean);
    }

    pub fn count(&self) -> u64 {
        self.n
    }

    /// Mean, or 0.0 when no samples have been seen.
    pub fn mean(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.mean }
    }

    /// Sample variance (`n-1`), or 0.0 when fewer than two samples have been
    /// seen. Zero rather than NaN keeps line-protocol output parseable.
    pub fn variance(&self) -> f64 {
        if self.n < 2 { 0.0 } else { self.m2 / (self.n - 1) as f64 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arrival-delta mean and variance (O4) are accumulated online, so the
    /// estimator must agree with the two-pass computation.
    #[test]
    fn matches_the_two_pass_moments() {
        let xs = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let mut w = Welford::new();
        for x in xs {
            w.push(x);
        }
        assert_eq!(w.count(), 8);
        assert!((w.mean() - 5.0).abs() < 1e-12);
        // Sample variance (n-1): sum of squared deviations is 32, over 7.
        assert!((w.variance() - 32.0 / 7.0).abs() < 1e-12, "var {}", w.variance());
    }

    /// A single sample has a defined mean and an undefined spread; report
    /// zero rather than NaN so exports stay parseable.
    #[test]
    fn degenerate_inputs_are_zero_not_nan() {
        let empty = Welford::new();
        assert_eq!(empty.count(), 0);
        assert_eq!(empty.mean(), 0.0);
        assert_eq!(empty.variance(), 0.0);

        let mut one = Welford::new();
        one.push(3.5);
        assert!((one.mean() - 3.5).abs() < 1e-12);
        assert_eq!(one.variance(), 0.0);
    }

    /// Clock offsets are large relative to the jitter being measured, which
    /// is exactly where the naive sum-of-squares estimator loses precision.
    ///
    /// 500 samples at each of two values 1 apart have a sum of squared
    /// deviations of exactly 250; `variance()` is the *sample* (n-1)
    /// estimator (consistent with `matches_the_two_pass_moments` above), so
    /// the expected value is `250 / 999`, not the population variance
    /// `250 / 1000 = 0.25`. The brief's original assertion compared against
    /// 0.25, which is off by ~2.5e-4 — far outside its own 1e-6 tolerance —
    /// because it used the population-variance divisor while the
    /// implementation (correctly, and per its own doc comment) uses n-1.
    #[test]
    fn stays_precise_with_a_large_offset() {
        let mut w = Welford::new();
        for i in 0..1000 {
            w.push(1.0e9 + (i % 2) as f64);
        }
        assert!((w.mean() - (1.0e9 + 0.5)).abs() < 1e-6);
        let expected = 250.0 / 999.0;
        assert!(
            (w.variance() - expected).abs() < 1e-6,
            "var {}",
            w.variance()
        );
    }
}
