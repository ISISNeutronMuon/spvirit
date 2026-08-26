//! Read-path bridge: converts client-side decoded PVA values
//! ([`DecodedValue`], from a [`spvirit_client::PvaClient::pvget`] call) into
//! the codec-independent [`PvValue`]/[`NtPayload`] tree the gateway's
//! `spvirit_server::pvstore::Source::get`/`subscribe` implementations must
//! produce.
//!
//! This is the *get/subscribe* direction only. The *put* direction
//! (`DecodedValue` -> `serde_json::Value`, for forwarding downstream PUTs
//! upstream) lives in a separate `convert.rs` module (Task 12) — kept apart
//! per the Task 0 spike's recommendation, since the two conversions have
//! different lossy edge cases and no shared logic.
//!
//! See `.superpowers/sdd/2026-08-26-spvirit-gateway-m1-passthrough/task-0-report.md`
//! §2 for the full design rationale, including why this does **not** reuse
//! `spvirit_server::convert::decoded_to_scalar_value` (it misclassifies
//! numeric values as `Bool`).

use spvirit_client::{MonitorUpdate, PvGetResult};
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_types::{NtPayload, PvValue, ScalarArrayValue, ScalarValue};

/// Recursively converts a decoded PVA value into the codec-independent
/// [`PvValue`] tree used by [`NtPayload::Generic`].
///
/// Lossy only for:
/// - [`DecodedValue::Null`] (no null variant in [`ScalarValue`]; falls back
///   to `Scalar(I32(0))`, matching the fallback already used elsewhere, e.g.
///   `spvirit-server/src/group.rs:320`).
/// - [`DecodedValue::Raw`] (undecoded union/`any` bytes; represented as a
///   `U8` scalar array, losing the original type tag).
/// - [`DecodedValue::Array`] containing any `Structure`/nested `Array`
///   element (array-of-structure has no lossless `PvValue` representation —
///   see Task 0 report limitation 1). Not exercised by the M1 test suite.
pub fn decoded_to_pv_value(v: &DecodedValue) -> PvValue {
    match v {
        DecodedValue::Null => PvValue::Scalar(ScalarValue::I32(0)),
        DecodedValue::Boolean(b) => PvValue::Scalar(ScalarValue::Bool(*b)),
        DecodedValue::Int8(x) => PvValue::Scalar(ScalarValue::I8(*x)),
        DecodedValue::Int16(x) => PvValue::Scalar(ScalarValue::I16(*x)),
        DecodedValue::Int32(x) => PvValue::Scalar(ScalarValue::I32(*x)),
        DecodedValue::Int64(x) => PvValue::Scalar(ScalarValue::I64(*x)),
        DecodedValue::UInt8(x) => PvValue::Scalar(ScalarValue::U8(*x)),
        DecodedValue::UInt16(x) => PvValue::Scalar(ScalarValue::U16(*x)),
        DecodedValue::UInt32(x) => PvValue::Scalar(ScalarValue::U32(*x)),
        DecodedValue::UInt64(x) => PvValue::Scalar(ScalarValue::U64(*x)),
        DecodedValue::Float32(x) => PvValue::Scalar(ScalarValue::F32(*x)),
        DecodedValue::Float64(x) => PvValue::Scalar(ScalarValue::F64(*x)),
        DecodedValue::String(s) => PvValue::Scalar(ScalarValue::Str(s.clone())),
        DecodedValue::Structure(fields) => PvValue::Structure {
            struct_id: String::new(),
            fields: fields
                .iter()
                .map(|(n, val)| (n.clone(), decoded_to_pv_value(val)))
                .collect(),
        },
        DecodedValue::Raw(bytes) => PvValue::ScalarArray(ScalarArrayValue::U8(bytes.clone())),
        DecodedValue::Array(items) => decoded_array_to_pv_value(items),
    }
}

/// Converts a `DecodedValue::Array` payload. Homogeneous scalar arrays map
/// to `PvValue::ScalarArray`, inferring the element kind from `items[0]`
/// (empty arrays default to `ScalarArrayValue::F64(vec![])`). Arrays
/// containing a `Structure` or nested `Array` element have no lossless
/// `PvValue` target (no `PvValue::Array` variant exists — Task 0 report
/// limitation 1); for M1 these are represented as a `PvValue::Structure`
/// whose fields are the stringified element indices, so the data is at
/// least visible/inspectable rather than silently dropped.
fn decoded_array_to_pv_value(items: &[DecodedValue]) -> PvValue {
    let Some(first) = items.first() else {
        return PvValue::ScalarArray(ScalarArrayValue::F64(vec![]));
    };

    let is_homogeneous_scalar = matches!(
        first,
        DecodedValue::Null
            | DecodedValue::Boolean(_)
            | DecodedValue::Int8(_)
            | DecodedValue::Int16(_)
            | DecodedValue::Int32(_)
            | DecodedValue::Int64(_)
            | DecodedValue::UInt8(_)
            | DecodedValue::UInt16(_)
            | DecodedValue::UInt32(_)
            | DecodedValue::UInt64(_)
            | DecodedValue::Float32(_)
            | DecodedValue::Float64(_)
            | DecodedValue::String(_)
    ) && items
        .iter()
        .all(|it| std::mem::discriminant(it) == std::mem::discriminant(first));

    if !is_homogeneous_scalar {
        return PvValue::Structure {
            struct_id: String::new(),
            fields: items
                .iter()
                .enumerate()
                .map(|(i, e)| (i.to_string(), decoded_to_pv_value(e)))
                .collect(),
        };
    }

    macro_rules! collect_scalar_array {
        ($variant:ident, $conv:expr) => {
            ScalarArrayValue::$variant(items.iter().map($conv).collect())
        };
    }

    let arr = match first {
        DecodedValue::Null => {
            // Null has no ScalarArrayValue equivalent; fall back to the same
            // I32(0) lossy mapping used for a scalar Null.
            collect_scalar_array!(I32, |_| 0i32)
        }
        DecodedValue::Boolean(_) => collect_scalar_array!(Bool, |it| matches!(
            it,
            DecodedValue::Boolean(b) if *b
        )),
        DecodedValue::Int8(_) => collect_scalar_array!(I8, |it| match it {
            DecodedValue::Int8(x) => *x,
            _ => 0,
        }),
        DecodedValue::Int16(_) => collect_scalar_array!(I16, |it| match it {
            DecodedValue::Int16(x) => *x,
            _ => 0,
        }),
        DecodedValue::Int32(_) => collect_scalar_array!(I32, |it| match it {
            DecodedValue::Int32(x) => *x,
            _ => 0,
        }),
        DecodedValue::Int64(_) => collect_scalar_array!(I64, |it| match it {
            DecodedValue::Int64(x) => *x,
            _ => 0,
        }),
        DecodedValue::UInt8(_) => collect_scalar_array!(U8, |it| match it {
            DecodedValue::UInt8(x) => *x,
            _ => 0,
        }),
        DecodedValue::UInt16(_) => collect_scalar_array!(U16, |it| match it {
            DecodedValue::UInt16(x) => *x,
            _ => 0,
        }),
        DecodedValue::UInt32(_) => collect_scalar_array!(U32, |it| match it {
            DecodedValue::UInt32(x) => *x,
            _ => 0,
        }),
        DecodedValue::UInt64(_) => collect_scalar_array!(U64, |it| match it {
            DecodedValue::UInt64(x) => *x,
            _ => 0,
        }),
        DecodedValue::Float32(_) => collect_scalar_array!(F32, |it| match it {
            DecodedValue::Float32(x) => *x,
            _ => 0.0,
        }),
        DecodedValue::Float64(_) => collect_scalar_array!(F64, |it| match it {
            DecodedValue::Float64(x) => *x,
            _ => 0.0,
        }),
        DecodedValue::String(_) => collect_scalar_array!(Str, |it| match it {
            DecodedValue::String(s) => s.clone(),
            _ => String::new(),
        }),
        _ => unreachable!("is_homogeneous_scalar guards to scalar-only first elements"),
    };
    PvValue::ScalarArray(arr)
}

/// Top-level assembly: converts a completed `pvget` result into an
/// [`NtPayload::Generic`]. If the decoded value is itself a structure, its
/// fields become the payload's fields directly; otherwise the value is
/// wrapped under a synthetic `"value"` field (matching the PVA convention
/// that non-structure top-level values are unusual but must still round-trip
/// somewhere).
pub fn nt_payload_from_get(result: &PvGetResult) -> NtPayload {
    let struct_id = result.introspection.struct_id.clone().unwrap_or_default();
    nt_payload_from_decoded(&result.value, struct_id)
}

/// Shared top-level assembly for both the `get` and `subscribe` read paths:
/// wraps a (possibly delta-merged) [`DecodedValue`] into an
/// [`NtPayload::Generic`] under `struct_id`.
pub fn nt_payload_from_decoded(value: &DecodedValue, struct_id: String) -> NtPayload {
    match decoded_to_pv_value(value) {
        PvValue::Structure { fields, .. } => NtPayload::Generic { struct_id, fields },
        other => NtPayload::Generic {
            struct_id,
            fields: vec![("value".to_string(), other)],
        },
    }
}

// --- Monitor delta merge (Task 0 spike limitation #4) -----------------------
//
// `MonitorUpdate.value` from `spvirit-client`/`spvirit-codec` is a partial
// per-tick decode: only the fields whose `changed` bit is set carry their
// real value, everything else is a placeholder/default. `subscribe` must
// keep a cached last-known-full `DecodedValue` per upstream PV and merge
// each delta into it before converting to `NtPayload`, or unchanged fields
// (e.g. `alarm`, `timeStamp`) will appear to reset to defaults on every
// tick downstream.

/// True if bit `bit` is set in `changed` (LSB-first: byte = bit/8,
/// mask = 1 << (bit % 8)). Matches `spvirit_codec::monitor::select_paths`'s
/// convention exactly.
fn bit_is_set(changed: &[u8], bit: usize) -> bool {
    let byte = bit / 8;
    byte < changed.len() && (changed[byte] & (1 << (bit % 8))) != 0
}

/// Reads the value at dotted `path` inside `v`, descending through
/// `DecodedValue::Structure` fields by name. Empty path or the literal
/// `"<whole structure>"` (bit 0's path) returns `v` itself.
fn get_at_path<'a>(v: &'a DecodedValue, path: &str) -> Option<&'a DecodedValue> {
    if path.is_empty() || path == "<whole structure>" {
        return Some(v);
    }
    let mut cur = v;
    for segment in path.split('.') {
        let DecodedValue::Structure(fields) = cur else {
            return None;
        };
        cur = &fields.iter().find(|(n, _)| n == segment)?.1;
    }
    Some(cur)
}

/// Writes `val` at dotted `path` inside `target`, creating intermediate
/// `DecodedValue::Structure(vec![])` fields as needed. Only meaningful when
/// `target` (and each intermediate) is a `Structure`; a no-op otherwise.
fn set_at_path(target: &mut DecodedValue, path: &str, val: DecodedValue) {
    let segments: Vec<&str> = path.split('.').collect();
    set_at_path_segments(target, &segments, val);
}

fn set_at_path_segments(target: &mut DecodedValue, segments: &[&str], val: DecodedValue) {
    let DecodedValue::Structure(fields) = target else {
        return;
    };
    let [head, rest @ ..] = segments else {
        return;
    };
    if rest.is_empty() {
        if let Some(entry) = fields.iter_mut().find(|(n, _)| n == head) {
            entry.1 = val;
        } else {
            fields.push((head.to_string(), val));
        }
        return;
    }
    if let Some(entry) = fields.iter_mut().find(|(n, _)| n == head) {
        set_at_path_segments(&mut entry.1, rest, val);
    } else {
        let mut child = DecodedValue::Structure(vec![]);
        set_at_path_segments(&mut child, rest, val);
        fields.push((head.to_string(), child));
    }
}

/// Merges a monitor delta into the cached full value `last_full`.
///
/// - If `changed` bit 0 (the whole structure) is set, or `last_full` was
///   `DecodedValue::Null` (no prior cached value), `last_full` is replaced
///   wholesale with `update.value`.
/// - Otherwise, for each set bit `i >= 1`, the field at `update.paths[i]` is
///   read out of `update.value` and written at the same path into
///   `last_full`, leaving every other (unchanged) field of `last_full`
///   untouched.
pub fn merge_monitor_delta(last_full: &mut DecodedValue, update: &MonitorUpdate) {
    let has_prior = !matches!(last_full, DecodedValue::Null);
    if !has_prior || bit_is_set(&update.changed, 0) {
        *last_full = update.value.clone();
        return;
    }

    for (bit, path) in update.paths.iter().enumerate().skip(1) {
        if !bit_is_set(&update.changed, bit) {
            continue;
        }
        let Some(new_val) = get_at_path(&update.value, path) else {
            continue;
        };
        let new_val = new_val.clone();
        set_at_path(last_full, path, new_val);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_float64_converts_directly() {
        let v = DecodedValue::Float64(42.5);
        assert_eq!(decoded_to_pv_value(&v), PvValue::Scalar(ScalarValue::F64(42.5)));
    }

    #[test]
    fn structure_with_float64_leaf_converts() {
        let v = DecodedValue::Structure(vec![("value".to_string(), DecodedValue::Float64(22.5))]);
        let expected = PvValue::Structure {
            struct_id: String::new(),
            fields: vec![("value".to_string(), PvValue::Scalar(ScalarValue::F64(22.5)))],
        };
        assert_eq!(decoded_to_pv_value(&v), expected);
    }

    #[test]
    fn homogeneous_int32_array_converts_to_scalar_array() {
        let v = DecodedValue::Array(vec![
            DecodedValue::Int32(1),
            DecodedValue::Int32(2),
            DecodedValue::Int32(3),
        ]);
        assert_eq!(
            decoded_to_pv_value(&v),
            PvValue::ScalarArray(ScalarArrayValue::I32(vec![1, 2, 3]))
        );
    }

    #[test]
    fn empty_array_defaults_to_f64() {
        let v = DecodedValue::Array(vec![]);
        assert_eq!(
            decoded_to_pv_value(&v),
            PvValue::ScalarArray(ScalarArrayValue::F64(vec![]))
        );
    }

    #[test]
    fn null_falls_back_to_i32_zero() {
        assert_eq!(
            decoded_to_pv_value(&DecodedValue::Null),
            PvValue::Scalar(ScalarValue::I32(0))
        );
    }

    #[test]
    fn raw_bytes_become_u8_scalar_array() {
        let v = DecodedValue::Raw(vec![1, 2, 3]);
        assert_eq!(
            decoded_to_pv_value(&v),
            PvValue::ScalarArray(ScalarArrayValue::U8(vec![1, 2, 3]))
        );
    }

    fn sample_last_full() -> DecodedValue {
        DecodedValue::Structure(vec![
            ("value".to_string(), DecodedValue::Float64(1.0)),
            (
                "alarm".to_string(),
                DecodedValue::Structure(vec![
                    ("severity".to_string(), DecodedValue::Int32(0)),
                    ("message".to_string(), DecodedValue::String("ok".to_string())),
                ]),
            ),
        ])
    }

    /// Mirrors `bit_paths`' layout for the sample structure: bit 0 is the
    /// whole structure, then depth-first self-then-nested field order.
    fn sample_paths() -> Vec<String> {
        vec![
            "<whole structure>".to_string(),
            "value".to_string(),
            "alarm".to_string(),
            "alarm.severity".to_string(),
            "alarm.message".to_string(),
        ]
    }

    #[test]
    fn merge_only_touches_the_changed_field_leaving_siblings_intact() {
        let mut last_full = sample_last_full();

        // Delta claims to update "value" only (bit 1); the alarm sub-fields
        // in update.value are garbage/default, proving they must NOT be
        // copied over since their bits are unset.
        let update = MonitorUpdate {
            value: DecodedValue::Structure(vec![
                ("value".to_string(), DecodedValue::Float64(2.0)),
                (
                    "alarm".to_string(),
                    DecodedValue::Structure(vec![
                        ("severity".to_string(), DecodedValue::Int32(0)),
                        ("message".to_string(), DecodedValue::String(String::new())),
                    ]),
                ),
            ]),
            changed: vec![0b0000_0010], // bit 1 = "value"
            overrun: vec![],
            consumed: 0,
            paths: sample_paths(),
        };

        merge_monitor_delta(&mut last_full, &update);

        let DecodedValue::Structure(fields) = &last_full else {
            panic!("expected structure");
        };
        let value = &fields.iter().find(|(n, _)| n == "value").unwrap().1;
        assert!(matches!(value, DecodedValue::Float64(x) if (*x - 2.0).abs() < 1e-9));
        let alarm = fields
            .iter()
            .find_map(|(n, v)| if n == "alarm" { Some(v) } else { None })
            .unwrap();
        let DecodedValue::Structure(alarm_fields) = alarm else {
            panic!("expected alarm structure");
        };
        let message = &alarm_fields.iter().find(|(n, _)| n == "message").unwrap().1;
        assert!(
            matches!(message, DecodedValue::String(s) if s == "ok"),
            "unchanged sibling field must be preserved, not clobbered by the delta's default; got {message:?}"
        );
    }

    #[test]
    fn merge_with_bit_zero_set_replaces_wholesale() {
        let mut last_full = sample_last_full();
        let replacement =
            DecodedValue::Structure(vec![("value".to_string(), DecodedValue::Float64(9.0))]);
        let update = MonitorUpdate {
            value: replacement,
            changed: vec![0b0000_0001], // bit 0 = whole structure
            overrun: vec![],
            consumed: 0,
            paths: sample_paths(),
        };

        merge_monitor_delta(&mut last_full, &update);
        let DecodedValue::Structure(fields) = &last_full else {
            panic!("expected structure");
        };
        assert_eq!(fields.len(), 1, "alarm must be gone: wholesale replace");
        let value = &fields[0].1;
        assert!(matches!(value, DecodedValue::Float64(x) if (*x - 9.0).abs() < 1e-9));
    }

    #[test]
    fn merge_replaces_wholesale_when_no_prior_value() {
        let mut last_full = DecodedValue::Null;
        let update = MonitorUpdate {
            value: DecodedValue::Float64(5.0),
            changed: vec![0b0000_0010],
            overrun: vec![],
            consumed: 0,
            paths: sample_paths(),
        };

        merge_monitor_delta(&mut last_full, &update);
        assert!(matches!(last_full, DecodedValue::Float64(x) if (x - 5.0).abs() < 1e-9));
    }

    #[test]
    fn get_at_path_descends_nested_structures() {
        let v = sample_last_full();
        let message = get_at_path(&v, "alarm.message").expect("alarm.message present");
        assert!(matches!(message, DecodedValue::String(s) if s == "ok"));
        assert!(get_at_path(&v, "<whole structure>").is_some());
        assert!(get_at_path(&v, "missing").is_none());
    }

    #[test]
    fn set_at_path_creates_missing_intermediate_structures() {
        let mut v = DecodedValue::Structure(vec![]);
        set_at_path(&mut v, "a.b", DecodedValue::Int32(7));
        let got = get_at_path(&v, "a.b").expect("a.b present");
        assert!(matches!(got, DecodedValue::Int32(7)));
    }

    #[test]
    fn bit_is_set_matches_lsb_first_convention() {
        assert!(bit_is_set(&[0b0000_0010], 1));
        assert!(!bit_is_set(&[0b0000_0010], 0));
        assert!(!bit_is_set(&[0b0000_0010], 8)); // out of range byte
    }
}
