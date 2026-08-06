//! Functions that apply decoded PUT values to Normative Type payloads.

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_types::*;

use crate::convert::*;
use crate::types::{RecordData, RecordInstance, now_nt_timestamp};

/// Apply a scalar value update from a decoded PUT body to an `NtScalar`.
pub fn apply_value_update(nt: &mut NtScalar, val: &DecodedValue, compute_alarms: bool) -> bool {
    if let DecodedValue::Structure(fields) = val {
        if let Some((_, inner)) = fields.iter().find(|(name, _)| name == "value") {
            return apply_value_update(nt, inner, compute_alarms);
        }
    }
    match &mut nt.value {
        ScalarValue::Bool(current) => {
            if let Some(v) = decoded_to_bool(val) {
                *current = v;
                if compute_alarms {
                    nt.update_alarm_from_value();
                }
                return true;
            }
        }
        ScalarValue::I32(current) => {
            if let Some(v) = decoded_to_i32(val) {
                *current = v;
                if compute_alarms {
                    nt.update_alarm_from_value();
                }
                return true;
            }
        }
        ScalarValue::F64(current) => {
            if let Some(v) = decoded_to_f64(val) {
                *current = v;
                if compute_alarms {
                    nt.update_alarm_from_value();
                }
                return true;
            }
        }
        ScalarValue::Str(current) => {
            if let Some(v) = decoded_to_string(val) {
                *current = v;
                if compute_alarms {
                    nt.update_alarm_from_value();
                }
                return true;
            }
        }
        _ => {
            if let Some(v) = decoded_to_f64(val) {
                match &mut nt.value {
                    ScalarValue::I8(c) => {
                        *c = v as i8;
                    }
                    ScalarValue::I16(c) => {
                        *c = v as i16;
                    }
                    ScalarValue::I64(c) => {
                        *c = v as i64;
                    }
                    ScalarValue::U8(c) => {
                        *c = v as u8;
                    }
                    ScalarValue::U16(c) => {
                        *c = v as u16;
                    }
                    ScalarValue::U32(c) => {
                        *c = v as u32;
                    }
                    ScalarValue::U64(c) => {
                        *c = v as u64;
                    }
                    ScalarValue::F32(c) => {
                        *c = v as f32;
                    }
                    _ => return false,
                }
                if compute_alarms {
                    nt.update_alarm_from_value();
                }
                return true;
            }
        }
    }
    false
}

/// Apply an alarm structure update to an `NtScalar`.
pub fn apply_alarm_update(nt: &mut NtScalar, val: &DecodedValue) -> bool {
    let DecodedValue::Structure(fields) = val else {
        return false;
    };
    let mut changed = false;
    for (name, v) in fields {
        match name.as_str() {
            "severity" => {
                if let Some(i) = decoded_to_i32(v) {
                    nt.alarm_severity = i;
                    changed = true;
                }
            }
            "status" => {
                if let Some(i) = decoded_to_i32(v) {
                    nt.alarm_status = i;
                    changed = true;
                }
            }
            "message" => {
                if let Some(s) = decoded_to_string(v) {
                    nt.alarm_message = s;
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}

/// Apply a display structure update to an `NtScalar`.
pub fn apply_display_update(nt: &mut NtScalar, val: &DecodedValue) -> bool {
    let DecodedValue::Structure(fields) = val else {
        return false;
    };
    let mut changed = false;
    for (name, v) in fields {
        match name.as_str() {
            "low" | "limitLow" => {
                if let Some(f) = decoded_to_f64(v) {
                    nt.display_low = f;
                    changed = true;
                }
            }
            "high" | "limitHigh" => {
                if let Some(f) = decoded_to_f64(v) {
                    nt.display_high = f;
                    changed = true;
                }
            }
            "description" => {
                if let Some(s) = decoded_to_string(v) {
                    nt.display_description = s;
                    changed = true;
                }
            }
            "units" => {
                if let Some(s) = decoded_to_string(v) {
                    nt.units = s;
                    changed = true;
                }
            }
            "precision" => {
                if let Some(i) = decoded_to_i32(v) {
                    nt.display_precision = i;
                    changed = true;
                }
            }
            "form" => {
                if let DecodedValue::Structure(form_fields) = v {
                    let mut updated = false;
                    for (fname, fval) in form_fields {
                        match fname.as_str() {
                            "index" => {
                                if let Some(i) = decoded_to_i32(fval) {
                                    nt.display_form_index = i;
                                    updated = true;
                                }
                            }
                            "choices" => {
                                if let DecodedValue::Array(items) = fval {
                                    let mut choices = Vec::new();
                                    for item in items {
                                        if let DecodedValue::String(s) = item {
                                            choices.push(s.clone());
                                        }
                                    }
                                    if !choices.is_empty() {
                                        nt.display_form_choices = choices;
                                        updated = true;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    if updated {
                        changed = true;
                    }
                }
            }
            _ => {}
        }
    }
    changed
}

/// Apply a control structure update to an `NtScalar`.
pub fn apply_control_update(nt: &mut NtScalar, val: &DecodedValue) -> bool {
    let DecodedValue::Structure(fields) = val else {
        return false;
    };
    let mut changed = false;
    for (name, v) in fields {
        match name.as_str() {
            "low" | "limitLow" => {
                if let Some(f) = decoded_to_f64(v) {
                    nt.control_low = f;
                    changed = true;
                }
            }
            "high" | "limitHigh" => {
                if let Some(f) = decoded_to_f64(v) {
                    nt.control_high = f;
                    changed = true;
                }
            }
            "minStep" => {
                if let Some(f) = decoded_to_f64(v) {
                    nt.control_min_step = f;
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}

/// Apply a scalar-array PUT update to an `NtScalarArray`.
pub fn apply_scalar_array_put(
    nt: &mut NtScalarArray,
    nord: &mut usize,
    value: &DecodedValue,
) -> bool {
    let field_value = match value {
        DecodedValue::Structure(fields) => fields
            .iter()
            .find(|(name, _)| name == "value")
            .map(|(_, v)| v)
            .unwrap_or(value),
        _ => value,
    };
    if let Some(next) = decoded_to_scalar_array(field_value, &nt.value) {
        let changed = nt.value != next;
        if changed {
            *nord = next.len();
            nt.value = next;
        }
        return changed;
    }
    false
}

/// Apply a table PUT update to an `NtTable`.
pub fn apply_table_put(nt: &mut NtTable, value: &DecodedValue) -> bool {
    let DecodedValue::Structure(fields) = value else {
        return false;
    };
    let mut changed = false;
    for (name, field_value) in fields {
        match name.as_str() {
            "labels" => {
                if let DecodedValue::Array(items) = field_value {
                    let labels: Vec<String> = items.iter().filter_map(decoded_to_string).collect();
                    if !labels.is_empty() && nt.labels != labels {
                        nt.labels = labels;
                        changed = true;
                    }
                }
            }
            "value" => {
                if let DecodedValue::Structure(cols) = field_value {
                    for (col_name, col_value) in cols {
                        if let Some(col) = nt.columns.iter_mut().find(|c| c.name == *col_name) {
                            if let Some(next) = decoded_to_scalar_array(col_value, &col.values) {
                                if col.values != next {
                                    col.values = next;
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }
            "descriptor" => {
                if let Some(s) = decoded_to_string(field_value) {
                    let next = if s.is_empty() { None } else { Some(s) };
                    if nt.descriptor != next {
                        nt.descriptor = next;
                        changed = true;
                    }
                }
            }
            "alarm" => {
                if let Some(alarm) = decode_nt_alarm(field_value) {
                    if nt.alarm.as_ref() != Some(&alarm) {
                        nt.alarm = Some(alarm);
                        changed = true;
                    }
                }
            }
            "timeStamp" => {
                if let Some(ts) = decode_nt_timestamp(field_value) {
                    if nt.time_stamp.as_ref() != Some(&ts) {
                        nt.time_stamp = Some(ts);
                        changed = true;
                    }
                }
            }
            _ => {}
        }
    }
    changed
}

/// Apply an NdArray PUT update to an `NtNdArray`.
pub fn apply_ndarray_put(nt: &mut NtNdArray, value: &DecodedValue) -> bool {
    let DecodedValue::Structure(fields) = value else {
        return false;
    };
    let mut changed = false;
    for (name, field_value) in fields {
        match name.as_str() {
            "value" => {
                if let Some(next) = decoded_to_scalar_array(field_value, &nt.value) {
                    if nt.value != next {
                        nt.value = next;
                        changed = true;
                    }
                }
            }
            "compressedSize" => {
                if let Some(v) = decoded_to_i64(field_value) {
                    if nt.compressed_size != v {
                        nt.compressed_size = v;
                        changed = true;
                    }
                }
            }
            "uncompressedSize" => {
                if let Some(v) = decoded_to_i64(field_value) {
                    if nt.uncompressed_size != v {
                        nt.uncompressed_size = v;
                        changed = true;
                    }
                }
            }
            "uniqueId" => {
                if let Some(v) = decoded_to_i32(field_value) {
                    if nt.unique_id != v {
                        nt.unique_id = v;
                        changed = true;
                    }
                }
            }
            "codec" => {
                if let DecodedValue::Structure(codec_fields) = field_value {
                    for (cname, cval) in codec_fields {
                        if cname == "name" {
                            if let Some(s) = decoded_to_string(cval) {
                                if nt.codec.name != s {
                                    nt.codec.name = s;
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }
            "dimension" => {
                if let DecodedValue::Array(items) = field_value {
                    let dims: Vec<NdDimension> = items
                        .iter()
                        .filter_map(|item| {
                            if let DecodedValue::Structure(fs) = item {
                                Some(NdDimension {
                                    size: fs
                                        .iter()
                                        .find(|(n, _)| n == "size")
                                        .and_then(|(_, v)| decoded_to_i32(v))
                                        .unwrap_or(0),
                                    offset: fs
                                        .iter()
                                        .find(|(n, _)| n == "offset")
                                        .and_then(|(_, v)| decoded_to_i32(v))
                                        .unwrap_or(0),
                                    full_size: fs
                                        .iter()
                                        .find(|(n, _)| n == "fullSize")
                                        .and_then(|(_, v)| decoded_to_i32(v))
                                        .unwrap_or(0),
                                    binning: fs
                                        .iter()
                                        .find(|(n, _)| n == "binning")
                                        .and_then(|(_, v)| decoded_to_i32(v))
                                        .unwrap_or(1),
                                    reverse: fs
                                        .iter()
                                        .find(|(n, _)| n == "reverse")
                                        .and_then(|(_, v)| decoded_to_bool(v))
                                        .unwrap_or(false),
                                })
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !dims.is_empty() && nt.dimension != dims {
                        nt.dimension = dims;
                        changed = true;
                    }
                }
            }
            "descriptor" => {
                if let Some(s) = decoded_to_string(field_value) {
                    let next = if s.is_empty() { None } else { Some(s) };
                    if nt.descriptor != next {
                        nt.descriptor = next;
                        changed = true;
                    }
                }
            }
            "alarm" => {
                if let Some(alarm) = decode_nt_alarm(field_value) {
                    if nt.alarm.as_ref() != Some(&alarm) {
                        nt.alarm = Some(alarm);
                        changed = true;
                    }
                }
            }
            "timeStamp" => {
                if let Some(ts) = decode_nt_timestamp(field_value) {
                    if nt.time_stamp.as_ref() != Some(&ts) {
                        nt.time_stamp = Some(ts);
                        changed = true;
                    }
                }
            }
            "dataTimeStamp" => {
                if let Some(ts) = decode_nt_timestamp(field_value) {
                    if nt.data_time_stamp != ts {
                        nt.data_time_stamp = ts;
                        changed = true;
                    }
                }
            }
            "display" => {
                if let Some(display) = decode_nt_display(field_value) {
                    if nt.display.as_ref() != Some(&display) {
                        nt.display = Some(display);
                        changed = true;
                    }
                }
            }
            "attribute" => {
                if let DecodedValue::Array(items) = field_value {
                    let attrs: Vec<NtAttribute> = items
                        .iter()
                        .filter_map(|item| {
                            if let DecodedValue::Structure(fs) = item {
                                let attr_name = fs
                                    .iter()
                                    .find(|(n, _)| n == "name")
                                    .and_then(|(_, v)| decoded_to_string(v))
                                    .unwrap_or_default();
                                let attr_value = fs
                                    .iter()
                                    .find(|(n, _)| n == "value")
                                    .map(|(_, v)| decoded_to_scalar_value(v))
                                    .unwrap_or(ScalarValue::I32(0));
                                let descriptor = fs
                                    .iter()
                                    .find(|(n, _)| n == "descriptor")
                                    .and_then(|(_, v)| decoded_to_string(v))
                                    .unwrap_or_default();
                                let source_type = fs
                                    .iter()
                                    .find(|(n, _)| n == "sourceType")
                                    .and_then(|(_, v)| decoded_to_i32(v))
                                    .unwrap_or(0);
                                let source = fs
                                    .iter()
                                    .find(|(n, _)| n == "source")
                                    .and_then(|(_, v)| decoded_to_string(v))
                                    .unwrap_or_default();
                                Some(NtAttribute {
                                    name: attr_name,
                                    value: attr_value,
                                    descriptor,
                                    source_type,
                                    source,
                                })
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !attrs.is_empty() && nt.attribute != attrs {
                        nt.attribute = attrs;
                        changed = true;
                    }
                }
            }
            _ => {}
        }
    }
    changed
}

/// What an accepted PUT did to a record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PutOutcome {
    /// Any of value/alarm/display/control actually changed.
    pub value_changed: bool,
    /// The PUT carried a usable `timeStamp`, so the record kept it instead of
    /// being stamped with server time.
    pub client_stamped: bool,
}

/// Pull a usable `timeStamp` out of a decoded PUT body.
///
/// Returns `None` when the field is absent or decodes to the epoch-0 default:
/// that is the untouched value a client sends when it does not care about the
/// timestamp, never a real acquisition time.
fn client_timestamp(value: &DecodedValue) -> Option<NtTimeStamp> {
    let DecodedValue::Structure(fields) = value else {
        return None;
    };
    let (_, field) = fields.iter().find(|(name, _)| name == "timeStamp")?;
    let ts = decode_nt_timestamp(field)?;
    (ts != NtTimeStamp::default()).then_some(ts)
}

impl RecordInstance {
    /// Apply a decoded client PUT to this record.
    ///
    /// The record is always restamped: with the client's `timeStamp` when the
    /// PUT carried a non-default one (so a gateway can forward the
    /// originating acquisition time), otherwise with server time. EPICS Base
    /// advances TIME on every record process, so an accepted PUT that happens
    /// not to change the value still moves the timestamp.
    pub fn apply_put(&mut self, value: &DecodedValue, compute_alarms: bool) -> PutOutcome {
        // A bare scalar arrives unwrapped; treat it as `{value: <scalar>}` so
        // the field walk below is the only code path.
        let wrapped;
        let value = match value {
            DecodedValue::Structure(_) => value,
            other => {
                wrapped = DecodedValue::Structure(vec![("value".to_string(), other.clone())]);
                &wrapped
            }
        };

        let value_changed = self.apply_put_fields(value, compute_alarms);

        let client_ts = client_timestamp(value);
        let client_stamped = client_ts.is_some();
        self.set_time_stamp(client_ts.unwrap_or_else(now_nt_timestamp));

        PutOutcome {
            value_changed,
            client_stamped,
        }
    }

    /// Apply the data-carrying fields of a PUT body. `value` is guaranteed to
    /// be a `Structure` by the caller.
    fn apply_put_fields(&mut self, value: &DecodedValue, compute_alarms: bool) -> bool {
        let DecodedValue::Structure(fields) = value else {
            return false;
        };

        match &mut self.data {
            RecordData::Ai { nt, .. }
            | RecordData::Ao { nt, .. }
            | RecordData::Bi { nt, .. }
            | RecordData::Bo { nt, .. }
            | RecordData::StringIn { nt, .. }
            | RecordData::StringOut { nt, .. } => {
                // `apply_value_update` always returns `true` on a successful
                // decode, even when the decoded value equals the current one
                // (it has no equality check of its own). Compare the scalar
                // before/after instead of trusting that return, so a PUT that
                // re-sends the current value is correctly reported as
                // unchanged while still being restamped by the caller.
                let before = nt.value.clone();
                let mut changed = false;
                for (name, val) in fields {
                    match name.as_str() {
                        "value" => {
                            apply_value_update(nt, val, compute_alarms);
                        }
                        "alarm" => changed |= apply_alarm_update(nt, val),
                        "display" => changed |= apply_display_update(nt, val),
                        "control" => changed |= apply_control_update(nt, val),
                        _ => {}
                    }
                }
                changed || nt.value != before
            }
            RecordData::Waveform { nt, nord, .. }
            | RecordData::Aai { nt, nord, .. }
            | RecordData::Aao { nt, nord, .. }
            | RecordData::SubArray { nt, nord, .. } => apply_scalar_array_put(nt, nord, value),
            RecordData::NtTable { nt, .. } => apply_table_put(nt, value),
            RecordData::NtNdArray { nt, .. } => apply_ndarray_put(nt, value),
            RecordData::NtEnum { nt, .. } => {
                // Accept index updates for NtEnum PVs. Known gap #1: a wire
                // PUT delivers `value` as a sub-structure, which no arm here
                // matches — deliberately left alone, see known-gaps.md.
                let mut changed = false;
                for (name, val) in fields {
                    if name != "value" {
                        continue;
                    }
                    let idx = match val {
                        DecodedValue::Int32(v) => Some(*v),
                        DecodedValue::Int64(v) => Some(*v as i32),
                        DecodedValue::Int16(v) => Some(*v as i32),
                        DecodedValue::Int8(v) => Some(*v as i32),
                        DecodedValue::Float64(v) => Some(*v as i32),
                        _ => None,
                    };
                    if let Some(idx) = idx {
                        if idx < 0 || (idx as usize) >= nt.choices.len() {
                            // out-of-range index — reject, keep value
                        } else if nt.index != idx {
                            nt.index = idx;
                            changed = true;
                        }
                    }
                }
                changed
            }
            RecordData::Generic { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DbCommonState, OutputMode, RecordData, RecordInstance, RecordType};
    use spvirit_types::{NdCodec, NdDimension, NtEnum, NtNdArray, NtScalarArray, ScalarArrayValue};
    use std::collections::HashMap;

    const OLD: NtTimeStamp = NtTimeStamp {
        seconds_past_epoch: 1_000,
        nanoseconds: 0,
        user_tag: 0,
    };

    fn ai_record(val: f64) -> RecordInstance {
        let mut nt = NtScalar::from_value(ScalarValue::F64(val));
        nt.time_stamp = Some(OLD);
        RecordInstance {
            name: "T".to_string(),
            record_type: RecordType::Ai,
            common: DbCommonState::default(),
            data: RecordData::Ai {
                nt,
                inp: None,
                siml: None,
                siol: None,
                simm: false,
            },
            raw_fields: HashMap::new(),
        }
    }

    fn stamp_of(rec: &RecordInstance) -> NtTimeStamp {
        let RecordData::Ai { nt, .. } = &rec.data else {
            panic!("expected Ai");
        };
        nt.time_stamp.clone().expect("stamped")
    }

    fn ts_field(seconds: i64, nanos: i32) -> DecodedValue {
        DecodedValue::Structure(vec![
            (
                "secondsPastEpoch".to_string(),
                DecodedValue::Int64(seconds),
            ),
            ("nanoseconds".to_string(), DecodedValue::Int32(nanos)),
            ("userTag".to_string(), DecodedValue::Int32(0)),
        ])
    }

    fn put_value(v: f64) -> DecodedValue {
        DecodedValue::Structure(vec![("value".to_string(), DecodedValue::Float64(v))])
    }

    #[test]
    fn put_without_timestamp_stamps_server_time() {
        let mut rec = ai_record(1.0);
        let outcome = rec.apply_put(&put_value(2.0), false);

        assert!(outcome.value_changed);
        assert!(!outcome.client_stamped);
        assert!(stamp_of(&rec).seconds_past_epoch > OLD.seconds_past_epoch);
    }

    #[test]
    fn put_with_client_timestamp_keeps_it_verbatim() {
        let mut rec = ai_record(1.0);
        let body = DecodedValue::Structure(vec![
            ("value".to_string(), DecodedValue::Float64(2.0)),
            ("timeStamp".to_string(), ts_field(5_000, 42)),
        ]);
        let outcome = rec.apply_put(&body, false);

        assert!(outcome.client_stamped);
        assert_eq!(
            stamp_of(&rec),
            NtTimeStamp {
                seconds_past_epoch: 5_000,
                nanoseconds: 42,
                user_tag: 0,
            }
        );
    }

    #[test]
    fn epoch_zero_client_timestamp_falls_back_to_server_time() {
        let mut rec = ai_record(1.0);
        let body = DecodedValue::Structure(vec![
            ("value".to_string(), DecodedValue::Float64(2.0)),
            ("timeStamp".to_string(), ts_field(0, 0)),
        ]);
        let outcome = rec.apply_put(&body, false);

        assert!(!outcome.client_stamped);
        assert!(stamp_of(&rec).seconds_past_epoch > OLD.seconds_past_epoch);
    }

    #[test]
    fn unchanged_value_still_restamps() {
        let mut rec = ai_record(1.0);
        let outcome = rec.apply_put(&put_value(1.0), false);

        assert!(!outcome.value_changed);
        assert!(stamp_of(&rec).seconds_past_epoch > OLD.seconds_past_epoch);
    }

    #[test]
    fn bare_scalar_body_is_wrapped_and_stamped() {
        let mut rec = ai_record(1.0);
        let outcome = rec.apply_put(&DecodedValue::Float64(3.0), false);

        assert!(outcome.value_changed);
        assert!(stamp_of(&rec).seconds_past_epoch > OLD.seconds_past_epoch);
        let RecordData::Ai { nt, .. } = &rec.data else {
            panic!("expected Ai");
        };
        assert_eq!(nt.value, ScalarValue::F64(3.0));
    }

    #[test]
    fn unrecognised_fields_still_restamp() {
        let mut rec = ai_record(1.0);
        let body = DecodedValue::Structure(vec![(
            "nosuchfield".to_string(),
            DecodedValue::Int32(1),
        )]);
        let outcome = rec.apply_put(&body, false);

        assert!(!outcome.value_changed);
        assert!(stamp_of(&rec).seconds_past_epoch > OLD.seconds_past_epoch);
    }

    fn waveform_record(vals: Vec<f64>) -> RecordInstance {
        let mut nt = NtScalarArray::from_value(ScalarArrayValue::F64(vals.clone()));
        nt.time_stamp = OLD;
        RecordInstance {
            name: "W".to_string(),
            record_type: RecordType::Waveform,
            common: DbCommonState::default(),
            data: RecordData::Waveform {
                nt,
                nord: vals.len(),
                nelm: 16,
                inp: None,
                ftvl: "DOUBLE".to_string(),
            },
            raw_fields: HashMap::new(),
        }
    }

    #[test]
    fn array_put_restamps() {
        let mut rec = waveform_record(vec![1.0, 2.0]);
        let body = DecodedValue::Structure(vec![(
            "value".to_string(),
            DecodedValue::Array(vec![
                DecodedValue::Float64(3.0),
                DecodedValue::Float64(4.0),
            ]),
        )]);
        let outcome = rec.apply_put(&body, false);

        assert!(outcome.value_changed);
        let RecordData::Waveform { nt, .. } = &rec.data else {
            panic!("expected Waveform");
        };
        assert!(nt.time_stamp.seconds_past_epoch > OLD.seconds_past_epoch);
    }

    #[test]
    fn array_put_with_unchanged_value_still_restamps() {
        let mut rec = waveform_record(vec![1.0, 2.0]);
        let body = DecodedValue::Structure(vec![(
            "value".to_string(),
            DecodedValue::Array(vec![
                DecodedValue::Float64(1.0),
                DecodedValue::Float64(2.0),
            ]),
        )]);
        let outcome = rec.apply_put(&body, false);

        assert!(!outcome.value_changed);
        let RecordData::Waveform { nt, .. } = &rec.data else {
            panic!("expected Waveform");
        };
        assert!(nt.time_stamp.seconds_past_epoch > OLD.seconds_past_epoch);
    }

    #[test]
    fn enum_put_restamps() {
        let mut nt = NtEnum::new(0, vec!["A".to_string(), "B".to_string()]);
        nt.time_stamp = OLD;
        let mut rec = RecordInstance {
            name: "E".to_string(),
            record_type: RecordType::Mbbo,
            common: DbCommonState::default(),
            data: RecordData::NtEnum {
                nt,
                inp: None,
                out: None,
                omsl: OutputMode::Supervisory,
            },
            raw_fields: HashMap::new(),
        };
        let outcome = rec.apply_put(&DecodedValue::Int32(1), false);

        assert!(outcome.value_changed);
        let RecordData::NtEnum { nt, .. } = &rec.data else {
            panic!("expected NtEnum");
        };
        assert!(nt.time_stamp.seconds_past_epoch > OLD.seconds_past_epoch);
    }

    fn ndarray_record() -> RecordInstance {
        let nt = NtNdArray {
            value: ScalarArrayValue::U8(vec![1, 2, 3, 4]),
            codec: NdCodec {
                name: "none".to_string(),
                parameters: HashMap::new(),
            },
            compressed_size: 4,
            uncompressed_size: 4,
            dimension: vec![NdDimension {
                size: 4,
                offset: 0,
                full_size: 4,
                binning: 1,
                reverse: false,
            }],
            unique_id: 1,
            data_time_stamp: OLD,
            attribute: vec![],
            descriptor: Some("ndarray".to_string()),
            alarm: None,
            time_stamp: Some(OLD),
            display: None,
        };
        RecordInstance {
            name: "N".to_string(),
            record_type: RecordType::NtNdArray,
            common: DbCommonState::default(),
            data: RecordData::NtNdArray {
                nt,
                inp: None,
                out: None,
                omsl: OutputMode::Supervisory,
            },
            raw_fields: HashMap::new(),
        }
    }

    #[test]
    fn ndarray_put_restamps_both_time_stamp_and_data_time_stamp() {
        let mut rec = ndarray_record();
        let body = DecodedValue::Structure(vec![(
            "value".to_string(),
            DecodedValue::Array(vec![
                DecodedValue::UInt8(5),
                DecodedValue::UInt8(6),
                DecodedValue::UInt8(7),
                DecodedValue::UInt8(8),
            ]),
        )]);
        let outcome = rec.apply_put(&body, false);

        assert!(outcome.value_changed);
        let RecordData::NtNdArray { nt, .. } = &rec.data else {
            panic!("expected NtNdArray");
        };
        assert!(
            nt.time_stamp
                .as_ref()
                .expect("stamped")
                .seconds_past_epoch
                > OLD.seconds_past_epoch
        );
        assert!(nt.data_time_stamp.seconds_past_epoch > OLD.seconds_past_epoch);
    }
}
