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

use spvirit_client::PvGetResult;
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
    match decoded_to_pv_value(&result.value) {
        PvValue::Structure { fields, .. } => NtPayload::Generic { struct_id, fields },
        other => NtPayload::Generic {
            struct_id,
            fields: vec![("value".to_string(), other)],
        },
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
}
