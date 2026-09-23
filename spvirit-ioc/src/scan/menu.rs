//! `SCAN`-field parsing — a from-scratch emulation of EPICS `menuScan`.

use std::time::Duration;

/// What a record's `SCAN` field asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanSpec {
    /// Not on any scan list; processed only when something pokes it.
    Passive,
    /// On the periodic list for this period.
    Periodic(Duration),
    /// On the event list named by the record's `EVNT` field.
    Event,
    /// On an I/O-interrupt list, bound to a source by explicit registration.
    IoIntr,
}

/// A `SCAN` value that is not a recognized menu choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanParseError {
    pub raw: String,
}

impl std::fmt::Display for ScanParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "'{}' is not a valid SCAN value", self.raw)
    }
}
impl std::error::Error for ScanParseError {}

/// The shortest periodic scan period accepted from a `SCAN` field.
///
/// `Duration::from_secs_f64(1e-300)` is exactly zero, and a record on the
/// periodic list with a zero period is a hot spin. 1 us is four orders of
/// magnitude below the fastest real EPICS scan rate, so nothing legitimate is
/// excluded.
pub const MIN_SCAN_PERIOD_SECS: f64 = 1.0e-6;

/// The longest periodic scan period accepted from a `SCAN` field.
///
/// `Duration::from_secs_f64` *panics* for finite values that do not fit a
/// `Duration` — above roughly 1.8e19 seconds (`2^64`) — and a `SCAN` field is
/// client-writable, so the bound must sit below that. 1e9 s is ~31.7 years.
/// It also keeps the period under 1e18 ns, which is where `scanner`'s
/// `period.as_nanos() as u64` list key would start to truncate.
pub const MAX_SCAN_PERIOD_SECS: f64 = 1.0e9;

/// Parse a `SCAN` field value. Case-insensitive; surrounding whitespace ignored.
pub fn parse_scan(raw: &str) -> Result<ScanSpec, ScanParseError> {
    let err = || ScanParseError { raw: raw.to_string() };
    let t = raw.trim();
    let lower = t.to_ascii_lowercase();
    match lower.as_str() {
        "passive" => return Ok(ScanSpec::Passive),
        "event" => return Ok(ScanSpec::Event),
        "i/o intr" => return Ok(ScanSpec::IoIntr),
        _ => {}
    }
    // "<number> second[s]"
    let secs_word = lower
        .strip_suffix("seconds")
        .or_else(|| lower.strip_suffix("second"))
        .ok_or_else(err)?;
    let n: f64 = secs_word.trim().parse().map_err(|_| err())?;
    // `is_finite() && n > 0.0` is not enough: `from_secs_f64` panics above
    // ~1.8e19 (a Duration cannot hold it) and rounds anything below half a
    // nanosecond to a zero Duration. A `SCAN` field is client-writable, so
    // both are remote.
    if !(n.is_finite() && (MIN_SCAN_PERIOD_SECS..=MAX_SCAN_PERIOD_SECS).contains(&n)) {
        return Err(err());
    }
    Ok(ScanSpec::Periodic(Duration::from_secs_f64(n)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn passive_and_case_insensitivity() {
        assert_eq!(parse_scan("Passive").unwrap(), ScanSpec::Passive);
        assert_eq!(parse_scan("passive").unwrap(), ScanSpec::Passive);
        assert_eq!(parse_scan("  PASSIVE  ").unwrap(), ScanSpec::Passive);
    }

    #[test]
    fn standard_periods() {
        assert_eq!(parse_scan("1 second").unwrap(), ScanSpec::Periodic(Duration::from_millis(1000)));
        assert_eq!(parse_scan(".5 second").unwrap(), ScanSpec::Periodic(Duration::from_millis(500)));
        assert_eq!(parse_scan(".1 second").unwrap(), ScanSpec::Periodic(Duration::from_millis(100)));
        assert_eq!(parse_scan("10 second").unwrap(), ScanSpec::Periodic(Duration::from_secs(10)));
    }

    #[test]
    fn custom_period_and_plural() {
        assert_eq!(parse_scan("0.25 seconds").unwrap(), ScanSpec::Periodic(Duration::from_millis(250)));
    }

    #[test]
    fn event_and_io_intr() {
        assert_eq!(parse_scan("Event").unwrap(), ScanSpec::Event);
        assert_eq!(parse_scan("I/O Intr").unwrap(), ScanSpec::IoIntr);
        assert_eq!(parse_scan("i/o intr").unwrap(), ScanSpec::IoIntr);
    }

    #[test]
    fn zero_and_garbage_are_errors() {
        assert!(parse_scan("0 second").is_err());
        assert!(parse_scan("-1 second").is_err());
        assert!(parse_scan("banana").is_err());
        assert!(parse_scan("").is_err());
    }

    /// Site 8. `Duration::from_secs_f64` panics for finite inputs that do not
    /// fit a `Duration` (above ~1.8e19 seconds), and the old guard
    /// (`is_finite() && n > 0.0`) let every one of them through. A client PUT
    /// of `SCAN = "1e30 seconds"` reaches this function via
    /// `source::put_scan`, so the panic is remote.
    ///
    /// The `1e30` assertion is deliberately FIRST: it is the one that panics
    /// pre-fix. `1e12` does *not* panic (it fits a `Duration`) — it is here
    /// because a 31 700-year period is still nonsense on a scan list, and it
    /// is the assertion that proves the new bound is `1e9`, not just "whatever
    /// `Duration` accepts".
    #[test]
    fn an_absurdly_large_period_is_rejected_not_a_panic() {
        assert!(parse_scan("1e30 seconds").is_err());
        assert!(parse_scan("1e12 seconds").is_err());
        // A huge finite literal (~1.8e38), spelled out rather than in
        // exponent form, to prove the guard is on the value and not on the
        // spelling.
        assert!(parse_scan("179769313486231570000000000000000000000 seconds").is_err());
    }

    /// The other end of the same hole: `Duration::from_secs_f64(1e-300)` is
    /// exactly zero, and a zero-period entry on the periodic list is a hot
    /// spin -- a second denial of service through the same PUT.
    #[test]
    fn an_absurdly_small_period_is_rejected_not_a_zero_duration() {
        assert!(parse_scan("1e-300 seconds").is_err());
        assert!(parse_scan("0.0000000001 seconds").is_err());
    }

    /// The bounds must not have eaten any realistic scan rate. 10 kHz is
    /// already faster than any EPICS periodic scan, and a one-hour period is
    /// a perfectly ordinary slow record.
    #[test]
    fn realistic_periods_survive_the_new_bounds() {
        assert_eq!(
            parse_scan("0.0001 seconds").unwrap(),
            ScanSpec::Periodic(Duration::from_micros(100))
        );
        assert_eq!(
            parse_scan("3600 seconds").unwrap(),
            ScanSpec::Periodic(Duration::from_secs(3600))
        );
    }

    #[test]
    fn the_scan_period_bounds_match_the_documented_values() {
        assert_eq!(MIN_SCAN_PERIOD_SECS, 1.0e-6);
        assert_eq!(MAX_SCAN_PERIOD_SECS, 1.0e9);
    }
}
