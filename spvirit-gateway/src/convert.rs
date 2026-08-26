//! Put-path bridge: converts downstream-decoded PVA values
//! ([`DecodedValue`], from a `Source::put` call) into [`serde_json::Value`]
//! for forwarding to [`spvirit_client::PvaClient::pvput`].
//!
//! This is the *put* direction only. The *get/subscribe* direction
//! (`DecodedValue` -> `PvValue`/`NtPayload`) lives in `bridge.rs` (Task 10).
//! The two conversions are kept in separate modules deliberately (per the
//! Task 0 spike's recommendation): they have different lossy edge cases and
//! no shared logic, and — critically — this module does **not** build on
//! `spvirit_server::convert::decoded_to_scalar_value`, which misclassifies
//! numeric values as `Bool`. Every variant below is matched directly against
//! `DecodedValue`.
//!
//! See `.superpowers/sdd/2026-08-26-spvirit-gateway-m1-passthrough/task-0-report.md`
//! §2 for the full rationale.

use serde_json::{Map, Number, Value};
use spvirit_codec::spvd_decode::DecodedValue;

/// Recursively converts a decoded downstream PUT value into a
/// [`serde_json::Value`] suitable for [`spvirit_client::PvaClient::pvput`].
///
/// Lossy/rejecting only for:
/// - [`DecodedValue::Raw`] (undecoded union/`any` bytes): rejected with
///   `Err`, since an unresolved union has no meaningful JSON representation
///   and silently coercing raw bytes would corrupt the write.
/// - Non-finite [`DecodedValue::Float32`]/[`DecodedValue::Float64`] (NaN or
///   +/-Infinity): `serde_json::Number` cannot represent these, so they map
///   to `Value::Null` rather than erroring or panicking. The same rule
///   applies to float elements inside an [`DecodedValue::Array`].
/// - [`DecodedValue::Array`] containing `Structure` elements (array-of-
///   structure): mapped to a JSON array of JSON objects (each element run
///   through this same function). This mirrors the read-path bridge's
///   choice to make array-of-structure visible/inspectable rather than
///   silently dropped, adapted to JSON's native array-of-object support.
///   Not exercised by the M1 test suite.
pub fn decoded_to_json(v: &DecodedValue) -> Result<Value, String> {
    match v {
        DecodedValue::Null => Ok(Value::Null),
        DecodedValue::Boolean(b) => Ok(Value::Bool(*b)),
        DecodedValue::Int8(x) => Ok(Value::Number(Number::from(*x as i64))),
        DecodedValue::Int16(x) => Ok(Value::Number(Number::from(*x as i64))),
        DecodedValue::Int32(x) => Ok(Value::Number(Number::from(*x as i64))),
        DecodedValue::Int64(x) => Ok(Value::Number(Number::from(*x))),
        DecodedValue::UInt8(x) => Ok(Value::Number(Number::from(*x as u64))),
        DecodedValue::UInt16(x) => Ok(Value::Number(Number::from(*x as u64))),
        DecodedValue::UInt32(x) => Ok(Value::Number(Number::from(*x as u64))),
        DecodedValue::UInt64(x) => Ok(Value::Number(Number::from(*x))),
        DecodedValue::Float32(x) => Ok(finite_f64_to_json(*x as f64)),
        DecodedValue::Float64(x) => Ok(finite_f64_to_json(*x)),
        DecodedValue::String(s) => Ok(Value::String(s.clone())),
        DecodedValue::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(decoded_to_json(item)?);
            }
            Ok(Value::Array(out))
        }
        DecodedValue::Structure(fields) => {
            let mut map = Map::with_capacity(fields.len());
            for (name, val) in fields {
                map.insert(name.clone(), decoded_to_json(val)?);
            }
            Ok(Value::Object(map))
        }
        DecodedValue::Raw(_) => {
            Err("cannot proxy an unresolved union/any (DecodedValue::Raw) on put".to_string())
        }
    }
}

/// Maps a finite `f64` to `Value::Number`; NaN/+-Infinity map to `Value::Null`
/// since `serde_json::Number` has no representation for them.
fn finite_f64_to_json(x: f64) -> Value {
    if x.is_finite() {
        Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
    } else {
        Value::Null
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_float64_converts_to_json_number() {
        assert_eq!(
            decoded_to_json(&DecodedValue::Float64(42.5)).unwrap(),
            Value::from(42.5)
        );
    }

    #[test]
    fn signed_int32_converts_to_json_number() {
        assert_eq!(
            decoded_to_json(&DecodedValue::Int32(-7)).unwrap(),
            Value::from(-7)
        );
    }

    #[test]
    fn unsigned_uint32_converts_to_json_number() {
        assert_eq!(
            decoded_to_json(&DecodedValue::UInt32(42)).unwrap(),
            Value::from(42u32)
        );
    }

    #[test]
    fn boolean_converts_to_json_bool_not_number() {
        let got = decoded_to_json(&DecodedValue::Boolean(true)).unwrap();
        assert_eq!(got, Value::Bool(true));
        assert!(
            got.is_boolean() && !got.is_number(),
            "guard against the decoded_to_scalar_value bug: bool must not become a number"
        );
    }

    #[test]
    fn string_converts_directly() {
        assert_eq!(
            decoded_to_json(&DecodedValue::String("hi".to_string())).unwrap(),
            Value::String("hi".to_string())
        );
    }

    #[test]
    fn scalar_int_array_converts_to_json_array() {
        let v = DecodedValue::Array(vec![
            DecodedValue::Int32(1),
            DecodedValue::Int32(2),
            DecodedValue::Int32(3),
        ]);
        assert_eq!(
            decoded_to_json(&v).unwrap(),
            Value::Array(vec![Value::from(1), Value::from(2), Value::from(3)])
        );
    }

    #[test]
    fn nan_float_becomes_json_null() {
        assert_eq!(
            decoded_to_json(&DecodedValue::Float64(f64::NAN)).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn infinite_float_becomes_json_null() {
        assert_eq!(
            decoded_to_json(&DecodedValue::Float64(f64::INFINITY)).unwrap(),
            Value::Null
        );
        assert_eq!(
            decoded_to_json(&DecodedValue::Float32(f32::NEG_INFINITY)).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn raw_bytes_are_rejected() {
        assert!(decoded_to_json(&DecodedValue::Raw(vec![1, 2, 3])).is_err());
    }

    #[test]
    fn raw_inside_array_propagates_error() {
        let v = DecodedValue::Array(vec![DecodedValue::Int32(1), DecodedValue::Raw(vec![9])]);
        assert!(decoded_to_json(&v).is_err());
    }

    #[test]
    fn structure_converts_to_json_object() {
        let v = DecodedValue::Structure(vec![
            ("value".to_string(), DecodedValue::Float64(1.5)),
            ("name".to_string(), DecodedValue::String("x".to_string())),
        ]);
        let got = decoded_to_json(&v).unwrap();
        assert_eq!(got["value"], Value::from(1.5));
        assert_eq!(got["name"], Value::from("x"));
    }
}
