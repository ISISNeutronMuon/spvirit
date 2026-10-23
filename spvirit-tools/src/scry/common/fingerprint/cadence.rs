//! Update cadence structure — observable O3 of the source fingerprint.
//!
//! The modal inter-update interval is the source's scan period; the tail of
//! the distribution is its deadband policy. Scan rate and deadband are
//! configured independently, so the pair discriminates between sources far
//! better than either alone (spec §2.3).

use std::collections::HashMap;

/// Width of the histogram bin used to find the modal interval, in seconds.
///
/// 50 ms is narrow enough to separate a 10 Hz scan from a 1 Hz scan and wide
/// enough to absorb ordinary scheduler jitter on a 1 s scan.
pub const MODAL_BIN_S: f64 = 0.05;

/// Summary of a channel's inter-update interval distribution.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CadenceStats {
    /// Mean of the most populous histogram bin — the apparent scan period.
    pub modal_s: f64,
    /// Median interval (nearest rank).
    pub p50_s: f64,
    /// 95th percentile interval (nearest rank) — exposes deadband gaps.
    pub p95_s: f64,
    /// Longest observed interval.
    pub max_s: f64,
    /// Number of intervals that survived filtering.
    pub count: usize,
}

/// Summarise a set of inter-update intervals, in seconds.
///
/// Non-finite and non-positive intervals are discarded: they cannot occur on
/// a monotonic capture clock, and admitting them would drag the mode to zero.
/// Returns `None` when nothing survives.
pub fn cadence_stats(intervals_s: &[f64]) -> Option<CadenceStats> {
    let mut vals: Vec<f64> = intervals_s
        .iter()
        .copied()
        .filter(|v| v.is_finite() && *v > 0.0)
        .collect();
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).expect("non-finite values filtered above"));

    // Histogram on a fixed grid, so the mode is a property of the data rather
    // than of the arrival order.
    let mut bins: HashMap<i64, (usize, f64)> = HashMap::new();
    for v in &vals {
        let bin = (v / MODAL_BIN_S).floor() as i64;
        let entry = bins.entry(bin).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += *v;
    }
    // Ties break towards the shorter interval, so the reported mode is the
    // fastest cadence consistent with the evidence.
    let (_, (best_n, best_sum)) = bins
        .iter()
        .min_by(|a, b| {
            b.1.0
                .cmp(&a.1.0)
                .then_with(|| a.0.cmp(b.0))
        })
        .expect("bins is non-empty because vals is non-empty");
    let modal_s = best_sum / *best_n as f64;

    Some(CadenceStats {
        modal_s,
        p50_s: nearest_rank(&vals, 0.50),
        p95_s: nearest_rank(&vals, 0.95),
        max_s: *vals.last().expect("vals is non-empty"),
        count: vals.len(),
    })
}

/// Nearest-rank percentile over an ascending slice: rank `ceil(p * n)`,
/// 1-indexed, clamped into range.
fn nearest_rank(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    let rank = ((p * n as f64).ceil() as usize).clamp(1, n);
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec §2.3 / §10: R55IWATER runs a 1 s scan with a deadband that opens
    /// long gaps. Thirty 1 s intervals plus a 25 s and a 207 s gap give
    /// modal 1 s, p50 1 s, p95 25 s under nearest-rank ranking.
    #[test]
    fn recovers_r55iwater_reference_cadence() {
        let mut intervals = vec![1.0f64; 30];
        intervals.push(25.0);
        intervals.push(207.0);
        let c = cadence_stats(&intervals).unwrap();
        assert_eq!(c.count, 32);
        assert!((c.modal_s - 1.0).abs() < 1e-9, "modal was {}", c.modal_s);
        assert!((c.p50_s - 1.0).abs() < 1e-9, "p50 was {}", c.p50_s);
        assert!((c.p95_s - 25.0).abs() < 1e-9, "p95 was {}", c.p95_s);
        assert!((c.max_s - 207.0).abs() < 1e-9);
    }

    /// Jitter around the scan period must land in one bin and be reported as
    /// the mean of that bin, not as whichever raw value happened to repeat.
    #[test]
    fn modal_interval_absorbs_jitter_within_a_bin() {
        let intervals = [0.98, 1.01, 0.99, 1.02, 1.0, 5.0, 5.3];
        let c = cadence_stats(&intervals).unwrap();
        assert!((c.modal_s - 1.0).abs() < 0.02, "modal was {}", c.modal_s);
    }

    /// A 10 Hz source and a 1 Hz source must not collapse into the same bin.
    #[test]
    fn distinguishes_fast_from_slow_scan_rates() {
        let fast = vec![0.1f64; 20];
        let slow = vec![1.0f64; 20];
        let f = cadence_stats(&fast).unwrap();
        let s = cadence_stats(&slow).unwrap();
        assert!((f.modal_s - 0.1).abs() < 0.01);
        assert!((s.modal_s - 1.0).abs() < 0.01);
        assert!(f.modal_s < s.modal_s);
    }

    /// A bimodal source (two scan groups) reports the more populous mode,
    /// while the percentiles still expose the second population.
    #[test]
    fn reports_the_dominant_mode_of_a_bimodal_source() {
        let mut intervals = vec![1.0f64; 15];
        intervals.extend(vec![10.0f64; 5]);
        let c = cadence_stats(&intervals).unwrap();
        assert!((c.modal_s - 1.0).abs() < 0.01);
        assert!((c.p95_s - 10.0).abs() < 1e-9);
    }

    /// Zero and negative intervals are impossible on a monotonic clock and
    /// must be discarded rather than dragging the mode to zero.
    #[test]
    fn discards_non_positive_and_nonfinite_intervals() {
        let intervals = [1.0, 0.0, -3.0, f64::NAN, 1.0, 1.0, 2.0];
        let c = cadence_stats(&intervals).unwrap();
        assert_eq!(c.count, 4);
        assert!((c.modal_s - 1.0).abs() < 0.01);
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(cadence_stats(&[]).is_none());
        assert!(cadence_stats(&[0.0, -1.0]).is_none());
    }
}
