//! Adapter from a decoded PVAccess value to the observables the fingerprint
//! estimators consume.
//!
//! Also computes the synthesised-metadata marker of spec §3.1: a `valueAlarm`
//! or `display` structure whose limits are all NaN or all zero indicates a
//! bridge reconstructing a value structure from a configuration database
//! rather than reading it from a device.
//!
//! Monitor updates are bitset-encoded, so `display` and `valueAlarm` are
//! usually absent from all but the first update in a subscription. This
//! function reports only what it can see; the engine OR-accumulates the
//! marker across a window, and a window-level `false` therefore means "never
//! observed as synthesised", not "confirmed genuine".

use spvirit_codec::spvd_decode::DecodedValue;

/// Alarm-limit field names checked for the synthesised-metadata marker.
const VALUE_ALARM_LIMITS: [&str; 4] = [
    "lowAlarmLimit",
    "lowWarningLimit",
    "highWarningLimit",
    "highAlarmLimit",
];

/// Display-range field names checked for the synthesised-metadata marker.
const DISPLAY_LIMITS: [&str; 2] = ["limitLow", "limitHigh"];

/// What one decoded monitor update contributes to a fingerprint.
#[derive(Debug, Clone, PartialEq)]
pub struct NtObservation {
    /// `timeStamp.secondsPastEpoch` — POSIX epoch, as pvData defines it.
    pub stamp_seconds: i64,
    /// `timeStamp.nanoseconds` — the sub-second phase sample (O1).
    pub stamp_nanos: u32,
    /// The scalar value, if this channel carries one (O2). `None` for
    /// waveforms, enums and strings, which still contribute O1, O3 and O4.
    pub scalar_value: Option<f64>,
    /// Spec §3.1 synthesised-metadata marker, for this update only.
    pub has_nan_limits: bool,
}

/// Extract the fingerprint observables from a decoded value.
///
/// Returns `None` when the value carries no `timeStamp`: without a stamp
/// there is no phase sample, and the update cannot be fingerprinted.
pub fn probe(value: &DecodedValue) -> Option<NtObservation> {
    let stamp = field(value, "timeStamp")?;
    let stamp_seconds = as_i64(field(stamp, "secondsPastEpoch")?)?;
    let stamp_nanos = as_i64(field(stamp, "nanoseconds")?)?;
    if !(0..1_000_000_000).contains(&stamp_nanos) {
        return None;
    }

    let scalar_value = field(value, "value").and_then(as_f64);

    let has_nan_limits = field(value, "valueAlarm")
        .map(|s| limits_are_synthetic(s, &VALUE_ALARM_LIMITS))
        .unwrap_or(false)
        || field(value, "display")
            .map(|s| limits_are_synthetic(s, &DISPLAY_LIMITS))
            .unwrap_or(false);

    Some(NtObservation {
        stamp_seconds,
        stamp_nanos: stamp_nanos as u32,
        scalar_value,
        has_nan_limits,
    })
}

/// True when every named limit present in `s` is NaN, or every one is zero.
///
/// A structure that carries none of the named limits is not evidence either
/// way and returns false.
///
/// A monitor delta can re-send this substructure with only some of its
/// limit fields marked in the substructure's own bitmask, so a partial
/// decode is possible even when the substructure itself is present. This
/// function's caller has no way to express "not determined" (`probe`
/// returns a plain `bool`), so a partial decode is deliberately treated as
/// NEGATIVE (not synthetic) rather than positive: `has_nan_limits` is
/// sticky across a window (spec §3.1), so a false positive here is
/// permanent for the whole window and corrupts D3, while a false negative
/// merely loses a marker that a later, more complete update may still
/// supply.
fn limits_are_synthetic(s: &DecodedValue, names: &[&str]) -> bool {
    let present: Vec<f64> = names.iter().filter_map(|n| field(s, n).and_then(as_f64)).collect();
    if present.len() != names.len() {
        return false;
    }
    present.iter().all(|v| v.is_nan()) || present.iter().all(|v| *v == 0.0)
}

/// Look up a named member of a decoded structure.
fn field<'a>(value: &'a DecodedValue, name: &str) -> Option<&'a DecodedValue> {
    match value {
        DecodedValue::Structure(members) => {
            members.iter().find(|(k, _)| k == name).map(|(_, v)| v)
        }
        _ => None,
    }
}

/// Coerce a numeric scalar to `f64`. Non-numeric variants yield `None`.
fn as_f64(value: &DecodedValue) -> Option<f64> {
    match value {
        DecodedValue::Float64(v) => Some(*v),
        DecodedValue::Float32(v) => Some(*v as f64),
        DecodedValue::Int8(v) => Some(*v as f64),
        DecodedValue::Int16(v) => Some(*v as f64),
        DecodedValue::Int32(v) => Some(*v as f64),
        DecodedValue::Int64(v) => Some(*v as f64),
        DecodedValue::UInt8(v) => Some(*v as f64),
        DecodedValue::UInt16(v) => Some(*v as f64),
        DecodedValue::UInt32(v) => Some(*v as f64),
        DecodedValue::UInt64(v) => Some(*v as f64),
        _ => None,
    }
}

/// Coerce an integral scalar to `i64`.
fn as_i64(value: &DecodedValue) -> Option<i64> {
    match value {
        DecodedValue::Int8(v) => Some(*v as i64),
        DecodedValue::Int16(v) => Some(*v as i64),
        DecodedValue::Int32(v) => Some(*v as i64),
        DecodedValue::Int64(v) => Some(*v),
        DecodedValue::UInt8(v) => Some(*v as i64),
        DecodedValue::UInt16(v) => Some(*v as i64),
        DecodedValue::UInt32(v) => Some(*v as i64),
        DecodedValue::UInt64(v) => i64::try_from(*v).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_codec::spvd_decode::DecodedValue;

    fn ts(seconds: i64, nanos: u32) -> (String, DecodedValue) {
        (
            "timeStamp".to_string(),
            DecodedValue::Structure(vec![
                (
                    "secondsPastEpoch".to_string(),
                    DecodedValue::Int64(seconds),
                ),
                ("nanoseconds".to_string(), DecodedValue::Int32(nanos as i32)),
                ("userTag".to_string(), DecodedValue::Int32(0)),
            ]),
        )
    }

    /// The ordinary case: an NTScalar carrying a double and a POSIX timestamp.
    #[test]
    fn extracts_value_and_timestamp_from_an_ntscalar() {
        let v = DecodedValue::Structure(vec![
            ("value".to_string(), DecodedValue::Float64(13.1625)),
            ts(1_755_756_000, 262_016_000),
        ]);
        let o = probe(&v).unwrap();
        assert_eq!(o.stamp_seconds, 1_755_756_000);
        assert_eq!(o.stamp_nanos, 262_016_000);
        assert!((o.scalar_value.unwrap() - 13.1625).abs() < 1e-12);
        assert!(!o.has_nan_limits);
    }

    /// Integer-valued channels quantise too, so integer scalars must be
    /// promoted rather than dropped.
    #[test]
    fn promotes_integer_scalars_to_f64() {
        for value in [
            DecodedValue::Int32(42),
            DecodedValue::Int64(42),
            DecodedValue::UInt32(42),
            DecodedValue::Float32(42.0),
        ] {
            let v = DecodedValue::Structure(vec![
                ("value".to_string(), value),
                ts(1, 0),
            ]);
            let o = probe(&v).unwrap();
            assert!((o.scalar_value.unwrap() - 42.0).abs() < 1e-6);
        }
    }

    /// Spec §3.1: NaN alarm limits are the synthesised-metadata marker.
    #[test]
    fn flags_nan_value_alarm_limits() {
        let v = DecodedValue::Structure(vec![
            ("value".to_string(), DecodedValue::Float64(1.0)),
            ts(1, 0),
            (
                "valueAlarm".to_string(),
                DecodedValue::Structure(vec![
                    ("active".to_string(), DecodedValue::Boolean(false)),
                    ("lowAlarmLimit".to_string(), DecodedValue::Float64(f64::NAN)),
                    (
                        "lowWarningLimit".to_string(),
                        DecodedValue::Float64(f64::NAN),
                    ),
                    (
                        "highWarningLimit".to_string(),
                        DecodedValue::Float64(f64::NAN),
                    ),
                    ("highAlarmLimit".to_string(), DecodedValue::Float64(f64::NAN)),
                ]),
            ),
        ]);
        assert!(probe(&v).unwrap().has_nan_limits);
    }

    /// Spec §3.1: an all-zero display range is the same marker by another
    /// route — a bridge that filled the structure in from a config DB.
    #[test]
    fn flags_all_zero_display_limits() {
        let v = DecodedValue::Structure(vec![
            ("value".to_string(), DecodedValue::Float64(1.0)),
            ts(1, 0),
            (
                "display".to_string(),
                DecodedValue::Structure(vec![
                    ("limitLow".to_string(), DecodedValue::Float64(0.0)),
                    ("limitHigh".to_string(), DecodedValue::Float64(0.0)),
                    ("units".to_string(), DecodedValue::String("degC".into())),
                ]),
            ),
        ]);
        assert!(probe(&v).unwrap().has_nan_limits);
    }

    /// A monitor delta can re-send `valueAlarm` with only some of its limit
    /// fields marked in the substructure's own bitmask. A partial decode
    /// carrying only the zero-valued members must not be mistaken for the
    /// synthesised-metadata marker: the verdict requires the complete set of
    /// expected limit fields, not whichever subset happened to arrive.
    #[test]
    fn does_not_flag_a_partially_decoded_value_alarm() {
        let v = DecodedValue::Structure(vec![
            ("value".to_string(), DecodedValue::Float64(1.0)),
            ts(1, 0),
            (
                "valueAlarm".to_string(),
                DecodedValue::Structure(vec![
                    ("lowAlarmLimit".to_string(), DecodedValue::Float64(0.0)),
                    ("highAlarmLimit".to_string(), DecodedValue::Float64(0.0)),
                ]),
            ),
        ]);
        assert!(!probe(&v).unwrap().has_nan_limits);
    }

    /// A genuine device-sourced display range must not be flagged.
    #[test]
    fn does_not_flag_a_real_display_range() {
        let v = DecodedValue::Structure(vec![
            ("value".to_string(), DecodedValue::Float64(1.0)),
            ts(1, 0),
            (
                "display".to_string(),
                DecodedValue::Structure(vec![
                    ("limitLow".to_string(), DecodedValue::Float64(-10.0)),
                    ("limitHigh".to_string(), DecodedValue::Float64(120.0)),
                ]),
            ),
        ]);
        assert!(!probe(&v).unwrap().has_nan_limits);
    }

    /// Bitset-encoded monitor updates usually omit display and valueAlarm
    /// entirely. Absence is not the synthesised marker.
    #[test]
    fn absent_limit_structures_are_not_a_marker() {
        let v = DecodedValue::Structure(vec![
            ("value".to_string(), DecodedValue::Float64(1.0)),
            ts(1, 0),
        ]);
        assert!(!probe(&v).unwrap().has_nan_limits);
    }

    /// Without a timeStamp there is no O1 sample, so the update is unusable.
    #[test]
    fn rejects_updates_without_a_timestamp() {
        let v = DecodedValue::Structure(vec![(
            "value".to_string(),
            DecodedValue::Float64(1.0),
        )]);
        assert!(probe(&v).is_none());
        assert!(probe(&DecodedValue::Float64(1.0)).is_none());
    }

    /// Non-scalar channels (waveforms, enums, strings) still contribute O1,
    /// O3 and O4; only the lattice is unavailable.
    #[test]
    fn keeps_timestamp_when_the_value_is_not_a_scalar() {
        let v = DecodedValue::Structure(vec![
            (
                "value".to_string(),
                DecodedValue::Array(vec![DecodedValue::Float64(1.0)]),
            ),
            ts(7, 500_000_000),
        ]);
        let o = probe(&v).unwrap();
        assert_eq!(o.stamp_nanos, 500_000_000);
        assert!(o.scalar_value.is_none());
    }
}
