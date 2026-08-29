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
    if !(n.is_finite() && n > 0.0) {
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
}
