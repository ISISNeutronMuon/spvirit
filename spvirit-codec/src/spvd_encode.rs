//! PVD (pvData) Encoding Helpers
//!
//! Minimal encoder for NTScalar introspection and value updates.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::spvd_decode::{FieldDesc, FieldType, StructureDesc, TypeCode};

use spvirit_types::{
    NdDimension, NtAlarm, NtAttribute, NtDisplay, NtEnum, NtNdArray, NtPayload, NtScalar,
    NtScalarArray, NtTable, NtTableColumn, NtTimeStamp, PvValue, ScalarArrayValue, ScalarValue,
    STANDARD_FORM_CHOICES,
};

// The PVD and PVA size/string prefixes are the same wire format. There is one
// implementation, in `encode_common`; `spvirit_encode` re-exports it publicly
// as `encode_size_pva` / `encode_string_pva`. These private aliases keep this
// module's existing call sites reading `_pvd`, which matches the pvData spec
// language used throughout the file.
use crate::encode_common::{encode_size as encode_size_pvd, encode_string as encode_string_pvd};

pub fn encode_structure_desc(desc: &StructureDesc, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let struct_id = desc.struct_id.clone().unwrap_or_default();
    out.extend_from_slice(&encode_string_pvd(&struct_id, is_be));
    out.extend_from_slice(&encode_size_pvd(desc.fields.len(), is_be));
    for field in &desc.fields {
        out.extend_from_slice(&encode_field_desc(field, is_be));
    }
    out
}

fn encode_field_desc(field: &FieldDesc, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_string_pvd(&field.name, is_be));
    out.extend_from_slice(&encode_type_desc(&field.field_type, is_be));
    out
}

fn encode_type_desc(field_type: &FieldType, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    match field_type {
        FieldType::Structure(desc) => {
            out.push(0x80);
            out.extend_from_slice(&encode_structure_desc(desc, is_be));
        }
        FieldType::StructureArray(desc) => {
            out.push(0x88);
            out.push(0x80); // inner structure element tag
            out.extend_from_slice(&encode_structure_desc(desc, is_be));
        }
        FieldType::Union(fields) => {
            out.push(0x81);
            let desc = StructureDesc {
                struct_id: None,
                fields: fields.clone(),
            };
            out.extend_from_slice(&encode_structure_desc(&desc, is_be));
        }
        FieldType::UnionArray(fields) => {
            out.push(0x89);
            out.push(0x81); // inner union element tag
            let desc = StructureDesc {
                struct_id: None,
                fields: fields.clone(),
            };
            out.extend_from_slice(&encode_structure_desc(&desc, is_be));
        }
        FieldType::Variant => out.push(0x82),
        FieldType::VariantArray => out.push(0x8A),
        FieldType::BoundedString(bound) => {
            out.push(0x83);
            out.extend_from_slice(&encode_size_pvd(*bound as usize, is_be));
        }
        FieldType::String => out.push(0x60),
        FieldType::StringArray => out.push(0x68),
        FieldType::Scalar(tc) => out.push(*tc as u8),
        FieldType::ScalarArray(tc) => out.push((*tc as u8) | 0x08),
    }
    out
}

fn encode_scalar_value(value: &ScalarValue, is_be: bool) -> Vec<u8> {
    match value {
        ScalarValue::Bool(v) => vec![if *v { 1 } else { 0 }],
        ScalarValue::I8(v) => vec![*v as u8],
        ScalarValue::I16(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        ScalarValue::I32(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        ScalarValue::I64(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        ScalarValue::U8(v) => vec![*v],
        ScalarValue::U16(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        ScalarValue::U32(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        ScalarValue::U64(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        ScalarValue::F32(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        ScalarValue::F64(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        ScalarValue::Str(v) => encode_string_pvd(v, is_be),
    }
}

fn encode_alarm(nt: &NtScalar, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_i32(nt.alarm_severity, is_be));
    out.extend_from_slice(&encode_i32(nt.alarm_status, is_be));
    out.extend_from_slice(&encode_string_pvd(&nt.alarm_message, is_be));
    out
}

fn encode_bool(value: bool) -> Vec<u8> {
    vec![if value { 1 } else { 0 }]
}

fn encode_string_array<S: AsRef<str>>(values: &[S], is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_size_pvd(values.len(), is_be));
    for v in values {
        out.extend_from_slice(&encode_string_pvd(v.as_ref(), is_be));
    }
    out
}

fn encode_enum<S: AsRef<str>>(index: i32, choices: &[S], is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_i32(index, is_be));
    out.extend_from_slice(&encode_string_array(choices, is_be));
    out
}

fn encode_timestamp(nt: &NtScalar, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();

    // Prefer an explicit, caller-supplied timestamp. Deriving `now()` here is
    // unstable: the monitor delta path re-encodes both the previous and next
    // snapshots at send time, so two `now()` samples taken microseconds apart
    // leave `secondsPastEpoch` identical (never flagged changed) while
    // `nanoseconds` differs (spuriously flagged) — freezing the seconds on the
    // client. A stored `time_stamp` is stable across encodes and fixes this.
    let (seconds_past_epoch, nanos, user_tag) = match &nt.time_stamp {
        Some(ts) => (ts.seconds_past_epoch, ts.nanoseconds, ts.user_tag),
        None => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            (now.as_secs() as i64, now.subsec_nanos() as i32, 0)
        }
    };

    out.extend_from_slice(&encode_i64(seconds_past_epoch, is_be));
    out.extend_from_slice(&encode_i32(nanos, is_be));
    out.extend_from_slice(&encode_i32(user_tag, is_be));
    out
}

fn encode_display(nt: &NtScalar, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_f64(nt.display_low, is_be));
    out.extend_from_slice(&encode_f64(nt.display_high, is_be));
    out.extend_from_slice(&encode_string_pvd(&nt.display_description, is_be));
    out.extend_from_slice(&encode_string_pvd(&nt.units, is_be));
    out.extend_from_slice(&encode_i32(nt.display_precision, is_be));
    // Empty `display_form_choices` is the "standard choices" sentinel: fall
    // back to the 7 standard choices so the wire bytes are identical to an
    // explicitly-populated NtScalar, without allocating them per value.
    if nt.display_form_choices.is_empty() {
        out.extend_from_slice(&encode_enum(
            nt.display_form_index,
            &STANDARD_FORM_CHOICES[..],
            is_be,
        ));
    } else {
        out.extend_from_slice(&encode_enum(
            nt.display_form_index,
            &nt.display_form_choices,
            is_be,
        ));
    }
    out
}

fn encode_control(nt: &NtScalar, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_f64(nt.control_low, is_be));
    out.extend_from_slice(&encode_f64(nt.control_high, is_be));
    out.extend_from_slice(&encode_f64(nt.control_min_step, is_be));
    out
}

fn encode_value_alarm(nt: &NtScalar, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_bool(nt.value_alarm_active));
    out.extend_from_slice(&encode_f64(nt.value_alarm_low_alarm_limit, is_be));
    out.extend_from_slice(&encode_f64(nt.value_alarm_low_warning_limit, is_be));
    out.extend_from_slice(&encode_f64(nt.value_alarm_high_warning_limit, is_be));
    out.extend_from_slice(&encode_f64(nt.value_alarm_high_alarm_limit, is_be));
    out.extend_from_slice(&encode_i32(nt.value_alarm_low_alarm_severity, is_be));
    out.extend_from_slice(&encode_i32(nt.value_alarm_low_warning_severity, is_be));
    out.extend_from_slice(&encode_i32(nt.value_alarm_high_warning_severity, is_be));
    out.extend_from_slice(&encode_i32(nt.value_alarm_high_alarm_severity, is_be));
    out.push(nt.value_alarm_hysteresis);
    out
}

fn encode_i32(value: i32, is_be: bool) -> Vec<u8> {
    if is_be {
        value.to_be_bytes().to_vec()
    } else {
        value.to_le_bytes().to_vec()
    }
}

fn encode_i64(value: i64, is_be: bool) -> Vec<u8> {
    if is_be {
        value.to_be_bytes().to_vec()
    } else {
        value.to_le_bytes().to_vec()
    }
}

fn encode_f64(value: f64, is_be: bool) -> Vec<u8> {
    if is_be {
        value.to_be_bytes().to_vec()
    } else {
        value.to_le_bytes().to_vec()
    }
}

pub fn nt_scalar_desc(value: &ScalarValue) -> StructureDesc {
    let value_type = match value {
        ScalarValue::Bool(_) => FieldType::Scalar(TypeCode::Boolean),
        ScalarValue::I8(_) => FieldType::Scalar(TypeCode::Int8),
        ScalarValue::I16(_) => FieldType::Scalar(TypeCode::Int16),
        ScalarValue::I32(_) => FieldType::Scalar(TypeCode::Int32),
        ScalarValue::I64(_) => FieldType::Scalar(TypeCode::Int64),
        ScalarValue::U8(_) => FieldType::Scalar(TypeCode::UInt8),
        ScalarValue::U16(_) => FieldType::Scalar(TypeCode::UInt16),
        ScalarValue::U32(_) => FieldType::Scalar(TypeCode::UInt32),
        ScalarValue::U64(_) => FieldType::Scalar(TypeCode::UInt64),
        ScalarValue::F32(_) => FieldType::Scalar(TypeCode::Float32),
        ScalarValue::F64(_) => FieldType::Scalar(TypeCode::Float64),
        ScalarValue::Str(_) => FieldType::String,
    };

    StructureDesc {
        struct_id: Some("epics:nt/NTScalar:1.0".to_string()),
        fields: vec![
            FieldDesc {
                name: "value".to_string(),
                field_type: value_type,
            },
            FieldDesc {
                name: "alarm".to_string(),
                field_type: FieldType::Structure(alarm_desc()),
            },
            FieldDesc {
                name: "timeStamp".to_string(),
                field_type: FieldType::Structure(timestamp_desc()),
            },
            FieldDesc {
                name: "display".to_string(),
                field_type: FieldType::Structure(display_desc()),
            },
            FieldDesc {
                name: "control".to_string(),
                field_type: FieldType::Structure(control_desc()),
            },
            FieldDesc {
                name: "valueAlarm".to_string(),
                field_type: FieldType::Structure(value_alarm_desc()),
            },
        ],
    }
}

/// Append the full `epics:nt/NTScalar:1.0` value block to `out`.
///
/// Sink form: the caller's frame buffer is written directly, so a multi-MB
/// body is not built in a temporary and then copied.
pub(crate) fn push_nt_scalar_full(out: &mut Vec<u8>, nt: &NtScalar, is_be: bool) {
    out.extend_from_slice(&encode_scalar_value(&nt.value, is_be));
    out.extend_from_slice(&encode_alarm(nt, is_be));
    out.extend_from_slice(&encode_timestamp(nt, is_be));
    out.extend_from_slice(&encode_display(nt, is_be));
    out.extend_from_slice(&encode_control(nt, is_be));
    out.extend_from_slice(&encode_value_alarm(nt, is_be));
}

pub fn encode_nt_scalar_full(nt: &NtScalar, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    push_nt_scalar_full(&mut out, nt, is_be);
    out
}

fn encode_structure_bitset(desc: &StructureDesc, is_be: bool) -> Vec<u8> {
    encode_all_ones_bitset(1 + crate::spvd_decode::count_structure_fields(desc), is_be)
}

/// The number of field bits (bit 0, the whole-structure bit, excluded) in the
/// descriptor [`nt_payload_desc`] would build for `payload`.
///
/// Building that descriptor costs ~45 allocations for an `NTScalar`, and the
/// monitor encoder used to do it once per frame purely to reach this number.
/// The four fixed-shape normative types have a descriptor whose *shape* does
/// not depend on the payload — only the leaf `TypeCode`s vary — so their counts
/// are computed once, from the real descriptor builders, and memoised. `NTTable`
/// and `Generic` are payload-shaped and are counted structurally from the
/// payload itself, still without building a `StructureDesc`.
///
/// `nt_payload_field_count_matches_the_descriptor_for_every_variant` and
/// `fixed_shape_descriptors_have_payload_independent_bit_counts` pin this
/// against `nt_payload_desc`. Keep them passing.
pub(crate) fn nt_payload_field_count(payload: &NtPayload) -> usize {
    use crate::spvd_decode::count_structure_fields;
    match payload {
        NtPayload::Scalar(_) => nt_scalar_field_count(),
        NtPayload::ScalarArray(_) => {
            static N: OnceLock<usize> = OnceLock::new();
            *N.get_or_init(|| {
                count_structure_fields(&nt_scalar_array_desc(&ScalarArrayValue::I32(Vec::new())))
            })
        }
        NtPayload::NdArray(_) => {
            static N: OnceLock<usize> = OnceLock::new();
            *N.get_or_init(|| count_structure_fields(&nt_ndarray_desc()))
        }
        NtPayload::Enum(_) => {
            static N: OnceLock<usize> = OnceLock::new();
            *N.get_or_init(|| count_structure_fields(&nt_enum_desc()))
        }
        NtPayload::Table(nt) => {
            // Everything around the columns is fixed: labels, value, descriptor,
            // alarm (+3) and timeStamp (+3). Each column adds exactly one bit.
            static FIXED: OnceLock<usize> = OnceLock::new();
            let fixed = *FIXED.get_or_init(|| {
                count_structure_fields(&nt_table_desc(&NtTable {
                    labels: Vec::new(),
                    columns: Vec::new(),
                    descriptor: None,
                    alarm: None,
                    time_stamp: None,
                }))
            });
            fixed + nt.columns.len()
        }
        NtPayload::Generic { fields, .. } => pv_value_field_count(fields),
    }
}

/// Field-bit count for `NTScalar`. `nt_scalar_desc`'s only payload-dependent
/// part is the leaf `TypeCode` of `value`, which is never a `Structure`, so the
/// count is constant. One memoised cell, shared with `encode_nt_scalar_bitset`
/// and `encode_nt_scalar_bitset_parts`.
pub(crate) fn nt_scalar_field_count() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        crate::spvd_decode::count_structure_fields(&nt_scalar_desc(&ScalarValue::I32(0)))
    })
}

/// Field-bit count for a `PvValue` field list. Mirrors `count_structure_fields`
/// over `pv_value_field_type`: only `PvValue::Structure` becomes a
/// `FieldType::Structure`, so only it contributes nested bits.
fn pv_value_field_count(fields: &[(String, PvValue)]) -> usize {
    let mut count = 0;
    for (_, val) in fields {
        count += 1;
        if let PvValue::Structure { fields: nested, .. } = val {
            count += pv_value_field_count(nested);
        }
    }
    count
}

/// Append an all-ones changed bitset covering `total_bits` bits.
///
/// `total_bits` includes bit 0 (the whole structure), matching
/// [`encode_all_ones_bitset`] from Task 1 — callers pass `1 + field_count`.
/// Whole bytes are `0xFF` by construction; only the final partial byte needs
/// a mask, because trailing bits beyond `total_bits` must stay clear.
pub(crate) fn push_all_ones_bitset(out: &mut Vec<u8>, total_bits: usize, is_be: bool) {
    let bitset_size = total_bits.div_ceil(8);
    out.extend_from_slice(&crate::encode_common::encode_size(bitset_size, is_be));
    let start = out.len();
    out.resize(start + bitset_size, 0xFF);
    let spare = bitset_size * 8 - total_bits;
    if spare != 0 {
        let last = out.len() - 1;
        out[last] = 0xFFu8 >> spare;
    }
}

/// A size-prefixed bitset with `total_bits` bits set.
pub(crate) fn encode_all_ones_bitset(total_bits: usize, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    push_all_ones_bitset(&mut out, total_bits, is_be);
    out
}

pub fn encode_nt_scalar_bitset(nt: &NtScalar, is_be: bool) -> Vec<u8> {
    let mut out = encode_all_ones_bitset(1 + nt_scalar_field_count(), is_be);
    out.extend_from_slice(&encode_nt_scalar_full(nt, is_be));
    out
}

pub fn encode_nt_scalar_bitset_parts(nt: &NtScalar, is_be: bool) -> (Vec<u8>, Vec<u8>) {
    (
        encode_all_ones_bitset(1 + nt_scalar_field_count(), is_be),
        encode_nt_scalar_full(nt, is_be),
    )
}

pub fn alarm_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("alarm_t".to_string()),
        fields: vec![
            FieldDesc {
                name: "severity".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "status".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "message".to_string(),
                field_type: FieldType::String,
            },
        ],
    }
}

pub fn timestamp_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("time_t".to_string()),
        fields: vec![
            FieldDesc {
                name: "secondsPastEpoch".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int64),
            },
            FieldDesc {
                name: "nanoseconds".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "userTag".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
        ],
    }
}

fn display_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("display_t".to_string()),
        fields: vec![
            FieldDesc {
                name: "limitLow".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "limitHigh".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "description".to_string(),
                field_type: FieldType::String,
            },
            FieldDesc {
                name: "units".to_string(),
                field_type: FieldType::String,
            },
            FieldDesc {
                name: "precision".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "form".to_string(),
                field_type: FieldType::Structure(StructureDesc {
                    struct_id: Some("enum_t".to_string()),
                    fields: vec![
                        FieldDesc {
                            name: "index".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Int32),
                        },
                        FieldDesc {
                            name: "choices".to_string(),
                            field_type: FieldType::StringArray,
                        },
                    ],
                }),
            },
        ],
    }
}

fn control_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("control_t".to_string()),
        fields: vec![
            FieldDesc {
                name: "limitLow".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "limitHigh".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "minStep".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
        ],
    }
}

fn value_alarm_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("valueAlarm_t".to_string()),
        fields: vec![
            FieldDesc {
                name: "active".to_string(),
                field_type: FieldType::Scalar(TypeCode::Boolean),
            },
            FieldDesc {
                name: "lowAlarmLimit".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "lowWarningLimit".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "highWarningLimit".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "highAlarmLimit".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "lowAlarmSeverity".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "lowWarningSeverity".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "highWarningSeverity".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "highAlarmSeverity".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "hysteresis".to_string(),
                field_type: FieldType::Scalar(TypeCode::UInt8),
            },
        ],
    }
}

fn scalar_array_field_type(value: &ScalarArrayValue) -> FieldType {
    match value {
        ScalarArrayValue::Bool(_) => FieldType::ScalarArray(TypeCode::Boolean),
        ScalarArrayValue::I8(_) => FieldType::ScalarArray(TypeCode::Int8),
        ScalarArrayValue::I16(_) => FieldType::ScalarArray(TypeCode::Int16),
        ScalarArrayValue::I32(_) => FieldType::ScalarArray(TypeCode::Int32),
        ScalarArrayValue::I64(_) => FieldType::ScalarArray(TypeCode::Int64),
        ScalarArrayValue::U8(_) => FieldType::ScalarArray(TypeCode::UInt8),
        ScalarArrayValue::U16(_) => FieldType::ScalarArray(TypeCode::UInt16),
        ScalarArrayValue::U32(_) => FieldType::ScalarArray(TypeCode::UInt32),
        ScalarArrayValue::U64(_) => FieldType::ScalarArray(TypeCode::UInt64),
        ScalarArrayValue::F32(_) => FieldType::ScalarArray(TypeCode::Float32),
        ScalarArrayValue::F64(_) => FieldType::ScalarArray(TypeCode::Float64),
        ScalarArrayValue::Str(_) => FieldType::StringArray,
    }
}

fn encode_scalar_array_value_pvd(value: &ScalarArrayValue, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    match value {
        ScalarArrayValue::Bool(v) => {
            out.extend_from_slice(&encode_size_pvd(v.len(), is_be));
            for i in v {
                out.push(if *i { 1 } else { 0 });
            }
        }
        ScalarArrayValue::I8(v) => {
            out.extend_from_slice(&encode_size_pvd(v.len(), is_be));
            for i in v {
                out.push(*i as u8);
            }
        }
        ScalarArrayValue::I16(v) => {
            out.extend_from_slice(&encode_size_pvd(v.len(), is_be));
            for i in v {
                let b = if is_be {
                    i.to_be_bytes()
                } else {
                    i.to_le_bytes()
                };
                out.extend_from_slice(&b);
            }
        }
        ScalarArrayValue::I32(v) => {
            out.extend_from_slice(&encode_size_pvd(v.len(), is_be));
            for i in v {
                out.extend_from_slice(&encode_i32(*i, is_be));
            }
        }
        ScalarArrayValue::I64(v) => {
            out.extend_from_slice(&encode_size_pvd(v.len(), is_be));
            for i in v {
                out.extend_from_slice(&encode_i64(*i, is_be));
            }
        }
        ScalarArrayValue::U8(v) => {
            out.extend_from_slice(&encode_size_pvd(v.len(), is_be));
            out.extend_from_slice(v);
        }
        ScalarArrayValue::U16(v) => {
            out.extend_from_slice(&encode_size_pvd(v.len(), is_be));
            for i in v {
                let b = if is_be {
                    i.to_be_bytes()
                } else {
                    i.to_le_bytes()
                };
                out.extend_from_slice(&b);
            }
        }
        ScalarArrayValue::U32(v) => {
            out.extend_from_slice(&encode_size_pvd(v.len(), is_be));
            for i in v {
                let b = if is_be {
                    i.to_be_bytes()
                } else {
                    i.to_le_bytes()
                };
                out.extend_from_slice(&b);
            }
        }
        ScalarArrayValue::U64(v) => {
            out.extend_from_slice(&encode_size_pvd(v.len(), is_be));
            for i in v {
                let b = if is_be {
                    i.to_be_bytes()
                } else {
                    i.to_le_bytes()
                };
                out.extend_from_slice(&b);
            }
        }
        ScalarArrayValue::F32(v) => {
            out.extend_from_slice(&encode_size_pvd(v.len(), is_be));
            for i in v {
                let b = if is_be {
                    i.to_be_bytes()
                } else {
                    i.to_le_bytes()
                };
                out.extend_from_slice(&b);
            }
        }
        ScalarArrayValue::F64(v) => {
            out.extend_from_slice(&encode_size_pvd(v.len(), is_be));
            for i in v {
                out.extend_from_slice(&encode_f64(*i, is_be));
            }
        }
        ScalarArrayValue::Str(v) => {
            out.extend_from_slice(&encode_string_array(v, is_be));
        }
    }
    out
}

fn encode_nt_alarm(alarm: &NtAlarm, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_i32(alarm.severity, is_be));
    out.extend_from_slice(&encode_i32(alarm.status, is_be));
    out.extend_from_slice(&encode_string_pvd(&alarm.message, is_be));
    out
}

fn encode_nt_timestamp(ts: &NtTimeStamp, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_i64(ts.seconds_past_epoch, is_be));
    out.extend_from_slice(&encode_i32(ts.nanoseconds, is_be));
    out.extend_from_slice(&encode_i32(ts.user_tag, is_be));
    out
}

fn encode_nt_display(display: &NtDisplay, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_f64(display.limit_low, is_be));
    out.extend_from_slice(&encode_f64(display.limit_high, is_be));
    out.extend_from_slice(&encode_string_pvd(&display.description, is_be));
    out.extend_from_slice(&encode_string_pvd(&display.units, is_be));
    out.extend_from_slice(&encode_i32(display.precision, is_be));
    // `display_t` carries a `form` enum. `NtDisplay` has no field for it, so
    // emit the standard choices at index 0 — the same bytes `encode_display`
    // produces for a default `NtScalar`. Omitting it here left the value
    // stream one enum short of the descriptor, and a strict client (pvxs)
    // read past the end of the frame and dropped the whole connection.
    out.extend_from_slice(&encode_enum(0, &STANDARD_FORM_CHOICES[..], is_be));
    out
}

pub fn nt_scalar_array_desc(value: &ScalarArrayValue) -> StructureDesc {
    StructureDesc {
        struct_id: Some("epics:nt/NTScalarArray:1.0".to_string()),
        fields: vec![
            FieldDesc {
                name: "value".to_string(),
                field_type: scalar_array_field_type(value),
            },
            FieldDesc {
                name: "alarm".to_string(),
                field_type: FieldType::Structure(alarm_desc()),
            },
            FieldDesc {
                name: "timeStamp".to_string(),
                field_type: FieldType::Structure(timestamp_desc()),
            },
            FieldDesc {
                name: "display".to_string(),
                field_type: FieldType::Structure(display_desc()),
            },
            FieldDesc {
                name: "control".to_string(),
                field_type: FieldType::Structure(StructureDesc {
                    struct_id: Some("control_t".to_string()),
                    fields: vec![
                        FieldDesc {
                            name: "limitLow".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Float64),
                        },
                        FieldDesc {
                            name: "limitHigh".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Float64),
                        },
                        FieldDesc {
                            name: "minStep".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Float64),
                        },
                    ],
                }),
            },
        ],
    }
}

/// Append the full `epics:nt/NTScalarArray:1.0` value block to `out` (sink form).
pub(crate) fn push_nt_scalar_array_full(out: &mut Vec<u8>, nt: &NtScalarArray, is_be: bool) {
    out.extend_from_slice(&encode_scalar_array_value_pvd(&nt.value, is_be));
    out.extend_from_slice(&encode_nt_alarm(&nt.alarm, is_be));
    out.extend_from_slice(&encode_nt_timestamp(&nt.time_stamp, is_be));
    out.extend_from_slice(&encode_nt_display(&nt.display, is_be));
    out.extend_from_slice(&encode_f64(nt.control.limit_low, is_be));
    out.extend_from_slice(&encode_f64(nt.control.limit_high, is_be));
    out.extend_from_slice(&encode_f64(nt.control.min_step, is_be));
}

pub fn encode_nt_scalar_array_full(nt: &NtScalarArray, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    push_nt_scalar_array_full(&mut out, nt, is_be);
    out
}

pub fn nt_table_desc(nt: &NtTable) -> StructureDesc {
    let mut value_fields: Vec<FieldDesc> = Vec::new();
    for col in &nt.columns {
        value_fields.push(FieldDesc {
            name: col.name.clone(),
            field_type: scalar_array_field_type(&col.values),
        });
    }
    StructureDesc {
        struct_id: Some("epics:nt/NTTable:1.0".to_string()),
        fields: vec![
            FieldDesc {
                name: "labels".to_string(),
                field_type: FieldType::StringArray,
            },
            FieldDesc {
                name: "value".to_string(),
                field_type: FieldType::Structure(StructureDesc {
                    struct_id: None,
                    fields: value_fields,
                }),
            },
            FieldDesc {
                name: "descriptor".to_string(),
                field_type: FieldType::String,
            },
            FieldDesc {
                name: "alarm".to_string(),
                field_type: FieldType::Structure(alarm_desc()),
            },
            // Required by archiving clients: the EPICS Archiver Appliance
            // refuses (or NPEs on) structures without a top-level timeStamp.
            FieldDesc {
                name: "timeStamp".to_string(),
                field_type: FieldType::Structure(timestamp_desc()),
            },
        ],
    }
}

/// Append the full `epics:nt/NTTable:1.0` value block to `out` (sink form).
pub(crate) fn push_nt_table_full(out: &mut Vec<u8>, nt: &NtTable, is_be: bool) {
    out.extend_from_slice(&encode_string_array(&nt.labels, is_be));
    for NtTableColumn { values, .. } in &nt.columns {
        out.extend_from_slice(&encode_scalar_array_value_pvd(values, is_be));
    }
    out.extend_from_slice(&encode_string_pvd(
        nt.descriptor.as_deref().unwrap_or(""),
        is_be,
    ));
    out.extend_from_slice(&encode_nt_alarm(
        nt.alarm.as_ref().unwrap_or(&NtAlarm::default()),
        is_be,
    ));
    out.extend_from_slice(&encode_nt_timestamp(
        nt.time_stamp.as_ref().unwrap_or(&NtTimeStamp::default()),
        is_be,
    ));
}

pub fn encode_nt_table_full(nt: &NtTable, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    push_nt_table_full(&mut out, nt, is_be);
    out
}

fn nt_ndarray_value_union_fields() -> Vec<FieldDesc> {
    vec![
        FieldDesc {
            name: "booleanValue".to_string(),
            field_type: FieldType::ScalarArray(TypeCode::Boolean),
        },
        FieldDesc {
            name: "byteValue".to_string(),
            field_type: FieldType::ScalarArray(TypeCode::Int8),
        },
        FieldDesc {
            name: "shortValue".to_string(),
            field_type: FieldType::ScalarArray(TypeCode::Int16),
        },
        FieldDesc {
            name: "intValue".to_string(),
            field_type: FieldType::ScalarArray(TypeCode::Int32),
        },
        FieldDesc {
            name: "longValue".to_string(),
            field_type: FieldType::ScalarArray(TypeCode::Int64),
        },
        FieldDesc {
            name: "ubyteValue".to_string(),
            field_type: FieldType::ScalarArray(TypeCode::UInt8),
        },
        FieldDesc {
            name: "ushortValue".to_string(),
            field_type: FieldType::ScalarArray(TypeCode::UInt16),
        },
        FieldDesc {
            name: "uintValue".to_string(),
            field_type: FieldType::ScalarArray(TypeCode::UInt32),
        },
        FieldDesc {
            name: "ulongValue".to_string(),
            field_type: FieldType::ScalarArray(TypeCode::UInt64),
        },
        FieldDesc {
            name: "floatValue".to_string(),
            field_type: FieldType::ScalarArray(TypeCode::Float32),
        },
        FieldDesc {
            name: "doubleValue".to_string(),
            field_type: FieldType::ScalarArray(TypeCode::Float64),
        },
        FieldDesc {
            name: "stringValue".to_string(),
            field_type: FieldType::StringArray,
        },
    ]
}

fn ndarray_union_index(value: &ScalarArrayValue) -> usize {
    match value {
        ScalarArrayValue::Bool(_) => 0,
        ScalarArrayValue::I8(_) => 1,
        ScalarArrayValue::I16(_) => 2,
        ScalarArrayValue::I32(_) => 3,
        ScalarArrayValue::I64(_) => 4,
        ScalarArrayValue::U8(_) => 5,
        ScalarArrayValue::U16(_) => 6,
        ScalarArrayValue::U32(_) => 7,
        ScalarArrayValue::U64(_) => 8,
        ScalarArrayValue::F32(_) => 9,
        ScalarArrayValue::F64(_) => 10,
        ScalarArrayValue::Str(_) => 11,
    }
}

fn encode_ndarray_union(value: &ScalarArrayValue, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_size_pvd(ndarray_union_index(value), is_be));
    out.extend_from_slice(&encode_scalar_array_value_pvd(value, is_be));
    out
}

/// The `codec.parameters` variant: a `0x80` full-structure introspection whose
/// fields are the parameter names, followed by the values in the same order.
///
/// Keys are sorted so the frame is byte-stable across processes and so the two
/// walks below cannot disagree — `HashMap` iteration order is unspecified and
/// `dv_nt_ndarray_fields` walks the same map a third time.
fn encode_codec_parameters(
    parameters: &std::collections::HashMap<String, String>,
    is_be: bool,
) -> Vec<u8> {
    if parameters.is_empty() {
        return vec![0xFF];
    }
    let mut keys: Vec<&String> = parameters.keys().collect();
    keys.sort();
    let desc = StructureDesc {
        struct_id: None,
        fields: keys
            .iter()
            .map(|key| FieldDesc {
                name: (*key).clone(),
                field_type: FieldType::String,
            })
            .collect(),
    };
    let mut out = Vec::new();
    out.push(0x80);
    out.extend_from_slice(&encode_structure_desc(&desc, is_be));
    for key in &keys {
        out.extend_from_slice(&encode_string_pvd(&parameters[*key], is_be));
    }
    out
}

/// The NTNDArray descriptor. Constant — NTNDArray's shape does not vary with
/// its payload; the `value` field is a union covering every element type.
pub fn nt_ndarray_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("epics:nt/NTNDArray:1.0".to_string()),
        fields: vec![
            FieldDesc {
                name: "value".to_string(),
                field_type: FieldType::Union(nt_ndarray_value_union_fields()),
            },
            FieldDesc {
                name: "codec".to_string(),
                field_type: FieldType::Structure(StructureDesc {
                    struct_id: Some("codec_t".to_string()),
                    fields: vec![
                        FieldDesc {
                            name: "name".to_string(),
                            field_type: FieldType::String,
                        },
                        FieldDesc {
                            name: "parameters".to_string(),
                            field_type: FieldType::Variant,
                        },
                    ],
                }),
            },
            FieldDesc {
                name: "compressedSize".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int64),
            },
            FieldDesc {
                name: "uncompressedSize".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int64),
            },
            FieldDesc {
                name: "dimension".to_string(),
                field_type: FieldType::StructureArray(StructureDesc {
                    struct_id: Some("dimension_t".to_string()),
                    fields: vec![
                        FieldDesc {
                            name: "size".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Int32),
                        },
                        FieldDesc {
                            name: "offset".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Int32),
                        },
                        FieldDesc {
                            name: "fullSize".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Int32),
                        },
                        FieldDesc {
                            name: "binning".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Int32),
                        },
                        FieldDesc {
                            name: "reverse".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Boolean),
                        },
                    ],
                }),
            },
            FieldDesc {
                name: "uniqueId".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "dataTimeStamp".to_string(),
                field_type: FieldType::Structure(timestamp_desc()),
            },
            FieldDesc {
                name: "attribute".to_string(),
                field_type: FieldType::StructureArray(StructureDesc {
                    struct_id: Some("NTAttribute".to_string()),
                    fields: vec![
                        FieldDesc {
                            name: "name".to_string(),
                            field_type: FieldType::String,
                        },
                        FieldDesc {
                            name: "value".to_string(),
                            field_type: FieldType::Variant,
                        },
                        FieldDesc {
                            name: "descriptor".to_string(),
                            field_type: FieldType::String,
                        },
                        FieldDesc {
                            name: "sourceType".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Int32),
                        },
                        FieldDesc {
                            name: "source".to_string(),
                            field_type: FieldType::String,
                        },
                    ],
                }),
            },
            FieldDesc {
                name: "descriptor".to_string(),
                field_type: FieldType::String,
            },
            FieldDesc {
                name: "alarm".to_string(),
                field_type: FieldType::Structure(alarm_desc()),
            },
            FieldDesc {
                name: "timeStamp".to_string(),
                field_type: FieldType::Structure(timestamp_desc()),
            },
            FieldDesc {
                name: "display".to_string(),
                field_type: FieldType::Structure(display_desc()),
            },
        ],
    }
}

fn encode_attribute_variant(attr: &NtAttribute, is_be: bool) -> Vec<u8> {
    match &attr.value {
        ScalarValue::Bool(v) => {
            let mut out = vec![TypeCode::Boolean as u8];
            out.push(if *v { 1 } else { 0 });
            out
        }
        ScalarValue::I8(v) => {
            let mut out = vec![TypeCode::Int8 as u8];
            out.push(*v as u8);
            out
        }
        ScalarValue::I16(v) => {
            let mut out = vec![TypeCode::Int16 as u8];
            out.extend_from_slice(&if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            });
            out
        }
        ScalarValue::I32(v) => {
            let mut out = vec![TypeCode::Int32 as u8];
            out.extend_from_slice(&encode_i32(*v, is_be));
            out
        }
        ScalarValue::I64(v) => {
            let mut out = vec![TypeCode::Int64 as u8];
            out.extend_from_slice(&encode_i64(*v, is_be));
            out
        }
        ScalarValue::U8(v) => {
            let mut out = vec![TypeCode::UInt8 as u8];
            out.push(*v);
            out
        }
        ScalarValue::U16(v) => {
            let mut out = vec![TypeCode::UInt16 as u8];
            out.extend_from_slice(&if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            });
            out
        }
        ScalarValue::U32(v) => {
            let mut out = vec![TypeCode::UInt32 as u8];
            out.extend_from_slice(&if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            });
            out
        }
        ScalarValue::U64(v) => {
            let mut out = vec![TypeCode::UInt64 as u8];
            out.extend_from_slice(&if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            });
            out
        }
        ScalarValue::F32(v) => {
            let mut out = vec![TypeCode::Float32 as u8];
            out.extend_from_slice(&if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            });
            out
        }
        ScalarValue::F64(v) => {
            let mut out = vec![TypeCode::Float64 as u8];
            out.extend_from_slice(&encode_f64(*v, is_be));
            out
        }
        ScalarValue::Str(v) => {
            let mut out = vec![TypeCode::String as u8];
            out.extend_from_slice(&encode_string_pvd(v, is_be));
            out
        }
    }
}

/// Append the full `epics:nt/NTNDArray:1.0` value block to `out` (sink form).
///
/// The image payload is the largest body the codec writes; appending it to
/// the caller's frame buffer avoids a multi-MB temporary and copy.
pub(crate) fn push_nt_ndarray_full(out: &mut Vec<u8>, nt: &NtNdArray, is_be: bool) {
    out.extend_from_slice(&encode_ndarray_union(&nt.value, is_be));
    out.extend_from_slice(&encode_string_pvd(&nt.codec.name, is_be));
    out.extend_from_slice(&encode_codec_parameters(&nt.codec.parameters, is_be));
    out.extend_from_slice(&encode_i64(nt.compressed_size, is_be));
    out.extend_from_slice(&encode_i64(nt.uncompressed_size, is_be));
    out.extend_from_slice(&encode_size_pvd(nt.dimension.len(), is_be));
    for NdDimension {
        size,
        offset,
        full_size,
        binning,
        reverse,
    } in &nt.dimension
    {
        out.push(1); // non-null element indicator
        out.extend_from_slice(&encode_i32(*size, is_be));
        out.extend_from_slice(&encode_i32(*offset, is_be));
        out.extend_from_slice(&encode_i32(*full_size, is_be));
        out.extend_from_slice(&encode_i32(*binning, is_be));
        out.push(if *reverse { 1 } else { 0 });
    }
    out.extend_from_slice(&encode_i32(nt.unique_id, is_be));
    out.extend_from_slice(&encode_nt_timestamp(&nt.data_time_stamp, is_be));
    out.extend_from_slice(&encode_size_pvd(nt.attribute.len(), is_be));
    for attr in &nt.attribute {
        out.push(1); // non-null element indicator
        out.extend_from_slice(&encode_string_pvd(&attr.name, is_be));
        out.extend_from_slice(&encode_attribute_variant(attr, is_be));
        out.extend_from_slice(&encode_string_pvd(&attr.descriptor, is_be));
        out.extend_from_slice(&encode_i32(attr.source_type, is_be));
        out.extend_from_slice(&encode_string_pvd(&attr.source, is_be));
    }
    out.extend_from_slice(&encode_string_pvd(
        nt.descriptor.as_deref().unwrap_or(""),
        is_be,
    ));
    out.extend_from_slice(&encode_nt_alarm(
        nt.alarm.as_ref().unwrap_or(&NtAlarm::default()),
        is_be,
    ));
    out.extend_from_slice(&encode_nt_timestamp(
        nt.time_stamp.as_ref().unwrap_or(&NtTimeStamp::default()),
        is_be,
    ));
    out.extend_from_slice(&encode_nt_display(
        nt.display.as_ref().unwrap_or(&NtDisplay::default()),
        is_be,
    ));
}

pub fn encode_nt_ndarray_full(nt: &NtNdArray, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    push_nt_ndarray_full(&mut out, nt, is_be);
    out
}

// ---------------------------------------------------------------------------
// NTEnum descriptor & encoder
// ---------------------------------------------------------------------------

pub fn nt_enum_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("epics:nt/NTEnum:1.0".to_string()),
        fields: vec![
            FieldDesc {
                name: "value".to_string(),
                field_type: FieldType::Structure(StructureDesc {
                    struct_id: Some("enum_t".to_string()),
                    fields: vec![
                        FieldDesc {
                            name: "index".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Int32),
                        },
                        FieldDesc {
                            name: "choices".to_string(),
                            field_type: FieldType::StringArray,
                        },
                    ],
                }),
            },
            FieldDesc {
                name: "alarm".to_string(),
                field_type: FieldType::Structure(alarm_desc()),
            },
            FieldDesc {
                name: "timeStamp".to_string(),
                field_type: FieldType::Structure(timestamp_desc()),
            },
        ],
    }
}

/// Append the full `epics:nt/NTEnum:1.0` value block to `out` (sink form).
pub(crate) fn push_nt_enum_full(out: &mut Vec<u8>, nt: &NtEnum, is_be: bool) {
    // value — enum_t { index, choices }
    out.extend_from_slice(&encode_enum(nt.index, &nt.choices, is_be));
    // alarm
    out.extend_from_slice(&encode_nt_alarm(&nt.alarm, is_be));
    // timeStamp
    out.extend_from_slice(&encode_nt_timestamp(&nt.time_stamp, is_be));
}

pub fn encode_nt_enum_full(nt: &NtEnum, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    push_nt_enum_full(&mut out, nt, is_be);
    out
}

// ---------------------------------------------------------------------------
// PvValue (generic recursive) descriptor & encoder
// ---------------------------------------------------------------------------

fn scalar_value_type_code(v: &ScalarValue) -> TypeCode {
    match v {
        ScalarValue::Bool(_) => TypeCode::Boolean,
        ScalarValue::I8(_) => TypeCode::Int8,
        ScalarValue::I16(_) => TypeCode::Int16,
        ScalarValue::I32(_) => TypeCode::Int32,
        ScalarValue::I64(_) => TypeCode::Int64,
        ScalarValue::U8(_) => TypeCode::UInt8,
        ScalarValue::U16(_) => TypeCode::UInt16,
        ScalarValue::U32(_) => TypeCode::UInt32,
        ScalarValue::U64(_) => TypeCode::UInt64,
        ScalarValue::F32(_) => TypeCode::Float32,
        ScalarValue::F64(_) => TypeCode::Float64,
        ScalarValue::Str(_) => TypeCode::String,
    }
}

/// Build a [`StructureDesc`] from a [`PvValue::Structure`].
pub fn pv_value_desc(struct_id: &str, fields: &[(String, PvValue)]) -> StructureDesc {
    StructureDesc {
        struct_id: if struct_id.is_empty() {
            None
        } else {
            Some(struct_id.to_string())
        },
        fields: fields
            .iter()
            .map(|(name, val)| FieldDesc {
                name: name.clone(),
                field_type: pv_value_field_type(val),
            })
            .collect(),
    }
}

fn pv_value_field_type(val: &PvValue) -> FieldType {
    match val {
        PvValue::Scalar(sv) => {
            if matches!(sv, ScalarValue::Str(_)) {
                FieldType::String
            } else {
                FieldType::Scalar(scalar_value_type_code(sv))
            }
        }
        PvValue::ScalarArray(sa) => scalar_array_field_type(sa),
        PvValue::Structure { struct_id, fields } => {
            FieldType::Structure(pv_value_desc(struct_id, fields))
        }
    }
}

/// Append a [`PvValue`] tree's PVA wire bytes (values only, no descriptor)
/// to `out` (sink form).
pub(crate) fn push_pv_value(out: &mut Vec<u8>, val: &PvValue, is_be: bool) {
    match val {
        PvValue::Scalar(sv) => out.extend_from_slice(&encode_scalar_value(sv, is_be)),
        PvValue::ScalarArray(sa) => {
            out.extend_from_slice(&encode_scalar_array_value_pvd(sa, is_be))
        }
        PvValue::Structure { fields, .. } => push_pv_value_fields(out, fields, is_be),
    }
}

/// Append the values of a [`PvValue`] field list, in order, to `out`
/// (sink form). Used for `PvValue::Structure` and `NtPayload::Generic`.
pub(crate) fn push_pv_value_fields(out: &mut Vec<u8>, fields: &[(String, PvValue)], is_be: bool) {
    for (_, v) in fields {
        push_pv_value(out, v, is_be);
    }
}

pub fn nt_payload_desc(payload: &NtPayload) -> StructureDesc {
    match payload {
        NtPayload::Scalar(nt) => nt_scalar_desc(&nt.value),
        NtPayload::ScalarArray(nt) => nt_scalar_array_desc(&nt.value),
        NtPayload::Table(nt) => nt_table_desc(nt),
        NtPayload::NdArray(_) => nt_ndarray_desc(),
        NtPayload::Enum(_) => nt_enum_desc(),
        NtPayload::Generic { struct_id, fields } => pv_value_desc(struct_id, fields),
    }
}

/// Append the full value block for any [`NtPayload`] to `out`.
pub(crate) fn push_nt_payload_full(out: &mut Vec<u8>, payload: &NtPayload, is_be: bool) {
    match payload {
        NtPayload::Scalar(nt) => push_nt_scalar_full(out, nt, is_be),
        NtPayload::ScalarArray(nt) => push_nt_scalar_array_full(out, nt, is_be),
        NtPayload::Table(nt) => push_nt_table_full(out, nt, is_be),
        NtPayload::NdArray(nt) => push_nt_ndarray_full(out, nt, is_be),
        NtPayload::Enum(nt) => push_nt_enum_full(out, nt, is_be),
        NtPayload::Generic { fields, .. } => push_pv_value_fields(out, fields, is_be),
    }
}

pub fn encode_nt_payload_full(payload: &NtPayload, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    push_nt_payload_full(&mut out, payload, is_be);
    out
}

/// Append an all-ones bitset followed by the full value block.
pub(crate) fn push_nt_payload_bitset(out: &mut Vec<u8>, payload: &NtPayload, is_be: bool) {
    push_all_ones_bitset(out, 1 + nt_payload_field_count(payload), is_be);
    push_nt_payload_full(out, payload, is_be);
}

pub fn encode_nt_payload_bitset(payload: &NtPayload, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    push_nt_payload_bitset(&mut out, payload, is_be);
    out
}

pub fn encode_nt_payload_bitset_parts(payload: &NtPayload, is_be: bool) -> (Vec<u8>, Vec<u8>) {
    (
        encode_all_ones_bitset(1 + nt_payload_field_count(payload), is_be),
        encode_nt_payload_full(payload, is_be),
    )
}

// ---------------------------------------------------------------------------
// Generic DecodedValue → wire bytes encoder
// ---------------------------------------------------------------------------

use crate::spvd_decode::DecodedValue;

/// Encode a `DecodedValue` back to PVA wire bytes.
pub fn encode_decoded_value(val: &DecodedValue, is_be: bool) -> Vec<u8> {
    match val {
        DecodedValue::Null => Vec::new(),
        DecodedValue::Boolean(v) => vec![if *v { 1 } else { 0 }],
        DecodedValue::Int8(v) => vec![*v as u8],
        DecodedValue::Int16(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        DecodedValue::Int32(v) => encode_i32(*v, is_be),
        DecodedValue::Int64(v) => encode_i64(*v, is_be),
        DecodedValue::UInt8(v) => vec![*v],
        DecodedValue::UInt16(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        DecodedValue::UInt32(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        DecodedValue::UInt64(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        DecodedValue::Float32(v) => {
            if is_be {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        }
        DecodedValue::Float64(v) => encode_f64(*v, is_be),
        DecodedValue::String(v) => encode_string_pvd(v, is_be),
        DecodedValue::Array(arr) => {
            let mut out = encode_size_pvd(arr.len(), is_be);
            for item in arr {
                out.extend_from_slice(&encode_decoded_value(item, is_be));
            }
            out
        }
        DecodedValue::Structure(fields) => {
            let mut out = Vec::new();
            for (_name, value) in fields {
                out.extend_from_slice(&encode_decoded_value(value, is_be));
            }
            out
        }
        DecodedValue::Raw(data) => data.clone(),
    }
}

// ---------------------------------------------------------------------------
// pvRequest parsing & descriptor filtering
// ---------------------------------------------------------------------------

/// Parse a pvRequest structure from the INIT body bytes and return the list
/// of requested field paths.
///
/// Paths are returned as dot-separated strings (e.g. `"value"`,
/// `"alarm.severity"`). An empty inner `field {}` structure, or a body that
/// cannot be parsed, is reported as `None`, meaning "return all fields" (no
/// filtering).
///
/// A field whose inner pvRequest sub-structure is itself empty selects the
/// whole sub-tree rooted at that field (so `field(alarm)` → `["alarm"]`
/// selects the entire `alarm` structure). Non-empty sub-structures expand
/// into one entry per leaf path (so `field(alarm{severity}) →
/// ["alarm.severity"]`).
pub fn decode_pv_request_fields(body: &[u8], is_be: bool) -> Option<Vec<String>> {
    if body.is_empty() {
        return None;
    }
    let decoder = crate::spvd_decode::PvdDecoder::new(is_be);
    let desc = decoder.parse_introspection(body).ok()?;
    for field in &desc.fields {
        if field.name == "field" {
            if let FieldType::Structure(ref inner) = field.field_type {
                if inner.fields.is_empty() {
                    return None;
                }
                let mut paths = Vec::new();
                collect_pv_request_paths(inner, "", &mut paths);
                if paths.is_empty() {
                    return None;
                }
                return Some(paths);
            }
        }
    }
    None
}

fn collect_pv_request_paths(desc: &StructureDesc, prefix: &str, out: &mut Vec<String>) {
    for field in &desc.fields {
        let joined = if prefix.is_empty() {
            field.name.clone()
        } else {
            format!("{}.{}", prefix, field.name)
        };
        match &field.field_type {
            FieldType::Structure(nested) if !nested.fields.is_empty() => {
                collect_pv_request_paths(nested, &joined, out);
            }
            _ => out.push(joined),
        }
    }
}

/// Decode the `record._options` key/value pairs from a pvRequest body.
///
/// Returns `None` if the pvRequest does not include a `record._options`
/// substructure, or if the option values cannot be decoded as strings.
pub fn decode_pv_request_options(body: &[u8], is_be: bool) -> Option<Vec<(String, String)>> {
    if body.is_empty() {
        return None;
    }
    let decoder = crate::spvd_decode::PvdDecoder::new(is_be);
    let desc = decoder.parse_introspection(body).ok()?;
    let options_desc = desc.fields.iter().find_map(|f| {
        if f.name != "record" {
            return None;
        }
        if let FieldType::Structure(inner) = &f.field_type {
            inner.fields.iter().find_map(|g| {
                if g.name != "_options" {
                    return None;
                }
                if let FieldType::Structure(opts) = &g.field_type {
                    Some(opts.clone())
                } else {
                    None
                }
            })
        } else {
            None
        }
    })?;

    // The pvRequest body is `0x80 <desc> <values>`. The `field` sub-tree
    // encodes empty structs only, so contributes no value bytes; the
    // option strings follow immediately after the descriptor.
    let desc_bytes = encode_structure_desc(&desc, is_be);
    let values_start = 1 + desc_bytes.len();
    if values_start > body.len() {
        return None;
    }
    let mut cursor = &body[values_start..];
    let mut out = Vec::with_capacity(options_desc.fields.len());
    for f in &options_desc.fields {
        if !matches!(f.field_type, FieldType::String) {
            return None;
        }
        let (s, consumed) = crate::epics_decode::decode_string(cursor, is_be)?;
        out.push((f.name.clone(), s));
        cursor = &cursor[consumed..];
    }
    Some(out)
}

/// Filter a [`StructureDesc`] to include only the listed field paths.
///
/// Paths may be dot-separated to descend into nested structures (e.g.
/// `"alarm.severity"`). A bare name selects the entire sub-tree rooted at
/// that field. Unknown paths are silently dropped. If `requested` is empty
/// the original descriptor is returned unchanged.
pub fn filter_structure_desc(desc: &StructureDesc, requested: &[String]) -> StructureDesc {
    if requested.is_empty() {
        return desc.clone();
    }
    let tree = build_path_tree(requested);
    prune_structure(desc, &tree)
}

#[derive(Default, Debug, Clone)]
struct PathNode {
    /// When true, the whole sub-tree rooted at this node is selected and
    /// `children` should be ignored.
    select_all: bool,
    /// Insertion-ordered children. Using a Vec of pairs rather than a map
    /// preserves the field order implied by the caller, which matters for
    /// the PVA wire format (introspection field order is significant).
    children: Vec<(String, PathNode)>,
}

impl PathNode {
    fn child_mut(&mut self, name: &str) -> &mut PathNode {
        if let Some(idx) = self.children.iter().position(|(n, _)| n == name) {
            return &mut self.children[idx].1;
        }
        self.children.push((name.to_string(), PathNode::default()));
        &mut self.children.last_mut().unwrap().1
    }

    fn child(&self, name: &str) -> Option<&PathNode> {
        self.children
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| c)
    }
}

fn build_path_tree(paths: &[String]) -> PathNode {
    let mut root = PathNode::default();
    for p in paths {
        let parts: Vec<&str> = p.split('.').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        let mut node = &mut root;
        for (i, part) in parts.iter().enumerate() {
            let is_last = i == parts.len() - 1;
            let child = node.child_mut(part);
            if is_last {
                child.select_all = true;
                child.children.clear();
            }
            node = child;
        }
    }
    root
}

fn prune_structure(desc: &StructureDesc, node: &PathNode) -> StructureDesc {
    if node.select_all {
        return desc.clone();
    }
    let mut fields = Vec::new();
    for field in &desc.fields {
        let Some(child) = node.child(&field.name) else {
            continue;
        };
        if child.select_all {
            fields.push(field.clone());
            continue;
        }
        match &field.field_type {
            FieldType::Structure(inner) => {
                let pruned = prune_structure(inner, child);
                if !pruned.fields.is_empty() {
                    fields.push(FieldDesc {
                        name: field.name.clone(),
                        field_type: FieldType::Structure(pruned),
                    });
                }
            }
            FieldType::StructureArray(inner) => {
                // For structure arrays we can only narrow the element
                // descriptor; we never drop the array field itself when a
                // sub-path is requested.
                let pruned = prune_structure(inner, child);
                if !pruned.fields.is_empty() {
                    fields.push(FieldDesc {
                        name: field.name.clone(),
                        field_type: FieldType::StructureArray(pruned),
                    });
                }
            }
            _ => {
                // Leaf referenced with a deeper path – drop it (unresolved).
            }
        }
    }
    StructureDesc {
        struct_id: desc.struct_id.clone(),
        fields,
    }
}

/// Encode only the fields of an [`NtPayload`] whose paths appear in
/// `filtered_desc`.  The bitset and value bytes are computed against the
/// filtered descriptor so that a client that received the filtered INIT
/// descriptor will decode them correctly.
///
/// Supports nested filtering (e.g. a filtered descriptor that only contains
/// `alarm.severity`).
pub fn encode_nt_payload_filtered(
    payload: &NtPayload,
    filtered_desc: &StructureDesc,
    is_be: bool,
) -> (Vec<u8>, Vec<u8>) {
    let bitset = encode_structure_bitset(filtered_desc, is_be);
    let values = encode_nt_payload_values_for_desc(payload, filtered_desc, is_be);
    (bitset, values)
}

/// Cheap necessary condition for `structure_desc_equal(&nt_payload_desc(payload), desc)`.
///
/// Compares only the struct id and the top-level field arity, both of which are
/// known from the payload without materialising a `StructureDesc`. A `false`
/// here is a definitive "not equal"; a `true` still needs the deep comparison.
/// Pinned by `shape_precheck_never_disagrees_with_the_deep_comparison`.
fn nt_payload_shape_matches(payload: &NtPayload, desc: &StructureDesc) -> bool {
    let (struct_id, top_level_fields): (Option<&str>, usize) = match payload {
        NtPayload::Scalar(_) => (Some("epics:nt/NTScalar:1.0"), 6),
        NtPayload::ScalarArray(_) => (Some("epics:nt/NTScalarArray:1.0"), 5),
        // NTTable's *top-level* arity is fixed at 5 (labels, value, descriptor,
        // alarm, timeStamp); the columns live one level down inside `value`.
        NtPayload::Table(_) => (Some("epics:nt/NTTable:1.0"), 5),
        NtPayload::NdArray(_) => (Some("epics:nt/NTNDArray:1.0"), 12),
        NtPayload::Enum(_) => (Some("epics:nt/NTEnum:1.0"), 3),
        NtPayload::Generic { struct_id, fields } => (
            if struct_id.is_empty() {
                None
            } else {
                Some(struct_id.as_str())
            },
            fields.len(),
        ),
    };
    desc.struct_id.as_deref() == struct_id && desc.fields.len() == top_level_fields
}

/// Encode the value bytes of an `NtPayload` projected onto a (possibly
/// narrowed) descriptor. Fields not represented in `desc` are omitted;
/// sub-structures are encoded recursively.
pub fn encode_nt_payload_values_for_desc(
    payload: &NtPayload,
    desc: &StructureDesc,
    is_be: bool,
) -> Vec<u8> {
    // Reject the common filtered case (a narrowed descriptor) without building
    // the payload's own descriptor at all.
    if nt_payload_shape_matches(payload, desc) {
        let full_desc = nt_payload_desc(payload);
        if structure_desc_equal(&full_desc, desc) {
            // Fast path: no narrowing.
            return encode_nt_payload_full(payload, is_be);
        }
    }
    let decoded = nt_payload_to_decoded_for(payload, Some(desc), is_be);
    encode_decoded_projected(&decoded, desc, is_be)
}

fn structure_desc_equal(a: &StructureDesc, b: &StructureDesc) -> bool {
    if a.struct_id != b.struct_id {
        return false;
    }
    if a.fields.len() != b.fields.len() {
        return false;
    }
    a.fields
        .iter()
        .zip(&b.fields)
        .all(|(x, y)| x.name == y.name && field_type_equal(&x.field_type, &y.field_type))
}

fn field_type_equal(a: &FieldType, b: &FieldType) -> bool {
    match (a, b) {
        (FieldType::Scalar(x), FieldType::Scalar(y)) => x == y,
        (FieldType::ScalarArray(x), FieldType::ScalarArray(y)) => x == y,
        (FieldType::String, FieldType::String) => true,
        (FieldType::StringArray, FieldType::StringArray) => true,
        (FieldType::Structure(x), FieldType::Structure(y)) => structure_desc_equal(x, y),
        (FieldType::StructureArray(x), FieldType::StructureArray(y)) => structure_desc_equal(x, y),
        (FieldType::Variant, FieldType::Variant) => true,
        (FieldType::VariantArray, FieldType::VariantArray) => true,
        (FieldType::BoundedString(x), FieldType::BoundedString(y)) => x == y,
        // Treat unions as equal only by count (rare in NT; fine for fast-path).
        (FieldType::Union(x), FieldType::Union(y)) => x.len() == y.len(),
        (FieldType::UnionArray(x), FieldType::UnionArray(y)) => x.len() == y.len(),
        _ => false,
    }
}

/// Round-trip an NtPayload through its full descriptor to obtain a
/// `DecodedValue::Structure` we can project against a narrowed descriptor.
///
/// This is the reference implementation retained for the differential
/// byte-identity tests; the production delta path uses the direct
/// [`nt_payload_to_decoded_for`] projection below, which produces a
/// byte-identical tree without the wire encode+decode round-trip (see
/// finding codec-C1).
#[cfg(test)]
fn decode_payload_to_structure(payload: &NtPayload, is_be: bool) -> Option<DecodedValue> {
    let desc = nt_payload_desc(payload);
    let bytes = encode_nt_payload_full(payload, is_be);
    let decoder = crate::spvd_decode::PvdDecoder::new(is_be);
    decoder.decode_structure(&bytes, &desc).ok().map(|(v, _)| v)
}

// ---------------------------------------------------------------------------
// Direct NtPayload -> DecodedValue projection (codec-C1)
//
// Mirrors `encode_nt_payload_full` composed with
// `PvdDecoder::decode_structure` EXACTLY, but builds the in-memory tree with no
// wire round-trip. This must stay byte-identical to the round-trip path: field
// order and names come from `nt_payload_desc`, and the `DecodedValue` variant
// chosen for each leaf must match what `decode_scalar`/`decode_value` yields
// for the corresponding wire bytes. The `direct_projection_matches_roundtrip_*`
// differential tests assert this over every NtPayload variant.
// ---------------------------------------------------------------------------

fn dv_scalar(v: &ScalarValue) -> DecodedValue {
    match v {
        ScalarValue::Bool(x) => DecodedValue::Boolean(*x),
        ScalarValue::I8(x) => DecodedValue::Int8(*x),
        ScalarValue::I16(x) => DecodedValue::Int16(*x),
        ScalarValue::I32(x) => DecodedValue::Int32(*x),
        ScalarValue::I64(x) => DecodedValue::Int64(*x),
        ScalarValue::U8(x) => DecodedValue::UInt8(*x),
        ScalarValue::U16(x) => DecodedValue::UInt16(*x),
        ScalarValue::U32(x) => DecodedValue::UInt32(*x),
        ScalarValue::U64(x) => DecodedValue::UInt64(*x),
        ScalarValue::F32(x) => DecodedValue::Float32(*x),
        ScalarValue::F64(x) => DecodedValue::Float64(*x),
        ScalarValue::Str(x) => DecodedValue::String(x.clone()),
    }
}

fn dv_scalar_array(v: &ScalarArrayValue) -> DecodedValue {
    let items: Vec<DecodedValue> = match v {
        ScalarArrayValue::Bool(a) => a.iter().map(|x| DecodedValue::Boolean(*x)).collect(),
        ScalarArrayValue::I8(a) => a.iter().map(|x| DecodedValue::Int8(*x)).collect(),
        ScalarArrayValue::I16(a) => a.iter().map(|x| DecodedValue::Int16(*x)).collect(),
        ScalarArrayValue::I32(a) => a.iter().map(|x| DecodedValue::Int32(*x)).collect(),
        ScalarArrayValue::I64(a) => a.iter().map(|x| DecodedValue::Int64(*x)).collect(),
        ScalarArrayValue::U8(a) => a.iter().map(|x| DecodedValue::UInt8(*x)).collect(),
        ScalarArrayValue::U16(a) => a.iter().map(|x| DecodedValue::UInt16(*x)).collect(),
        ScalarArrayValue::U32(a) => a.iter().map(|x| DecodedValue::UInt32(*x)).collect(),
        ScalarArrayValue::U64(a) => a.iter().map(|x| DecodedValue::UInt64(*x)).collect(),
        ScalarArrayValue::F32(a) => a.iter().map(|x| DecodedValue::Float32(*x)).collect(),
        ScalarArrayValue::F64(a) => a.iter().map(|x| DecodedValue::Float64(*x)).collect(),
        ScalarArrayValue::Str(a) => a.iter().map(|x| DecodedValue::String(x.clone())).collect(),
    };
    DecodedValue::Array(items)
}

fn dv_string_array(v: &[String]) -> DecodedValue {
    DecodedValue::Array(v.iter().map(|s| DecodedValue::String(s.clone())).collect())
}

/// `display.form.choices` for the DecodedValue (delta) path. An empty slice is
/// the "standard choices" sentinel and falls back to `STANDARD_FORM_CHOICES`,
/// keeping this path byte-identical to `encode_display` (both put the 7 standard
/// choices on the wire) so the differential test holds.
fn dv_form_choices(v: &[String]) -> DecodedValue {
    if v.is_empty() {
        DecodedValue::Array(
            STANDARD_FORM_CHOICES
                .iter()
                .map(|s| DecodedValue::String((*s).to_string()))
                .collect(),
        )
    } else {
        dv_string_array(v)
    }
}

fn dv_alarm(severity: i32, status: i32, message: &str) -> DecodedValue {
    DecodedValue::Structure(vec![
        ("severity".to_string(), DecodedValue::Int32(severity)),
        ("status".to_string(), DecodedValue::Int32(status)),
        ("message".to_string(), DecodedValue::String(message.to_string())),
    ])
}

fn dv_timestamp(secs: i64, nanos: i32, user_tag: i32) -> DecodedValue {
    DecodedValue::Structure(vec![
        ("secondsPastEpoch".to_string(), DecodedValue::Int64(secs)),
        ("nanoseconds".to_string(), DecodedValue::Int32(nanos)),
        ("userTag".to_string(), DecodedValue::Int32(user_tag)),
    ])
}

fn dv_nt_timestamp(ts: &NtTimeStamp) -> DecodedValue {
    dv_timestamp(ts.seconds_past_epoch, ts.nanoseconds, ts.user_tag)
}

/// Mirror `encode_timestamp`'s stored-value-or-`now()` fallback for NtScalar.
fn dv_scalar_timestamp(nt: &NtScalar) -> DecodedValue {
    let (secs, nanos, user_tag) = match &nt.time_stamp {
        Some(ts) => (ts.seconds_past_epoch, ts.nanoseconds, ts.user_tag),
        None => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            (now.as_secs() as i64, now.subsec_nanos() as i32, 0)
        }
    };
    dv_timestamp(secs, nanos, user_tag)
}

/// display_t with the 5 common fields (used by NTScalarArray / NTNDArray).
/// The `display_t` tree for an [`NtDisplay`]. Mirrors `encode_nt_display`,
/// including the standard `form` enum that `NtDisplay` has no field for.
fn dv_display(d: &NtDisplay) -> DecodedValue {
    DecodedValue::Structure(vec![
        ("limitLow".to_string(), DecodedValue::Float64(d.limit_low)),
        ("limitHigh".to_string(), DecodedValue::Float64(d.limit_high)),
        (
            "description".to_string(),
            DecodedValue::String(d.description.clone()),
        ),
        ("units".to_string(), DecodedValue::String(d.units.clone())),
        ("precision".to_string(), DecodedValue::Int32(d.precision)),
        (
            "form".to_string(),
            DecodedValue::Structure(vec![
                ("index".to_string(), DecodedValue::Int32(0)),
                ("choices".to_string(), dv_form_choices(&[])),
            ]),
        ),
    ])
}

fn dv_pv_value(v: &PvValue) -> DecodedValue {
    match v {
        PvValue::Scalar(sv) => dv_scalar(sv),
        PvValue::ScalarArray(sa) => dv_scalar_array(sa),
        PvValue::Structure { fields, .. } => DecodedValue::Structure(
            fields
                .iter()
                .map(|(n, val)| (n.clone(), dv_pv_value(val)))
                .collect(),
        ),
    }
}

/// The NTNDArray union member name selected for a given array element type,
/// matching `nt_ndarray_value_union_fields` order and `ndarray_union_index`.
fn ndarray_union_field_name(v: &ScalarArrayValue) -> &'static str {
    match v {
        ScalarArrayValue::Bool(_) => "booleanValue",
        ScalarArrayValue::I8(_) => "byteValue",
        ScalarArrayValue::I16(_) => "shortValue",
        ScalarArrayValue::I32(_) => "intValue",
        ScalarArrayValue::I64(_) => "longValue",
        ScalarArrayValue::U8(_) => "ubyteValue",
        ScalarArrayValue::U16(_) => "ushortValue",
        ScalarArrayValue::U32(_) => "uintValue",
        ScalarArrayValue::U64(_) => "ulongValue",
        ScalarArrayValue::F32(_) => "floatValue",
        ScalarArrayValue::F64(_) => "doubleValue",
        ScalarArrayValue::Str(_) => "stringValue",
    }
}

/// True when the caller wants a top-level field named `name`.
///
/// `None` means "unfiltered — build everything". `Some(desc)` means the caller
/// will project onto `desc`, so any top-level field `desc` does not name is
/// going to be discarded and need never be built.
fn wants_field(desc: Option<&StructureDesc>, name: &str) -> bool {
    match desc {
        None => true,
        Some(d) => d.fields.iter().any(|f| f.name == name),
    }
}

/// Direct, allocation-only projection of an [`NtPayload`] into the
/// `DecodedValue::Structure` tree that the encode→decode round-trip would
/// yield. Byte-identity with [`decode_payload_to_structure`] is enforced by
/// the differential tests; the production monitor delta hot path uses this
/// to avoid the redundant wire serialization (finding codec-C1).
///
/// `desc: None` builds every field; `Some(desc)` builds only the top-level
/// fields `desc` names, skipping the rest (finding codec-perf, Task 7).
fn nt_payload_to_decoded_for(
    payload: &NtPayload,
    desc: Option<&StructureDesc>,
    is_be: bool,
) -> DecodedValue {
    // Endianness only affects wire bytes, which this tree skips; the parameter
    // is kept for signature parity with `decode_payload_to_structure`.
    let _ = is_be;
    DecodedValue::Structure(match payload {
        NtPayload::Scalar(nt) => dv_nt_scalar_fields(nt, desc),
        NtPayload::ScalarArray(nt) => dv_nt_scalar_array_fields(nt, desc),
        NtPayload::Table(nt) => dv_nt_table_fields(nt, desc),
        NtPayload::NdArray(nt) => dv_nt_ndarray_fields(nt, desc),
        NtPayload::Enum(nt) => dv_nt_enum_fields(nt, desc),
        NtPayload::Generic { fields, .. } => dv_generic_fields(fields, desc),
    })
}

/// Unfiltered (`desc: None`) form of [`nt_payload_to_decoded_for`], builds
/// every field. Test-only: production code calls `nt_payload_to_decoded_for`
/// directly. Retained so the differential tests and
/// [`decode_payload_to_structure`] comparisons keep their exact meaning.
#[cfg(test)]
fn nt_payload_to_decoded(payload: &NtPayload, is_be: bool) -> DecodedValue {
    nt_payload_to_decoded_for(payload, None, is_be)
}

/// The `epics:nt/NTScalar:1.0` field list, restricted to the top-level fields
/// `desc` names. Mirrors `encode_nt_scalar_full`.
fn dv_nt_scalar_fields(nt: &NtScalar, desc: Option<&StructureDesc>) -> Vec<(String, DecodedValue)> {
    let mut out: Vec<(String, DecodedValue)> = Vec::with_capacity(6);
    if wants_field(desc, "value") {
        out.push(("value".to_string(), dv_scalar(&nt.value)));
    }
    if wants_field(desc, "alarm") {
        out.push((
            "alarm".to_string(),
            dv_alarm(nt.alarm_severity, nt.alarm_status, &nt.alarm_message),
        ));
    }
    if wants_field(desc, "timeStamp") {
        out.push(("timeStamp".to_string(), dv_scalar_timestamp(nt)));
    }
    if wants_field(desc, "display") {
        out.push((
            "display".to_string(),
            DecodedValue::Structure(vec![
                ("limitLow".to_string(), DecodedValue::Float64(nt.display_low)),
                (
                    "limitHigh".to_string(),
                    DecodedValue::Float64(nt.display_high),
                ),
                (
                    "description".to_string(),
                    DecodedValue::String(nt.display_description.clone()),
                ),
                ("units".to_string(), DecodedValue::String(nt.units.clone())),
                (
                    "precision".to_string(),
                    DecodedValue::Int32(nt.display_precision),
                ),
                (
                    "form".to_string(),
                    DecodedValue::Structure(vec![
                        (
                            "index".to_string(),
                            DecodedValue::Int32(nt.display_form_index),
                        ),
                        (
                            "choices".to_string(),
                            dv_form_choices(&nt.display_form_choices),
                        ),
                    ]),
                ),
            ]),
        ));
    }
    if wants_field(desc, "control") {
        out.push((
            "control".to_string(),
            DecodedValue::Structure(vec![
                ("limitLow".to_string(), DecodedValue::Float64(nt.control_low)),
                (
                    "limitHigh".to_string(),
                    DecodedValue::Float64(nt.control_high),
                ),
                (
                    "minStep".to_string(),
                    DecodedValue::Float64(nt.control_min_step),
                ),
            ]),
        ));
    }
    if wants_field(desc, "valueAlarm") {
        out.push((
            "valueAlarm".to_string(),
            DecodedValue::Structure(vec![
                (
                    "active".to_string(),
                    DecodedValue::Boolean(nt.value_alarm_active),
                ),
                (
                    "lowAlarmLimit".to_string(),
                    DecodedValue::Float64(nt.value_alarm_low_alarm_limit),
                ),
                (
                    "lowWarningLimit".to_string(),
                    DecodedValue::Float64(nt.value_alarm_low_warning_limit),
                ),
                (
                    "highWarningLimit".to_string(),
                    DecodedValue::Float64(nt.value_alarm_high_warning_limit),
                ),
                (
                    "highAlarmLimit".to_string(),
                    DecodedValue::Float64(nt.value_alarm_high_alarm_limit),
                ),
                (
                    "lowAlarmSeverity".to_string(),
                    DecodedValue::Int32(nt.value_alarm_low_alarm_severity),
                ),
                (
                    "lowWarningSeverity".to_string(),
                    DecodedValue::Int32(nt.value_alarm_low_warning_severity),
                ),
                (
                    "highWarningSeverity".to_string(),
                    DecodedValue::Int32(nt.value_alarm_high_warning_severity),
                ),
                (
                    "highAlarmSeverity".to_string(),
                    DecodedValue::Int32(nt.value_alarm_high_alarm_severity),
                ),
                (
                    "hysteresis".to_string(),
                    DecodedValue::UInt8(nt.value_alarm_hysteresis),
                ),
            ]),
        ));
    }
    out
}

/// The `epics:nt/NTScalarArray:1.0` field list, restricted to the top-level
/// fields `desc` names.
fn dv_nt_scalar_array_fields(
    nt: &NtScalarArray,
    desc: Option<&StructureDesc>,
) -> Vec<(String, DecodedValue)> {
    let mut out: Vec<(String, DecodedValue)> = Vec::with_capacity(5);
    if wants_field(desc, "value") {
        out.push(("value".to_string(), dv_scalar_array(&nt.value)));
    }
    if wants_field(desc, "alarm") {
        out.push((
            "alarm".to_string(),
            dv_alarm(nt.alarm.severity, nt.alarm.status, &nt.alarm.message),
        ));
    }
    if wants_field(desc, "timeStamp") {
        out.push(("timeStamp".to_string(), dv_nt_timestamp(&nt.time_stamp)));
    }
    if wants_field(desc, "display") {
        out.push(("display".to_string(), dv_display(&nt.display)));
    }
    if wants_field(desc, "control") {
        out.push((
            "control".to_string(),
            DecodedValue::Structure(vec![
                (
                    "limitLow".to_string(),
                    DecodedValue::Float64(nt.control.limit_low),
                ),
                (
                    "limitHigh".to_string(),
                    DecodedValue::Float64(nt.control.limit_high),
                ),
                (
                    "minStep".to_string(),
                    DecodedValue::Float64(nt.control.min_step),
                ),
            ]),
        ));
    }
    out
}

/// The `epics:nt/NTTable:1.0` field list, restricted to the top-level fields
/// `desc` names.
fn dv_nt_table_fields(nt: &NtTable, desc: Option<&StructureDesc>) -> Vec<(String, DecodedValue)> {
    let mut out: Vec<(String, DecodedValue)> = Vec::with_capacity(5);
    if wants_field(desc, "labels") {
        out.push(("labels".to_string(), dv_string_array(&nt.labels)));
    }
    if wants_field(desc, "value") {
        let value_fields: Vec<(String, DecodedValue)> = nt
            .columns
            .iter()
            .map(|c| (c.name.clone(), dv_scalar_array(&c.values)))
            .collect();
        out.push(("value".to_string(), DecodedValue::Structure(value_fields)));
    }
    if wants_field(desc, "descriptor") {
        out.push((
            "descriptor".to_string(),
            DecodedValue::String(nt.descriptor.clone().unwrap_or_default()),
        ));
    }
    if wants_field(desc, "alarm") {
        let alarm = nt.alarm.clone().unwrap_or_default();
        out.push((
            "alarm".to_string(),
            dv_alarm(alarm.severity, alarm.status, &alarm.message),
        ));
    }
    if wants_field(desc, "timeStamp") {
        let ts = nt.time_stamp.clone().unwrap_or_default();
        out.push(("timeStamp".to_string(), dv_nt_timestamp(&ts)));
    }
    out
}

/// The `epics:nt/NTNDArray:1.0` field list, restricted to the top-level fields
/// `desc` names.
fn dv_nt_ndarray_fields(
    nt: &NtNdArray,
    desc: Option<&StructureDesc>,
) -> Vec<(String, DecodedValue)> {
    let mut out: Vec<(String, DecodedValue)> = Vec::with_capacity(12);
    if wants_field(desc, "value") {
        // value — union member selected by element type
        let value = DecodedValue::Structure(vec![(
            ndarray_union_field_name(&nt.value).to_string(),
            dv_scalar_array(&nt.value),
        )]);
        out.push(("value".to_string(), value));
    }
    if wants_field(desc, "codec") {
        // codec.parameters — Variant: empty -> Null, else Structure of
        // string key/value pairs.
        let codec_parameters = if nt.codec.parameters.is_empty() {
            DecodedValue::Null
        } else {
            DecodedValue::Structure({
                // Keys sorted, matching `encode_codec_parameters`. Both
                // walks must agree, and `HashMap` order is unspecified.
                let mut keys: Vec<&String> = nt.codec.parameters.keys().collect();
                keys.sort();
                keys.into_iter()
                    .map(|k| {
                        (
                            k.clone(),
                            DecodedValue::String(nt.codec.parameters[k].clone()),
                        )
                    })
                    .collect::<Vec<_>>()
            })
        };
        let codec = DecodedValue::Structure(vec![
            (
                "name".to_string(),
                DecodedValue::String(nt.codec.name.clone()),
            ),
            ("parameters".to_string(), codec_parameters),
        ]);
        out.push(("codec".to_string(), codec));
    }
    if wants_field(desc, "compressedSize") {
        out.push((
            "compressedSize".to_string(),
            DecodedValue::Int64(nt.compressed_size),
        ));
    }
    if wants_field(desc, "uncompressedSize") {
        out.push((
            "uncompressedSize".to_string(),
            DecodedValue::Int64(nt.uncompressed_size),
        ));
    }
    if wants_field(desc, "dimension") {
        let dimension = DecodedValue::Array(
            nt.dimension
                .iter()
                .map(|d| {
                    DecodedValue::Structure(vec![
                        ("size".to_string(), DecodedValue::Int32(d.size)),
                        ("offset".to_string(), DecodedValue::Int32(d.offset)),
                        ("fullSize".to_string(), DecodedValue::Int32(d.full_size)),
                        ("binning".to_string(), DecodedValue::Int32(d.binning)),
                        ("reverse".to_string(), DecodedValue::Boolean(d.reverse)),
                    ])
                })
                .collect(),
        );
        out.push(("dimension".to_string(), dimension));
    }
    if wants_field(desc, "uniqueId") {
        out.push(("uniqueId".to_string(), DecodedValue::Int32(nt.unique_id)));
    }
    if wants_field(desc, "dataTimeStamp") {
        out.push((
            "dataTimeStamp".to_string(),
            dv_nt_timestamp(&nt.data_time_stamp),
        ));
    }
    if wants_field(desc, "attribute") {
        let attribute = DecodedValue::Array(
            nt.attribute
                .iter()
                .map(|a| {
                    DecodedValue::Structure(vec![
                        ("name".to_string(), DecodedValue::String(a.name.clone())),
                        ("value".to_string(), dv_scalar(&a.value)),
                        (
                            "descriptor".to_string(),
                            DecodedValue::String(a.descriptor.clone()),
                        ),
                        ("sourceType".to_string(), DecodedValue::Int32(a.source_type)),
                        ("source".to_string(), DecodedValue::String(a.source.clone())),
                    ])
                })
                .collect(),
        );
        out.push(("attribute".to_string(), attribute));
    }
    if wants_field(desc, "descriptor") {
        out.push((
            "descriptor".to_string(),
            DecodedValue::String(nt.descriptor.clone().unwrap_or_default()),
        ));
    }
    if wants_field(desc, "alarm") {
        let alarm = nt.alarm.clone().unwrap_or_default();
        out.push((
            "alarm".to_string(),
            dv_alarm(alarm.severity, alarm.status, &alarm.message),
        ));
    }
    if wants_field(desc, "timeStamp") {
        let ts = nt.time_stamp.clone().unwrap_or_default();
        out.push(("timeStamp".to_string(), dv_nt_timestamp(&ts)));
    }
    if wants_field(desc, "display") {
        let display = nt.display.clone().unwrap_or_default();
        out.push(("display".to_string(), dv_display(&display)));
    }
    out
}

/// The `epics:nt/NTEnum:1.0` field list, restricted to the top-level fields
/// `desc` names.
fn dv_nt_enum_fields(nt: &NtEnum, desc: Option<&StructureDesc>) -> Vec<(String, DecodedValue)> {
    let mut out: Vec<(String, DecodedValue)> = Vec::with_capacity(3);
    if wants_field(desc, "value") {
        out.push((
            "value".to_string(),
            DecodedValue::Structure(vec![
                ("index".to_string(), DecodedValue::Int32(nt.index)),
                ("choices".to_string(), dv_string_array(&nt.choices)),
            ]),
        ));
    }
    if wants_field(desc, "alarm") {
        out.push((
            "alarm".to_string(),
            dv_alarm(nt.alarm.severity, nt.alarm.status, &nt.alarm.message),
        ));
    }
    if wants_field(desc, "timeStamp") {
        out.push(("timeStamp".to_string(), dv_nt_timestamp(&nt.time_stamp)));
    }
    out
}

/// The `Generic` payload's field list — a flat pass-through of `PvValue`s,
/// restricted to the entries `desc` names.
fn dv_generic_fields(
    fields: &[(String, PvValue)],
    desc: Option<&StructureDesc>,
) -> Vec<(String, DecodedValue)> {
    fields
        .iter()
        .filter(|(name, _)| wants_field(desc, name))
        .map(|(name, val)| (name.clone(), dv_pv_value(val)))
        .collect()
}

/// Re-encode a `DecodedValue::Structure` against a (possibly narrowed)
/// descriptor, omitting fields that are not present in the descriptor.
pub fn encode_decoded_projected(
    value: &DecodedValue,
    desc: &StructureDesc,
    is_be: bool,
) -> Vec<u8> {
    let DecodedValue::Structure(fields) = value else {
        // Fallback: not a structure – emit raw bytes.
        return encode_decoded_value(value, is_be);
    };
    let mut out = Vec::new();
    for target in &desc.fields {
        let Some((_, sub_value)) = fields.iter().find(|(n, _)| n == &target.name) else {
            continue;
        };
        match &target.field_type {
            FieldType::Structure(inner) => {
                out.extend_from_slice(&encode_decoded_projected(sub_value, inner, is_be));
            }
            _ => {
                out.extend_from_slice(&encode_decoded_value(sub_value, is_be));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Sparse delta encoding (Phase 3)
// ---------------------------------------------------------------------------

/// Project an [`NtPayload`] onto `desc`, returning a [`DecodedValue::Structure`]
/// that contains only the fields represented in `desc`. Missing descriptor
/// fields are silently dropped.
fn project_payload_on_desc(payload: &NtPayload, desc: &StructureDesc, is_be: bool) -> DecodedValue {
    let decoded = nt_payload_to_decoded_for(payload, Some(desc), is_be);
    project_decoded(&decoded, desc)
}

fn project_decoded(value: &DecodedValue, desc: &StructureDesc) -> DecodedValue {
    let DecodedValue::Structure(fields) = value else {
        return value.clone();
    };
    let mut out: Vec<(String, DecodedValue)> = Vec::new();
    for target in &desc.fields {
        let Some((_, v)) = fields.iter().find(|(n, _)| n == &target.name) else {
            continue;
        };
        match &target.field_type {
            FieldType::Structure(inner) => {
                out.push((target.name.clone(), project_decoded(v, inner)));
            }
            _ => {
                out.push((target.name.clone(), v.clone()));
            }
        }
    }
    DecodedValue::Structure(out)
}

/// Structural equality for [`DecodedValue`] with NaN treated as equal to NaN
/// (avoids spurious monitor flaps when a float field is NaN on both sides).
pub fn decoded_values_equal(a: &DecodedValue, b: &DecodedValue) -> bool {
    use DecodedValue::*;
    match (a, b) {
        (Null, Null) => true,
        (Boolean(x), Boolean(y)) => x == y,
        (Int8(x), Int8(y)) => x == y,
        (Int16(x), Int16(y)) => x == y,
        (Int32(x), Int32(y)) => x == y,
        (Int64(x), Int64(y)) => x == y,
        (UInt8(x), UInt8(y)) => x == y,
        (UInt16(x), UInt16(y)) => x == y,
        (UInt32(x), UInt32(y)) => x == y,
        (UInt64(x), UInt64(y)) => x == y,
        (Float32(x), Float32(y)) => x == y || (x.is_nan() && y.is_nan()),
        (Float64(x), Float64(y)) => x == y || (x.is_nan() && y.is_nan()),
        (String(x), String(y)) => x == y,
        (Raw(x), Raw(y)) => x == y,
        (Array(x), Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| decoded_values_equal(a, b))
        }
        (Structure(x), Structure(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|((ln, lv), (rn, rv))| ln == rn && decoded_values_equal(lv, rv))
        }
        _ => false,
    }
}

/// Walks `desc` in pre-order (matching the PVA wire convention that bit 0
/// represents the whole root structure and subsequent bits correspond to
/// fields in pre-order) and returns a per-bit flag vector marking leaves
/// whose value differs between `prev` and `next`.
///
/// Returns `None` if no leaves changed. Structure-type fields always have
/// their bit cleared — changes propagate to the descendants so a filtered
/// monitor client sees only the true differences.
pub fn compute_changed_bits(
    prev: &DecodedValue,
    next: &DecodedValue,
    desc: &StructureDesc,
) -> Option<Vec<bool>> {
    let total = 1 + crate::spvd_decode::count_structure_fields(desc);
    let mut bits = vec![false; total];
    let mut idx = 1usize;
    let any = fill_changed_bits(prev, next, desc, &mut bits, &mut idx);
    if any { Some(bits) } else { None }
}

fn get_field_by_name<'a>(val: &'a DecodedValue, name: &str) -> Option<&'a DecodedValue> {
    match val {
        DecodedValue::Structure(f) => f.iter().find(|(n, _)| n == name).map(|(_, v)| v),
        _ => None,
    }
}

fn fill_changed_bits(
    prev: &DecodedValue,
    next: &DecodedValue,
    desc: &StructureDesc,
    bits: &mut [bool],
    idx: &mut usize,
) -> bool {
    let mut any = false;
    for field in &desc.fields {
        let this = *idx;
        *idx += 1;
        let p = get_field_by_name(prev, &field.name);
        let n = get_field_by_name(next, &field.name);
        match &field.field_type {
            FieldType::Structure(inner) => {
                let empty = DecodedValue::Structure(Vec::new());
                let pv = p.unwrap_or(&empty);
                let nv = n.unwrap_or(&empty);
                if fill_changed_bits(pv, nv, inner, bits, idx) {
                    any = true;
                }
            }
            _ => {
                let changed = match (p, n) {
                    (Some(a), Some(b)) => !decoded_values_equal(a, b),
                    (Some(_), None) | (None, Some(_)) => true,
                    (None, None) => false,
                };
                if changed {
                    bits[this] = true;
                    any = true;
                }
            }
        }
    }
    any
}

fn encode_values_for_bits(
    value: &DecodedValue,
    desc: &StructureDesc,
    bits: &[bool],
    idx: &mut usize,
    is_be: bool,
    out: &mut Vec<u8>,
) {
    for field in &desc.fields {
        let this = *idx;
        *idx += 1;
        let sub = get_field_by_name(value, &field.name);
        match &field.field_type {
            FieldType::Structure(inner) => {
                let empty = DecodedValue::Structure(Vec::new());
                let v = sub.unwrap_or(&empty);
                encode_values_for_bits(v, inner, bits, idx, is_be, out);
            }
            _ => {
                if bits[this] {
                    if let Some(v) = sub {
                        out.extend_from_slice(&encode_decoded_value(v, is_be));
                    }
                }
            }
        }
    }
}

fn encode_bitset_from_flags(bits: &[bool], is_be: bool) -> Vec<u8> {
    let bitset_size = (bits.len() + 7) / 8;
    let mut bitset = vec![0u8; bitset_size];
    for (i, b) in bits.iter().enumerate() {
        if *b {
            bitset[i / 8] |= 1 << (i % 8);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(&encode_size_pvd(bitset_size, is_be));
    out.extend_from_slice(&bitset);
    out
}

/// Encode a sparse monitor-data delta between `prev` and `next` projected onto
/// `filtered_desc`. Returns `None` if nothing changed in the filtered view
/// (caller should suppress the update). Otherwise returns `(bitset, values)`
/// with only the changed leaves marked and encoded.
pub fn encode_nt_payload_delta(
    prev: &NtPayload,
    next: &NtPayload,
    filtered_desc: &StructureDesc,
    is_be: bool,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let prev_proj = project_payload_on_desc(prev, filtered_desc, is_be);
    let next_proj = project_payload_on_desc(next, filtered_desc, is_be);
    let bits = compute_changed_bits(&prev_proj, &next_proj, filtered_desc)?;
    let bitset = encode_bitset_from_flags(&bits, is_be);
    let mut values = Vec::new();
    let mut idx = 1usize;
    encode_values_for_bits(
        &next_proj,
        filtered_desc,
        &bits,
        &mut idx,
        is_be,
        &mut values,
    );
    Some((bitset, values))
}

/// True if the projections of `prev` and `next` onto `filtered_desc` differ.
///
/// The change detector for filtered subscriptions. Exactly equivalent to
/// `encode_nt_payload_delta(prev, next, filtered_desc, is_be).is_some()`, but
/// stops after the comparison instead of also building a bitset and a value
/// block the caller discards. For an NTNDArray subscriber that discarded block
/// is the whole image.
///
/// Deliberately has **no** `prev == next` fast path: an `NtScalar` with no
/// stored timestamp is stamped with `SystemTime::now()` on each projection, so
/// two projections of one payload legitimately differ and the delta encoder
/// reports a change. Short-circuiting on equality would silently suppress
/// those updates — a behaviour change, not a speed-up.
pub fn nt_payload_changed_on_desc(
    prev: &NtPayload,
    next: &NtPayload,
    filtered_desc: &StructureDesc,
    is_be: bool,
) -> bool {
    let prev_proj = project_payload_on_desc(prev, filtered_desc, is_be);
    let next_proj = project_payload_on_desc(next, filtered_desc, is_be);
    compute_changed_bits(&prev_proj, &next_proj, filtered_desc).is_some()
}

// ---------------------------------------------------------------------------
// pvRequest builder
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// pvRequest builder
// ---------------------------------------------------------------------------

/// Build a pvRequest structure for the given field paths.
///
/// Each entry may be a simple top-level name (e.g. `"value"`) or a
/// dot-separated nested path (e.g. `"alarm.severity"`,
/// `"timeStamp.secondsPastEpoch"`).
///
/// A bare name selects the entire sub-tree rooted at that field. Nested
/// paths produce the corresponding nested sub-structure in the pvRequest so
/// that a PVA server can filter down to the requested leaves.
///
/// Examples:
/// - `encode_pv_request(&["value", "alarm", "timeStamp"], false)` →
///   `field(value,alarm,timeStamp)`
/// - `encode_pv_request(&["alarm.severity"], false)` →
///   `field(alarm{severity})`
///
/// The output is the *full* type-described pvRequest structure: a `0x80`
/// tag followed by the structure descriptor and empty-struct field values.
pub fn encode_pv_request(fields: &[&str], is_be: bool) -> Vec<u8> {
    encode_pv_request_with_options(fields, &[], is_be)
}

/// Build a pvRequest structure with extra `record._options` key/value pairs.
///
/// `options` is an ordered list of `(name, value)` pairs (both strings) that
/// are encoded as `structure record { structure _options { string name; ... } }`
/// alongside the usual `field(...)` selector. This is the standard PVAccess
/// mechanism for requesting transport options such as
/// `pipeline=true,queueSize=N` on a monitor.
///
/// Empty `options` is equivalent to [`encode_pv_request`].
pub fn encode_pv_request_with_options(
    fields: &[&str],
    options: &[(&str, &str)],
    is_be: bool,
) -> Vec<u8> {
    let tree = build_path_tree_from_strs(fields);
    let inner_fields = path_tree_to_field_descs(&tree);

    let field_desc = StructureDesc {
        struct_id: None,
        fields: inner_fields,
    };

    let mut top_fields = vec![FieldDesc {
        name: "field".to_string(),
        field_type: FieldType::Structure(field_desc),
    }];

    if !options.is_empty() {
        let options_desc = StructureDesc {
            struct_id: None,
            fields: options
                .iter()
                .map(|(k, _)| FieldDesc {
                    name: (*k).to_string(),
                    field_type: FieldType::String,
                })
                .collect(),
        };
        let record_desc = StructureDesc {
            struct_id: None,
            fields: vec![FieldDesc {
                name: "_options".to_string(),
                field_type: FieldType::Structure(options_desc),
            }],
        };
        top_fields.push(FieldDesc {
            name: "record".to_string(),
            field_type: FieldType::Structure(record_desc),
        });
    }

    let pv_request_desc = StructureDesc {
        struct_id: None,
        fields: top_fields,
    };

    let mut out = Vec::new();
    out.push(0x80); // structure tag
    out.extend_from_slice(&encode_structure_desc(&pv_request_desc, is_be));
    // Values: the `field` sub-tree is entirely empty structs (no leaves), so
    // it contributes no bytes. If options are present, append the string
    // values for record._options in declared order.
    for (_, v) in options {
        out.extend_from_slice(&encode_string_pvd(v, is_be));
    }
    out
}

fn build_path_tree_from_strs(paths: &[&str]) -> PathNode {
    let owned: Vec<String> = paths.iter().map(|s| (*s).to_string()).collect();
    build_path_tree(&owned)
}

fn path_tree_to_field_descs(node: &PathNode) -> Vec<FieldDesc> {
    node.children
        .iter()
        .map(|(name, child)| {
            let nested_fields = if child.select_all {
                Vec::new()
            } else {
                path_tree_to_field_descs(child)
            };
            FieldDesc {
                name: name.clone(),
                field_type: FieldType::Structure(StructureDesc {
                    struct_id: None,
                    fields: nested_fields,
                }),
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::spvd_decode::PvdDecoder;

    #[test]
    fn codec_parameters_encode_in_sorted_key_order() {
        use std::collections::HashMap;
        let mut params = HashMap::new();
        // Deliberately inserted out of order, and chosen so sorted order differs
        // from insertion order under any hasher.
        params.insert("zlevel".to_string(), "9".to_string());
        params.insert("algo".to_string(), "blosc".to_string());
        params.insert("mode".to_string(), "shuffle".to_string());

        for is_be in [false, true] {
            let bytes = encode_codec_parameters(&params, is_be);
            // 0x80 (full structure, no id) then the structure descriptor.
            assert_eq!(bytes[0], 0x80);
            let decoder = PvdDecoder::new(is_be);
            // NOTE: `PvdDecoder::parse_structure_desc` is PRIVATE to `spvd_decode`
            // and is not reachable from this module's tests. `parse_introspection`
            // is the public entry point and consumes the leading 0x80 itself.
            let desc = decoder
                .parse_introspection(&bytes)
                .expect("descriptor parses");
            let names: Vec<&str> = desc.fields.iter().map(|f| f.name.as_str()).collect();
            assert_eq!(
                names,
                vec!["algo", "mode", "zlevel"],
                "codec.parameters must encode in sorted key order, is_be={is_be}"
            );
        }
    }

    #[test]
    fn codec_parameters_values_follow_their_keys() {
        use std::collections::HashMap;
        let mut params = HashMap::new();
        params.insert("zlevel".to_string(), "9".to_string());
        params.insert("algo".to_string(), "blosc".to_string());
        params.insert("mode".to_string(), "shuffle".to_string());

        let mut nt = NtNdArray::empty();
        nt.codec.parameters = params;
        let payload = pin_timestamps(NtPayload::NdArray(nt));
        for is_be in [false, true] {
            let desc = nt_payload_desc(&payload);
            assert_eq!(
                encode_decoded_projected(&nt_payload_to_decoded(&payload, is_be), &desc, is_be),
                encode_decoded_projected(
                    // `decode_payload_to_structure` returns `Option<DecodedValue>`
                    // (`spvd_encode.rs:1667`) — it must be unwrapped.
                    &decode_payload_to_structure(&payload, is_be).expect("round trip"),
                    &desc,
                    is_be
                ),
                "decoded tree and round trip disagree on parameter order, is_be={is_be}"
            );
        }
    }

    /// The cheap pre-check must never reject a descriptor that `structure_desc_equal`
    /// would have accepted — that would silently switch the encoder from the full
    /// path to the projected path and change the bytes.
    #[test]
    fn shape_precheck_never_disagrees_with_the_deep_comparison() {
        for (i, payload) in all_payload_samples().into_iter().enumerate() {
            let full = nt_payload_desc(&payload);
            assert!(
                nt_payload_shape_matches(&payload, &full),
                "sample {i}: pre-check rejected the payload's own descriptor"
            );
            // A narrowed descriptor: drop the last top-level field.
            let mut narrowed = full.clone();
            narrowed.fields.pop();
            assert!(
                !nt_payload_shape_matches(&payload, &narrowed),
                "sample {i}: pre-check accepted a narrowed descriptor"
            );
            // Same arity, different identity.
            let mut renamed = full.clone();
            renamed.struct_id = Some("not:the/same:1.0".to_string());
            assert!(
                !nt_payload_shape_matches(&payload, &renamed),
                "sample {i}: pre-check ignored the struct id"
            );
        }
    }

    #[test]
    fn values_for_desc_bytes_unchanged_by_the_precheck() {
        /// The pre-Task-2 implementation, retained verbatim as the reference.
        fn reference_values_for_desc(
            payload: &NtPayload,
            desc: &StructureDesc,
            is_be: bool,
        ) -> Vec<u8> {
            let full_desc = nt_payload_desc(payload);
            if structure_desc_equal(&full_desc, desc) {
                return encode_nt_payload_full(payload, is_be);
            }
            let decoded = nt_payload_to_decoded(payload, is_be);
            encode_decoded_projected(&decoded, desc, is_be)
        }

        for (i, payload) in all_payload_samples().into_iter().enumerate() {
            let full = nt_payload_desc(&payload);
            let mut narrowed = full.clone();
            narrowed.fields.truncate(1);
            for desc in [&full, &narrowed] {
                for is_be in [false, true] {
                    assert_eq!(
                        encode_nt_payload_values_for_desc(&payload, desc, is_be),
                        reference_values_for_desc(&payload, desc, is_be),
                        "sample {i}, is_be={is_be}"
                    );
                }
            }
        }
    }

    #[test]
    fn nt_payload_field_count_matches_the_descriptor_for_every_variant() {
        for (i, payload) in all_payload_samples().into_iter().enumerate() {
            let desc = nt_payload_desc(&payload);
            assert_eq!(
                nt_payload_field_count(&payload),
                crate::spvd_decode::count_structure_fields(&desc),
                "sample {i}: field count disagrees with nt_payload_desc"
            );
        }
    }

    /// The memoised counts assume the four fixed-shape normative types have a
    /// descriptor whose *shape* does not depend on the payload — only the leaf
    /// `TypeCode`s vary. If that ever stops being true the memoised value would be
    /// silently wrong for every payload after the first, so pin it directly.
    #[test]
    fn fixed_shape_descriptors_have_payload_independent_bit_counts() {
        let scalar_baseline =
            crate::spvd_decode::count_structure_fields(&nt_scalar_desc(&ScalarValue::I32(0)));
        let array_baseline = crate::spvd_decode::count_structure_fields(&nt_scalar_array_desc(
            &ScalarArrayValue::I32(Vec::new()),
        ));
        for (i, payload) in all_payload_samples().into_iter().enumerate() {
            match &payload {
                NtPayload::Scalar(nt) => assert_eq!(
                    crate::spvd_decode::count_structure_fields(&nt_scalar_desc(&nt.value)),
                    scalar_baseline,
                    "sample {i}: NTScalar descriptor shape varies with the value type"
                ),
                NtPayload::ScalarArray(nt) => assert_eq!(
                    crate::spvd_decode::count_structure_fields(&nt_scalar_array_desc(&nt.value)),
                    array_baseline,
                    "sample {i}: NTScalarArray descriptor shape varies with the value type"
                ),
                _ => {}
            }
        }
    }

    /// Tables are shaped by their column list; the memoised part must be the
    /// column-independent remainder. Two column counts, so an off-by-one in the
    /// fixed part cannot hide.
    #[test]
    fn table_field_count_tracks_the_column_list() {
        for columns in [
            Vec::new(),
            vec![NtTableColumn {
                name: "a".to_string(),
                values: ScalarArrayValue::I32(vec![1, 2]),
            }],
            vec![
                NtTableColumn {
                    name: "a".to_string(),
                    values: ScalarArrayValue::I32(vec![1, 2]),
                },
                NtTableColumn {
                    name: "b".to_string(),
                    values: ScalarArrayValue::F64(vec![1.0, 2.0]),
                },
                NtTableColumn {
                    name: "c".to_string(),
                    values: ScalarArrayValue::Str(vec!["x".to_string(), "y".to_string()]),
                },
            ],
        ] {
            let table = NtTable {
                labels: columns.iter().map(|c| c.name.clone()).collect(),
                columns,
                descriptor: None,
                alarm: None,
                time_stamp: None,
            };
            let payload = NtPayload::Table(table);
            assert_eq!(
                nt_payload_field_count(&payload),
                crate::spvd_decode::count_structure_fields(&nt_payload_desc(&payload))
            );
        }
    }

    #[test]
    fn all_ones_bitset_matches_the_byte_at_a_time_reference() {
        /// The pre-Task-1 implementation, retained verbatim as the reference.
        fn reference_all_ones(total_bits: usize, is_be: bool) -> Vec<u8> {
            let bitset_size = total_bits.div_ceil(8);
            let mut bitset = vec![0u8; bitset_size];
            for bit in 0..total_bits {
                let byte_idx = bit / 8;
                let bit_idx = bit % 8;
                bitset[byte_idx] |= 1 << bit_idx;
            }
            let mut out = Vec::new();
            out.extend_from_slice(&encode_size_pvd(bitset_size, is_be));
            out.extend_from_slice(&bitset);
            out
        }

        for total_bits in 0..=300usize {
            for is_be in [false, true] {
                assert_eq!(
                    encode_all_ones_bitset(total_bits, is_be),
                    reference_all_ones(total_bits, is_be),
                    "total_bits={total_bits}, is_be={is_be}"
                );
            }
        }
    }

    #[test]
    fn bitset_parts_bytes_unchanged_for_every_sample() {
        /// The pre-Task-1 implementation of `encode_nt_payload_bitset_parts`.
        fn reference_parts(payload: &NtPayload, is_be: bool) -> (Vec<u8>, Vec<u8>) {
            let desc = nt_payload_desc(payload);
            (
                encode_structure_bitset(&desc, is_be),
                encode_nt_payload_full(payload, is_be),
            )
        }

        for (i, payload) in all_payload_samples().into_iter().enumerate() {
            for is_be in [false, true] {
                let (bitset, _) = encode_nt_payload_bitset_parts(&payload, is_be);
                let (want_bitset, _) = reference_parts(&payload, is_be);
                assert_eq!(bitset, want_bitset, "sample {i}, is_be={is_be}");
            }
        }
    }

    #[test]
    fn nt_scalar_roundtrip() {
        let nt = NtScalar::from_value(ScalarValue::F64(12.5));
        let desc = nt_scalar_desc(&nt.value);
        let desc_bytes = encode_structure_desc(&desc, false);
        let mut pvd = Vec::new();
        pvd.push(0x80);
        pvd.extend_from_slice(&desc_bytes);
        pvd.extend_from_slice(&encode_nt_scalar_full(&nt, false));

        let decoder = PvdDecoder::new(false);
        let parsed_desc = decoder.parse_introspection(&pvd).expect("desc");
        let (_, consumed) = decoder
            .decode_structure(&pvd[1 + desc_bytes.len()..], &parsed_desc)
            .expect("value");
        assert!(consumed > 0);
    }

    #[test]
    fn empty_form_choices_sentinel_encodes_standard_choices_on_wire() {
        // 4f regression: `from_value` now leaves `display_form_choices` empty as
        // a "standard choices" sentinel instead of allocating the 7 Strings. The
        // encoder MUST fall back to the 7 standard choices so the wire bytes are
        // byte-identical to explicitly populating them (an empty array on the
        // wire would be a protocol change).
        for is_be in [false, true] {
            let sentinel = NtScalar::from_value(ScalarValue::F64(1.0));
            assert!(
                sentinel.display_form_choices.is_empty(),
                "from_value must use the empty sentinel"
            );

            let mut explicit = NtScalar::from_value(ScalarValue::F64(1.0));
            explicit.display_form_choices = STANDARD_FORM_CHOICES
                .iter()
                .map(|s| s.to_string())
                .collect();

            // Byte-identity: sentinel form encoding == explicit-choices encoding.
            assert_eq!(
                encode_display(&sentinel, is_be),
                encode_display(&explicit, is_be),
                "empty sentinel must encode identically to explicit standard choices (is_be={is_be})"
            );

            // And the 7 choices genuinely reach the wire (decode round-trip).
            let decoded = decode_nt_full(&sentinel, is_be);
            let choices = find_form_choices(&decoded);
            assert_eq!(
                choices,
                STANDARD_FORM_CHOICES.to_vec(),
                "the 7 standard choices must be present on the wire (is_be={is_be})"
            );
        }
    }

    /// Walk the decoded NtScalar tree and return `display.form.choices`.
    fn find_form_choices(v: &DecodedValue) -> Vec<String> {
        fn get<'a>(v: &'a DecodedValue, key: &str) -> Option<&'a DecodedValue> {
            if let DecodedValue::Structure(fields) = v {
                fields.iter().find(|(n, _)| n == key).map(|(_, val)| val)
            } else {
                None
            }
        }
        let choices = get(v, "display")
            .and_then(|d| get(d, "form"))
            .and_then(|f| get(f, "choices"));
        match choices {
            Some(DecodedValue::Array(items)) => items
                .iter()
                .filter_map(|i| match i {
                    DecodedValue::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    // --- timeStamp encoding: stored value honored, stable, and distinct -----
    //
    // Regression tests for the "monitor seconds freeze, only nanoseconds tick"
    // bug: `encode_timestamp` used to always stamp `SystemTime::now()`, so the
    // delta path (which re-encodes prev and next at send time) sampled now()
    // twice microseconds apart — leaving secondsPastEpoch identical (never
    // flagged changed) while nanoseconds spuriously differed. A stored
    // `time_stamp` must be encoded verbatim and be stable across encodes so
    // that prev/next deltas compare correctly.

    fn decode_nt_full(nt: &NtScalar, is_be: bool) -> DecodedValue {
        let desc = nt_scalar_desc(&nt.value);
        let desc_bytes = encode_structure_desc(&desc, is_be);
        let mut pvd = Vec::new();
        pvd.push(0x80);
        pvd.extend_from_slice(&desc_bytes);
        pvd.extend_from_slice(&encode_nt_scalar_full(nt, is_be));
        let decoder = PvdDecoder::new(is_be);
        let parsed_desc = decoder.parse_introspection(&pvd).expect("desc");
        let (val, _) = decoder
            .decode_structure(&pvd[1 + desc_bytes.len()..], &parsed_desc)
            .expect("value");
        val
    }

    fn timestamp_fields(v: &DecodedValue) -> (i64, i32) {
        let DecodedValue::Structure(fields) = v else {
            panic!("top-level not a structure");
        };
        let (_, ts) = fields
            .iter()
            .find(|(n, _)| n == "timeStamp")
            .expect("timeStamp field");
        let DecodedValue::Structure(ts_fields) = ts else {
            panic!("timeStamp not a structure");
        };
        let mut secs = None;
        let mut nanos = None;
        for (n, val) in ts_fields {
            match (n.as_str(), val) {
                ("secondsPastEpoch", DecodedValue::Int64(s)) => secs = Some(*s),
                ("nanoseconds", DecodedValue::Int32(ns)) => nanos = Some(*ns),
                _ => {}
            }
        }
        (secs.expect("secondsPastEpoch"), nanos.expect("nanoseconds"))
    }

    #[test]
    fn encode_timestamp_honors_stored_value() {
        for is_be in [false, true] {
            let nt = NtScalar::from_value(ScalarValue::F64(1.0)).with_timestamp(1_234_567_890, 42);
            let (secs, nanos) = timestamp_fields(&decode_nt_full(&nt, is_be));
            assert_eq!(
                secs, 1_234_567_890,
                "stored seconds must be encoded verbatim"
            );
            assert_eq!(nanos, 42, "stored nanoseconds must be encoded verbatim");
        }
    }

    #[test]
    fn stored_timestamp_is_stable_across_encodes() {
        // The property that fixes the delta: encoding the same NtScalar twice
        // yields byte-identical timeStamps (unlike the now() fallback).
        let nt = NtScalar::from_value(ScalarValue::F64(1.0)).with_timestamp(1000, 500);
        let a = encode_nt_scalar_full(&nt, false);
        let b = encode_nt_scalar_full(&nt, false);
        assert_eq!(a, b, "a stored timestamp must be stable across encodes");
    }

    #[test]
    fn distinct_stored_timestamps_flag_seconds_changed() {
        // Two same-value snapshots one second apart must produce a delta whose
        // secondsPastEpoch differs — i.e. the seconds are reported as changed.
        let prev =
            NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(1.0)).with_timestamp(1000, 0));
        let next =
            NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(1.0)).with_timestamp(1001, 0));
        let desc = nt_scalar_desc(&ScalarValue::F64(1.0));
        let (_bitset, values) =
            encode_nt_payload_delta(&prev, &next, &desc, false).expect("delta present");
        // The changed values must carry the new seconds (1001), not be empty.
        assert!(!values.is_empty(), "delta must carry changed field values");
        // Full-encode sanity: prev and next decode to different seconds.
        let NtPayload::Scalar(prev_nt) = &prev else {
            unreachable!()
        };
        let NtPayload::Scalar(next_nt) = &next else {
            unreachable!()
        };
        assert_eq!(timestamp_fields(&decode_nt_full(prev_nt, false)).0, 1000);
        assert_eq!(timestamp_fields(&decode_nt_full(next_nt, false)).0, 1001);
    }

    #[test]
    fn none_timestamp_falls_back_to_now() {
        // Backward compatibility: no stored timestamp -> stamp current time.
        let nt = NtScalar::from_value(ScalarValue::F64(1.0));
        let (secs, _) = timestamp_fields(&decode_nt_full(&nt, false));
        assert!(
            secs > 1_700_000_000,
            "now() fallback should yield a recent epoch, got {secs}"
        );
    }

    #[test]
    fn nt_ndarray_roundtrip() {
        use spvirit_types::{
            NdCodec, NdDimension, NtAlarm, NtNdArray, NtTimeStamp, ScalarArrayValue,
        };
        use std::collections::HashMap;

        let nt = NtNdArray {
            value: ScalarArrayValue::U8(vec![1, 2, 3, 4]),
            codec: NdCodec {
                name: String::new(),
                parameters: HashMap::new(),
            },
            compressed_size: 4,
            uncompressed_size: 4,
            dimension: vec![NdDimension {
                size: 2,
                offset: 0,
                full_size: 2,
                binning: 1,
                reverse: false,
            }],
            unique_id: 42,
            data_time_stamp: NtTimeStamp {
                seconds_past_epoch: 1000,
                nanoseconds: 500,
                user_tag: 0,
            },
            attribute: Vec::new(),
            descriptor: Some("test".to_string()),
            alarm: Some(NtAlarm::default()),
            time_stamp: Some(NtTimeStamp::default()),
            display: None,
        };

        let desc = nt_ndarray_desc();
        let desc_bytes = encode_structure_desc(&desc, false);
        let data_bytes = encode_nt_ndarray_full(&nt, false);

        // Build complete PVD: type_tag + desc + data
        let mut pvd = Vec::new();
        pvd.push(0x80);
        pvd.extend_from_slice(&desc_bytes);
        pvd.extend_from_slice(&data_bytes);

        let decoder = PvdDecoder::new(false);
        let parsed_desc = decoder
            .parse_introspection(&pvd)
            .expect("desc parse failed");
        let data_start = 1 + desc_bytes.len();
        let (_decoded, consumed) = decoder
            .decode_structure(&pvd[data_start..], &parsed_desc)
            .expect("data decode failed");
        assert!(consumed > 0, "consumed should be > 0");
        assert_eq!(
            consumed,
            data_bytes.len(),
            "consumed should match data_bytes.len()"
        );
    }

    #[test]
    fn nt_table_wire_format_carries_metadata() {
        use spvirit_types::{NtTable, NtTableColumn, ScalarArrayValue};

        // NTTable per the normative type spec carries descriptor, alarm and
        // timeStamp. The EPICS Archiver Appliance in particular requires a
        // top-level timeStamp field to archive a structure at all.
        let nt = NtTable {
            labels: vec!["a".to_string()],
            columns: vec![NtTableColumn {
                name: "a".to_string(),
                values: ScalarArrayValue::F64(vec![1.0, 2.0]),
            }],
            descriptor: Some("desc".to_string()),
            alarm: None,
            time_stamp: Some(NtTimeStamp {
                seconds_past_epoch: 1_700_000_000,
                nanoseconds: 42,
                user_tag: 0,
            }),
        };

        let desc = nt_table_desc(&nt);
        let names: Vec<&str> = desc.fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"timeStamp"), "desc must expose timeStamp");
        assert!(names.contains(&"alarm"), "desc must expose alarm");
        assert!(names.contains(&"descriptor"), "desc must expose descriptor");

        // Encoded data must decode fully and consistently against the desc.
        let data_bytes = encode_nt_table_full(&nt, false);
        let decoder = PvdDecoder::new(false);
        let (decoded, consumed) = decoder
            .decode_structure(&data_bytes, &desc)
            .expect("data decode failed");
        assert_eq!(consumed, data_bytes.len());
        let DecodedValue::Structure(fields) = decoded else {
            panic!("expected structure");
        };
        let (_, ts) = fields
            .iter()
            .find(|(n, _)| n == "timeStamp")
            .expect("timeStamp value present");
        let DecodedValue::Structure(ts_fields) = ts else {
            panic!("expected timeStamp structure");
        };
        let (_, secs) = ts_fields
            .iter()
            .find(|(n, _)| n == "secondsPastEpoch")
            .expect("secondsPastEpoch present");
        assert!(
            matches!(secs, DecodedValue::Int64(1_700_000_000)),
            "expected secondsPastEpoch=1700000000, got {secs:?}"
        );
    }

    #[test]
    fn pv_request_flat_roundtrip() {
        for is_be in [false, true] {
            let body = encode_pv_request(&["value", "alarm", "timeStamp"], is_be);
            let fields = decode_pv_request_fields(&body, is_be).expect("fields");
            assert_eq!(fields, vec!["value", "alarm", "timeStamp"]);
        }
    }

    #[test]
    fn pv_request_nested_roundtrip() {
        let body = encode_pv_request(&["alarm.severity", "timeStamp.secondsPastEpoch"], false);
        let fields = decode_pv_request_fields(&body, false).expect("fields");
        assert_eq!(
            fields,
            vec![
                "alarm.severity".to_string(),
                "timeStamp.secondsPastEpoch".to_string()
            ]
        );
    }

    #[test]
    fn pv_request_whole_subtree_beats_leaf() {
        // Requesting both "alarm" and "alarm.severity" should collapse to the
        // whole-subtree selection.
        let body = encode_pv_request(&["alarm.severity", "alarm"], false);
        let fields = decode_pv_request_fields(&body, false).expect("fields");
        assert_eq!(fields, vec!["alarm".to_string()]);
    }

    #[test]
    fn pv_request_with_pipeline_options_roundtrip() {
        for is_be in [false, true] {
            let body = encode_pv_request_with_options(
                &["value", "alarm"],
                &[("pipeline", "true"), ("queueSize", "4")],
                is_be,
            );
            // Field selectors still parse correctly.
            let fields = decode_pv_request_fields(&body, is_be).expect("fields");
            assert_eq!(fields, vec!["value".to_string(), "alarm".to_string()]);
            // Options round-trip.
            let opts = decode_pv_request_options(&body, is_be).expect("opts");
            assert_eq!(
                opts,
                vec![
                    ("pipeline".to_string(), "true".to_string()),
                    ("queueSize".to_string(), "4".to_string()),
                ]
            );
        }
    }

    #[test]
    fn pv_request_without_options_has_no_record() {
        let body = encode_pv_request(&["value"], false);
        assert!(decode_pv_request_options(&body, false).is_none());
    }

    #[test]
    fn pv_request_empty_body_none() {
        assert!(decode_pv_request_fields(&[], false).is_none());
    }

    #[test]
    fn filter_structure_desc_nested() {
        let alarm = StructureDesc {
            struct_id: Some("alarm_t".to_string()),
            fields: vec![
                FieldDesc {
                    name: "severity".into(),
                    field_type: FieldType::Scalar(TypeCode::Int32),
                },
                FieldDesc {
                    name: "status".into(),
                    field_type: FieldType::Scalar(TypeCode::Int32),
                },
                FieldDesc {
                    name: "message".into(),
                    field_type: FieldType::String,
                },
            ],
        };
        let desc = StructureDesc {
            struct_id: Some("epics:nt/NTScalar:1.0".into()),
            fields: vec![
                FieldDesc {
                    name: "value".into(),
                    field_type: FieldType::Scalar(TypeCode::Float64),
                },
                FieldDesc {
                    name: "alarm".into(),
                    field_type: FieldType::Structure(alarm.clone()),
                },
            ],
        };

        let pruned = filter_structure_desc(&desc, &["alarm.severity".to_string()]);
        assert_eq!(pruned.fields.len(), 1);
        assert_eq!(pruned.fields[0].name, "alarm");
        match &pruned.fields[0].field_type {
            FieldType::Structure(inner) => {
                assert_eq!(inner.fields.len(), 1);
                assert_eq!(inner.fields[0].name, "severity");
            }
            other => panic!("expected Structure, got {:?}", other),
        }

        // Whole-subtree selection preserves the full alarm.
        let pruned_all = filter_structure_desc(&desc, &["alarm".to_string()]);
        match &pruned_all.fields[0].field_type {
            FieldType::Structure(inner) => assert_eq!(inner.fields.len(), 3),
            other => panic!("expected Structure, got {:?}", other),
        }

        // Unknown paths are silently dropped.
        let pruned_unknown = filter_structure_desc(&desc, &["nope".into(), "alarm.missing".into()]);
        assert!(pruned_unknown.fields.is_empty());
    }

    #[test]
    fn filtered_monitor_round_trip_nested() {
        use crate::spvd_decode::PvdDecoder;
        use spvirit_types::{NtPayload, NtScalar, ScalarValue};

        let mut nt = NtScalar::from_value(ScalarValue::F64(42.0));
        nt.alarm_severity = 2;
        nt.alarm_status = 7;
        nt.alarm_message = "hi".into();
        let payload = NtPayload::Scalar(nt);

        // Client sends field(alarm.severity).
        let full_desc = nt_payload_desc(&payload);
        let paths = vec!["alarm.severity".to_string()];
        let filtered = filter_structure_desc(&full_desc, &paths);
        let (bitset, values) = encode_nt_payload_filtered(&payload, &filtered, false);

        // Round-trip the filtered body using the filtered descriptor.
        let decoder = PvdDecoder::new(false);
        let mut body = bitset.clone();
        body.extend_from_slice(&values);
        let (decoded, _) = decoder
            .decode_structure_with_bitset(&body, &filtered)
            .expect("decode filtered");

        let DecodedValue::Structure(fields) = decoded else {
            panic!("expected structure");
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, "alarm");
        match &fields[0].1 {
            DecodedValue::Structure(inner) => {
                assert_eq!(inner.len(), 1);
                assert_eq!(inner[0].0, "severity");
                assert!(matches!(inner[0].1, DecodedValue::Int32(2)));
            }
            other => panic!("expected Structure, got {:?}", other),
        }

        // Verify that the filtered payload is genuinely smaller than an
        // unfiltered one (proves we're not emitting status/message bytes).
        let full_body_len = encode_nt_payload_full(&payload, false).len();
        assert!(values.len() < full_body_len);
    }

    #[test]
    fn delta_returns_none_when_nothing_changed() {
        use spvirit_types::{NtPayload, NtScalar, ScalarValue};
        let mut a = NtScalar::from_value(ScalarValue::F64(1.0));
        a.alarm_severity = 1;
        let p1 = NtPayload::Scalar(a.clone());
        let p2 = NtPayload::Scalar(a);
        let desc = filter_structure_desc(&nt_payload_desc(&p1), &["alarm.severity".to_string()]);
        assert!(encode_nt_payload_delta(&p1, &p2, &desc, false).is_none());
    }

    #[test]
    fn delta_marks_only_changed_leaf() {
        use crate::spvd_decode::PvdDecoder;
        use spvirit_types::{NtPayload, NtScalar, ScalarValue};

        let mut a = NtScalar::from_value(ScalarValue::F64(1.0));
        a.alarm_severity = 1;
        a.alarm_status = 0;
        a.alarm_message = "ok".into();
        let mut b = a.clone();
        b.alarm_severity = 2; // only this leaf changes
        let p1 = NtPayload::Scalar(a);
        let p2 = NtPayload::Scalar(b);
        let desc = filter_structure_desc(
            &nt_payload_desc(&p1),
            &[
                "alarm.severity".to_string(),
                "alarm.status".to_string(),
                "alarm.message".to_string(),
            ],
        );

        let (bitset, values) = encode_nt_payload_delta(&p1, &p2, &desc, false)
            .expect("delta must produce a frame when a leaf changed");

        // Only one leaf bit set (severity). Bit layout for filtered_desc:
        //   bit 0 = root, bit 1 = alarm struct, bit 2 = severity,
        //   bit 3 = status, bit 4 = message.
        // => 5 bits → 1-byte bitset payload. The first byte is the size
        // prefix (1), followed by the bitset byte.
        assert_eq!(bitset[0], 1u8, "size prefix");
        let b0 = bitset[1];
        assert_eq!(b0 & 0x01, 0, "root bit must be clear");
        assert_eq!(b0 & 0x02, 0, "alarm struct bit must be clear");
        assert_eq!(b0 & 0x04, 0x04, "severity bit must be set");
        assert_eq!(b0 & 0x08, 0, "status bit must be clear");
        assert_eq!(b0 & 0x10, 0, "message bit must be clear");

        // values should contain exactly one i32 (4 bytes).
        assert_eq!(values.len(), 4);

        // Round-trip through the decoder and check severity.
        let decoder = PvdDecoder::new(false);
        let mut body = bitset.clone();
        body.extend_from_slice(&values);
        let (decoded, _) = decoder
            .decode_structure_with_bitset(&body, &desc)
            .expect("decode delta");
        let DecodedValue::Structure(fields) = decoded else {
            panic!("expected struct")
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, "alarm");
        match &fields[0].1 {
            DecodedValue::Structure(inner) => {
                assert_eq!(inner.len(), 1);
                assert_eq!(inner[0].0, "severity");
                assert!(matches!(inner[0].1, DecodedValue::Int32(2)));
            }
            other => panic!("expected struct got {:?}", other),
        }
    }

    #[test]
    fn decoded_values_equal_treats_nan_as_equal() {
        let a = DecodedValue::Float64(f64::NAN);
        let b = DecodedValue::Float64(f64::NAN);
        assert!(decoded_values_equal(&a, &b));
        let c = DecodedValue::Float32(f32::NAN);
        let d = DecodedValue::Float32(f32::NAN);
        assert!(decoded_values_equal(&c, &d));
        // But different concrete values are still different.
        assert!(!decoded_values_equal(
            &DecodedValue::Float64(1.0),
            &DecodedValue::Float64(2.0)
        ));
    }

    /// The encoded value bytes must line up EXACTLY with the descriptor the
    /// server advertises at GET/MONITOR init — same leaf count, whole buffer
    /// consumed.
    ///
    /// This is not pedantry. `Decoder::decode_structure` stops silently when it
    /// runs out of bytes, so spvirit's own client happily accepted a stream one
    /// field short of its descriptor. A strict client does not: pvxs read past
    /// the end of the frame, declared the message invalid, and dropped the TCP
    /// connection — taking every other channel on it down too. That is how a
    /// missing `display.form` in the NTScalarArray encoder made the gateway's
    /// `clients`/`cache` status PVs permanently unreadable to p4p and pvget.
    #[test]
    fn encoded_payload_bytes_match_the_advertised_descriptor() {
        for payload in all_payload_samples() {
            for is_be in [false, true] {
                let desc = nt_payload_desc(&payload);
                let bytes = encode_nt_payload_full(&payload, is_be);
                let decoder = crate::spvd_decode::PvdDecoder::new(is_be);
                let (decoded, consumed) = decoder
                    .decode_structure(&bytes, &desc)
                    .expect("descriptor-driven decode of our own bytes");

                assert_eq!(
                    consumed,
                    bytes.len(),
                    "descriptor/encoder disagree for {desc:?} (be={is_be}): consumed \
                     {consumed} of {} bytes",
                    bytes.len()
                );
                let DecodedValue::Structure(fields) = &decoded else {
                    panic!("expected a structure for {desc:?}");
                };
                assert_eq!(
                    fields.len(),
                    desc.fields.len(),
                    "descriptor declares {} top-level fields but only {} decoded \
                     for {desc:?} (be={is_be})",
                    desc.fields.len(),
                    fields.len()
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // codec-C1: differential byte-identity of the direct NtPayload projection
    //
    // The production monitor delta path replaced the encode→decode wire
    // round-trip (`decode_payload_to_structure`) with the direct in-memory
    // projection (`nt_payload_to_decoded`). These tests assert the two produce
    // BYTE-IDENTICAL delta output for every NtPayload variant. If the direct
    // mapping diverged from what `decode_structure` yields, deltas would be
    // silently corrupted on the wire — so byte-identity is the whole ballgame.
    // -----------------------------------------------------------------------

    use spvirit_types::{NdCodec, NtControl};
    use std::collections::HashMap;

    fn scalar_sample(v: ScalarValue) -> NtScalar {
        let mut nt = NtScalar::from_value(v).with_timestamp(1_700_000_000, 7);
        nt.time_stamp = Some(NtTimeStamp {
            seconds_past_epoch: 1_700_000_000,
            nanoseconds: 7,
            user_tag: 9,
        });
        nt.alarm_severity = 1;
        nt.alarm_status = 2;
        nt.alarm_message = "warn".into();
        nt.display_low = -1.5;
        nt.display_high = 5.25;
        nt.display_description = "desc".into();
        nt.units = "V".into();
        nt.display_precision = 3;
        nt.display_form_index = 2;
        nt.control_low = -2.0;
        nt.control_high = 6.0;
        nt.control_min_step = 0.5;
        nt.value_alarm_active = true;
        nt.value_alarm_low_alarm_limit = -3.0;
        nt.value_alarm_low_warning_limit = -2.5;
        nt.value_alarm_high_warning_limit = 4.5;
        nt.value_alarm_high_alarm_limit = 5.0;
        nt.value_alarm_low_alarm_severity = 2;
        nt.value_alarm_low_warning_severity = 1;
        nt.value_alarm_high_warning_severity = 1;
        nt.value_alarm_high_alarm_severity = 2;
        nt.value_alarm_hysteresis = 4;
        nt
    }

    fn scalar_array_sample(v: ScalarArrayValue) -> NtScalarArray {
        NtScalarArray {
            value: v,
            alarm: NtAlarm {
                severity: 1,
                status: 2,
                message: "arr".into(),
            },
            time_stamp: NtTimeStamp {
                seconds_past_epoch: 1_700_000_100,
                nanoseconds: 11,
                user_tag: 3,
            },
            display: NtDisplay {
                limit_low: -10.0,
                limit_high: 10.0,
                description: "d".into(),
                units: "A".into(),
                precision: 2,
            },
            control: NtControl {
                limit_low: -9.0,
                limit_high: 9.0,
                min_step: 0.25,
            },
        }
    }

    /// Every NtPayload variant, with non-trivial data in each field.
    fn all_payload_samples() -> Vec<NtPayload> {
        let scalar_values = vec![
            ScalarValue::Bool(true),
            ScalarValue::I8(-5),
            ScalarValue::I16(-300),
            ScalarValue::I32(-70_000),
            ScalarValue::I64(-5_000_000_000),
            ScalarValue::U8(250),
            ScalarValue::U16(60_000),
            ScalarValue::U32(4_000_000_000),
            ScalarValue::U64(18_000_000_000_000_000_000),
            ScalarValue::F32(1.5),
            ScalarValue::F64(2.5),
            ScalarValue::Str("hello".into()),
        ];
        let array_values = vec![
            ScalarArrayValue::Bool(vec![true, false, true]),
            ScalarArrayValue::I8(vec![-1, 2, -3]),
            ScalarArrayValue::I16(vec![-1, 300, -300]),
            ScalarArrayValue::I32(vec![-1, 70_000]),
            ScalarArrayValue::I64(vec![-1, 5_000_000_000]),
            ScalarArrayValue::U8(vec![1, 2, 250]),
            ScalarArrayValue::U16(vec![1, 60_000]),
            ScalarArrayValue::U32(vec![1, 4_000_000_000]),
            ScalarArrayValue::U64(vec![1, 18_000_000_000_000_000_000]),
            ScalarArrayValue::F32(vec![1.5, -2.5]),
            ScalarArrayValue::F64(vec![1.5, -2.5, 3.5]),
            ScalarArrayValue::Str(vec!["a".into(), "bb".into()]),
        ];

        let mut out: Vec<NtPayload> = Vec::new();
        for v in scalar_values {
            out.push(NtPayload::Scalar(scalar_sample(v)));
        }
        for v in array_values {
            out.push(NtPayload::ScalarArray(scalar_array_sample(v)));
        }

        // Table with mixed-type columns and metadata (alarm Some / None both).
        out.push(NtPayload::Table(NtTable {
            labels: vec!["x".into(), "y".into(), "s".into()],
            columns: vec![
                NtTableColumn {
                    name: "x".into(),
                    values: ScalarArrayValue::I32(vec![1, 2, 3]),
                },
                NtTableColumn {
                    name: "y".into(),
                    values: ScalarArrayValue::F64(vec![1.0, 2.0, 3.0]),
                },
                NtTableColumn {
                    name: "s".into(),
                    values: ScalarArrayValue::Str(vec!["p".into(), "q".into(), "r".into()]),
                },
            ],
            descriptor: Some("table".into()),
            alarm: Some(NtAlarm {
                severity: 1,
                status: 1,
                message: "t".into(),
            }),
            time_stamp: Some(NtTimeStamp {
                seconds_past_epoch: 1_700_000_200,
                nanoseconds: 5,
                user_tag: 0,
            }),
        }));
        out.push(NtPayload::Table(NtTable {
            labels: vec!["only".into()],
            columns: vec![NtTableColumn {
                name: "only".into(),
                values: ScalarArrayValue::U16(vec![7, 8]),
            }],
            descriptor: None,
            alarm: None,
            time_stamp: None,
        }));

        // NdArray: numeric value, non-empty codec params, dimensions and
        // attributes with mixed variant types, display Some.
        let mut params = HashMap::new();
        params.insert("level".to_string(), "6".to_string());
        params.insert("mode".to_string(), "fast".to_string());
        out.push(NtPayload::NdArray(NtNdArray {
            value: ScalarArrayValue::U8(vec![1, 2, 3, 4, 5, 6]),
            codec: NdCodec {
                name: "zlib".into(),
                parameters: params,
            },
            compressed_size: 6,
            uncompressed_size: 6,
            dimension: vec![
                NdDimension {
                    size: 2,
                    offset: 0,
                    full_size: 2,
                    binning: 1,
                    reverse: false,
                },
                NdDimension {
                    size: 3,
                    offset: 1,
                    full_size: 3,
                    binning: 1,
                    reverse: true,
                },
            ],
            unique_id: 42,
            data_time_stamp: NtTimeStamp {
                seconds_past_epoch: 1_700_000_300,
                nanoseconds: 1,
                user_tag: 0,
            },
            attribute: vec![
                NtAttribute {
                    name: "ColorMode".into(),
                    value: ScalarValue::Str("Mono".into()),
                    descriptor: "cm".into(),
                    source_type: 0,
                    source: "src".into(),
                },
                NtAttribute {
                    name: "Gain".into(),
                    value: ScalarValue::F64(1.25),
                    descriptor: "g".into(),
                    source_type: 1,
                    source: "cam".into(),
                },
            ],
            descriptor: Some("nd".into()),
            alarm: Some(NtAlarm {
                severity: 0,
                status: 0,
                message: String::new(),
            }),
            time_stamp: Some(NtTimeStamp {
                seconds_past_epoch: 1_700_000_301,
                nanoseconds: 2,
                user_tag: 0,
            }),
            display: Some(NtDisplay {
                limit_low: 0.0,
                limit_high: 255.0,
                description: "px".into(),
                units: "cnt".into(),
                precision: 0,
            }),
        }));
        // NdArray: string-valued union member, empty codec params (Variant Null),
        // no dimensions/attributes, all trailing metadata defaulted.
        out.push(NtPayload::NdArray(NtNdArray {
            value: ScalarArrayValue::Str(vec!["a".into(), "b".into()]),
            codec: NdCodec {
                name: String::new(),
                parameters: HashMap::new(),
            },
            compressed_size: 0,
            uncompressed_size: 0,
            dimension: vec![],
            unique_id: 0,
            data_time_stamp: NtTimeStamp::default(),
            attribute: vec![],
            descriptor: None,
            alarm: None,
            time_stamp: None,
            display: None,
        }));

        // Enum.
        out.push(NtPayload::Enum(NtEnum {
            index: 1,
            choices: vec!["OFF".into(), "ON".into(), "TRIP".into()],
            alarm: NtAlarm {
                severity: 1,
                status: 3,
                message: "e".into(),
            },
            time_stamp: NtTimeStamp {
                seconds_past_epoch: 1_700_000_400,
                nanoseconds: 9,
                user_tag: 0,
            },
        }));

        // Generic recursive structure with scalars, arrays, string arrays and
        // a nested sub-structure.
        out.push(NtPayload::Generic {
            struct_id: "custom_t".into(),
            fields: vec![
                ("i".into(), PvValue::Scalar(ScalarValue::I32(-7))),
                ("u".into(), PvValue::Scalar(ScalarValue::U64(99))),
                ("s".into(), PvValue::Scalar(ScalarValue::Str("txt".into()))),
                (
                    "arr".into(),
                    PvValue::ScalarArray(ScalarArrayValue::F64(vec![1.0, 2.0])),
                ),
                (
                    "strs".into(),
                    PvValue::ScalarArray(ScalarArrayValue::Str(vec!["m".into(), "n".into()])),
                ),
                (
                    "nested".into(),
                    PvValue::Structure {
                        struct_id: "inner_t".into(),
                        fields: vec![
                            ("b".into(), PvValue::Scalar(ScalarValue::Bool(true))),
                            ("f".into(), PvValue::Scalar(ScalarValue::F32(0.5))),
                        ],
                    },
                ),
            ],
        });

        out
    }

    /// Reference delta computed via the OLD encode→decode round-trip
    /// projection. The production `encode_nt_payload_delta` now uses the direct
    /// projection; this reproduces the pre-C1 behavior for differential
    /// comparison.
    fn reference_delta(
        prev: &NtPayload,
        next: &NtPayload,
        desc: &StructureDesc,
        is_be: bool,
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let empty = || DecodedValue::Structure(Vec::new());
        let prev_proj = project_decoded(
            &decode_payload_to_structure(prev, is_be).unwrap_or_else(empty),
            desc,
        );
        let next_proj = project_decoded(
            &decode_payload_to_structure(next, is_be).unwrap_or_else(empty),
            desc,
        );
        let bits = compute_changed_bits(&prev_proj, &next_proj, desc)?;
        let bitset = encode_bitset_from_flags(&bits, is_be);
        let mut values = Vec::new();
        let mut idx = 1usize;
        encode_values_for_bits(&next_proj, desc, &bits, &mut idx, is_be, &mut values);
        Some((bitset, values))
    }

    #[test]
    fn direct_projection_matches_roundtrip_all_variants() {
        for payload in all_payload_samples() {
            for is_be in [false, true] {
                let direct = nt_payload_to_decoded(&payload, is_be);
                let roundtrip = decode_payload_to_structure(&payload, is_be)
                    .expect("round-trip decode must succeed for a full encode");
                // 1. Structural + type-variant identity (names, order, variants).
                assert!(
                    decoded_values_equal(&direct, &roundtrip),
                    "direct projection differs structurally from round-trip\n  \
                     variant tag: {payload:?}\n  direct:    {direct:?}\n  roundtrip: {roundtrip:?}"
                );
                // 2. Byte-identity of a full re-encode (catches width differences
                //    that structural equality alone might miss).
                assert_eq!(
                    encode_decoded_value(&direct, is_be),
                    encode_decoded_value(&roundtrip, is_be),
                    "direct vs round-trip re-encode bytes differ (is_be={is_be})"
                );
            }
        }
    }

    #[test]
    fn direct_delta_output_byte_identical_all_variants() {
        // For each variant, build a "next" that differs from "prev" (mutating a
        // representative leaf) and assert the full sparse-delta output
        // (bitset + values) from the production direct path is byte-identical
        // to the reference round-trip path — over both endiannesses, and for
        // both the full descriptor and a narrowed descriptor.
        for payload in all_payload_samples() {
            let next = mutate_payload(&payload);
            for is_be in [false, true] {
                let full_desc = nt_payload_desc(&payload);

                // Full descriptor.
                let prod = encode_nt_payload_delta(&payload, &next, &full_desc, is_be);
                let refr = reference_delta(&payload, &next, &full_desc, is_be);
                assert_eq!(
                    prod, refr,
                    "delta bytes differ (full desc, is_be={is_be}) for {payload:?}"
                );

                // Narrowed descriptor: keep only the first top-level field
                // (usually `value`) to exercise projection/pruning too.
                if let Some(first) = full_desc.fields.first() {
                    let narrowed = filter_structure_desc(&full_desc, &[first.name.clone()]);
                    let prod_n = encode_nt_payload_delta(&payload, &next, &narrowed, is_be);
                    let refr_n = reference_delta(&payload, &next, &narrowed, is_be);
                    assert_eq!(
                        prod_n, refr_n,
                        "delta bytes differ (narrowed desc, is_be={is_be}) for {payload:?}"
                    );
                }
            }
        }
    }

    /// Produce a modified copy of `payload` that differs in at least one leaf,
    /// so the delta path emits a non-empty frame.
    pub(crate) fn mutate_payload(payload: &NtPayload) -> NtPayload {
        match payload {
            NtPayload::Scalar(nt) => {
                let mut n = nt.clone();
                n.alarm_severity += 1;
                n.time_stamp = Some(NtTimeStamp {
                    seconds_past_epoch: 1_700_000_999,
                    nanoseconds: 123,
                    user_tag: 0,
                });
                NtPayload::Scalar(n)
            }
            NtPayload::ScalarArray(nt) => {
                let mut n = nt.clone();
                n.alarm.severity += 1;
                n.time_stamp.seconds_past_epoch += 1;
                NtPayload::ScalarArray(n)
            }
            NtPayload::Table(nt) => {
                let mut n = nt.clone();
                n.descriptor = Some(format!("{}!", n.descriptor.clone().unwrap_or_default()));
                NtPayload::Table(n)
            }
            NtPayload::NdArray(nt) => {
                let mut n = nt.clone();
                n.unique_id += 1;
                NtPayload::NdArray(n)
            }
            NtPayload::Enum(nt) => {
                let mut n = nt.clone();
                n.index += 1;
                NtPayload::Enum(n)
            }
            NtPayload::Generic { struct_id, fields } => {
                let mut fields = fields.clone();
                if let Some((_, PvValue::Scalar(ScalarValue::I32(v)))) =
                    fields.iter_mut().find(|(n, _)| n == "i")
                {
                    *v += 1;
                }
                NtPayload::Generic {
                    struct_id: struct_id.clone(),
                    fields,
                }
            }
        }
    }

    /// `nt_ndarray_desc` ignored its `NtNdArray` argument entirely: the NTNDArray
    /// descriptor is constant. Pin that so the parameter cannot creep back.
    #[test]
    fn ndarray_descriptor_is_constant() {
        let a = NtNdArray::empty();
        let mut b = NtNdArray::empty();
        b.value = ScalarArrayValue::F64(vec![1.0, 2.0, 3.0]);
        b.compressed_size = 24;
        b.uncompressed_size = 24;
        b.dimension = vec![NdDimension {
            size: 3,
            offset: 0,
            full_size: 3,
            binning: 1,
            reverse: false,
        }];
        let _ = (&a, &b);
        let desc = nt_ndarray_desc();
        assert_eq!(desc.struct_id.as_deref(), Some("epics:nt/NTNDArray:1.0"));
        assert_eq!(desc.fields.len(), 12);
        assert_eq!(
            crate::spvd_decode::count_structure_fields(&desc),
            nt_payload_field_count(&NtPayload::NdArray(a))
        );
        assert_eq!(
            crate::spvd_decode::count_structure_fields(&desc),
            nt_payload_field_count(&NtPayload::NdArray(b))
        );
    }

    /// `encode_timestamp` and `dv_scalar_timestamp` fall back to `SystemTime::now()`
    /// when the payload carries no timestamp, so two encodes of the *same* payload
    /// differ. Any test that compares two encodes must pin the timestamps first.
    pub(crate) fn pin_timestamps(p: NtPayload) -> NtPayload {
        fn ts() -> NtTimeStamp {
            NtTimeStamp {
                seconds_past_epoch: 1_700_000_000,
                nanoseconds: 123_456_789,
                user_tag: 7,
            }
        }
        match p {
            NtPayload::Scalar(mut nt) => {
                nt.time_stamp = Some(ts());
                NtPayload::Scalar(nt)
            }
            NtPayload::Table(mut nt) => {
                nt.time_stamp = Some(ts());
                NtPayload::Table(nt)
            }
            NtPayload::NdArray(mut nt) => {
                nt.time_stamp = Some(ts());
                NtPayload::NdArray(nt)
            }
            // NtScalarArray and NtEnum hold a non-optional NtTimeStamp already.
            other => other,
        }
    }

    pub(crate) fn pinned_time_samples() -> Vec<NtPayload> {
        all_payload_samples().into_iter().map(pin_timestamps).collect()
    }

    /// FNV-1a 64-bit digest, inline so the frozen oracle needs no dependency.
    fn fnv1a_64(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// `(sample, is_be, full_len, full_fnv1a_64, bitset_len, bitset_fnv1a_64)`
    /// for every `pinned_time_samples()` entry, COMPUTED AT 99deee5 — before
    /// the value block was converted to sink form. Never regenerate this from
    /// HEAD: it is the byte guard the sink conversion is checked against.
    const PRE_SINK_VALUE_BLOCK_ORACLE: &[(usize, bool, usize, u64, usize, u64)] = &[
        (0, false, 194, 0x2dae694e3cd3c725, 200, 0xb382bec3407ede2d),
        (0, true, 194, 0xe1176a1e3cf23fe7, 200, 0x5dbf7597765f9b2f),
        (1, false, 194, 0x4b671876619a1717, 200, 0x52a406c152cfb74f),
        (1, true, 194, 0x166fdc9c05795bd1, 200, 0x5742b8c587af8f89),
        (2, false, 195, 0x18fd7ca769f01df0, 201, 0x4df7e18ac579bf78),
        (2, true, 195, 0x7f28d6e7db4d2eae, 201, 0x2b702eeb6dc77d36),
        (3, false, 197, 0x168c65b3f9dd5af1, 203, 0x560fc10538fd0359),
        (3, true, 197, 0x3961906999aefe85, 203, 0x3e52fb97efb4d4cd),
        (4, false, 201, 0x3b350975e6ee9740, 207, 0x7ac71205f4cb49d8),
        (4, true, 201, 0xf696701cbcc3d6a6, 207, 0x45ff88c038c00e9e),
        (5, false, 194, 0xee82b17087f61672, 200, 0x3c311e3914e2a5aa),
        (5, true, 194, 0x0b2ca8f04b698860, 200, 0x8bd4ff0827c1d4d8),
        (6, false, 195, 0x2e4c0799b02db668, 201, 0x1882e231caf31fd0),
        (6, true, 195, 0x8f829bf6a1213156, 201, 0xabf4827fc75ea2fe),
        (7, false, 197, 0x8b3c88a00a7b9b35, 203, 0xa8c89bf4a2d813bd),
        (7, true, 197, 0x11982e99ea2fe82d, 203, 0x37d619355d75f555),
        (8, false, 201, 0x05222dd7624db7b9, 207, 0xecb95514efd3bed1),
        (8, true, 201, 0xed91ef180c231785, 207, 0x313aa63cbaf593fd),
        (9, false, 197, 0xe07cbe21c60ef8b1, 203, 0xf9c54e26847c3199),
        (9, true, 197, 0x4acbca889237d811, 203, 0xd29ecc20fa3a3a59),
        (10, false, 201, 0x8b0f9fedc5d980a6, 207, 0x605c12ff3ed762fe),
        (10, true, 201, 0x7f7b5e9f8566880c, 207, 0xd5b308b593fd1524),
        (11, false, 199, 0xacaa44a68d281751, 205, 0x175f7690013a9b69),
        (11, true, 199, 0x35e83bb9f2552ad3, 205, 0xd4bdf80dc5ea410b),
        (12, false, 143, 0x6736f6edc3d9d88f, 147, 0xd54f94003609eaef),
        (12, true, 143, 0xc00c014175108e8d, 147, 0xb9f7ddcf4936266d),
        (13, false, 143, 0xf645fb74512f58f7, 147, 0xfbdfa643894ad2d7),
        (13, true, 143, 0x3ac03435c8e1f115, 147, 0xf744116b071fd675),
        (14, false, 146, 0xb70c95aed2e0c3ee, 150, 0x74eee2526d68b48e),
        (14, true, 146, 0xe897b92f2d86fa22, 150, 0xbe3067313dda74c2),
        (15, false, 148, 0xbfd045e9f4dc74a0, 152, 0xed1c86e7ae571980),
        (15, true, 148, 0x1c656b39c16abae2, 152, 0xb0ff5ace0d047b42),
        (16, false, 156, 0x12d792919555ff16, 160, 0x8a73171bde7981b6),
        (16, true, 156, 0x2551b692e95d5658, 160, 0x2efd997ef790b078),
        (17, false, 143, 0x2d23442683198f6a, 147, 0xbefece3bfd8c550a),
        (17, true, 143, 0x77492e3ff987d714, 147, 0x3d8708a46bcc53b4),
        (18, false, 144, 0xf5aae1a16a40f1c3, 148, 0x622055dcfa065d63),
        (18, true, 144, 0xc641f92aa9a4c05f, 148, 0xde6cab7f4335a17f),
        (19, false, 148, 0xfa58da65343252ea, 152, 0xbc85a3e1162a264a),
        (19, true, 148, 0x6cde1124b8e28ea0, 152, 0x92b1d365f2e47900),
        (20, false, 156, 0x3a511d50b110efaa, 160, 0x3c8c9d99e174cd4a),
        (20, true, 156, 0x24a0b3cef0ab413c, 160, 0x4b8a617089e7f55c),
        (21, false, 148, 0x9e7264197203b6e9, 152, 0x7a4fce635e347989),
        (21, true, 148, 0x2f4bf4e02588fc39, 152, 0x99b1b90d8757da59),
        (22, false, 164, 0x863a33c34e7a00fc, 168, 0xa7793ee9cc77da1c),
        (22, true, 164, 0x1bc1b55683212080, 168, 0x910dcfa66f3a76a0),
        (23, false, 145, 0x3999d6302f9ddb10, 149, 0xe15ebb48fb64d330),
        (23, true, 145, 0x1ad81954a3755472, 149, 0x4aa3bd57f55aac92),
        (24, false, 84, 0xd32d25aa5f106f8b, 87, 0x3fbd2823b0944499),
        (24, true, 84, 0x48c7fc27960ac185, 87, 0x74dd548410e8b1a3),
        (25, false, 37, 0xf55f6953f33e5ca7, 40, 0xcb3a549831cb36d9),
        (25, true, 37, 0x9895b4859c83e92d, 40, 0x9361254d286db40f),
        (26, false, 281, 0xff31b20edaf173f5, 286, 0x5a7697d58333c60f),
        (26, true, 281, 0x43b57a44eadc930b, 286, 0x0b065570943f79d5),
        (27, false, 157, 0x5bd2fdbb0e0f7caf, 162, 0x68b14ffa83efb77d),
        (27, true, 157, 0xa07d2d119bc9ad87, 162, 0x4c7983f8fd32e525),
        (28, false, 43, 0xb9121811c252f3fe, 46, 0x53b89caac59d5b40),
        (28, true, 43, 0x47a67c0520bb46f6, 46, 0xe8fa8b6734c4e048),
        (29, false, 43, 0x372258954369ccfd, 46, 0x7333b735318511b5),
        (29, true, 43, 0x988ee285d0963ce7, 46, 0x8720c62f13db901f),
    ];

    #[test]
    fn value_block_bytes_match_the_frozen_pre_sink_oracle() {
        let samples = pinned_time_samples();
        assert_eq!(
            PRE_SINK_VALUE_BLOCK_ORACLE.len(),
            samples.len() * 2,
            "sample set changed; the oracle no longer covers it"
        );
        for &(i, is_be, full_len, full_fnv, bitset_len, bitset_fnv) in PRE_SINK_VALUE_BLOCK_ORACLE {
            let full = encode_nt_payload_full(&samples[i], is_be);
            assert_eq!(
                (full.len(), fnv1a_64(&full)),
                (full_len, full_fnv),
                "sample {i} full, is_be={is_be}"
            );
            let bitset = encode_nt_payload_bitset(&samples[i], is_be);
            assert_eq!(
                (bitset.len(), fnv1a_64(&bitset)),
                (bitset_len, bitset_fnv),
                "sample {i} bitset+full, is_be={is_be}"
            );
        }
    }

    // The frozen oracle above is the real byte guard; this only checks push == wrapper.
    #[test]
    fn push_and_encode_forms_agree_byte_for_byte() {
        for (i, payload) in pinned_time_samples().into_iter().enumerate() {
            for is_be in [false, true] {
                let mut sunk = Vec::new();
                push_nt_payload_full(&mut sunk, &payload, is_be);
                assert_eq!(
                    sunk,
                    encode_nt_payload_full(&payload, is_be),
                    "sample {i} full, is_be={is_be}"
                );

                let mut sunk = Vec::new();
                push_nt_payload_bitset(&mut sunk, &payload, is_be);
                assert_eq!(
                    sunk,
                    encode_nt_payload_bitset(&payload, is_be),
                    "sample {i} bitset+full, is_be={is_be}"
                );

                // The sink must append, not overwrite: a non-empty prefix survives.
                let mut sunk = vec![0xAA, 0xBB];
                push_nt_payload_full(&mut sunk, &payload, is_be);
                assert_eq!(&sunk[..2], &[0xAA, 0xBB]);
                assert_eq!(&sunk[2..], &encode_nt_payload_full(&payload, is_be)[..]);

                let mut sunk = vec![0xAA, 0xBB];
                push_nt_payload_bitset(&mut sunk, &payload, is_be);
                assert_eq!(&sunk[..2], &[0xAA, 0xBB]);
                assert_eq!(&sunk[2..], &encode_nt_payload_bitset(&payload, is_be)[..]);
            }
        }
    }

    #[test]
    fn changed_on_desc_agrees_with_the_delta_encoder() {
        for (i, payload) in pinned_time_samples().into_iter().enumerate() {
            let mutated = pin_timestamps(mutate_payload(&payload));
            let full = nt_payload_desc(&payload);
            let mut narrowed = full.clone();
            narrowed.fields.truncate(1);
            for desc in [&full, &narrowed] {
                for is_be in [false, true] {
                    assert_eq!(
                        nt_payload_changed_on_desc(&payload, &payload, desc, is_be),
                        encode_nt_payload_delta(&payload, &payload, desc, is_be).is_some(),
                        "sample {i}: disagreed on an unchanged payload, is_be={is_be}"
                    );
                    assert_eq!(
                        nt_payload_changed_on_desc(&payload, &mutated, desc, is_be),
                        encode_nt_payload_delta(&payload, &mutated, desc, is_be).is_some(),
                        "sample {i}: disagreed on a mutated payload, is_be={is_be}"
                    );
                }
            }
        }
    }

    /// Characterisation of `nt_payload_to_decoded`, written before it was split
    /// into per-type builders. Compares the projected *encoded bytes* — the only
    /// thing the caller of this function ever observes — for the full descriptor
    /// and for three progressively narrower ones.
    #[test]
    fn decoded_tree_projection_is_stable_for_every_sample() {
        for (i, payload) in pinned_time_samples().into_iter().enumerate() {
            let full = nt_payload_desc(&payload);
            let mut one_field = full.clone();
            one_field.fields.truncate(1);
            let mut two_fields = full.clone();
            two_fields.fields.truncate(2);
            for desc in [&full, &two_fields, &one_field] {
                for is_be in [false, true] {
                    let decoded = nt_payload_to_decoded(&payload, is_be);
                    let projected = encode_decoded_projected(&decoded, desc, is_be);
                    // Independent route to the same bytes: encode the payload for
                    // real and decode it back. Byte-identity between the two is the
                    // invariant `nt_payload_to_decoded` exists to preserve.
                    // NOTE: `decode_payload_to_structure` returns `Option<DecodedValue>`
                    // (`spvd_encode.rs:1667`), so it MUST be unwrapped here — passing
                    // the `Option` by reference does not compile.
                    let round_tripped = decode_payload_to_structure(&payload, is_be)
                        .expect("round-trip decode must succeed for a full encode");
                    assert_eq!(
                        projected,
                        encode_decoded_projected(&round_tripped, desc, is_be),
                        "sample {i}: decoded tree diverged from the round trip, is_be={is_be}"
                    );
                }
            }
        }
    }

    /// Skipping a top-level field the target descriptor does not name must not
    /// change one byte of the projected output — `encode_decoded_projected` walks
    /// the descriptor, so an unnamed field was already being discarded.
    #[test]
    fn descriptor_gated_build_projects_identically() {
        for (i, payload) in pinned_time_samples().into_iter().enumerate() {
            let full = nt_payload_desc(&payload);
            let mut one_field = full.clone();
            one_field.fields.truncate(1);
            let mut middle = full.clone();
            middle.fields.remove(0);
            // Non-contiguous multi-field subset (e.g. keep field 0 and field 2,
            // skipping field 1) — every sample here has at least 3 top-level
            // fields (Enum, the smallest, has exactly 3).
            let mut non_contiguous = full.clone();
            non_contiguous.fields = full
                .fields
                .iter()
                .enumerate()
                .filter(|(idx, _)| *idx == 0 || *idx == 2)
                .map(|(_, f)| f.clone())
                .collect();
            for desc in [&full, &one_field, &middle, &non_contiguous] {
                for is_be in [false, true] {
                    let gated = nt_payload_to_decoded_for(&payload, Some(desc), is_be);
                    let ungated = nt_payload_to_decoded_for(&payload, None, is_be);
                    assert_eq!(
                        encode_decoded_projected(&gated, desc, is_be),
                        encode_decoded_projected(&ungated, desc, is_be),
                        "sample {i}: gating changed the projected bytes, is_be={is_be}"
                    );
                }
            }
        }
    }

    /// The above test proves neutrality (gating never changes projected bytes)
    /// but would still pass if `wants_field` always returned `true` — it never
    /// checks that anything was actually skipped. This test proves the skip
    /// itself, for every variant: narrow to a single top-level field (`value`
    /// where the variant has one, otherwise its first field — `Generic`'s
    /// sample has no field named `value`) and assert the gated tree contains
    /// exactly that field and no other.
    ///
    /// RED proof (recorded before the fix, with `wants_field` hard-coded to
    /// `true`, then reverted):
    /// ```text
    /// ---- spvd_encode::tests::descriptor_gate_skips_unrequested_fields_for_every_variant stdout ----
    /// thread 'spvd_encode::tests::descriptor_gate_skips_unrequested_fields_for_every_variant' panicked:
    /// sample 0: gate built fields the descriptor did not name: ["value", "alarm", "timeStamp", "display", "control", "valueAlarm"] != ["value"]
    /// ```
    #[test]
    fn descriptor_gate_skips_unrequested_fields_for_every_variant() {
        for (i, payload) in pinned_time_samples().into_iter().enumerate() {
            let full = nt_payload_desc(&payload);
            let keep_name = full
                .fields
                .iter()
                .find(|f| f.name == "value")
                .map(|f| f.name.clone())
                .unwrap_or_else(|| full.fields[0].name.clone());
            let mut one_field = full.clone();
            one_field.fields.retain(|f| f.name == keep_name);
            let gated = nt_payload_to_decoded_for(&payload, Some(&one_field), false);
            let DecodedValue::Structure(fields) = &gated else {
                panic!("sample {i}: expected a structure");
            };
            let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(
                names,
                vec![keep_name.as_str()],
                "sample {i}: gate built fields the descriptor did not name: {names:?} != [{keep_name:?}]"
            );
        }
    }

    /// The point of the gate: a `field(value)` subscription must not build the
    /// display subtree at all, so the 7 standard form choices are never allocated.
    #[test]
    fn value_only_descriptor_builds_no_display_subtree() {
        let payload = pinned_time_samples()
            .into_iter()
            .find(|p| matches!(p, NtPayload::Scalar(_)))
            .expect("a scalar sample");
        let full = nt_payload_desc(&payload);
        let mut value_only = full.clone();
        value_only.fields.retain(|f| f.name == "value");
        let gated = nt_payload_to_decoded_for(&payload, Some(&value_only), false);
        let DecodedValue::Structure(fields) = &gated else {
            panic!("expected a structure");
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, "value");
    }
}
