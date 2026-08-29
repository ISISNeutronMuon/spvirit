use serde_json::Value;

use spvirit_codec::spvd_decode::{FieldDesc, FieldType, StructureDesc, TypeCode};
use spvirit_codec::spvd_encode::encode_structure_desc;
use spvirit_codec::spvirit_encode::encode_size_pva;

pub fn encode_put_payload(
    desc: &StructureDesc,
    input: &Value,
    is_be: bool,
) -> Result<Vec<u8>, String> {
    let root = normalize_root(desc, input)?;
    let mut bitset: Vec<u8> = Vec::new();
    let mut data: Vec<u8> = Vec::new();
    let mut bit_offset = 1usize;
    encode_structure_partial(desc, &root, &mut bitset, &mut bit_offset, &mut data, is_be)?;
    if bitset.is_empty() {
        return Err("no fields selected for PUT".to_string());
    }

    let mut out = Vec::new();
    out.extend_from_slice(&encode_size_pva(bitset.len(), is_be));
    out.extend_from_slice(&bitset);
    out.extend_from_slice(&data);
    Ok(out)
}

fn normalize_root(
    desc: &StructureDesc,
    input: &Value,
) -> Result<serde_json::Map<String, Value>, String> {
    if let Some(obj) = input.as_object() {
        return Ok(obj.clone());
    }
    if desc.fields.iter().any(|f| f.name == "value") {
        let mut map = serde_json::Map::new();
        map.insert("value".to_string(), input.clone());
        return Ok(map);
    }
    Err("non-object input requires a 'value' field in structure".to_string())
}

fn set_bit(bitset: &mut Vec<u8>, bit: usize) {
    let idx = bit / 8;
    let mask = 1u8 << (bit % 8);
    if bitset.len() <= idx {
        bitset.resize(idx + 1, 0);
    }
    bitset[idx] |= mask;
}

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

fn encode_structure_partial(
    desc: &StructureDesc,
    values: &serde_json::Map<String, Value>,
    bitset: &mut Vec<u8>,
    bit_offset: &mut usize,
    data: &mut Vec<u8>,
    is_be: bool,
) -> Result<(), String> {
    for key in values.keys() {
        if !desc.fields.iter().any(|f| f.name == *key) {
            return Err(format!("unknown field '{}' in input", key));
        }
    }
    for field in &desc.fields {
        let current_bit = *bit_offset;
        *bit_offset += 1;

        match &field.field_type {
            FieldType::Structure(nested) => {
                if let Some(val) = values.get(&field.name) {
                    if let Some(obj) = val.as_object() {
                        if obj.is_empty() {
                            return Err(format!("field '{}' object has no entries", field.name));
                        }
                        encode_structure_partial(nested, obj, bitset, bit_offset, data, is_be)?;
                    } else {
                        return Err(format!("field '{}' expects object", field.name));
                    }
                } else {
                    let child_count = count_structure_fields(nested);
                    *bit_offset += child_count;
                }
            }
            _ => {
                if let Some(val) = values.get(&field.name) {
                    set_bit(bitset, current_bit);
                    encode_field_value(&field.field_type, val, data, is_be, &field.name)?;
                }
            }
        }
    }
    Ok(())
}

fn encode_structure_full(
    desc: &StructureDesc,
    values: &serde_json::Map<String, Value>,
    data: &mut Vec<u8>,
    is_be: bool,
) -> Result<(), String> {
    for field in &desc.fields {
        let val = values
            .get(&field.name)
            .ok_or_else(|| format!("missing field '{}' for full structure encoding", field.name))?;
        match &field.field_type {
            FieldType::Structure(nested) => {
                let obj = val
                    .as_object()
                    .ok_or_else(|| format!("field '{}' expects object", field.name))?;
                encode_structure_full(nested, obj, data, is_be)?;
            }
            _ => {
                encode_field_value(&field.field_type, val, data, is_be, &field.name)?;
            }
        }
    }
    Ok(())
}

fn encode_field_value(
    field_type: &FieldType,
    val: &Value,
    data: &mut Vec<u8>,
    is_be: bool,
    field_name: &str,
) -> Result<(), String> {
    match field_type {
        FieldType::Scalar(tc) => encode_scalar_value(*tc, val, data, is_be, field_name),
        FieldType::ScalarArray(tc) => encode_scalar_array(*tc, val, data, is_be, field_name),
        FieldType::String | FieldType::BoundedString(_) => {
            encode_string_value(val, data, is_be, field_name)
        }
        FieldType::StringArray => encode_string_array(val, data, is_be, field_name),
        FieldType::Structure(_) => Err(format!(
            "field '{}' expects object (nested structure)",
            field_name
        )),
        FieldType::StructureArray(nested) => {
            encode_structure_array(nested, val, data, is_be, field_name)
        }
        FieldType::Union(fields) => encode_union(fields, val, data, is_be, field_name),
        FieldType::UnionArray(fields) => encode_union_array(fields, val, data, is_be, field_name),
        FieldType::Variant => encode_variant(val, data, is_be, field_name),
        FieldType::VariantArray => encode_variant_array(val, data, is_be, field_name),
    }
}

fn encode_string_value(
    val: &Value,
    data: &mut Vec<u8>,
    is_be: bool,
    field_name: &str,
) -> Result<(), String> {
    let s = match val {
        Value::String(s) => s.clone(),
        _ => return Err(format!("field '{}' expects string", field_name)),
    };
    data.extend_from_slice(&encode_size_pva(s.len(), is_be));
    data.extend_from_slice(s.as_bytes());
    Ok(())
}

fn encode_scalar_value(
    tc: TypeCode,
    val: &Value,
    data: &mut Vec<u8>,
    is_be: bool,
    field_name: &str,
) -> Result<(), String> {
    match tc {
        TypeCode::Boolean => {
            let b = match val {
                Value::Bool(v) => *v,
                Value::Number(n) => {
                    let v = n
                        .as_i64()
                        .ok_or_else(|| format!("field '{}' expects boolean", field_name))?;
                    v != 0
                }
                _ => return Err(format!("field '{}' expects boolean", field_name)),
            };
            data.push(if b { 1 } else { 0 });
            Ok(())
        }
        TypeCode::Int8 => {
            let v = json_to_int(val, field_name)?;
            ensure_range_i64(v, i8::MIN as i64, i8::MAX as i64, field_name)?;
            data.push(v as i8 as u8);
            Ok(())
        }
        TypeCode::Int16 => {
            let v = json_to_int(val, field_name)?;
            ensure_range_i64(v, i16::MIN as i64, i16::MAX as i64, field_name)?;
            let v = v as i16;
            push_i16(data, v, is_be);
            Ok(())
        }
        TypeCode::Int32 => {
            let v = json_to_int(val, field_name)?;
            ensure_range_i64(v, i32::MIN as i64, i32::MAX as i64, field_name)?;
            let v = v as i32;
            push_i32(data, v, is_be);
            Ok(())
        }
        TypeCode::Int64 => {
            let v = json_to_int(val, field_name)?;
            push_i64(data, v, is_be);
            Ok(())
        }
        TypeCode::UInt8 => {
            let v = json_to_uint(val, field_name)?;
            ensure_range_u64(v, u8::MIN as u64, u8::MAX as u64, field_name)?;
            let v = v as u8;
            data.push(v);
            Ok(())
        }
        TypeCode::UInt16 => {
            let v = json_to_uint(val, field_name)?;
            ensure_range_u64(v, u16::MIN as u64, u16::MAX as u64, field_name)?;
            let v = v as u16;
            push_u16(data, v, is_be);
            Ok(())
        }
        TypeCode::UInt32 => {
            let v = json_to_uint(val, field_name)?;
            ensure_range_u64(v, u32::MIN as u64, u32::MAX as u64, field_name)?;
            let v = v as u32;
            push_u32(data, v, is_be);
            Ok(())
        }
        TypeCode::UInt64 => {
            let v = json_to_uint(val, field_name)?;
            push_u64(data, v, is_be);
            Ok(())
        }
        TypeCode::Float32 => {
            let v = json_to_float(val, field_name)? as f32;
            push_f32(data, v, is_be);
            Ok(())
        }
        TypeCode::Float64 => {
            let v = json_to_float(val, field_name)?;
            push_f64(data, v, is_be);
            Ok(())
        }
        TypeCode::String => encode_string_value(val, data, is_be, field_name),
        _ => Err(format!(
            "field '{}' uses unsupported scalar type",
            field_name
        )),
    }
}

fn encode_scalar_array(
    tc: TypeCode,
    val: &Value,
    data: &mut Vec<u8>,
    is_be: bool,
    field_name: &str,
) -> Result<(), String> {
    let items = val
        .as_array()
        .ok_or_else(|| format!("field '{}' expects array", field_name))?;
    data.extend_from_slice(&encode_size_pva(items.len(), is_be));
    for item in items {
        encode_scalar_value(tc, item, data, is_be, field_name)?;
    }
    Ok(())
}

fn encode_string_array(
    val: &Value,
    data: &mut Vec<u8>,
    is_be: bool,
    field_name: &str,
) -> Result<(), String> {
    let items = val
        .as_array()
        .ok_or_else(|| format!("field '{}' expects array", field_name))?;
    data.extend_from_slice(&encode_size_pva(items.len(), is_be));
    for item in items {
        encode_string_value(item, data, is_be, field_name)?;
    }
    Ok(())
}

fn encode_structure_array(
    desc: &StructureDesc,
    val: &Value,
    data: &mut Vec<u8>,
    is_be: bool,
    field_name: &str,
) -> Result<(), String> {
    let items = val
        .as_array()
        .ok_or_else(|| format!("field '{}' expects array", field_name))?;
    data.extend_from_slice(&encode_size_pva(items.len(), is_be));
    for item in items {
        let obj = item
            .as_object()
            .ok_or_else(|| format!("field '{}' expects array of objects", field_name))?;
        encode_structure_full(desc, obj, data, is_be)?;
    }
    Ok(())
}

fn encode_union(
    fields: &[FieldDesc],
    val: &Value,
    data: &mut Vec<u8>,
    is_be: bool,
    field_name: &str,
) -> Result<(), String> {
    let obj = val
        .as_object()
        .ok_or_else(|| format!("field '{}' expects object for union", field_name))?;
    if obj.len() != 1 {
        return Err(format!(
            "field '{}' union expects exactly one selected member",
            field_name
        ));
    }
    let (selected_name, selected_val) = obj.iter().next().expect("len checked");
    let Some((index, field_desc)) = fields
        .iter()
        .enumerate()
        .find(|(_, f)| f.name == *selected_name)
    else {
        return Err(format!(
            "field '{}' union member '{}' is unknown",
            field_name, selected_name
        ));
    };
    data.extend_from_slice(&encode_size_pva(index, is_be));
    encode_field_value(
        &field_desc.field_type,
        selected_val,
        data,
        is_be,
        selected_name,
    )
}

fn encode_union_array(
    fields: &[FieldDesc],
    val: &Value,
    data: &mut Vec<u8>,
    is_be: bool,
    field_name: &str,
) -> Result<(), String> {
    let items = val
        .as_array()
        .ok_or_else(|| format!("field '{}' expects array for union[]", field_name))?;
    data.extend_from_slice(&encode_size_pva(items.len(), is_be));
    for item in items {
        encode_union(fields, item, data, is_be, field_name)?;
    }
    Ok(())
}

fn encode_variant(
    val: &Value,
    data: &mut Vec<u8>,
    is_be: bool,
    field_name: &str,
) -> Result<(), String> {
    match val {
        Value::Null => {
            data.push(0xFF);
            Ok(())
        }
        Value::Bool(_) => {
            data.push(TypeCode::Boolean as u8);
            encode_scalar_value(TypeCode::Boolean, val, data, is_be, field_name)
        }
        Value::Number(n) => {
            if n.as_i64().is_some() || n.as_u64().is_some() {
                data.push(TypeCode::Int32 as u8);
                encode_scalar_value(TypeCode::Int32, val, data, is_be, field_name)
            } else {
                data.push(TypeCode::Float64 as u8);
                encode_scalar_value(TypeCode::Float64, val, data, is_be, field_name)
            }
        }
        Value::String(_) => {
            data.push(TypeCode::String as u8);
            encode_string_value(val, data, is_be, field_name)
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                data.push((TypeCode::Float64 as u8) | 0x08);
                data.extend_from_slice(&encode_size_pva(0, is_be));
                return Ok(());
            }
            if arr.iter().all(Value::is_string) {
                data.push(0x68); // string[]
                return encode_string_array(val, data, is_be, field_name);
            }
            data.push((TypeCode::Float64 as u8) | 0x08);
            encode_scalar_array(TypeCode::Float64, val, data, is_be, field_name)
        }
        Value::Object(map) => {
            let mut desc_fields = Vec::new();
            for key in map.keys() {
                desc_fields.push(FieldDesc {
                    name: key.clone(),
                    field_type: FieldType::String,
                });
            }
            let desc = StructureDesc {
                struct_id: None,
                fields: desc_fields,
            };
            data.push(0x80);
            data.extend_from_slice(&encode_structure_desc(&desc, is_be));
            for key in map.keys() {
                let v = map.get(key).expect("iter key");
                let s = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => {
                        if *b {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        }
                    }
                    Value::Null => String::new(),
                    _ => v.to_string(),
                };
                data.extend_from_slice(&encode_size_pva(s.len(), is_be));
                data.extend_from_slice(s.as_bytes());
            }
            Ok(())
        }
    }
}

fn encode_variant_array(
    val: &Value,
    data: &mut Vec<u8>,
    is_be: bool,
    field_name: &str,
) -> Result<(), String> {
    let items = val
        .as_array()
        .ok_or_else(|| format!("field '{}' expects array for any[]", field_name))?;
    data.extend_from_slice(&encode_size_pva(items.len(), is_be));
    for item in items {
        encode_variant(item, data, is_be, field_name)?;
    }
    Ok(())
}

fn json_to_int(val: &Value, field_name: &str) -> Result<i64, String> {
    match val {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else if let Some(u) = n.as_u64() {
                i64::try_from(u).map_err(|_| format!("field '{}' out of range", field_name))
            } else if let Some(f) = n.as_f64() {
                if (f.fract()).abs() > f64::EPSILON {
                    Err(format!("field '{}' expects integer, got {}", field_name, f))
                } else {
                    Ok(f as i64)
                }
            } else {
                Err(format!("field '{}' expects integer", field_name))
            }
        }
        Value::String(s) => s
            .parse::<i64>()
            .map_err(|_| format!("field '{}' expects integer", field_name)),
        _ => Err(format!("field '{}' expects integer", field_name)),
    }
}

fn json_to_uint(val: &Value, field_name: &str) -> Result<u64, String> {
    match val {
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Ok(u)
            } else if let Some(i) = n.as_i64() {
                if i < 0 {
                    Err(format!("field '{}' expects unsigned integer", field_name))
                } else {
                    Ok(i as u64)
                }
            } else if let Some(f) = n.as_f64() {
                if (f.fract()).abs() > f64::EPSILON || f < 0.0 {
                    Err(format!("field '{}' expects unsigned integer", field_name))
                } else {
                    Ok(f as u64)
                }
            } else {
                Err(format!("field '{}' expects unsigned integer", field_name))
            }
        }
        Value::String(s) => s
            .parse::<u64>()
            .map_err(|_| format!("field '{}' expects unsigned integer", field_name)),
        _ => Err(format!("field '{}' expects unsigned integer", field_name)),
    }
}

fn json_to_float(val: &Value, field_name: &str) -> Result<f64, String> {
    match val {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| format!("field '{}' expects float", field_name)),
        Value::String(s) => s
            .parse::<f64>()
            .map_err(|_| format!("field '{}' expects float", field_name)),
        _ => Err(format!("field '{}' expects float", field_name)),
    }
}

fn ensure_range_i64(value: i64, min: i64, max: i64, field_name: &str) -> Result<(), String> {
    if value < min || value > max {
        Err(format!("field '{}' out of range", field_name))
    } else {
        Ok(())
    }
}

fn ensure_range_u64(value: u64, min: u64, max: u64, field_name: &str) -> Result<(), String> {
    if value < min || value > max {
        Err(format!("field '{}' out of range", field_name))
    } else {
        Ok(())
    }
}

/// Generate the little/big-endian scalar push helpers.
///
/// Each `push_<ty>(data, v, is_be)` appends `v`'s wire bytes to `data` in the
/// requested byte order. `to_le_bytes`/`to_be_bytes` are inherent methods on
/// every primitive numeric type (not a shared trait), so a declarative macro
/// removes the copy-paste while keeping the wire output byte-identical.
macro_rules! push_endian {
    ($($name:ident => $ty:ty),+ $(,)?) => {
        $(
            fn $name(data: &mut Vec<u8>, v: $ty, is_be: bool) {
                let bytes = if is_be {
                    v.to_be_bytes()
                } else {
                    v.to_le_bytes()
                };
                data.extend_from_slice(&bytes);
            }
        )+
    };
}

push_endian! {
    push_i16 => i16,
    push_i32 => i32,
    push_i64 => i64,
    push_u16 => u16,
    push_u32 => u32,
    push_u64 => u64,
    push_f32 => f32,
    push_f64 => f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_codec::spvd_decode::{FieldDesc, FieldType, StructureDesc, TypeCode};

    #[test]
    fn encode_put_scalar_value_bitset() {
        let desc = StructureDesc {
            struct_id: None,
            fields: vec![FieldDesc {
                name: "value".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            }],
        };
        let input = serde_json::json!(1234);
        let payload = encode_put_payload(&desc, &input, false).expect("payload");
        assert_eq!(payload[0], 0x01); // bitset size
        assert_eq!(payload[1], 0x02); // bit 1 set
        let val_bytes = &payload[2..6];
        assert_eq!(val_bytes, &1234i32.to_le_bytes());
    }

    #[test]
    fn encode_put_nested_partial() {
        let nested = StructureDesc {
            struct_id: None,
            fields: vec![FieldDesc {
                name: "unit".to_string(),
                field_type: FieldType::String,
            }],
        };
        let desc = StructureDesc {
            struct_id: None,
            fields: vec![
                FieldDesc {
                    name: "value".to_string(),
                    field_type: FieldType::Scalar(TypeCode::Float64),
                },
                FieldDesc {
                    name: "meta".to_string(),
                    field_type: FieldType::Structure(nested),
                },
            ],
        };
        let input = serde_json::json!({"value": 1.5, "meta": {"unit": "A"}});
        let payload = encode_put_payload(&desc, &input, false).expect("payload");
        assert_eq!(payload[0], 0x01);
        assert_eq!(payload[1], 0x0A); // bits 1 and 3
        let mut expected = Vec::new();
        expected.extend_from_slice(&1.5f64.to_le_bytes());
        expected.extend_from_slice(&encode_size_pva(1, false));
        expected.extend_from_slice(b"A");
        assert_eq!(&payload[2..], expected.as_slice());
    }
}
