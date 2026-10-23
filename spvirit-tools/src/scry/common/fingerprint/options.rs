//! Runtime configuration for the fingerprinter, as the binaries pass it down
//! into the capture loop.

use super::detect::DetectorConfig;
use super::engine::FingerprintConfig;

/// Fingerprinting knobs, threaded from the CLI into `process_packets`.
///
/// Bundled into one struct rather than added as loose parameters so that
/// later additions do not churn the `process_packets` signature again.
#[derive(Debug, Clone, PartialEq)]
pub struct FingerprintOptions {
    /// Master switch. Off by default: fingerprinting adds per-update work to
    /// the capture path, and an operator should opt into that.
    pub enabled: bool,
    pub config: FingerprintConfig,
    /// Detection rules D1-D4 (spec §5). D1 is off inside this too, and for
    /// its own reason: spec §6.2.
    pub detector: DetectorConfig,
}

impl Default for FingerprintOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            config: FingerprintConfig::default(),
            detector: DetectorConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fingerprinting must be opt-in, and its defaults must be the spec's
    /// recommendations (§4.2, §4.3).
    #[test]
    fn defaults_are_off_and_match_the_spec() {
        let o = FingerprintOptions::default();
        assert!(!o.enabled);
        assert_eq!(o.config.window_secs, 900);
        assert_eq!(o.config.min_samples, 30);
        assert_eq!(o.config.reservoir_cap, 200);
        assert_eq!(o.config.round_robin_slots, 1);
        // Spec §6.2 is unresolved, so D1 must not be on merely because
        // fingerprinting is.
        assert!(!o.detector.d1_enabled);
        assert!(o.detector.suppress_until.is_none());
    }
}
