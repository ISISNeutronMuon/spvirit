//! Functions that apply decoded PUT values to Normative Type payloads.

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_types::*;

use crate::convert::*;
use crate::types::{now_nt_timestamp, RecordData, RecordInstance};

/// Apply a scalar value update from a decoded PUT body to an `NtScalar`.
pub fn apply_value_update(nt: &mut NtScalar, val: &DecodedValue, compute_alarms: bool) -> bool {
    if let DecodedValue::Structure(fields) = val
        && let Some((_, inner)) = fields.iter().find(|(name, _)| name == "value") {
            return apply_value_update(nt, inner, compute_alarms);
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
        // Every remaining target width. Each one uses its own exact decoder
        // rather than a shared f64 round-trip, so a 64-bit PUT keeps its full
        // magnitude and an out-of-range PUT is refused instead of saturated.
        // This is the same policy `RecordInstance::set_scalar_value` applies
        // to host-API writes; the two paths must not disagree.
        _ => {
            let applied = match &mut nt.value {
                ScalarValue::I8(c) => decoded_to_i8(val).map(|v| *c = v),
                ScalarValue::I16(c) => decoded_to_i16(val).map(|v| *c = v),
                ScalarValue::I64(c) => decoded_to_i64(val).map(|v| *c = v),
                ScalarValue::U8(c) => decoded_to_u8(val).map(|v| *c = v),
                ScalarValue::U16(c) => decoded_to_u16(val).map(|v| *c = v),
                ScalarValue::U32(c) => decoded_to_u32(val).map(|v| *c = v),
                ScalarValue::U64(c) => decoded_to_u64(val).map(|v| *c = v),
                ScalarValue::F32(c) => decoded_to_f32(val).map(|v| *c = v),
                _ => None,
            };
            if applied.is_some() {
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
///
/// Returns `Some(changed)` when the PUT was accepted (including a PUT that
/// carries no `value` field at all), and `None` when it carried a `value`
/// that is not representable in the array's element type. A refused PUT
/// leaves the array untouched.
pub fn apply_scalar_array_put(
    nt: &mut NtScalarArray,
    nord: &mut usize,
    value: &DecodedValue,
) -> Option<bool> {
    let field_value = match value {
        DecodedValue::Structure(fields) => match fields.iter().find(|(name, _)| name == "value") {
            Some((_, v)) => v,
            None => return Some(false),
        },
        _ => value,
    };
    let next = decoded_to_scalar_array(field_value, &nt.value)?;
    let changed = nt.value != next;
    if changed {
        *nord = next.len();
        nt.value = next;
    }
    Some(changed)
}

/// Apply a table PUT update to an `NtTable`.
///
/// Does not apply `timeStamp`: stamping is owned by
/// [`RecordInstance::apply_put`], which restamps every accepted PUT.
///
/// Returns `None`, leaving the table untouched, when the PUT's `value`
/// carries a column whose data is not representable in that column's
/// element type; otherwise `Some(changed)`.
pub fn apply_table_put(nt: &mut NtTable, value: &DecodedValue) -> Option<bool> {
    let DecodedValue::Structure(fields) = value else {
        return Some(false);
    };
    // Decode every addressed column before mutating anything, so a refused
    // column cannot leave the labels or an earlier column half-applied.
    let mut next_columns: Vec<(usize, ScalarArrayValue)> = Vec::new();
    for (name, field_value) in fields {
        if name != "value" {
            continue;
        }
        if let DecodedValue::Structure(cols) = field_value {
            for (col_name, col_value) in cols {
                if let Some(idx) = nt.columns.iter().position(|c| c.name == *col_name) {
                    let next = decoded_to_scalar_array(col_value, &nt.columns[idx].values)?;
                    next_columns.push((idx, next));
                }
            }
        }
    }
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
                for (idx, next) in std::mem::take(&mut next_columns) {
                    let col = &mut nt.columns[idx];
                    if col.values != next {
                        col.values = next;
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
                if let Some(alarm) = decode_nt_alarm(field_value)
                    && nt.alarm.as_ref() != Some(&alarm) {
                        nt.alarm = Some(alarm);
                        changed = true;
                    }
            }
            // `timeStamp` is not a data field: `RecordInstance::apply_put`
            // owns stamping (it always restamps on an accepted PUT) and
            // treating it here as well would inflate `value_changed` for a
            // PUT that only carries a timestamp, wrongly triggering
            // `evaluate_links` (spec rule 5). `put_nt`/`set_nt_payload`
            // replace the whole `NtTable`/`NtNdArray` directly and never call
            // this field walk, so removing the arm here does not affect them.
            _ => {}
        }
    }
    Some(changed)
}

/// Apply an NdArray PUT update to an `NtNdArray`.
///
/// Does not apply `timeStamp` or `dataTimeStamp`: stamping is owned by
/// [`RecordInstance::apply_put`], which restamps every accepted PUT and
/// honours a client-supplied `dataTimeStamp` itself.
///
/// Returns `None`, leaving the array untouched, when the PUT carries a
/// `value` that is not representable in the array's element type;
/// otherwise `Some(changed)`.
pub fn apply_ndarray_put(nt: &mut NtNdArray, value: &DecodedValue) -> Option<bool> {
    let DecodedValue::Structure(fields) = value else {
        return Some(false);
    };
    // Decode `value` before mutating anything, so a refused PUT cannot leave
    // the other fields half-applied. Only an array-shaped `value` can be
    // refused: the union-shaped `value` a wire NTNDArray PUT delivers is not
    // decoded here at all (a separate gap), and calling that "not
    // representable" would be false.
    let mut next_value = match fields.iter().find(|(name, _)| name == "value") {
        Some((_, v @ DecodedValue::Array(_))) => Some(decoded_to_scalar_array(v, &nt.value)?),
        _ => None,
    };
    let mut changed = false;
    for (name, field_value) in fields {
        match name.as_str() {
            "value" => {
                if let Some(next) = next_value.take()
                    && nt.value != next {
                        nt.value = next;
                        changed = true;
                    }
            }
            "compressedSize" => {
                if let Some(v) = decoded_to_i64(field_value)
                    && nt.compressed_size != v {
                        nt.compressed_size = v;
                        changed = true;
                    }
            }
            "uncompressedSize" => {
                if let Some(v) = decoded_to_i64(field_value)
                    && nt.uncompressed_size != v {
                        nt.uncompressed_size = v;
                        changed = true;
                    }
            }
            "uniqueId" => {
                if let Some(v) = decoded_to_i32(field_value)
                    && nt.unique_id != v {
                        nt.unique_id = v;
                        changed = true;
                    }
            }
            "codec" => {
                if let DecodedValue::Structure(codec_fields) = field_value {
                    for (cname, cval) in codec_fields {
                        if cname == "name"
                            && let Some(s) = decoded_to_string(cval)
                                && nt.codec.name != s {
                                    nt.codec.name = s;
                                    changed = true;
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
                if let Some(alarm) = decode_nt_alarm(field_value)
                    && nt.alarm.as_ref() != Some(&alarm) {
                        nt.alarm = Some(alarm);
                        changed = true;
                    }
            }
            // `timeStamp`/`dataTimeStamp` are not data fields here — see the
            // comment on the matching arm removed from `apply_table_put`.
            // `apply_put` below honours a client-supplied `dataTimeStamp`
            // separately from `timeStamp`, since a gateway PUT can carry a
            // relay `timeStamp` distinct from the acquisition
            // `dataTimeStamp` and both must survive independently.
            "display" => {
                if let Some(display) = decode_nt_display(field_value)
                    && nt.display.as_ref() != Some(&display) {
                        nt.display = Some(display);
                        changed = true;
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
    Some(changed)
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

/// Pull a usable timestamp field out of a decoded PUT body.
///
/// Returns `None` when the field is absent or decodes to the epoch-0 default:
/// that is the untouched value a client sends when it does not care about the
/// timestamp, never a real acquisition time.
fn client_timestamp_named(value: &DecodedValue, field_name: &str) -> Option<NtTimeStamp> {
    let DecodedValue::Structure(fields) = value else {
        return None;
    };
    let ts = decode_nt_timestamp(crate::convert::field(fields, field_name)?)?;
    (ts != NtTimeStamp::default()).then_some(ts)
}

/// Pull a usable `timeStamp` out of a decoded PUT body.
fn client_timestamp(value: &DecodedValue) -> Option<NtTimeStamp> {
    client_timestamp_named(value, "timeStamp")
}

/// The NdArray acquisition time, distinct from the relay `timeStamp`.
fn client_data_timestamp(value: &DecodedValue) -> Option<NtTimeStamp> {
    client_timestamp_named(value, "dataTimeStamp")
}

impl RecordInstance {
    /// Apply a decoded client PUT to this record.
    ///
    /// The record is always restamped: with the client's `timeStamp` when the
    /// PUT carried a non-default one (so a gateway can forward the
    /// originating acquisition time), otherwise with server time. EPICS Base
    /// advances TIME on every record process, so an accepted PUT that happens
    /// not to change the value still moves the timestamp.
    ///
    /// A PUT whose `value` field is not representable in the record's type
    /// (for example 300 into a `byte` record, or NaN into an integer one) is
    /// refused with `Err`: the record is left exactly as it was — not even
    /// restamped — so the caller can answer the client with an error status
    /// instead of a success. So is an enum PUT whose index is not an integer
    /// or falls outside `choices`, and any PUT to a generic-structure record
    /// (which has no wire PUT path). The `Err` text names the record and the
    /// actual reason, e.g. `"B1: 300 not representable as ubyte"`.
    pub fn apply_put(
        &mut self,
        value: &DecodedValue,
        compute_alarms: bool,
    ) -> Result<PutOutcome, String> {
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

        let value_changed = self
            .apply_put_fields(value, compute_alarms)
            .map_err(|reason| format!("{}: {reason}", self.name))?;

        let client_ts = client_timestamp(value);
        let client_stamped = client_ts.is_some();
        self.set_time_stamp(client_ts.unwrap_or_else(now_nt_timestamp));

        // NtNdArray's `dataTimeStamp` is the acquisition time and can be
        // supplied independently of `timeStamp` (the relay time) — e.g. a
        // gateway forwarding {value, timeStamp: T_relay, dataTimeStamp:
        // T_acquire}. `set_time_stamp` just set both fields from the chosen
        // `timeStamp`; override `data_time_stamp` here if the client sent its
        // own.
        if let RecordData::NtNdArray { nt, .. } = &mut self.data
            && let Some(data_ts) = client_data_timestamp(value) {
                nt.data_time_stamp = data_ts;
            }

        Ok(PutOutcome {
            value_changed,
            client_stamped,
        })
    }

    /// Apply the data-carrying fields of a PUT body. `value` is guaranteed to
    /// be a `Structure` by the caller.
    ///
    /// `Err(reason)` means the PUT was refused (for example a `value` the
    /// record's type cannot represent); nothing was modified. Otherwise
    /// `Ok(changed)`.
    fn apply_put_fields(
        &mut self,
        value: &DecodedValue,
        compute_alarms: bool,
    ) -> Result<bool, String> {
        const ARRAY_REFUSAL: &str = "value not representable in the record's element type";
        let DecodedValue::Structure(fields) = value else {
            return Ok(false);
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
                //
                // Its `false` does mean something, though: the value is not
                // representable in the record's type. Find that out on a
                // scratch copy of the value before anything is touched, so a
                // refused PUT leaves alarm/display/control untouched too,
                // while the walk below keeps the PUT's own field order.
                if let Some((_, val)) = fields.iter().find(|(name, _)| name == "value")
                    && !apply_value_update(&mut NtScalar::from_value(nt.value.clone()), val, false)
                {
                    return Err(format!(
                        "{} not representable as {}",
                        put_value_text(val),
                        scalar_type_name(&nt.value)
                    ));
                }
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
                Ok(changed || nt.value != before)
            }
            RecordData::Waveform { nt, nord, .. }
            | RecordData::Aai { nt, nord, .. }
            | RecordData::Aao { nt, nord, .. }
            | RecordData::SubArray { nt, nord, .. } => {
                apply_scalar_array_put(nt, nord, value).ok_or_else(|| ARRAY_REFUSAL.to_string())
            }
            RecordData::NtTable { nt, .. } => {
                apply_table_put(nt, value).ok_or_else(|| ARRAY_REFUSAL.to_string())
            }
            RecordData::NtNdArray { nt, .. } => {
                apply_ndarray_put(nt, value).ok_or_else(|| ARRAY_REFUSAL.to_string())
            }
            RecordData::NtEnum { nt, .. } => {
                // A wire PUT delivers `value` as the NTEnum sub-structure
                // `{index, choices}`; a bare integer (host-side callers and
                // older clients) is accepted too. A `value` that yields no
                // integer, or an index outside `choices`, is refused: `Err`
                // leaves the record untouched, so the caller answers the
                // client with an error instead of a success.
                let Some((_, val)) = fields.iter().find(|(name, _)| name == "value") else {
                    return Ok(false);
                };
                let idx = match val {
                    DecodedValue::Structure(inner) => {
                        crate::convert::field(inner, "index").and_then(decoded_to_i32)
                    }
                    other => decoded_to_i32(other),
                }
                .ok_or_else(|| {
                    format!("enum value {} is not an integer index", put_value_text(val))
                })?;
                if idx < 0 || (idx as usize) >= nt.choices.len() {
                    return Err(format!("index {idx} outside {} choices", nt.choices.len()));
                }
                let changed = nt.index != idx;
                nt.index = idx;
                Ok(changed)
            }
            // A generic structure has no wire PUT path: refuse it rather than
            // report a success that changed nothing. Write it server-side with
            // `store.put_nt()`.
            RecordData::Generic { .. } => Err("no wire PUT path for this record type".to_string()),
        }
    }
}

/// PVA type name of a scalar record value, for refusal messages.
fn scalar_type_name(v: &ScalarValue) -> &'static str {
    match v {
        ScalarValue::Bool(_) => "boolean",
        ScalarValue::I8(_) => "byte",
        ScalarValue::I16(_) => "short",
        ScalarValue::I32(_) => "int",
        ScalarValue::I64(_) => "long",
        ScalarValue::U8(_) => "ubyte",
        ScalarValue::U16(_) => "ushort",
        ScalarValue::U32(_) => "uint",
        ScalarValue::U64(_) => "ulong",
        ScalarValue::F32(_) => "float",
        ScalarValue::F64(_) => "double",
        ScalarValue::Str(_) => "string",
    }
}

/// The PUT value as shown in a refusal message. Floats print without the
/// fixed six decimals the `Display` impl uses, so NaN reads as `NaN` and 1.5
/// as `1.5`.
fn put_value_text(val: &DecodedValue) -> String {
    match val {
        DecodedValue::Float64(v) => v.to_string(),
        DecodedValue::Float32(v) => v.to_string(),
        other => other.to_string(),
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

    fn ai_record_with_alarm_limits(
        val: f64,
        low: f64,
        high: f64,
        lolo: f64,
        hihi: f64,
    ) -> RecordInstance {
        let mut nt = NtScalar::from_value(ScalarValue::F64(val)).with_alarm_limits(
            Some(low),
            Some(high),
            Some(lolo),
            Some(hihi),
        );
        nt.time_stamp = Some(OLD);
        RecordInstance {
            name: "ALM".to_string(),
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

    fn alarm_of(rec: &RecordInstance) -> (i32, String) {
        let RecordData::Ai { nt, .. } = &rec.data else {
            panic!("expected Ai");
        };
        (nt.alarm_severity, nt.alarm_message.clone())
    }

    #[test]
    fn put_with_compute_alarms_true_sets_hihi_severity() {
        let mut rec = ai_record_with_alarm_limits(5.0, 0.0, 8.0, -5.0, 15.0);
        let outcome = rec.apply_put(&put_value(20.0), true).expect("PUT accepted");

        assert!(outcome.value_changed);
        let (severity, message) = alarm_of(&rec);
        assert_eq!(severity, 2);
        assert_eq!(message, "HIHI");
    }

    #[test]
    fn put_with_compute_alarms_true_sets_high_severity() {
        let mut rec = ai_record_with_alarm_limits(5.0, 0.0, 8.0, -5.0, 15.0);
        let outcome = rec.apply_put(&put_value(10.0), true).expect("PUT accepted");

        assert!(outcome.value_changed);
        let (severity, message) = alarm_of(&rec);
        assert_eq!(severity, 1);
        assert_eq!(message, "HIGH");
    }

    #[test]
    fn put_with_compute_alarms_true_clears_alarm_within_normal_range() {
        let mut rec = ai_record_with_alarm_limits(20.0, 0.0, 8.0, -5.0, 15.0);
        // Prime a HIHI alarm, then PUT a value back in range.
        rec.apply_put(&put_value(20.0), true).expect("PUT accepted");
        assert_eq!(alarm_of(&rec).0, 2);

        let outcome = rec.apply_put(&put_value(4.0), true).expect("PUT accepted");

        assert!(outcome.value_changed);
        let (severity, message) = alarm_of(&rec);
        assert_eq!(severity, 0);
        assert_eq!(message, "");
    }

    #[test]
    fn put_with_compute_alarms_false_does_not_update_alarm() {
        let mut rec = ai_record_with_alarm_limits(5.0, 0.0, 8.0, -5.0, 15.0);
        let outcome = rec
            .apply_put(&put_value(20.0), false)
            .expect("PUT accepted");

        assert!(outcome.value_changed);
        assert_eq!(alarm_of(&rec).0, 0);
    }

    fn stamp_of(rec: &RecordInstance) -> NtTimeStamp {
        let RecordData::Ai { nt, .. } = &rec.data else {
            panic!("expected Ai");
        };
        nt.time_stamp.clone().expect("stamped")
    }

    fn ts_field(seconds: i64, nanos: i32) -> DecodedValue {
        DecodedValue::Structure(vec![
            ("secondsPastEpoch".to_string(), DecodedValue::Int64(seconds)),
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
        let outcome = rec.apply_put(&put_value(2.0), false).expect("PUT accepted");

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
        let outcome = rec.apply_put(&body, false).expect("PUT accepted");

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
        let outcome = rec.apply_put(&body, false).expect("PUT accepted");

        assert!(!outcome.client_stamped);
        assert!(stamp_of(&rec).seconds_past_epoch > OLD.seconds_past_epoch);
    }

    #[test]
    fn unchanged_value_still_restamps() {
        let mut rec = ai_record(1.0);
        let outcome = rec.apply_put(&put_value(1.0), false).expect("PUT accepted");

        assert!(!outcome.value_changed);
        assert!(stamp_of(&rec).seconds_past_epoch > OLD.seconds_past_epoch);
    }

    #[test]
    fn bare_scalar_body_is_wrapped_and_stamped() {
        let mut rec = ai_record(1.0);
        let outcome = rec
            .apply_put(&DecodedValue::Float64(3.0), false)
            .expect("PUT accepted");

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
        let body =
            DecodedValue::Structure(vec![("nosuchfield".to_string(), DecodedValue::Int32(1))]);
        let outcome = rec.apply_put(&body, false).expect("PUT accepted");

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
            DecodedValue::Array(vec![DecodedValue::Float64(3.0), DecodedValue::Float64(4.0)]),
        )]);
        let outcome = rec.apply_put(&body, false).expect("PUT accepted");

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
            DecodedValue::Array(vec![DecodedValue::Float64(1.0), DecodedValue::Float64(2.0)]),
        )]);
        let outcome = rec.apply_put(&body, false).expect("PUT accepted");

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
        let outcome = rec
            .apply_put(&DecodedValue::Int32(1), false)
            .expect("PUT accepted");

        assert!(outcome.value_changed);
        let RecordData::NtEnum { nt, .. } = &rec.data else {
            panic!("expected NtEnum");
        };
        assert!(nt.time_stamp.seconds_past_epoch > OLD.seconds_past_epoch);
    }

    fn mbbo_record() -> RecordInstance {
        let mut nt = NtEnum::new(0, vec!["A".to_string(), "B".to_string()]);
        nt.time_stamp = OLD;
        RecordInstance {
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
        }
    }

    fn enum_index(rec: &RecordInstance) -> i32 {
        let RecordData::NtEnum { nt, .. } = &rec.data else {
            panic!("expected NtEnum");
        };
        nt.index
    }

    #[test]
    fn enum_put_unwraps_the_wire_index_substructure() {
        let mut rec = mbbo_record();
        let body = DecodedValue::Structure(vec![(
            "value".to_string(),
            DecodedValue::Structure(vec![("index".to_string(), DecodedValue::Int32(1))]),
        )]);
        let outcome = rec.apply_put(&body, false).expect("PUT accepted");
        assert!(outcome.value_changed);
        assert_eq!(enum_index(&rec), 1);
    }

    #[test]
    fn enum_put_out_of_range_or_without_an_index_is_refused() {
        for value in [
            DecodedValue::Structure(vec![("index".to_string(), DecodedValue::Int32(2))]),
            DecodedValue::Int32(-1),
            DecodedValue::Structure(vec![(
                "choices".to_string(),
                DecodedValue::Array(Vec::new()),
            )]),
            DecodedValue::String("B".to_string()),
        ] {
            let mut rec = mbbo_record();
            let body = DecodedValue::Structure(vec![("value".to_string(), value.clone())]);
            assert!(rec.apply_put(&body, false).is_err(), "{value:?} must be refused");
            assert_eq!(enum_index(&rec), 0);
            let RecordData::NtEnum { nt, .. } = &rec.data else {
                unreachable!()
            };
            assert_eq!(nt.time_stamp, OLD, "a refused PUT must not restamp");
        }
    }

    #[test]
    fn generic_put_is_refused() {
        let mut rec = RecordInstance {
            name: "G".to_string(),
            record_type: RecordType::Generic,
            common: DbCommonState::default(),
            data: RecordData::Generic {
                struct_id: "x:1.0".to_string(),
                fields: Vec::new(),
                inp: None,
                out: None,
                omsl: OutputMode::Supervisory,
            },
            raw_fields: HashMap::new(),
        };
        let err = rec
            .apply_put(&DecodedValue::Int32(1), false)
            .expect_err("a generic record has no wire PUT path");
        assert_eq!(err, "G: no wire PUT path for this record type");
    }

    #[test]
    fn enum_refusals_name_the_real_reason() {
        let mut rec = mbbo_record();
        let out_of_range = DecodedValue::Structure(vec![(
            "value".to_string(),
            DecodedValue::Structure(vec![("index".to_string(), DecodedValue::Int32(5))]),
        )]);
        assert_eq!(
            rec.apply_put(&out_of_range, false).unwrap_err(),
            "E: index 5 outside 2 choices"
        );
        assert_eq!(
            rec.apply_put(&DecodedValue::String("B".to_string()), false)
                .unwrap_err(),
            "E: enum value \"B\" is not an integer index"
        );
    }

    #[test]
    fn array_refusal_still_says_not_representable() {
        let mut rec = waveform_record(vec![1.0]);
        let RecordData::Waveform { nt, .. } = &mut rec.data else {
            unreachable!()
        };
        nt.value = ScalarArrayValue::U8(vec![1]);
        let body = DecodedValue::Array(vec![DecodedValue::Int32(300)]);
        let err = rec.apply_put(&body, false).unwrap_err();
        assert!(err.contains("not representable"), "{err}");
    }

    #[test]
    fn enum_put_of_the_whole_enum_t_applies_the_index_and_keeps_choices() {
        let mut rec = mbbo_record();
        let body = DecodedValue::Structure(vec![(
            "value".to_string(),
            DecodedValue::Structure(vec![
                ("index".to_string(), DecodedValue::Int32(1)),
                (
                    "choices".to_string(),
                    DecodedValue::Array(vec![
                        DecodedValue::String("X".to_string()),
                        DecodedValue::String("Y".to_string()),
                        DecodedValue::String("Z".to_string()),
                    ]),
                ),
            ]),
        )]);
        let outcome = rec.apply_put(&body, false).expect("PUT accepted");
        assert!(outcome.value_changed);
        let RecordData::NtEnum { nt, .. } = &rec.data else {
            panic!("expected NtEnum");
        };
        assert_eq!(nt.index, 1);
        assert_eq!(nt.choices, vec!["A".to_string(), "B".to_string()]);
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
    fn ndarray_put_honours_client_supplied_data_time_stamp() {
        // A gateway forwards {value, timeStamp: T_relay, dataTimeStamp:
        // T_acquire}: the acquisition time must survive, not be overwritten
        // by the relay's timeStamp (or by now()).
        let mut rec = ndarray_record();
        let body = DecodedValue::Structure(vec![
            (
                "value".to_string(),
                DecodedValue::Array(vec![
                    DecodedValue::UInt8(5),
                    DecodedValue::UInt8(6),
                    DecodedValue::UInt8(7),
                    DecodedValue::UInt8(8),
                ]),
            ),
            ("timeStamp".to_string(), ts_field(5_000, 42)),
            ("dataTimeStamp".to_string(), ts_field(9_000, 7)),
        ]);
        let outcome = rec.apply_put(&body, false).expect("PUT accepted");

        assert!(outcome.value_changed);
        assert!(outcome.client_stamped);
        let RecordData::NtNdArray { nt, .. } = &rec.data else {
            panic!("expected NtNdArray");
        };
        assert_eq!(
            nt.time_stamp,
            Some(NtTimeStamp {
                seconds_past_epoch: 5_000,
                nanoseconds: 42,
                user_tag: 0,
            }),
            "timeStamp should be the relay's stamp"
        );
        assert_eq!(
            nt.data_time_stamp,
            NtTimeStamp {
                seconds_past_epoch: 9_000,
                nanoseconds: 7,
                user_tag: 0,
            },
            "dataTimeStamp should be the acquisition time, not overwritten by timeStamp"
        );
    }

    #[test]
    fn ndarray_put_with_only_timestamp_fields_does_not_report_value_changed() {
        // A client-stamped PUT that touches only timeStamp/dataTimeStamp must
        // not be reported as a value change: apply_put owns stamping now, so
        // the field walk must not double-count timeStamp/dataTimeStamp as
        // data fields (spec rule 5: evaluate_links runs only when the value
        // actually changed).
        let mut rec = ndarray_record();
        let body = DecodedValue::Structure(vec![
            ("timeStamp".to_string(), ts_field(5_000, 42)),
            ("dataTimeStamp".to_string(), ts_field(9_000, 7)),
        ]);
        let outcome = rec.apply_put(&body, false).expect("PUT accepted");

        assert!(!outcome.value_changed);
        assert!(outcome.client_stamped);
    }

    #[test]
    fn table_put_with_only_timestamp_field_does_not_report_value_changed() {
        let mut nt = NtTable {
            labels: vec!["a".to_string()],
            columns: vec![spvirit_types::NtTableColumn {
                name: "a".to_string(),
                values: ScalarArrayValue::F64(vec![1.0]),
            }],
            descriptor: None,
            alarm: None,
            time_stamp: Some(OLD),
        };
        nt.time_stamp = Some(OLD);
        let mut rec = RecordInstance {
            name: "TBL".to_string(),
            record_type: RecordType::NtTable,
            common: DbCommonState::default(),
            data: RecordData::NtTable {
                nt,
                inp: None,
                out: None,
                omsl: OutputMode::Supervisory,
            },
            raw_fields: HashMap::new(),
        };
        let body = DecodedValue::Structure(vec![("timeStamp".to_string(), ts_field(5_000, 42))]);
        let outcome = rec.apply_put(&body, false).expect("PUT accepted");

        assert!(!outcome.value_changed);
        assert!(outcome.client_stamped);
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
        let outcome = rec.apply_put(&body, false).expect("PUT accepted");

        assert!(outcome.value_changed);
        let RecordData::NtNdArray { nt, .. } = &rec.data else {
            panic!("expected NtNdArray");
        };
        assert!(
            nt.time_stamp.as_ref().expect("stamped").seconds_past_epoch > OLD.seconds_past_epoch
        );
        assert!(nt.data_time_stamp.seconds_past_epoch > OLD.seconds_past_epoch);
    }

    fn nt_of(value: ScalarValue) -> NtScalar {
        NtScalar::from_value(value)
    }

    #[test]
    fn a_wire_put_into_a_u64_record_is_exact_above_two_to_the_53() {
        let mut nt = nt_of(ScalarValue::U64(0));
        let big: u64 = 9_007_199_254_740_993; // 2^53 + 1, not representable in f64
        assert!(apply_value_update(
            &mut nt,
            &DecodedValue::UInt64(big),
            false
        ));
        assert_eq!(
            nt.value,
            ScalarValue::U64(big),
            "the wire PUT path must not round 64-bit integers through f64"
        );
    }

    #[test]
    fn a_wire_put_that_does_not_fit_the_target_is_refused() {
        let mut nt = nt_of(ScalarValue::U8(7));
        assert!(
            !apply_value_update(&mut nt, &DecodedValue::Int32(300), false),
            "300 does not fit a U8 record; the PUT must be refused, not saturated"
        );
        assert_eq!(
            nt.value,
            ScalarValue::U8(7),
            "a refused PUT must not mutate"
        );
    }

    #[test]
    fn a_wire_put_that_does_fit_the_target_still_applies() {
        let mut nt = nt_of(ScalarValue::I16(0));
        assert!(apply_value_update(
            &mut nt,
            &DecodedValue::Int32(-300),
            false
        ));
        assert_eq!(nt.value, ScalarValue::I16(-300));
    }

    #[test]
    fn a_wire_put_into_an_f32_record_still_applies() {
        let mut nt = nt_of(ScalarValue::F32(0.0));
        assert!(apply_value_update(
            &mut nt,
            &DecodedValue::Float64(1.5),
            false
        ));
        assert_eq!(nt.value, ScalarValue::F32(1.5));
    }

    /// An `Ai`-shaped record holding a `byte` value, stamped `OLD`.
    fn byte_record(v: i8) -> RecordInstance {
        let mut rec = ai_record(0.0);
        let RecordData::Ai { nt, .. } = &mut rec.data else {
            unreachable!()
        };
        nt.value = ScalarValue::I8(v);
        rec
    }

    #[test]
    fn apply_put_refuses_an_unrepresentable_value_without_touching_the_record() {
        let mut rec = byte_record(7);
        // The alarm field comes first, so a field walk that applied as it
        // went would already have written it before reaching the refusal.
        let body = DecodedValue::Structure(vec![
            (
                "alarm".to_string(),
                DecodedValue::Structure(vec![(
                    "message".to_string(),
                    DecodedValue::String("SHOULD NOT LAND".to_string()),
                )]),
            ),
            ("value".to_string(), DecodedValue::Int32(300)),
            ("timeStamp".to_string(), ts_field(5_000, 0)),
        ]);
        let before = rec.clone();
        let err = rec
            .apply_put(&body, true)
            .expect_err("300 does not fit a byte");
        assert_eq!(err, "T: 300 not representable as byte");
        assert_eq!(
            rec, before,
            "a refused PUT must leave the record untouched, stamp included"
        );
        assert_eq!(stamp_of(&rec), OLD);

        let nan = DecodedValue::Float64(f64::NAN);
        let err = rec
            .apply_put(&nan, true)
            .expect_err("NaN does not fit a byte");
        assert_eq!(err, "T: NaN not representable as byte");
        assert_eq!(rec, before);
    }

    #[test]
    fn apply_put_of_the_current_value_is_accepted_unchanged() {
        let mut rec = byte_record(7);
        let outcome = rec
            .apply_put(&DecodedValue::Int32(7), false)
            .expect("re-sending the current value is not a refusal");
        assert!(!outcome.value_changed);
        assert!(stamp_of(&rec).seconds_past_epoch > OLD.seconds_past_epoch);
    }

    #[test]
    fn apply_put_without_a_value_field_is_not_refused() {
        let mut rec = byte_record(7);
        let body = DecodedValue::Structure(vec![("timeStamp".to_string(), ts_field(5_000, 0))]);
        assert!(rec.apply_put(&body, false).is_ok());

        let mut wf = waveform_record(vec![1.0]);
        assert!(wf.apply_put(&body, false).is_ok());
    }

    #[test]
    fn apply_put_refuses_an_array_with_an_unrepresentable_element() {
        let mut rec = waveform_record(vec![1.0, 2.0]);
        let RecordData::Waveform { nt, .. } = &mut rec.data else {
            unreachable!()
        };
        nt.value = ScalarArrayValue::U8(vec![1, 2]);
        let before = rec.clone();
        let body = DecodedValue::Array(vec![DecodedValue::Int32(3), DecodedValue::Int32(300)]);
        assert!(rec.apply_put(&body, false).is_err());
        assert_eq!(rec, before);
    }

    #[test]
    fn apply_put_refuses_a_table_column_without_applying_any_field() {
        let mut rec = RecordInstance {
            name: "TBL".to_string(),
            record_type: RecordType::NtTable,
            common: DbCommonState::default(),
            data: RecordData::NtTable {
                nt: NtTable {
                    labels: vec!["a".to_string(), "b".to_string()],
                    columns: vec![
                        spvirit_types::NtTableColumn {
                            name: "a".to_string(),
                            values: ScalarArrayValue::F64(vec![1.0]),
                        },
                        spvirit_types::NtTableColumn {
                            name: "b".to_string(),
                            values: ScalarArrayValue::U8(vec![1]),
                        },
                    ],
                    descriptor: None,
                    alarm: None,
                    time_stamp: Some(OLD),
                },
                inp: None,
                out: None,
                omsl: OutputMode::Supervisory,
            },
            raw_fields: HashMap::new(),
        };
        let before = rec.clone();
        let body = DecodedValue::Structure(vec![
            (
                "descriptor".to_string(),
                DecodedValue::String("new".to_string()),
            ),
            (
                "value".to_string(),
                DecodedValue::Structure(vec![
                    (
                        "a".to_string(),
                        DecodedValue::Array(vec![DecodedValue::Float64(9.0)]),
                    ),
                    (
                        "b".to_string(),
                        DecodedValue::Array(vec![DecodedValue::Int32(300)]),
                    ),
                ]),
            ),
        ]);
        assert!(rec.apply_put(&body, false).is_err());
        assert_eq!(rec, before, "neither the descriptor nor column a may land");
    }

    #[test]
    fn apply_put_refuses_an_ndarray_value_array_without_applying_any_field() {
        let mut rec = ndarray_record();
        let before = rec.clone();
        let body = DecodedValue::Structure(vec![
            ("uniqueId".to_string(), DecodedValue::Int32(99)),
            (
                "value".to_string(),
                DecodedValue::Array(vec![DecodedValue::Int32(300)]),
            ),
        ]);
        assert!(rec.apply_put(&body, false).is_err());
        assert_eq!(rec, before);
    }
}
