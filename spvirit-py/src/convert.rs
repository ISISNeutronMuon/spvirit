//! Conversion between Rust PVAccess value types and Python objects.

use pyo3::exceptions::{PyOverflowError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString};
use serde_json::Value as JsonValue;
use spvirit_codec::spvd_decode::{DecodedValue, TypeCode};
use spvirit_types::{ScalarArrayValue, ScalarValue};

// ─── DecodedValue → Python ───────────────────────────────────────────────────

pub fn decoded_to_py(py: Python<'_>, v: &DecodedValue) -> PyObject {
    match v {
        DecodedValue::Null => py.None(),
        DecodedValue::Boolean(b) => PyBool::new(py, *b).to_owned().into_any().unbind(),
        DecodedValue::Int8(n) => n.into_pyobject(py).expect("i8").into_any().unbind(),
        DecodedValue::Int16(n) => n.into_pyobject(py).expect("i16").into_any().unbind(),
        DecodedValue::Int32(n) => n.into_pyobject(py).expect("i32").into_any().unbind(),
        DecodedValue::Int64(n) => n.into_pyobject(py).expect("i64").into_any().unbind(),
        DecodedValue::UInt8(n) => n.into_pyobject(py).expect("u8").into_any().unbind(),
        DecodedValue::UInt16(n) => n.into_pyobject(py).expect("u16").into_any().unbind(),
        DecodedValue::UInt32(n) => n.into_pyobject(py).expect("u32").into_any().unbind(),
        DecodedValue::UInt64(n) => n.into_pyobject(py).expect("u64").into_any().unbind(),
        DecodedValue::Float32(f) => PyFloat::new(py, *f as f64).into_any().unbind(),
        DecodedValue::Float64(f) => PyFloat::new(py, *f).into_any().unbind(),
        DecodedValue::String(s) => PyString::new(py, s).into_any().unbind(),
        DecodedValue::Raw(data) => PyBytes::new(py, data).into_any().unbind(),
        DecodedValue::Array(arr) => {
            let items: Vec<PyObject> = arr.iter().map(|item| decoded_to_py(py, item)).collect();
            PyList::new(py, &items).expect("list").into_any().unbind()
        }
        DecodedValue::Structure(fields) => {
            let dict = PyDict::new(py);
            for (name, val) in fields {
                dict.set_item(name, decoded_to_py(py, val))
                    .expect("dict set");
            }
            dict.into_any().unbind()
        }
    }
}

// ─── ScalarValue → Python ────────────────────────────────────────────────────

pub fn scalar_to_py(py: Python<'_>, v: &ScalarValue) -> PyObject {
    match v {
        ScalarValue::Bool(b) => PyBool::new(py, *b).to_owned().into_any().unbind(),
        ScalarValue::I8(n) => n.into_pyobject(py).expect("i8").into_any().unbind(),
        ScalarValue::I16(n) => n.into_pyobject(py).expect("i16").into_any().unbind(),
        ScalarValue::I32(n) => n.into_pyobject(py).expect("i32").into_any().unbind(),
        ScalarValue::I64(n) => n.into_pyobject(py).expect("i64").into_any().unbind(),
        ScalarValue::U8(n) => n.into_pyobject(py).expect("u8").into_any().unbind(),
        ScalarValue::U16(n) => n.into_pyobject(py).expect("u16").into_any().unbind(),
        ScalarValue::U32(n) => n.into_pyobject(py).expect("u32").into_any().unbind(),
        ScalarValue::U64(n) => n.into_pyobject(py).expect("u64").into_any().unbind(),
        ScalarValue::F32(f) => PyFloat::new(py, *f as f64).into_any().unbind(),
        ScalarValue::F64(f) => PyFloat::new(py, *f).into_any().unbind(),
        ScalarValue::Str(s) => PyString::new(py, s).into_any().unbind(),
    }
}

// ─── ScalarArrayValue → Python ───────────────────────────────────────────────

pub fn scalar_array_to_py(py: Python<'_>, v: &ScalarArrayValue) -> PyObject {
    match v {
        ScalarArrayValue::U8(a) => PyBytes::new(py, a).into_any().unbind(),
        ScalarArrayValue::Bool(a) => {
            let items: Vec<PyObject> = a
                .iter()
                .map(|x| PyBool::new(py, *x).to_owned().into_any().unbind())
                .collect();
            PyList::new(py, &items).expect("list").into_any().unbind()
        }
        ScalarArrayValue::I8(a) => int_list_i8(py, a),
        ScalarArrayValue::I16(a) => int_list_i16(py, a),
        ScalarArrayValue::I32(a) => int_list_i32(py, a),
        ScalarArrayValue::I64(a) => int_list_i64(py, a),
        ScalarArrayValue::U16(a) => int_list_u16(py, a),
        ScalarArrayValue::U32(a) => int_list_u32(py, a),
        ScalarArrayValue::U64(a) => int_list_u64(py, a),
        ScalarArrayValue::F32(a) => {
            let items: Vec<PyObject> = a
                .iter()
                .map(|x| PyFloat::new(py, *x as f64).into_any().unbind())
                .collect();
            PyList::new(py, &items).expect("list").into_any().unbind()
        }
        ScalarArrayValue::F64(a) => {
            let items: Vec<PyObject> = a
                .iter()
                .map(|x| PyFloat::new(py, *x).into_any().unbind())
                .collect();
            PyList::new(py, &items).expect("list").into_any().unbind()
        }
        ScalarArrayValue::Str(a) => {
            let items: Vec<PyObject> = a
                .iter()
                .map(|x| PyString::new(py, x).into_any().unbind())
                .collect();
            PyList::new(py, &items).expect("list").into_any().unbind()
        }
    }
}

fn int_list_i8(py: Python<'_>, a: &[i8]) -> PyObject {
    let items: Vec<PyObject> = a
        .iter()
        .map(|x| x.into_pyobject(py).expect("i8").into_any().unbind())
        .collect();
    PyList::new(py, &items).expect("list").into_any().unbind()
}
fn int_list_i16(py: Python<'_>, a: &[i16]) -> PyObject {
    let items: Vec<PyObject> = a
        .iter()
        .map(|x| x.into_pyobject(py).expect("i16").into_any().unbind())
        .collect();
    PyList::new(py, &items).expect("list").into_any().unbind()
}
fn int_list_i32(py: Python<'_>, a: &[i32]) -> PyObject {
    let items: Vec<PyObject> = a
        .iter()
        .map(|x| x.into_pyobject(py).expect("i32").into_any().unbind())
        .collect();
    PyList::new(py, &items).expect("list").into_any().unbind()
}
fn int_list_i64(py: Python<'_>, a: &[i64]) -> PyObject {
    let items: Vec<PyObject> = a
        .iter()
        .map(|x| x.into_pyobject(py).expect("i64").into_any().unbind())
        .collect();
    PyList::new(py, &items).expect("list").into_any().unbind()
}
fn int_list_u16(py: Python<'_>, a: &[u16]) -> PyObject {
    let items: Vec<PyObject> = a
        .iter()
        .map(|x| x.into_pyobject(py).expect("u16").into_any().unbind())
        .collect();
    PyList::new(py, &items).expect("list").into_any().unbind()
}
fn int_list_u32(py: Python<'_>, a: &[u32]) -> PyObject {
    let items: Vec<PyObject> = a
        .iter()
        .map(|x| x.into_pyobject(py).expect("u32").into_any().unbind())
        .collect();
    PyList::new(py, &items).expect("list").into_any().unbind()
}
fn int_list_u64(py: Python<'_>, a: &[u64]) -> PyObject {
    let items: Vec<PyObject> = a
        .iter()
        .map(|x| x.into_pyobject(py).expect("u64").into_any().unbind())
        .collect();
    PyList::new(py, &items).expect("list").into_any().unbind()
}

// ─── Python → serde_json::Value ──────────────────────────────────────────────

pub fn py_to_json(obj: &Bound<'_, PyAny>) -> PyResult<JsonValue> {
    if obj.is_none() {
        Ok(JsonValue::Null)
    } else if let Ok(b) = obj.downcast::<PyBool>() {
        Ok(JsonValue::Bool(b.is_true()))
    } else if obj.is_instance_of::<PyInt>() {
        let v: i64 = obj.extract()?;
        Ok(JsonValue::Number(v.into()))
    } else if obj.is_instance_of::<PyFloat>() {
        let v: f64 = obj.extract()?;
        match serde_json::Number::from_f64(v) {
            Some(n) => Ok(JsonValue::Number(n)),
            None => Err(pyo3::exceptions::PyValueError::new_err(
                "float is NaN or Inf",
            )),
        }
    } else if obj.is_instance_of::<PyString>() {
        let v: String = obj.extract()?;
        Ok(JsonValue::String(v))
    } else if let Ok(list) = obj.downcast::<PyList>() {
        let mut arr = Vec::with_capacity(list.len());
        for item in list.iter() {
            arr.push(py_to_json(&item)?);
        }
        Ok(JsonValue::Array(arr))
    } else if let Ok(dict) = obj.downcast::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (k, v) in dict.iter() {
            let key: String = k.extract()?;
            map.insert(key, py_to_json(&v)?);
        }
        Ok(JsonValue::Object(map))
    } else {
        Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "cannot convert {} to PV value",
            obj.get_type().name()?
        )))
    }
}

// ─── Python → ScalarValue ────────────────────────────────────────────────────

pub fn py_to_scalar(obj: &Bound<'_, PyAny>) -> PyResult<ScalarValue> {
    if let Ok(b) = obj.downcast::<PyBool>() {
        Ok(ScalarValue::Bool(b.is_true()))
    } else if obj.is_instance_of::<PyInt>() {
        let v: i64 = obj.extract()?;
        Ok(ScalarValue::I64(v))
    } else if obj.is_instance_of::<PyFloat>() {
        let v: f64 = obj.extract()?;
        Ok(ScalarValue::F64(v))
    } else if obj.is_instance_of::<PyString>() {
        let v: String = obj.extract()?;
        Ok(ScalarValue::Str(v))
    } else {
        Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "cannot convert {} to ScalarValue",
            obj.get_type().name()?
        )))
    }
}

// ─── Python list → ScalarArrayValue ──────────────────────────────────────────

pub fn py_to_scalar_array(obj: &Bound<'_, PyAny>) -> PyResult<ScalarArrayValue> {
    if let Ok(bytes) = obj.downcast::<PyBytes>() {
        return Ok(ScalarArrayValue::U8(bytes.as_bytes().to_vec()));
    }
    let list = obj.downcast::<PyList>().map_err(|_| {
        pyo3::exceptions::PyTypeError::new_err("expected list or bytes for array value")
    })?;
    if list.is_empty() {
        return Ok(ScalarArrayValue::F64(Vec::new()));
    }
    let first = list.get_item(0)?;
    if first.downcast::<PyBool>().is_ok() {
        let v: Vec<bool> = list.extract()?;
        Ok(ScalarArrayValue::Bool(v))
    } else if first.is_instance_of::<PyInt>() {
        let v: Vec<i64> = list.extract()?;
        Ok(ScalarArrayValue::I64(v))
    } else if first.is_instance_of::<PyFloat>() {
        let v: Vec<f64> = list.extract()?;
        Ok(ScalarArrayValue::F64(v))
    } else if first.is_instance_of::<PyString>() {
        let v: Vec<String> = list.extract()?;
        Ok(ScalarArrayValue::Str(v))
    } else {
        Err(pyo3::exceptions::PyTypeError::new_err(
            "array elements must be bool, int, float, or str",
        ))
    }
}

// ─── Typed (strict) conversion ───────────────────────────────────────────────

/// Parse a value-type string per the PvInfo convention. Shared with
/// `source.rs` (PvInfo field types) — keep the vocabulary in one place.
pub(crate) fn parse_type_code(s: &str) -> Option<TypeCode> {
    Some(match s {
        "boolean" | "bool" => TypeCode::Boolean,
        "byte" | "int8" | "i8" => TypeCode::Int8,
        "short" | "int16" | "i16" => TypeCode::Int16,
        "int" | "int32" | "i32" => TypeCode::Int32,
        "long" | "int64" | "i64" => TypeCode::Int64,
        "ubyte" | "uint8" | "u8" => TypeCode::UInt8,
        "ushort" | "uint16" | "u16" => TypeCode::UInt16,
        "uint" | "uint32" | "u32" => TypeCode::UInt32,
        "ulong" | "uint64" | "u64" => TypeCode::UInt64,
        "float" | "float32" | "f32" => TypeCode::Float32,
        "double" | "float64" | "f64" => TypeCode::Float64,
        "string" | "str" => TypeCode::String,
        _ => return None,
    })
}

/// Parse a scalar value-type string, raising ValueError on unknown names.
pub(crate) fn parse_scalar_type(s: &str) -> PyResult<TypeCode> {
    parse_type_code(s.trim()).ok_or_else(|| {
        PyValueError::new_err(format!(
            "unknown value type {s:?}; expected one of boolean, byte, short, int, \
             long, ubyte, ushort, uint, ulong, float, double, string \
             (or aliases like bool/i32/u16/f64)"
        ))
    })
}

/// Canonical wire-type name for a TypeCode (`"ushort"`, `"double"`, ...).
pub(crate) fn wire_type_name(code: TypeCode) -> &'static str {
    match code {
        TypeCode::Boolean => "boolean",
        TypeCode::Int8 => "byte",
        TypeCode::Int16 => "short",
        TypeCode::Int32 => "int",
        TypeCode::Int64 => "long",
        TypeCode::UInt8 => "ubyte",
        TypeCode::UInt16 => "ushort",
        TypeCode::UInt32 => "uint",
        TypeCode::UInt64 => "ulong",
        TypeCode::Float32 => "float",
        TypeCode::Float64 => "double",
        TypeCode::String => "string",
        _ => "?",
    }
}

pub(crate) fn scalar_value_type_code(v: &ScalarValue) -> TypeCode {
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

pub(crate) fn scalar_array_type_code(v: &ScalarArrayValue) -> TypeCode {
    match v {
        ScalarArrayValue::Bool(_) => TypeCode::Boolean,
        ScalarArrayValue::I8(_) => TypeCode::Int8,
        ScalarArrayValue::I16(_) => TypeCode::Int16,
        ScalarArrayValue::I32(_) => TypeCode::Int32,
        ScalarArrayValue::I64(_) => TypeCode::Int64,
        ScalarArrayValue::U8(_) => TypeCode::UInt8,
        ScalarArrayValue::U16(_) => TypeCode::UInt16,
        ScalarArrayValue::U32(_) => TypeCode::UInt32,
        ScalarArrayValue::U64(_) => TypeCode::UInt64,
        ScalarArrayValue::F32(_) => TypeCode::Float32,
        ScalarArrayValue::F64(_) => TypeCode::Float64,
        ScalarArrayValue::Str(_) => TypeCode::String,
    }
}

fn overflow_err(v: impl std::fmt::Display, code: TypeCode) -> PyErr {
    PyOverflowError::new_err(format!(
        "value {v} out of range for {}",
        wire_type_name(code)
    ))
}

fn kind_err(got: &str, code: TypeCode) -> PyErr {
    PyTypeError::new_err(format!(
        "cannot convert {got} to {}",
        wire_type_name(code)
    ))
}

/// Range-check an integer into the requested numeric TypeCode.
fn int_to_code(v: i128, code: TypeCode) -> PyResult<ScalarValue> {
    Ok(match code {
        TypeCode::Int8 => ScalarValue::I8(i8::try_from(v).map_err(|_| overflow_err(v, code))?),
        TypeCode::Int16 => ScalarValue::I16(i16::try_from(v).map_err(|_| overflow_err(v, code))?),
        TypeCode::Int32 => ScalarValue::I32(i32::try_from(v).map_err(|_| overflow_err(v, code))?),
        TypeCode::Int64 => ScalarValue::I64(i64::try_from(v).map_err(|_| overflow_err(v, code))?),
        TypeCode::UInt8 => ScalarValue::U8(u8::try_from(v).map_err(|_| overflow_err(v, code))?),
        TypeCode::UInt16 => {
            ScalarValue::U16(u16::try_from(v).map_err(|_| overflow_err(v, code))?)
        }
        TypeCode::UInt32 => {
            ScalarValue::U32(u32::try_from(v).map_err(|_| overflow_err(v, code))?)
        }
        TypeCode::UInt64 => {
            ScalarValue::U64(u64::try_from(v).map_err(|_| overflow_err(v, code))?)
        }
        // i128 always fits an f32/f64 range (with precision loss, like Python).
        TypeCode::Float32 => ScalarValue::F32(v as f32),
        TypeCode::Float64 => ScalarValue::F64(v as f64),
        _ => return Err(kind_err("int", code)),
    })
}

/// Coerce an f64 into the requested numeric TypeCode (strict rules).
fn float_to_code(f: f64, code: TypeCode) -> PyResult<ScalarValue> {
    match code {
        TypeCode::Float64 => Ok(ScalarValue::F64(f)),
        TypeCode::Float32 => {
            // Precision loss is fine; magnitude overflow is not. NaN/±inf
            // pass through unchanged.
            let narrowed = f as f32;
            if f.is_finite() && narrowed.is_infinite() {
                Err(overflow_err(f, code))
            } else {
                Ok(ScalarValue::F32(narrowed))
            }
        }
        TypeCode::Int8
        | TypeCode::Int16
        | TypeCode::Int32
        | TypeCode::Int64
        | TypeCode::UInt8
        | TypeCode::UInt16
        | TypeCode::UInt32
        | TypeCode::UInt64 => {
            if !f.is_finite() || f.fract() != 0.0 {
                return Err(PyTypeError::new_err(format!(
                    "cannot convert non-integral float {f} to {}",
                    wire_type_name(code)
                )));
            }
            // Out-of-i128-range floats saturate on cast and then fail the
            // integer range check with OverflowError, which is what we want.
            int_to_code(f as i128, code)
        }
        _ => Err(kind_err("float", code)),
    }
}

/// Strictly convert a Python object to a `ScalarValue` of the requested type.
pub(crate) fn py_to_scalar_typed(
    obj: &Bound<'_, PyAny>,
    code: TypeCode,
) -> PyResult<ScalarValue> {
    // bool is an int subclass in Python — check it first and only ever
    // accept it for `boolean`.
    if let Ok(b) = obj.downcast::<PyBool>() {
        return if code == TypeCode::Boolean {
            Ok(ScalarValue::Bool(b.is_true()))
        } else {
            Err(kind_err("bool", code))
        };
    }
    match code {
        TypeCode::Boolean => Err(kind_err(&obj.get_type().name()?.to_string_lossy(), code)),
        TypeCode::String => {
            if obj.is_instance_of::<PyString>() {
                Ok(ScalarValue::Str(obj.extract()?))
            } else {
                Err(kind_err(&obj.get_type().name()?.to_string_lossy(), code))
            }
        }
        _ => {
            if obj.is_instance_of::<PyInt>() {
                // extract::<i128> covers u64; a Python int beyond i128 raises
                // OverflowError from pyo3 itself, consistent with our rules.
                int_to_code(obj.extract::<i128>()?, code)
            } else if obj.is_instance_of::<PyFloat>() {
                float_to_code(obj.extract::<f64>()?, code)
            } else {
                Err(kind_err(&obj.get_type().name()?.to_string_lossy(), code))
            }
        }
    }
}

/// Strictly convert a Python list/bytes to a `ScalarArrayValue` of the
/// requested element type.
pub(crate) fn py_to_scalar_array_typed(
    obj: &Bound<'_, PyAny>,
    code: TypeCode,
) -> PyResult<ScalarArrayValue> {
    if let Ok(bytes) = obj.downcast::<PyBytes>() {
        return match code {
            TypeCode::UInt8 => Ok(ScalarArrayValue::U8(bytes.as_bytes().to_vec())),
            TypeCode::Int8 => Ok(ScalarArrayValue::I8(
                bytes.as_bytes().iter().map(|b| *b as i8).collect(),
            )),
            _ => Err(PyTypeError::new_err(format!(
                "bytes only convert to byte[]/ubyte[] arrays, not {}[]",
                wire_type_name(code)
            ))),
        };
    }
    let list = obj.downcast::<PyList>().map_err(|_| {
        PyTypeError::new_err("expected list or bytes for array value")
    })?;

    macro_rules! collect {
        ($variant:ident, $sv:ident) => {{
            let mut out = Vec::with_capacity(list.len());
            for item in list.iter() {
                match py_to_scalar_typed(&item, code)? {
                    ScalarValue::$sv(x) => out.push(x),
                    _ => unreachable!("py_to_scalar_typed returned wrong variant"),
                }
            }
            ScalarArrayValue::$variant(out)
        }};
    }
    Ok(match code {
        TypeCode::Boolean => collect!(Bool, Bool),
        TypeCode::Int8 => collect!(I8, I8),
        TypeCode::Int16 => collect!(I16, I16),
        TypeCode::Int32 => collect!(I32, I32),
        TypeCode::Int64 => collect!(I64, I64),
        TypeCode::UInt8 => collect!(U8, U8),
        TypeCode::UInt16 => collect!(U16, U16),
        TypeCode::UInt32 => collect!(U32, U32),
        TypeCode::UInt64 => collect!(U64, U64),
        TypeCode::Float32 => collect!(F32, F32),
        TypeCode::Float64 => collect!(F64, F64),
        TypeCode::String => collect!(Str, Str),
        _ => {
            return Err(PyTypeError::new_err(format!(
                "unsupported array element type code {code:?}"
            )));
        }
    })
}

/// `type=`-kwarg helper: typed conversion when a type string is given,
/// today's inference otherwise.
pub(crate) fn py_to_scalar_maybe_typed(
    obj: &Bound<'_, PyAny>,
    ty: Option<&str>,
) -> PyResult<ScalarValue> {
    match ty {
        Some(t) => py_to_scalar_typed(obj, parse_scalar_type(t)?),
        None => py_to_scalar(obj),
    }
}

/// `type=`-kwarg helper for arrays.
pub(crate) fn py_to_scalar_array_maybe_typed(
    obj: &Bound<'_, PyAny>,
    ty: Option<&str>,
) -> PyResult<ScalarArrayValue> {
    match ty {
        Some(t) => py_to_scalar_array_typed(obj, parse_scalar_type(t)?),
        None => py_to_scalar_array(obj),
    }
}

/// Value-level strict coercion of an existing `ScalarValue` to a TypeCode —
/// used by store put paths where the record's current type is the authority.
pub(crate) fn coerce_scalar_value(v: ScalarValue, code: TypeCode) -> PyResult<ScalarValue> {
    if scalar_value_type_code(&v) == code {
        return Ok(v);
    }
    match v {
        ScalarValue::Bool(_) => Err(kind_err("boolean", code)),
        ScalarValue::Str(_) => Err(kind_err("string", code)),
        ScalarValue::F32(f) => float_to_code(f as f64, code),
        ScalarValue::F64(f) => float_to_code(f, code),
        ScalarValue::I8(n) => int_to_code(n as i128, code),
        ScalarValue::I16(n) => int_to_code(n as i128, code),
        ScalarValue::I32(n) => int_to_code(n as i128, code),
        ScalarValue::I64(n) => int_to_code(n as i128, code),
        ScalarValue::U8(n) => int_to_code(n as i128, code),
        ScalarValue::U16(n) => int_to_code(n as i128, code),
        ScalarValue::U32(n) => int_to_code(n as i128, code),
        ScalarValue::U64(n) => int_to_code(n as i128, code),
    }
}

/// Value-level strict coercion of an array to an element TypeCode.
pub(crate) fn coerce_scalar_array_value(
    v: ScalarArrayValue,
    code: TypeCode,
) -> PyResult<ScalarArrayValue> {
    if scalar_array_type_code(&v) == code {
        return Ok(v);
    }
    macro_rules! recollect {
        ($items:expr, $to_scalar:expr, $variant:ident, $sv:ident) => {{
            let mut out = Vec::with_capacity($items.len());
            for item in $items {
                match coerce_scalar_value($to_scalar(item), code)? {
                    ScalarValue::$sv(x) => out.push(x),
                    _ => unreachable!("coerce_scalar_value returned wrong variant"),
                }
            }
            ScalarArrayValue::$variant(out)
        }};
    }
    macro_rules! dispatch_target {
        ($items:expr, $to_scalar:expr) => {
            Ok(match code {
                TypeCode::Boolean => recollect!($items, $to_scalar, Bool, Bool),
                TypeCode::Int8 => recollect!($items, $to_scalar, I8, I8),
                TypeCode::Int16 => recollect!($items, $to_scalar, I16, I16),
                TypeCode::Int32 => recollect!($items, $to_scalar, I32, I32),
                TypeCode::Int64 => recollect!($items, $to_scalar, I64, I64),
                TypeCode::UInt8 => recollect!($items, $to_scalar, U8, U8),
                TypeCode::UInt16 => recollect!($items, $to_scalar, U16, U16),
                TypeCode::UInt32 => recollect!($items, $to_scalar, U32, U32),
                TypeCode::UInt64 => recollect!($items, $to_scalar, U64, U64),
                TypeCode::Float32 => recollect!($items, $to_scalar, F32, F32),
                TypeCode::Float64 => recollect!($items, $to_scalar, F64, F64),
                TypeCode::String => recollect!($items, $to_scalar, Str, Str),
                _ => {
                    return Err(PyTypeError::new_err(format!(
                        "unsupported array element type code {code:?}"
                    )));
                }
            })
        };
    }
    match v {
        ScalarArrayValue::Bool(a) => dispatch_target!(a, ScalarValue::Bool),
        ScalarArrayValue::I8(a) => dispatch_target!(a, ScalarValue::I8),
        ScalarArrayValue::I16(a) => dispatch_target!(a, ScalarValue::I16),
        ScalarArrayValue::I32(a) => dispatch_target!(a, ScalarValue::I32),
        ScalarArrayValue::I64(a) => dispatch_target!(a, ScalarValue::I64),
        ScalarArrayValue::U8(a) => dispatch_target!(a, ScalarValue::U8),
        ScalarArrayValue::U16(a) => dispatch_target!(a, ScalarValue::U16),
        ScalarArrayValue::U32(a) => dispatch_target!(a, ScalarValue::U32),
        ScalarArrayValue::U64(a) => dispatch_target!(a, ScalarValue::U64),
        ScalarArrayValue::F32(a) => dispatch_target!(a, ScalarValue::F32),
        ScalarArrayValue::F64(a) => dispatch_target!(a, ScalarValue::F64),
        ScalarArrayValue::Str(a) => dispatch_target!(a, ScalarValue::Str),
    }
}
