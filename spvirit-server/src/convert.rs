//! Type conversions from [`DecodedValue`] to Rust scalars and `ScalarValue`.

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_types::{ScalarArrayValue, ScalarValue};

/// A float converts to an integer only when it is finite and lands strictly
/// inside the target's range. `i64::MAX as f64` rounds UP to 2^63, so the
/// upper bound has to be the power of two itself and the comparison strict.
fn f64_to_i64_checked(v: f64) -> Option<i64> {
    const MIN: f64 = -9_223_372_036_854_775_808.0; // -2^63, exactly representable
    const LIMIT: f64 = 9_223_372_036_854_775_808.0; // 2^63, one past i64::MAX
    (v.is_finite() && (MIN..LIMIT).contains(&v)).then_some(v as i64)
}

fn f64_to_u64_checked(v: f64) -> Option<u64> {
    const LIMIT: f64 = 18_446_744_073_709_551_616.0; // 2^64, one past u64::MAX
    (v.is_finite() && (0.0..LIMIT).contains(&v)).then_some(v as u64)
}

pub fn decoded_to_bool(val: &DecodedValue) -> Option<bool> {
    match val {
        DecodedValue::Boolean(v) => Some(*v),
        DecodedValue::Int8(v) => Some(*v != 0),
        DecodedValue::Int16(v) => Some(*v != 0),
        DecodedValue::Int32(v) => Some(*v != 0),
        DecodedValue::Int64(v) => Some(*v != 0),
        DecodedValue::UInt8(v) => Some(*v != 0),
        DecodedValue::UInt16(v) => Some(*v != 0),
        DecodedValue::UInt32(v) => Some(*v != 0),
        DecodedValue::UInt64(v) => Some(*v != 0),
        DecodedValue::Float32(v) => Some(*v != 0.0),
        DecodedValue::Float64(v) => Some(*v != 0.0),
        _ => None,
    }
}

pub fn decoded_to_i8(val: &DecodedValue) -> Option<i8> {
    decoded_to_i64(val).and_then(|v| i8::try_from(v).ok())
}

pub fn decoded_to_i16(val: &DecodedValue) -> Option<i16> {
    decoded_to_i64(val).and_then(|v| i16::try_from(v).ok())
}

pub fn decoded_to_i32(val: &DecodedValue) -> Option<i32> {
    match val {
        // Widening from a narrower type is always exact.
        DecodedValue::Int8(v) => Some(*v as i32),
        DecodedValue::Int16(v) => Some(*v as i32),
        DecodedValue::Int32(v) => Some(*v),
        DecodedValue::UInt8(v) => Some(*v as i32),
        DecodedValue::UInt16(v) => Some(*v as i32),
        // Narrowing rejects rather than wrapping, matching decoded_to_i8/i16.
        DecodedValue::Int64(v) => i32::try_from(*v).ok(),
        DecodedValue::UInt32(v) => i32::try_from(*v).ok(),
        DecodedValue::UInt64(v) => i32::try_from(*v).ok(),
        DecodedValue::Boolean(v) => Some(if *v { 1 } else { 0 }),
        DecodedValue::Float32(v) => {
            f64_to_i64_checked(*v as f64).and_then(|i| i32::try_from(i).ok())
        }
        DecodedValue::Float64(v) => f64_to_i64_checked(*v).and_then(|i| i32::try_from(i).ok()),
        _ => None,
    }
}

pub fn decoded_to_i64(val: &DecodedValue) -> Option<i64> {
    match val {
        DecodedValue::Int8(v) => Some(*v as i64),
        DecodedValue::Int16(v) => Some(*v as i64),
        DecodedValue::Int32(v) => Some(*v as i64),
        DecodedValue::Int64(v) => Some(*v),
        DecodedValue::UInt8(v) => Some(*v as i64),
        DecodedValue::UInt16(v) => Some(*v as i64),
        DecodedValue::UInt32(v) => Some(*v as i64),
        DecodedValue::UInt64(v) => i64::try_from(*v).ok(),
        DecodedValue::Boolean(v) => Some(if *v { 1 } else { 0 }),
        DecodedValue::Float32(v) => f64_to_i64_checked(*v as f64),
        DecodedValue::Float64(v) => f64_to_i64_checked(*v),
        _ => None,
    }
}

pub fn decoded_to_u8(val: &DecodedValue) -> Option<u8> {
    decoded_to_u64(val).and_then(|v| u8::try_from(v).ok())
}

pub fn decoded_to_u16(val: &DecodedValue) -> Option<u16> {
    decoded_to_u64(val).and_then(|v| u16::try_from(v).ok())
}

pub fn decoded_to_u32(val: &DecodedValue) -> Option<u32> {
    decoded_to_u64(val).and_then(|v| u32::try_from(v).ok())
}

pub fn decoded_to_u64(val: &DecodedValue) -> Option<u64> {
    match val {
        DecodedValue::Int8(v) => (*v >= 0).then_some(*v as u64),
        DecodedValue::Int16(v) => (*v >= 0).then_some(*v as u64),
        DecodedValue::Int32(v) => (*v >= 0).then_some(*v as u64),
        DecodedValue::Int64(v) => (*v >= 0).then_some(*v as u64),
        DecodedValue::UInt8(v) => Some(*v as u64),
        DecodedValue::UInt16(v) => Some(*v as u64),
        DecodedValue::UInt32(v) => Some(*v as u64),
        DecodedValue::UInt64(v) => Some(*v),
        DecodedValue::Boolean(v) => Some(if *v { 1 } else { 0 }),
        DecodedValue::Float32(v) => f64_to_u64_checked(*v as f64),
        DecodedValue::Float64(v) => f64_to_u64_checked(*v),
        _ => None,
    }
}

pub fn decoded_to_f32(val: &DecodedValue) -> Option<f32> {
    decoded_to_f64(val).map(|v| v as f32)
}

pub fn decoded_to_f64(val: &DecodedValue) -> Option<f64> {
    match val {
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
        DecodedValue::Boolean(v) => Some(if *v { 1.0 } else { 0.0 }),
        _ => None,
    }
}

pub fn decoded_to_string(val: &DecodedValue) -> Option<String> {
    match val {
        DecodedValue::String(s) => Some(s.clone()),
        _ => None,
    }
}

pub fn decoded_to_scalar_value(val: &DecodedValue) -> ScalarValue {
    if let Some(b) = decoded_to_bool(val) {
        return ScalarValue::Bool(b);
    }
    if let Some(i) = decoded_to_i32(val) {
        return ScalarValue::I32(i);
    }
    if let Some(f) = decoded_to_f64(val) {
        return ScalarValue::F64(f);
    }
    if let Some(s) = decoded_to_string(val) {
        return ScalarValue::Str(s);
    }
    ScalarValue::I32(0)
}

/// Convert a decoded array PUT to the template's element type.
///
/// Returns `None` if ANY element fails to convert. It must never return a
/// shorter array than it was given: a partially applied array PUT reports
/// success to the client while silently discarding and re-indexing elements.
pub fn decoded_to_scalar_array(
    val: &DecodedValue,
    template: &ScalarArrayValue,
) -> Option<ScalarArrayValue> {
    let DecodedValue::Array(items) = val else {
        return None;
    };
    match template {
        ScalarArrayValue::Bool(_) => Some(ScalarArrayValue::Bool(
            items
                .iter()
                .map(decoded_to_bool)
                .collect::<Option<Vec<_>>>()?,
        )),
        ScalarArrayValue::I8(_) => Some(ScalarArrayValue::I8(
            items
                .iter()
                .map(decoded_to_i8)
                .collect::<Option<Vec<_>>>()?,
        )),
        ScalarArrayValue::I16(_) => Some(ScalarArrayValue::I16(
            items
                .iter()
                .map(decoded_to_i16)
                .collect::<Option<Vec<_>>>()?,
        )),
        ScalarArrayValue::I32(_) => Some(ScalarArrayValue::I32(
            items
                .iter()
                .map(decoded_to_i32)
                .collect::<Option<Vec<_>>>()?,
        )),
        ScalarArrayValue::I64(_) => Some(ScalarArrayValue::I64(
            items
                .iter()
                .map(decoded_to_i64)
                .collect::<Option<Vec<_>>>()?,
        )),
        ScalarArrayValue::U8(_) => Some(ScalarArrayValue::U8(
            items
                .iter()
                .map(decoded_to_u8)
                .collect::<Option<Vec<_>>>()?,
        )),
        ScalarArrayValue::U16(_) => Some(ScalarArrayValue::U16(
            items
                .iter()
                .map(decoded_to_u16)
                .collect::<Option<Vec<_>>>()?,
        )),
        ScalarArrayValue::U32(_) => Some(ScalarArrayValue::U32(
            items
                .iter()
                .map(decoded_to_u32)
                .collect::<Option<Vec<_>>>()?,
        )),
        ScalarArrayValue::U64(_) => Some(ScalarArrayValue::U64(
            items
                .iter()
                .map(decoded_to_u64)
                .collect::<Option<Vec<_>>>()?,
        )),
        ScalarArrayValue::F32(_) => Some(ScalarArrayValue::F32(
            items
                .iter()
                .map(decoded_to_f32)
                .collect::<Option<Vec<_>>>()?,
        )),
        ScalarArrayValue::F64(_) => Some(ScalarArrayValue::F64(
            items
                .iter()
                .map(decoded_to_f64)
                .collect::<Option<Vec<_>>>()?,
        )),
        ScalarArrayValue::Str(_) => Some(ScalarArrayValue::Str(
            items
                .iter()
                .map(decoded_to_string)
                .collect::<Option<Vec<_>>>()?,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_out_of_range_element_rejects_the_whole_array_put() {
        // 999 does not fit in an i8. The PUT must be refused, not shortened.
        let put = DecodedValue::Array(vec![
            DecodedValue::Int32(1),
            DecodedValue::Int32(2),
            DecodedValue::Int32(999),
            DecodedValue::Int32(4),
        ]);
        let template = ScalarArrayValue::I8(vec![0, 0, 0, 0]);
        assert_eq!(
            decoded_to_scalar_array(&put, &template),
            None,
            "an element that does not fit must reject the whole array PUT"
        );
    }

    #[test]
    fn an_in_range_array_put_still_converts() {
        let put = DecodedValue::Array(vec![DecodedValue::Int32(1), DecodedValue::Int32(-2)]);
        let template = ScalarArrayValue::I8(vec![0, 0]);
        assert_eq!(
            decoded_to_scalar_array(&put, &template),
            Some(ScalarArrayValue::I8(vec![1, -2]))
        );
    }

    #[test]
    fn i32_rejects_a_value_that_does_not_fit_instead_of_wrapping() {
        assert_eq!(decoded_to_i32(&DecodedValue::UInt64(u64::MAX)), None);
        assert_eq!(decoded_to_i32(&DecodedValue::Int64(i64::MAX)), None);
        assert_eq!(decoded_to_i32(&DecodedValue::Int64(5)), Some(5));
    }

    #[test]
    fn i64_rejects_an_out_of_range_float_instead_of_saturating() {
        assert_eq!(decoded_to_i64(&DecodedValue::Float64(1e30)), None);
        assert_eq!(decoded_to_i64(&DecodedValue::Float64(f64::NAN)), None);
        assert_eq!(decoded_to_i64(&DecodedValue::Float64(-7.0)), Some(-7));
    }

    #[test]
    fn u64_rejects_an_out_of_range_float_instead_of_saturating() {
        assert_eq!(decoded_to_u64(&DecodedValue::Float64(1e30)), None);
        assert_eq!(decoded_to_u64(&DecodedValue::Float64(-1.0)), None);
        assert_eq!(decoded_to_u64(&DecodedValue::Float64(7.0)), Some(7));
    }

    #[test]
    fn alarm_severity_out_of_range_yields_default_not_a_wrapped_value() {
        // (1 << 40) + 3 does not fit in i32, but its low 32 bits are 3 (not
        // 0), so an `as i32` wrap would produce Some(3) -- distinguishable
        // from the default. decode_nt_alarm's field lookup must fall back to
        // the default (0), never that wrapped value.
        let val = DecodedValue::Structure(vec![
            (
                "severity".to_string(),
                DecodedValue::Int64((1i64 << 40) + 3),
            ),
            ("status".to_string(), DecodedValue::Int32(0)),
            ("message".to_string(), DecodedValue::String(String::new())),
        ]);
        let alarm = decode_nt_alarm(&val).expect("structure decodes");
        assert_eq!(
            alarm.severity, 0,
            "out-of-range severity must default, not wrap"
        );
    }

    #[test]
    fn f64_to_i64_checked_boundaries() {
        assert_eq!(f64_to_i64_checked(9_223_372_036_854_775_808.0), None); // 2^63
        assert_eq!(
            f64_to_i64_checked(-9_223_372_036_854_775_808.0),
            Some(i64::MIN)
        ); // -2^63
    }

    #[test]
    fn f64_to_u64_checked_boundary() {
        assert_eq!(f64_to_u64_checked(18_446_744_073_709_551_616.0), None); // 2^64
    }
}

/// Decode an `NtAlarm` from a decoded PVA alarm structure.
pub fn decode_nt_alarm(val: &DecodedValue) -> Option<spvirit_types::NtAlarm> {
    let DecodedValue::Structure(fields) = val else {
        return None;
    };
    let severity = fields
        .iter()
        .find(|(n, _)| n == "severity")
        .and_then(|(_, v)| decoded_to_i32(v))
        .unwrap_or(0);
    let status = fields
        .iter()
        .find(|(n, _)| n == "status")
        .and_then(|(_, v)| decoded_to_i32(v))
        .unwrap_or(0);
    let message = fields
        .iter()
        .find(|(n, _)| n == "message")
        .and_then(|(_, v)| decoded_to_string(v))
        .unwrap_or_default();
    Some(spvirit_types::NtAlarm {
        severity,
        status,
        message,
    })
}

/// Decode an `NtTimeStamp` from a decoded PVA timestamp structure.
pub fn decode_nt_timestamp(val: &DecodedValue) -> Option<spvirit_types::NtTimeStamp> {
    let DecodedValue::Structure(fields) = val else {
        return None;
    };
    let seconds = fields
        .iter()
        .find(|(n, _)| n == "secondsPastEpoch")
        .and_then(|(_, v)| decoded_to_i64(v))
        .unwrap_or(0);
    let nanos = fields
        .iter()
        .find(|(n, _)| n == "nanoseconds")
        .and_then(|(_, v)| decoded_to_i32(v))
        .unwrap_or(0);
    let user_tag = fields
        .iter()
        .find(|(n, _)| n == "userTag")
        .and_then(|(_, v)| decoded_to_i32(v))
        .unwrap_or(0);
    Some(spvirit_types::NtTimeStamp {
        seconds_past_epoch: seconds,
        nanoseconds: nanos,
        user_tag,
    })
}

/// Decode an `NtDisplay` from a decoded PVA display structure.
pub fn decode_nt_display(val: &DecodedValue) -> Option<spvirit_types::NtDisplay> {
    let DecodedValue::Structure(fields) = val else {
        return None;
    };
    let limit_low = fields
        .iter()
        .find(|(n, _)| n == "limitLow")
        .and_then(|(_, v)| decoded_to_f64(v))
        .unwrap_or(0.0);
    let limit_high = fields
        .iter()
        .find(|(n, _)| n == "limitHigh")
        .and_then(|(_, v)| decoded_to_f64(v))
        .unwrap_or(0.0);
    let description = fields
        .iter()
        .find(|(n, _)| n == "description")
        .and_then(|(_, v)| decoded_to_string(v))
        .unwrap_or_default();
    let units = fields
        .iter()
        .find(|(n, _)| n == "units")
        .and_then(|(_, v)| decoded_to_string(v))
        .unwrap_or_default();
    let precision = fields
        .iter()
        .find(|(n, _)| n == "precision")
        .and_then(|(_, v)| decoded_to_i32(v))
        .unwrap_or(0);
    Some(spvirit_types::NtDisplay {
        limit_low,
        limit_high,
        description,
        units,
        precision,
    })
}
