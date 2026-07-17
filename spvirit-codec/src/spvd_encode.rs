//! PVD (pvData) Encoding Helpers
//!
//! Minimal encoder for NTScalar introspection and value updates.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::spvd_decode::{FieldDesc, FieldType, StructureDesc, TypeCode};

use spvirit_types::{
    NdDimension, NtAlarm, NtAttribute, NtDisplay, NtEnum, NtNdArray, NtPayload, NtScalar,
    NtScalarArray, NtTable, NtTableColumn, NtTimeStamp, PvValue, ScalarArrayValue, ScalarValue,
};

fn count_structure_fields(desc: &StructureDesc) -> usize {
    let mut count = 0;
    for field in &desc.fields {
        count += 1;
        if let FieldType::Structure(nested) = &field.field_type {
            count += count_structure_fields(nested);
        }
    }
    count
}

pub fn encode_size_pvd(size: usize, is_be: bool) -> Vec<u8> {
    crate::encode_common::encode_size(size, is_be)
}

pub fn encode_string_pvd(value: &str, is_be: bool) -> Vec<u8> {
    crate::encode_common::encode_string(value, is_be)
}

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

fn encode_string_array(values: &[String], is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_size_pvd(values.len(), is_be));
    for v in values {
        out.extend_from_slice(&encode_string_pvd(v, is_be));
    }
    out
}

fn encode_enum(index: i32, choices: &[String], is_be: bool) -> Vec<u8> {
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
    out.extend_from_slice(&encode_enum(
        nt.display_form_index,
        &nt.display_form_choices,
        is_be,
    ));
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
                field_type: FieldType::Structure(StructureDesc {
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
                }),
            },
            FieldDesc {
                name: "timeStamp".to_string(),
                field_type: FieldType::Structure(StructureDesc {
                    struct_id: None,
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
                }),
            },
            FieldDesc {
                name: "display".to_string(),
                field_type: FieldType::Structure(StructureDesc {
                    struct_id: None,
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
                }),
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
            FieldDesc {
                name: "valueAlarm".to_string(),
                field_type: FieldType::Structure(StructureDesc {
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
                }),
            },
        ],
    }
}

pub fn encode_nt_scalar_full(nt: &NtScalar, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_scalar_value(&nt.value, is_be));
    out.extend_from_slice(&encode_alarm(nt, is_be));
    out.extend_from_slice(&encode_timestamp(nt, is_be));
    out.extend_from_slice(&encode_display(nt, is_be));
    out.extend_from_slice(&encode_control(nt, is_be));
    out.extend_from_slice(&encode_value_alarm(nt, is_be));
    out
}

fn encode_structure_bitset(desc: &StructureDesc, is_be: bool) -> Vec<u8> {
    let total_bits = 1 + count_structure_fields(desc);
    let bitset_size = (total_bits + 7) / 8;
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

fn encode_structure_with_bitset(desc: &StructureDesc, nt: &NtScalar, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_structure_bitset(desc, is_be));
    out.extend_from_slice(&encode_nt_scalar_full(nt, is_be));
    out
}

pub fn encode_nt_scalar_bitset(nt: &NtScalar, is_be: bool) -> Vec<u8> {
    let desc = nt_scalar_desc(&nt.value);
    encode_structure_with_bitset(&desc, nt, is_be)
}

pub fn encode_nt_scalar_bitset_parts(nt: &NtScalar, is_be: bool) -> (Vec<u8>, Vec<u8>) {
    let desc = nt_scalar_desc(&nt.value);
    let bitset = encode_structure_bitset(&desc, is_be);
    let values = encode_nt_scalar_full(nt, is_be);
    (bitset, values)
}

fn alarm_desc() -> StructureDesc {
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

fn timestamp_desc() -> StructureDesc {
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

pub fn encode_nt_scalar_array_full(nt: &NtScalarArray, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_scalar_array_value_pvd(&nt.value, is_be));
    out.extend_from_slice(&encode_nt_alarm(&nt.alarm, is_be));
    out.extend_from_slice(&encode_nt_timestamp(&nt.time_stamp, is_be));
    out.extend_from_slice(&encode_nt_display(&nt.display, is_be));
    out.extend_from_slice(&encode_f64(nt.control.limit_low, is_be));
    out.extend_from_slice(&encode_f64(nt.control.limit_high, is_be));
    out.extend_from_slice(&encode_f64(nt.control.min_step, is_be));
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

pub fn encode_nt_table_full(nt: &NtTable, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
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

fn encode_codec_parameters(
    parameters: &std::collections::HashMap<String, String>,
    is_be: bool,
) -> Vec<u8> {
    if parameters.is_empty() {
        return vec![0xFF];
    }
    let mut out = Vec::new();
    out.push(0x80);
    let mut fields = Vec::new();
    for key in parameters.keys() {
        fields.push(FieldDesc {
            name: key.clone(),
            field_type: FieldType::String,
        });
    }
    let desc = StructureDesc {
        struct_id: None,
        fields,
    };
    out.extend_from_slice(&encode_structure_desc(&desc, is_be));
    for value in parameters.values() {
        out.extend_from_slice(&encode_string_pvd(value, is_be));
    }
    out
}

pub fn nt_ndarray_desc_default() -> StructureDesc {
    nt_ndarray_desc(&NtNdArray::empty())
}

pub fn nt_ndarray_desc(_nt: &NtNdArray) -> StructureDesc {
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

pub fn encode_nt_ndarray_full(nt: &NtNdArray, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
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

pub fn encode_nt_enum_full(nt: &NtEnum, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    // value — enum_t { index, choices }
    out.extend_from_slice(&encode_enum(nt.index, &nt.choices, is_be));
    // alarm
    out.extend_from_slice(&encode_nt_alarm(&nt.alarm, is_be));
    // timeStamp
    out.extend_from_slice(&encode_nt_timestamp(&nt.time_stamp, is_be));
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

/// Encode a [`PvValue`] tree to PVA wire bytes (values only, no descriptor).
pub fn encode_pv_value(val: &PvValue, is_be: bool) -> Vec<u8> {
    match val {
        PvValue::Scalar(sv) => encode_scalar_value(sv, is_be),
        PvValue::ScalarArray(sa) => encode_scalar_array_value_pvd(sa, is_be),
        PvValue::Structure { fields, .. } => {
            let mut out = Vec::new();
            for (_, v) in fields {
                out.extend_from_slice(&encode_pv_value(v, is_be));
            }
            out
        }
    }
}

pub fn nt_payload_desc(payload: &NtPayload) -> StructureDesc {
    match payload {
        NtPayload::Scalar(nt) => nt_scalar_desc(&nt.value),
        NtPayload::ScalarArray(nt) => nt_scalar_array_desc(&nt.value),
        NtPayload::Table(nt) => nt_table_desc(nt),
        NtPayload::NdArray(nt) => nt_ndarray_desc(nt),
        NtPayload::Enum(_) => nt_enum_desc(),
        NtPayload::Generic { struct_id, fields } => pv_value_desc(struct_id, fields),
    }
}

pub fn encode_nt_payload_full(payload: &NtPayload, is_be: bool) -> Vec<u8> {
    match payload {
        NtPayload::Scalar(nt) => encode_nt_scalar_full(nt, is_be),
        NtPayload::ScalarArray(nt) => encode_nt_scalar_array_full(nt, is_be),
        NtPayload::Table(nt) => encode_nt_table_full(nt, is_be),
        NtPayload::NdArray(nt) => encode_nt_ndarray_full(nt, is_be),
        NtPayload::Enum(nt) => encode_nt_enum_full(nt, is_be),
        NtPayload::Generic { fields, .. } => {
            let mut out = Vec::new();
            for (_, v) in fields {
                out.extend_from_slice(&encode_pv_value(v, is_be));
            }
            out
        }
    }
}

pub fn encode_nt_payload_bitset(payload: &NtPayload, is_be: bool) -> Vec<u8> {
    let desc = nt_payload_desc(payload);
    let mut out = Vec::new();
    out.extend_from_slice(&encode_structure_bitset(&desc, is_be));
    out.extend_from_slice(&encode_nt_payload_full(payload, is_be));
    out
}

pub fn encode_nt_payload_bitset_parts(payload: &NtPayload, is_be: bool) -> (Vec<u8>, Vec<u8>) {
    let desc = nt_payload_desc(payload);
    (
        encode_structure_bitset(&desc, is_be),
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
    let desc = decoder.parse_introspection(body)?;
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
    let desc = decoder.parse_introspection(body)?;
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

/// Encode the value bytes of an `NtPayload` projected onto a (possibly
/// narrowed) descriptor. Fields not represented in `desc` are omitted;
/// sub-structures are encoded recursively.
pub fn encode_nt_payload_values_for_desc(
    payload: &NtPayload,
    desc: &StructureDesc,
    is_be: bool,
) -> Vec<u8> {
    let full_desc = nt_payload_desc(payload);
    if structure_desc_equal(&full_desc, desc) {
        // Fast path: no narrowing.
        return encode_nt_payload_full(payload, is_be);
    }
    let decoded = decode_payload_to_structure(payload, is_be)
        .unwrap_or_else(|| DecodedValue::Structure(Vec::new()));
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
fn decode_payload_to_structure(payload: &NtPayload, is_be: bool) -> Option<DecodedValue> {
    let desc = nt_payload_desc(payload);
    let bytes = encode_nt_payload_full(payload, is_be);
    let decoder = crate::spvd_decode::PvdDecoder::new(is_be);
    decoder.decode_structure(&bytes, &desc).map(|(v, _)| v)
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
    let decoded = decode_payload_to_structure(payload, is_be)
        .unwrap_or_else(|| DecodedValue::Structure(Vec::new()));
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
    let total = 1 + spvd_count_structure_fields(desc);
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

fn spvd_count_structure_fields(desc: &StructureDesc) -> usize {
    let mut count = 0;
    for field in &desc.fields {
        count += 1;
        if let FieldType::Structure(inner) = &field.field_type {
            count += spvd_count_structure_fields(inner);
        }
    }
    count
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
mod tests {
    use super::*;
    use crate::spvd_decode::PvdDecoder;

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

        let desc = nt_ndarray_desc(&nt);
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
}
