# Python NT Value-Type Selection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let Python users select any of the twelve NTScalar wire value types (`boolean`, `byte`, `short`, `int`, `long`, `ubyte`, `ushort`, `uint`, `ulong`, `float`, `double`, `string`) when constructing NT payloads, PV handles, builder records, and when writing through the store — instead of everything collapsing to `long`/`double`.

**Architecture:** A shared type-string parser and a strict coercion layer live in `spvirit-py/src/convert.rs`. NT classes and factories gain keyword-only `type=`/`types=` parameters. A single new dynamic handle kind `PvKind::Typed(Pv<ScalarValue>, TypeCode)` covers all scalar wire types, backed by one small core change (`impl PvScalar for ScalarValue`). Store put paths coerce to the record's existing wire type.

**Tech Stack:** Rust, pyo3 0.24 (abi3), maturin, Tokio. Python tests are plain-assert scripts run directly (no pytest).

**Spec:** `docs/superpowers/specs/2026-07-16-python-nt-value-types-design.md`

## Global Constraints

- **Backward compatibility:** every new parameter is optional (except `type` on the new `spvirit.scalar` factory) and appended so existing positional calls keep working. `type=None` means today's inference behavior, byte-for-byte.
- **Coercion is strict:** out-of-range numerics raise `OverflowError`; wrong kinds raise `TypeError`; unknown type strings raise `ValueError`. Allowed widenings: `int` → `float`/`double`; lossy `float` → `float` (f32) precision; integral `float` (e.g. `2.0`) → integer types.
- **Type-string vocabulary** (canonical | aliases): `boolean`|`bool`, `byte`|`int8`,`i8`, `short`|`int16`,`i16`, `int`|`int32`,`i32`, `long`|`int64`,`i64`, `ubyte`|`uint8`,`u8`, `ushort`|`uint16`,`u16`, `uint`|`uint32`,`u32`, `ulong`|`uint64`,`u64`, `float`|`float32`,`f32`, `double`|`float64`,`f64`, `string`|`str`.
- **The working tree contains unrelated in-progress changes** (`spvirit-codec/src/spvd_encode.rs`, `spvirit-server/src/simple_store.rs`, `spvirit-server/src/types.rs` — a timestamp/NTTable-metadata feature). NEVER `git add -A` or `git add .`; stage only the files each task explicitly touches. Do not modify `simple_store.rs` or `spvd_encode.rs`.
- **Build command** (from `C:\spvirit\spvirit-py`): `.\.venv\Scripts\maturin.exe develop` (debug build is fine for tests; if maturin is missing run `.\.venv\Scripts\pip.exe install maturin` first).
- **Python test command** (from `C:\spvirit\spvirit-py`): `.\.venv\Scripts\python.exe tests\test_value_types.py`
- **Rust test command** (from `C:\spvirit`): `cargo test -p spvirit-server pv::`
- Python tests use plain `assert` functions named `test_*`, collected by the `main()` loop at the bottom of the file (mirror `tests/test_pv_handles.py`). Every test that starts a `Server` must use its own unique port pair; this plan allocates 16060–16099.
- In Rust, the Python keyword `type` is spelled `r#type` (pyo3 exposes it to Python as `type`).
- Commit messages end with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

---

### Task 1: Core — `PvScalar for ScalarValue` and dynamic scalar constructors

**Files:**
- Modify: `spvirit-server/src/pv.rs` (impls near the existing `PvScalar` impls at lines 105–170; constructors near `impl Pv<i32>` at line 463; tests in the existing `mod tests`)

**Interfaces:**
- Consumes: existing `PvScalar` trait (`spvirit-server/src/pv.rs:85`), `make_scalar_record`/`make_output_record` (`spvirit-server/src/pva_server.rs:869,918` — already `pub(crate)` and support `Ai/Bi/StringIn/LongIn` + `Ao/Bo/StringOut/LongOut` with any `ScalarValue`).
- Produces: `impl PvScalar for ScalarValue` (so `Pv<ScalarValue>` and `server.pv::<ScalarValue>(&name)` compile), `Pv::<ScalarValue>::scalar_in(name, initial)`, `Pv::<ScalarValue>::scalar_out(name, initial)`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `spvirit-server/src/pv.rs` (it already has `empty_store()` and imports `AnyPv`, `ScalarValue`, `DecodedValue`):

```rust
#[test]
fn scalar_value_handle_constructors() {
    let p = Pv::<ScalarValue>::scalar_out("S:U16", ScalarValue::U16(7));
    let rec = p.pending_record().unwrap();
    assert_eq!(rec.record_type, crate::types::RecordType::LongOut);
    assert_eq!(rec.current_value(), ScalarValue::U16(7));
    assert!(rec.writable());

    let q = Pv::<ScalarValue>::scalar_in("S:F32", ScalarValue::F32(1.5));
    let rec = q.pending_record().unwrap();
    assert_eq!(rec.record_type, crate::types::RecordType::Ai);
    assert!(!rec.writable());

    let s = Pv::<ScalarValue>::scalar_in("S:STR", ScalarValue::Str("x".into()));
    assert_eq!(
        s.pending_record().unwrap().record_type,
        crate::types::RecordType::StringIn
    );

    let b = Pv::<ScalarValue>::scalar_out("S:B", ScalarValue::Bool(true));
    assert_eq!(
        b.pending_record().unwrap().record_type,
        crate::types::RecordType::Bo
    );
}

#[tokio::test]
async fn scalar_value_handle_set_get_preserves_variant() {
    let store = empty_store();
    let pv = Pv::<ScalarValue>::scalar_out("S:U64", ScalarValue::U64(1));
    let any: AnyPv = pv.clone().into();
    let rec = any.take_record().unwrap();
    store.insert(rec.name.clone(), rec).await;
    any.bind(&store);

    pv.set(ScalarValue::U64(u64::MAX)).await.unwrap();
    assert_eq!(pv.get().await, Ok(ScalarValue::U64(u64::MAX)));
}

#[test]
fn scalar_value_from_decoded_maps_one_to_one() {
    assert_eq!(
        ScalarValue::from_decoded(&DecodedValue::UInt32(7)),
        Some(ScalarValue::U32(7))
    );
    assert_eq!(
        ScalarValue::from_decoded(&DecodedValue::Int8(-3)),
        Some(ScalarValue::I8(-3))
    );
    assert_eq!(
        ScalarValue::from_decoded(&DecodedValue::Boolean(true)),
        Some(ScalarValue::Bool(true))
    );
    assert_eq!(
        ScalarValue::from_decoded(&DecodedValue::String("hi".into())),
        Some(ScalarValue::Str("hi".into()))
    );
    assert!(ScalarValue::from_decoded(&DecodedValue::Null).is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run (from `C:\spvirit`): `cargo test -p spvirit-server pv::`
Expected: compile error — `scalar_out`/`scalar_in` not found, `PvScalar` not implemented for `ScalarValue`.

- [ ] **Step 3: Implement**

In `spvirit-server/src/pv.rs`, after `impl PvScalar for String` (line ~170), add:

```rust
impl PvScalar for ScalarValue {
    const TYPE_NAME: &'static str = "scalar";
    fn into_scalar(self) -> ScalarValue {
        self
    }
    fn from_scalar(v: ScalarValue) -> Option<Self> {
        Some(v)
    }
    /// Faithful 1:1 structural mapping — deliberately NOT the generic
    /// `decoded_to_scalar_value`, whose truthy-check-first order turns any
    /// nonzero numeric into `Bool` (see the trait doc above). Callers that
    /// need a specific wire type re-coerce the returned variant themselves.
    fn from_decoded(dv: &DecodedValue) -> Option<Self> {
        Some(match dv {
            DecodedValue::Boolean(b) => ScalarValue::Bool(*b),
            DecodedValue::Int8(n) => ScalarValue::I8(*n),
            DecodedValue::Int16(n) => ScalarValue::I16(*n),
            DecodedValue::Int32(n) => ScalarValue::I32(*n),
            DecodedValue::Int64(n) => ScalarValue::I64(*n),
            DecodedValue::UInt8(n) => ScalarValue::U8(*n),
            DecodedValue::UInt16(n) => ScalarValue::U16(*n),
            DecodedValue::UInt32(n) => ScalarValue::U32(*n),
            DecodedValue::UInt64(n) => ScalarValue::U64(*n),
            DecodedValue::Float32(f) => ScalarValue::F32(*f),
            DecodedValue::Float64(f) => ScalarValue::F64(*f),
            DecodedValue::String(s) => ScalarValue::Str(s.clone()),
            _ => return None,
        })
    }
}
```

After `impl Pv<i32>` (the block ending with `from_enum_record`), add:

```rust
/// Family record type for a dynamically typed scalar: the record *shape*
/// (RTYP, writability) comes from the value family, while the `NtScalar`
/// payload's `ScalarValue` variant carries the precise wire type.
fn scalar_family_record_type(v: &ScalarValue, writable: bool) -> RecordType {
    match (v, writable) {
        (ScalarValue::F32(_) | ScalarValue::F64(_), false) => RecordType::Ai,
        (ScalarValue::F32(_) | ScalarValue::F64(_), true) => RecordType::Ao,
        (ScalarValue::Bool(_), false) => RecordType::Bi,
        (ScalarValue::Bool(_), true) => RecordType::Bo,
        (ScalarValue::Str(_), false) => RecordType::StringIn,
        (ScalarValue::Str(_), true) => RecordType::StringOut,
        (_, false) => RecordType::LongIn,
        (_, true) => RecordType::LongOut,
    }
}

impl Pv<ScalarValue> {
    /// Dynamically typed scalar record, read-only over the wire. The wire
    /// value type is whatever `ScalarValue` variant `initial` holds.
    pub fn scalar_in(name: impl Into<String>, initial: ScalarValue) -> Self {
        let name = name.into();
        let rt = scalar_family_record_type(&initial, false);
        Self::from_record(make_scalar_record(&name, rt, initial))
    }
    /// Dynamically typed scalar record, writable over the wire.
    pub fn scalar_out(name: impl Into<String>, initial: ScalarValue) -> Self {
        let name = name.into();
        let rt = scalar_family_record_type(&initial, true);
        Self::from_record(make_output_record(&name, rt, initial))
    }
}
```

(`make_scalar_record`, `make_output_record`, `RecordType` are already imported at the top of `pv.rs` — verify and add to the existing `use` lines if not.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p spvirit-server pv::`
Expected: all pass, including the three new tests.

- [ ] **Step 5: Commit**

```powershell
git add spvirit-server/src/pv.rs
git commit -m "feat(server): PvScalar for ScalarValue + dynamic scalar_in/scalar_out constructors

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: Coercion layer + `type=` on NtScalar/NtScalarArray

**Files:**
- Modify: `spvirit-py/src/convert.rs` (new functions at the bottom)
- Modify: `spvirit-py/src/source.rs` (delete its private `parse_type_code`, import the shared one)
- Modify: `spvirit-py/src/nt.rs` (`PyNtScalar::py_new` line ~237, `PyNtScalarArray::py_new` line ~365, new `value_type` getters)
- Create: `spvirit-py/tests/test_value_types.py`

**Interfaces:**
- Consumes: `TypeCode` from `spvirit_codec::spvd_decode`, existing `py_to_scalar`/`py_to_scalar_array`.
- Produces (all `pub(crate)` in `convert.rs`, used by every later task):
  - `parse_type_code(s: &str) -> Option<TypeCode>` (moved from source.rs, plus `"string" | "str" => TypeCode::String`)
  - `parse_scalar_type(s: &str) -> PyResult<TypeCode>` — ValueError on unknown
  - `wire_type_name(code: TypeCode) -> &'static str` — `"ushort"`-style canonical names
  - `scalar_value_type_code(v: &ScalarValue) -> TypeCode`
  - `scalar_array_type_code(v: &ScalarArrayValue) -> TypeCode`
  - `py_to_scalar_typed(obj: &Bound<'_, PyAny>, code: TypeCode) -> PyResult<ScalarValue>`
  - `py_to_scalar_array_typed(obj: &Bound<'_, PyAny>, code: TypeCode) -> PyResult<ScalarArrayValue>`
  - `py_to_scalar_maybe_typed(obj, ty: Option<&str>) -> PyResult<ScalarValue>`
  - `py_to_scalar_array_maybe_typed(obj, ty: Option<&str>) -> PyResult<ScalarArrayValue>`
  - `coerce_scalar_value(v: ScalarValue, code: TypeCode) -> PyResult<ScalarValue>` (value-level, for Task 8)
  - `coerce_scalar_array_value(v: ScalarArrayValue, code: TypeCode) -> PyResult<ScalarArrayValue>` (for Task 8)
  - Python: `NtScalar(..., type=None)`, `NtScalarArray(value, type=None)`, read-only `.value_type` on both.

- [ ] **Step 1: Write the failing Python tests**

Create `spvirit-py/tests/test_value_types.py`:

```python
"""Plain-assert tests for explicit NT value-type selection. Run directly:
   ./.venv/Scripts/python.exe tests/test_value_types.py
"""
import spvirit


def _expect(exc, fn):
    try:
        fn()
    except exc:
        return
    raise AssertionError(f"expected {exc.__name__}")


def test_ntscalar_type_selection():
    # every wire type constructible, reported via .value_type
    cases = [
        (True, "boolean", True), (-5, "byte", -5), (300, "short", 300),
        (70000, "int", 70000), (2**40, "long", 2**40), (200, "ubyte", 200),
        (60000, "ushort", 60000), (3_000_000_000, "uint", 3_000_000_000),
        (2**63 + 5, "ulong", 2**63 + 5), (1.5, "float", 1.5),
        (1.5, "double", 1.5), ("hi", "string", "hi"),
    ]
    for value, tname, expect in cases:
        nt = spvirit.NtScalar(value, type=tname)
        assert nt.value_type == tname, (tname, nt.value_type)
        assert nt.value == expect, (tname, nt.value)


def test_ntscalar_type_aliases_and_default():
    assert spvirit.NtScalar(1, type="u16").value_type == "ushort"
    assert spvirit.NtScalar(1, type="float64").value_type == "double"
    # no type= keeps today's inference: int -> long, float -> double
    assert spvirit.NtScalar(1).value_type == "long"
    assert spvirit.NtScalar(1.0).value_type == "double"
    assert spvirit.NtScalar(True).value_type == "boolean"
    assert spvirit.NtScalar("s").value_type == "string"


def test_ntscalar_widening_rules():
    # int -> float/double is allowed
    assert spvirit.NtScalar(3, type="double").value == 3.0
    assert spvirit.NtScalar(3, type="float").value == 3.0
    # integral float -> int is allowed
    assert spvirit.NtScalar(2.0, type="int").value == 2


def test_ntscalar_strict_rejections():
    _expect(OverflowError, lambda: spvirit.NtScalar(300, type="ubyte"))
    _expect(OverflowError, lambda: spvirit.NtScalar(-1, type="uint"))
    _expect(OverflowError, lambda: spvirit.NtScalar(2**63, type="long"))
    _expect(OverflowError, lambda: spvirit.NtScalar(1e300, type="float"))
    _expect(TypeError, lambda: spvirit.NtScalar(2.5, type="int"))
    _expect(TypeError, lambda: spvirit.NtScalar("x", type="int"))
    _expect(TypeError, lambda: spvirit.NtScalar(True, type="int"))
    _expect(TypeError, lambda: spvirit.NtScalar(1, type="boolean"))
    _expect(TypeError, lambda: spvirit.NtScalar(1, type="string"))
    _expect(ValueError, lambda: spvirit.NtScalar(1, type="quint"))


def test_ntscalararray_type_selection():
    a = spvirit.NtScalarArray([1, 2, 3], type="ushort")
    assert a.value_type == "ushort"
    assert a.value == [1, 2, 3]
    f = spvirit.NtScalarArray([1, 2.5], type="float")
    assert f.value == [1.0, 2.5]
    # empty list gets the requested element type (not the double fallback)
    assert spvirit.NtScalarArray([], type="uint").value_type == "uint"
    assert spvirit.NtScalarArray([]).value_type == "double"
    # bytes only for byte/ubyte element types
    b = spvirit.NtScalarArray(b"\x01\x02", type="ubyte")
    assert b.value == b"\x01\x02"
    sb = spvirit.NtScalarArray(b"\xff", type="byte")
    assert sb.value_type == "byte" and sb.value == [-1]
    _expect(TypeError, lambda: spvirit.NtScalarArray(b"\x01", type="int"))
    _expect(OverflowError, lambda: spvirit.NtScalarArray([1, 999], type="ubyte"))
    # untyped default unchanged: ints -> long[]
    assert spvirit.NtScalarArray([1, 2]).value_type == "long"


def main():
    for fn in sorted(k for k in globals() if k.startswith("test_")):
        globals()[fn]()
        print(f"{fn}: ok")
    print("ALL OK")


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Build current code and verify the tests fail**

```powershell
cd C:\spvirit\spvirit-py
.\.venv\Scripts\maturin.exe develop
.\.venv\Scripts\python.exe tests\test_value_types.py
```
Expected: FAIL — `TypeError: ... got an unexpected keyword argument 'type'` (or missing `value_type` attribute).

- [ ] **Step 3: Implement the coercion layer in `convert.rs`**

Add at the top of `spvirit-py/src/convert.rs`:

```rust
use pyo3::exceptions::{PyOverflowError, PyTypeError, PyValueError};
use spvirit_codec::spvd_decode::TypeCode;
```

Append at the bottom of the file:

```rust
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
        TypeCode::Boolean => Err(kind_err(obj.get_type().name()?.to_str()?, code)),
        TypeCode::String => {
            if obj.is_instance_of::<PyString>() {
                Ok(ScalarValue::Str(obj.extract()?))
            } else {
                Err(kind_err(obj.get_type().name()?.to_str()?, code))
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
                Err(kind_err(obj.get_type().name()?.to_str()?, code))
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
```

In `spvirit-py/src/source.rs`: delete its private `fn parse_type_code` (lines 47–62) and add `use crate::convert::parse_type_code;` to the imports. Behavior note: `parse_field_type` checks `"string"`/`"str"` and `"[]"` suffixes *before* calling `parse_type_code`, so the added String arm changes nothing for PvInfo.

- [ ] **Step 4: Add `type=` and `.value_type` to `NtScalar`/`NtScalarArray` in `nt.rs`**

Update the import in `spvirit-py/src/nt.rs`:

```rust
use crate::convert::{
    py_to_scalar, py_to_scalar_array, py_to_scalar_array_maybe_typed, py_to_scalar_maybe_typed,
    scalar_array_to_py, scalar_array_type_code, scalar_to_py, scalar_value_type_code,
    wire_type_name,
};
```

(`py_to_scalar`/`py_to_scalar_array` stay imported only if still referenced; remove from the list otherwise.)

In `PyNtScalar::py_new`, append `r#type=None` **last** in the signature (all existing positional call sites keep working) and route through the maybe-typed helper:

```rust
#[new]
#[pyo3(signature = (value, units=String::new(), display_low=0.0, display_high=0.0, display_description=String::new(), display_precision=0, control_low=0.0, control_high=0.0, control_min_step=0.0, alarm_severity=0, alarm_status=0, alarm_message=String::new(), r#type=None))]
#[allow(clippy::too_many_arguments)]
fn py_new(
    value: &Bound<'_, PyAny>,
    units: String,
    display_low: f64,
    display_high: f64,
    display_description: String,
    display_precision: i32,
    control_low: f64,
    control_high: f64,
    control_min_step: f64,
    alarm_severity: i32,
    alarm_status: i32,
    alarm_message: String,
    r#type: Option<String>,
) -> PyResult<Self> {
    let sv = py_to_scalar_maybe_typed(value, r#type.as_deref())?;
    // ... rest unchanged (NtScalar::from_value(sv) etc.)
}
```

Add a getter to `PyNtScalar`:

```rust
/// Canonical wire value-type name, e.g. "double", "ushort", "string".
#[getter]
fn value_type(&self) -> &'static str {
    wire_type_name(scalar_value_type_code(&self.inner.value))
}
```

In `PyNtScalarArray::py_new`:

```rust
#[new]
#[pyo3(signature = (value, r#type=None))]
fn py_new(value: &Bound<'_, PyAny>, r#type: Option<String>) -> PyResult<Self> {
    let arr = py_to_scalar_array_maybe_typed(value, r#type.as_deref())?;
    Ok(Self {
        inner: NtScalarArray::from_value(arr),
    })
}
```

And its getter:

```rust
/// Canonical wire element-type name, e.g. "double", "ushort".
#[getter]
fn value_type(&self) -> &'static str {
    wire_type_name(scalar_array_type_code(&self.inner.value))
}
```

- [ ] **Step 5: Build and run the tests**

```powershell
cd C:\spvirit\spvirit-py
.\.venv\Scripts\maturin.exe develop
.\.venv\Scripts\python.exe tests\test_value_types.py
```
Expected: `ALL OK` (5 tests). Also run the existing suite to catch regressions:
`.\.venv\Scripts\python.exe tests\test_pv_handles.py` → `ALL OK`.

- [ ] **Step 6: Commit**

```powershell
git add spvirit-py/src/convert.rs spvirit-py/src/source.rs spvirit-py/src/nt.rs spvirit-py/tests/test_value_types.py
git commit -m "feat(py): strict typed conversion layer; type= and value_type on NtScalar/NtScalarArray

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: Python constructors for NtTable and NtNdArray

**Files:**
- Modify: `spvirit-py/src/nt.rs` (`PyNtTable` line ~412, `PyNtNdArray` line ~477)
- Modify: `spvirit-py/tests/test_value_types.py` (new tests)

**Interfaces:**
- Consumes: `py_to_scalar_array_maybe_typed`, `parse_scalar_type`, `py_to_scalar_array_typed`, `wire_type_name`, `scalar_array_type_code` from Task 2.
- Produces: `NtTable(columns, *, labels=None, types=None, descriptor=None)`, `NtNdArray(value, dims, *, type=None)`, `NtTable.column_types() -> dict`, `NtNdArray.value_type`.

- [ ] **Step 1: Write the failing tests**

Add to `spvirit-py/tests/test_value_types.py`:

```python
def test_nttable_constructor_with_types():
    t = spvirit.NtTable(
        {"name": ["a", "b"], "count": [1, 2]},
        types={"count": "uint"},
        descriptor="demo",
    )
    assert t.labels == ["name", "count"]
    assert t.columns() == {"name": ["a", "b"], "count": [1, 2]}
    assert t.column_types() == {"name": "string", "count": "uint"}
    assert t.descriptor == "demo"
    # untyped columns keep inference (ints -> long)
    t2 = spvirit.NtTable({"x": [1]})
    assert t2.column_types() == {"x": "long"}
    # mismatched column lengths rejected
    _expect(ValueError, lambda: spvirit.NtTable({"a": [1], "b": [1, 2]}))
    # custom labels
    t3 = spvirit.NtTable({"a": [1]}, labels=["Column A"])
    assert t3.labels == ["Column A"]


def test_ntndarray_constructor():
    nd = spvirit.NtNdArray([0] * 12, [4, 3], type="ushort")
    assert nd.value_type == "ushort"
    assert [d["size"] for d in nd.dimensions()] == [4, 3]
    assert [d["offset"] for d in nd.dimensions()] == [0, 0]
    assert nd.uncompressed_size == 24   # 12 elements x 2 bytes
    raw = spvirit.NtNdArray(bytes(6), [3, 2])
    assert raw.value_type == "ubyte"
    assert raw.value == bytes(6)
```

- [ ] **Step 2: Run to verify failure**

`.\.venv\Scripts\python.exe tests\test_value_types.py`
Expected: FAIL — `TypeError: cannot create 'builtins.NtTable' instances` (no constructor yet).

- [ ] **Step 3: Implement**

In `spvirit-py/src/nt.rs`, extend the spvirit_types import with `NdCodec, NdDimension, NtTableColumn` (`NtTimeStamp` is already imported). Add to `#[pymethods] impl PyNtTable` (and update its doc comment — it is no longer read-only):

```rust
/// Create an NTTable from a `{name: list|bytes}` dict of columns.
/// `types` optionally maps column names to value-type strings; `labels`
/// defaults to the column names.
#[new]
#[pyo3(signature = (columns, *, labels=None, types=None, descriptor=None))]
fn py_new(
    columns: &Bound<'_, PyDict>,
    labels: Option<Vec<String>>,
    types: Option<&Bound<'_, PyDict>>,
    descriptor: Option<String>,
) -> PyResult<Self> {
    let mut cols = Vec::with_capacity(columns.len());
    for (key, val) in columns.iter() {
        let name: String = key.extract()?;
        let ty: Option<String> = match types {
            Some(d) => match d.get_item(&name)? {
                Some(t) => Some(t.extract()?),
                None => None,
            },
            None => None,
        };
        let values = crate::convert::py_to_scalar_array_maybe_typed(&val, ty.as_deref())?;
        cols.push(NtTableColumn { name, values });
    }
    let labels = labels.unwrap_or_else(|| cols.iter().map(|c| c.name.clone()).collect());
    let nt = NtTable {
        labels,
        columns: cols,
        descriptor,
        alarm: None,
        time_stamp: None,
    };
    nt.validate()
        .map_err(pyo3::exceptions::PyValueError::new_err)?;
    Ok(Self { inner: nt })
}

/// Return `{column_name: wire_type_name}` for every column.
fn column_types(&self, py: Python<'_>) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    for col in &self.inner.columns {
        dict.set_item(
            &col.name,
            crate::convert::wire_type_name(crate::convert::scalar_array_type_code(&col.values)),
        )?;
    }
    Ok(dict.into_any().unbind())
}
```

Add to `#[pymethods] impl PyNtNdArray` (and update its doc comment):

```rust
/// Create an NTNDArray from flat data and per-dimension sizes (offsets 0).
#[new]
#[pyo3(signature = (value, dims, *, r#type=None))]
fn py_new(value: &Bound<'_, PyAny>, dims: Vec<i32>, r#type: Option<String>) -> PyResult<Self> {
    let arr = crate::convert::py_to_scalar_array_maybe_typed(value, r#type.as_deref())?;
    let uncompressed = (arr.len() * arr.element_size_bytes().max(1)) as i64;
    let dimension: Vec<NdDimension> = dims
        .into_iter()
        .map(|size| NdDimension {
            size,
            offset: 0,
            full_size: size,
            binning: 1,
            reverse: false,
        })
        .collect();
    Ok(Self {
        inner: NtNdArray {
            value: arr,
            codec: NdCodec {
                name: String::new(),
                parameters: Default::default(),
            },
            compressed_size: uncompressed,
            uncompressed_size: uncompressed,
            dimension,
            unique_id: 0,
            data_time_stamp: NtTimeStamp::default(),
            attribute: vec![],
            descriptor: None,
            alarm: None,
            time_stamp: None,
            display: None,
        },
    })
}

/// Canonical wire element-type name, e.g. "ubyte", "ushort".
#[getter]
fn value_type(&self) -> &'static str {
    crate::convert::wire_type_name(crate::convert::scalar_array_type_code(&self.inner.value))
}
```

- [ ] **Step 4: Build and run**

```powershell
.\.venv\Scripts\maturin.exe develop
.\.venv\Scripts\python.exe tests\test_value_types.py
```
Expected: `ALL OK`.

- [ ] **Step 5: Commit**

```powershell
git add spvirit-py/src/nt.rs spvirit-py/tests/test_value_types.py
git commit -m "feat(py): NtTable/NtNdArray Python constructors with per-column/element types

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: `spvirit.scalar()` factory and `PvKind::Typed`

**Files:**
- Modify: `spvirit-py/src/pv.rs` (enum at line 24, every `match &self.kind`, new factory at the bottom)
- Modify: `spvirit-py/src/lib.rs` (register `pv::scalar`, line ~68)
- Modify: `spvirit-py/tests/test_value_types.py`

**Interfaces:**
- Consumes: `Pv::<ScalarValue>::scalar_in/scalar_out` (Task 1); `py_to_scalar_typed`, `parse_scalar_type`, `wire_type_name`, `scalar_to_py` (Task 2).
- Produces: `spvirit.scalar(name, initial, *, type, writable=False, units=None, prec=None, desc=None, adel=None, mdel=None, drive_limits=None, alarm_limits=None) -> Pv`; `PvKind::Typed(Pv<ScalarValue>, TypeCode)` (used by Task 5/6); `PutVal::Scalar(ScalarValue)`.

- [ ] **Step 1: Write the failing tests**

Add to `spvirit-py/tests/test_value_types.py` (top of file also gains `import time`):

```python
def test_scalar_factory_all_types_serve_and_roundtrip():
    pvs = [
        spvirit.scalar("VT:UL", 2**63 + 9, type="ulong", writable=True),
        spvirit.scalar("VT:F32", 1.5, type="float", writable=True, units="V"),
        spvirit.scalar("VT:U8", 200, type="ubyte"),
        spvirit.scalar("VT:S", "hey", type="string"),
        spvirit.scalar("VT:B", True, type="boolean"),
    ]
    assert "(ulong)" in repr(pvs[0])
    server = spvirit.Server(pvs=pvs, port=16060, udp_port=16061,
                            listen_ip="127.0.0.1")
    server.start()
    assert pvs[0].get() == 2**63 + 9
    pvs[0].set(2**64 - 1)
    assert pvs[0].get() == 2**64 - 1
    _expect(OverflowError, lambda: pvs[0].set(-1))
    _expect(TypeError, lambda: pvs[0].set("nope"))
    assert pvs[1].get() == 1.5
    assert pvs[2].get() == 200
    _expect(OverflowError, lambda: pvs[2].set(300))
    assert pvs[3].get() == "hey"
    assert pvs[4].get() is True
    # wire introspection reports the requested value type
    from spvirit.lowlevel import Channel
    with Channel.connect("VT:UL", "127.0.0.1:16060", timeout=5.0) as ch:
        desc = ch.introspect()
        assert desc.field("value").type_code == "uint64"
    with Channel.connect("VT:F32", "127.0.0.1:16060", timeout=5.0) as ch:
        assert ch.introspect().field("value").type_code == "float32"


def test_scalar_factory_validation():
    _expect(ValueError, lambda: spvirit.scalar("VT:BAD", 1, type="nope"))
    _expect(OverflowError, lambda: spvirit.scalar("VT:OV", 300, type="ubyte"))
    _expect(TypeError, lambda: spvirit.scalar("VT:K", 1.5, type="int"))


def test_scalar_factory_on_put_and_wire_write():
    seen = []
    ct = spvirit.scalar("VTW:CT", 5, type="ushort", writable=True)

    @ct.on_put
    def _check(pv, value):
        seen.append(value)
        if value > 1000:
            return False

    server = spvirit.Server(pvs=[ct], port=16062, udp_port=16063,
                            listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.3)
    client = (spvirit.Client.builder()
              .server_addr("127.0.0.1:16062").udp_port(16063).build())
    client.put("VTW:CT", 42)
    assert ct.get() == 42
    assert seen == [42]
    try:
        client.put("VTW:CT", 2000)   # rejected by on_put
    except spvirit.SpviritError:
        pass
    assert ct.get() == 42


def test_scalar_factory_scan():
    hb = spvirit.scalar("VTS:HB", 0, type="uint")
    counter = iter(range(1, 100))

    @hb.scan(period=0.05)
    def _tick(pv):
        return next(counter)

    server = spvirit.Server(pvs=[hb], port=16064, udp_port=16065,
                            listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.5)
    assert hb.get() >= 1
```

- [ ] **Step 2: Run to verify failure**

`.\.venv\Scripts\python.exe tests\test_value_types.py`
Expected: FAIL — `AttributeError: module 'spvirit' has no attribute 'scalar'`.

- [ ] **Step 3: Implement in `spvirit-py/src/pv.rs`**

Imports: add

```rust
use spvirit_codec::spvd_decode::TypeCode;
use spvirit_types::ScalarValue;

use crate::convert::{
    parse_scalar_type, py_to_scalar_array, py_to_scalar_typed, scalar_array_to_py, scalar_to_py,
    wire_type_name,
};
```

(keep the existing imported names; this consolidates the `crate::convert` line.)

Enum — add a variant:

```rust
#[derive(Clone)]
pub(crate) enum PvKind {
    F64(Pv<f64>),
    Bool(Pv<bool>),
    I32(Pv<i32>),
    Str(Pv<String>),
    Array(PvArray),
    /// Dynamically typed scalar — covers all twelve NTScalar wire types.
    /// The TypeCode is the record's wire type; Python values are strictly
    /// coerced against it at the boundary.
    Typed(Pv<ScalarValue>, TypeCode),
}
```

Then add a `Typed` arm to every existing `match &self.kind` (the compiler will list them all). The arms:

`any()`:
```rust
PvKind::Typed(p, _) => AnyPv::from(p.clone()),
```

`name()`:
```rust
PvKind::Typed(p, _) => p.name(),
```

`__repr__` (the `ty` binding changes from `&'static str` to `String` — adjust the four existing arms with `.to_string()` or build with a match returning `String`):
```rust
let ty: String = match &self.kind {
    PvKind::F64(_) => "float".into(),
    PvKind::Bool(_) => "bool".into(),
    PvKind::I32(_) => "int".into(),
    PvKind::Str(_) => "str".into(),
    PvKind::Array(_) => "array".into(),
    PvKind::Typed(_, code) => wire_type_name(*code).into(),
};
```

`set`:
```rust
PvKind::Typed(p, code) => {
    let v = py_to_scalar_typed(value, *code)?;
    block_on_py(py, p.set(v)).map_err(pv_err)
}
```

`get`:
```rust
PvKind::Typed(p, _) => {
    let v = block_on_py(py, p.get()).map_err(pv_err)?;
    Ok(scalar_to_py(py, &v))
}
```
(note: the F64/Bool/I32/Str arms use `v.into_py_any(py)`; the Typed arm returns `Ok(...)` directly, same as the Array arm.)

`set_async`:
```rust
PvKind::Typed(p, code) => {
    let v = py_to_scalar_typed(value, *code)?;
    let handle = p.clone();
    future_into_py(py, async move {
        handle.set(v).await.map_err(pv_err)?;
        Python::with_gil(|py| py.None().into_py_any(py))
    })
}
```

`get_async`:
```rust
PvKind::Typed(p, _) => {
    let handle = p.clone();
    future_into_py(py, async move {
        let v = handle.get().await.map_err(pv_err)?;
        Ok(Python::with_gil(|py| scalar_to_py(py, &v)))
    })
}
```

`set_alarm`:
```rust
PvKind::Typed(p, _) => {
    block_on_py(py, p.set_alarm(severity, status, message)).map_err(pv_err)
}
```

`on_put` (goes before the final `PvKind::Array(_) => unreachable!` arm):
```rust
PvKind::Typed(p, code) => {
    let handle = p.clone();
    let c = *code;
    let cb = callback.clone_ref(py);
    let _ = p.clone().on_put(move |_pv, v: ScalarValue| {
        py_on_put(&cb, PvKind::Typed(handle.clone(), c), PutVal::Scalar(v))
    });
}
```

`PutVal` gains a variant and `py_on_put` an arm:
```rust
pub(crate) enum PutVal {
    F64(f64),
    Bool(bool),
    I32(i32),
    Str(String),
    Scalar(ScalarValue),
}
// in py_on_put's `let arg = match val {...}`:
PutVal::Scalar(v) => Ok(scalar_to_py(py, &v)),
```
(the other four arms return `PyResult` via `into_py_any` — wrap the new arm to match the existing expression type.)

`register_scan` gains an arm plus a dedicated bridge (the `scan_bridge_fn!` macro doesn't fit — the Typed bridge needs the TypeCode):
```rust
PvKind::Typed(p, code) => {
    let cache = Mutex::new(None);
    let c = *code;
    let _ = p
        .clone()
        .scan(dur, move |h| scan_bridge_typed(&cb, &cache, h, c));
}
```

```rust
/// Scan bridge for dynamically typed scalars: the Python return value is
/// strictly coerced to the record's wire type; failures fall back to the
/// cached last value or the type's zero default (same contract as the four
/// monomorphic bridges above).
fn scan_bridge_typed(
    cb: &PyObject,
    cache: &Mutex<Option<ScalarValue>>,
    h: &Pv<ScalarValue>,
    code: TypeCode,
) -> ScalarValue {
    Python::with_gil(|py| {
        let pv = PyPv {
            kind: PvKind::Typed(h.clone(), code),
        };
        let result = match cb.call1(py, (pv,)) {
            Ok(ret) if ret.is_none(py) => None,
            Ok(ret) => py_to_scalar_typed(ret.bind(py), code).ok(),
            Err(e) => {
                tracing::error!("scan callback error: {e}");
                None
            }
        };
        let mut guard = cache.lock().unwrap();
        match result {
            Some(v) => {
                *guard = Some(v.clone());
                v
            }
            None => guard.clone().unwrap_or_else(|| default_scalar(code)),
        }
    })
}

/// Zero/empty default for each wire type (scan fallback before first tick).
fn default_scalar(code: TypeCode) -> ScalarValue {
    match code {
        TypeCode::Boolean => ScalarValue::Bool(false),
        TypeCode::Int8 => ScalarValue::I8(0),
        TypeCode::Int16 => ScalarValue::I16(0),
        TypeCode::Int32 => ScalarValue::I32(0),
        TypeCode::Int64 => ScalarValue::I64(0),
        TypeCode::UInt8 => ScalarValue::U8(0),
        TypeCode::UInt16 => ScalarValue::U16(0),
        TypeCode::UInt32 => ScalarValue::U32(0),
        TypeCode::UInt64 => ScalarValue::U64(0),
        TypeCode::Float32 => ScalarValue::F32(0.0),
        TypeCode::String => ScalarValue::Str(String::new()),
        _ => ScalarValue::F64(0.0),
    }
}
```

The factory, after the `pv()` function:

```rust
/// Generic scalar record covering all twelve NTScalar wire value types.
///
/// `type` is required (no inference): "boolean", "byte", "short", "int",
/// "long", "ubyte", "ushort", "uint", "ulong", "float", "double", "string"
/// (aliases like "u16"/"f32" accepted). `writable=False` serves the
/// read-only (input) flavor, `True` the writable (output) flavor. The
/// initial value is strictly coerced: out-of-range raises OverflowError,
/// wrong kinds raise TypeError.
#[pyfunction]
#[pyo3(signature = (name, initial, *, r#type, writable=false, units=None, prec=None,
                    desc=None, adel=None, mdel=None, drive_limits=None, alarm_limits=None))]
#[allow(clippy::too_many_arguments)]
pub fn scalar(
    name: String,
    initial: &Bound<'_, PyAny>,
    r#type: String,
    writable: bool,
    units: Option<String>,
    prec: Option<i32>,
    desc: Option<String>,
    adel: Option<f64>,
    mdel: Option<f64>,
    drive_limits: Option<(f64, f64)>,
    alarm_limits: Option<(f64, f64, f64, f64)>,
) -> PyResult<PyPv> {
    let code = parse_scalar_type(&r#type)?;
    let sv = py_to_scalar_typed(initial, code)?;
    let handle = if writable {
        Pv::<ScalarValue>::scalar_out(name, sv)
    } else {
        Pv::<ScalarValue>::scalar_in(name, sv)
    };
    let handle = apply_opts(
        handle,
        units,
        prec,
        desc,
        adel,
        mdel,
        drive_limits,
        alarm_limits,
    );
    Ok(PyPv {
        kind: PvKind::Typed(handle, code),
    })
}
```

In `spvirit-py/src/lib.rs`, next to the other pv registrations:

```rust
m.add_function(wrap_pyfunction!(pv::scalar, m)?)?;
```

- [ ] **Step 4: Build and run**

```powershell
.\.venv\Scripts\maturin.exe develop
.\.venv\Scripts\python.exe tests\test_value_types.py
.\.venv\Scripts\python.exe tests\test_pv_handles.py
```
Expected: `ALL OK` for both.

- [ ] **Step 5: Commit**

```powershell
git add spvirit-py/src/pv.rs spvirit-py/src/lib.rs spvirit-py/tests/test_value_types.py
git commit -m "feat(py): spvirit.scalar() factory with PvKind::Typed covering all NTScalar wire types

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: `type=` on array factories, typed array sets, and `spvirit.pv(type=)`

**Files:**
- Modify: `spvirit-py/src/pv.rs` (`waveform`/`aai`/`aao` line ~598, `pv()` line ~676, Array arms of `set`/`set_async`)
- Modify: `spvirit-py/tests/test_value_types.py`

**Interfaces:**
- Consumes: `py_to_scalar_array_maybe_typed`, `py_to_scalar_array_typed`, `scalar_array_type_code` (Task 2), `PvKind::Typed` + `scalar()` mapping logic (Task 4).
- Produces: `waveform(name, data, *, type=None)`, `aai(...)`, `aao(...)` same; `spvirit.pv(name, initial, *, type=None, ...)`; array-handle `set()` coerces to the record's current element type instead of re-inferring.

- [ ] **Step 1: Write the failing tests**

```python
def test_typed_waveform_keeps_element_type_across_set():
    wf = spvirit.waveform("VTA:WF", [0] * 4, type="ushort")
    server = spvirit.Server(pvs=[wf], port=16066, udp_port=16067,
                            listen_ip="127.0.0.1")
    server.start()
    wf.set([1, 2, 3])                       # plain int list must stay ushort
    assert wf.get() == [1, 2, 3]
    _expect(OverflowError, lambda: wf.set([70000]))
    _expect(TypeError, lambda: wf.set(["x"]))
    from spvirit.lowlevel import Channel
    with Channel.connect("VTA:WF", "127.0.0.1:16066", timeout=5.0) as ch:
        assert ch.introspect().field("value").type_code == "uint16"


def test_typed_aai_aao_and_empty_list():
    r = spvirit.aai("VTA:R", [], type="float")
    w = spvirit.aao("VTA:W", [1.0, 2.0], type="float")
    server = spvirit.Server(pvs=[r, w], port=16068, udp_port=16069,
                            listen_ip="127.0.0.1")
    server.start()
    assert r.get() == []
    assert w.get() == [1.0, 2.0]
    _expect(ValueError, lambda: spvirit.waveform("VTA:BAD", [], type="nope"))


def test_pv_factory_type_override():
    p = spvirit.pv("VTP:U32", 7, type="uint")
    assert "(uint)" in repr(p)
    q = spvirit.pv("VTP:WF", [0] * 3, type="short")
    d = spvirit.pv("VTP:D", 7, type="double")     # maps onto the native float kind
    assert "(float)" in repr(d)
    server = spvirit.Server(pvs=[p, q, d], port=16070, udp_port=16071,
                            listen_ip="127.0.0.1")
    server.start()
    assert p.get() == 7
    assert d.get() == 7.0
    _expect(OverflowError, lambda: q.set([2**20]))
```

- [ ] **Step 2: Run to verify failure**

Expected: FAIL — `waveform() got an unexpected keyword argument 'type'`.

- [ ] **Step 3: Implement**

Replace the three array factories:

```rust
/// Array record (writable over the wire). `data` is a list of bool/int/
/// float/str, or `bytes` for a `U8` array. `type=` selects the element
/// type explicitly (e.g. "ushort", "float").
#[pyfunction]
#[pyo3(signature = (name, data, *, r#type=None))]
pub fn waveform(name: String, data: &Bound<'_, PyAny>, r#type: Option<String>) -> PyResult<PyPv> {
    let arr = crate::convert::py_to_scalar_array_maybe_typed(data, r#type.as_deref())?;
    Ok(PyPv {
        kind: PvKind::Array(PvArray::waveform(name, arr)),
    })
}
```

and identically for `aai` / `aao` (same signature change, `PvArray::aai` / `PvArray::aao`).

Array-handle `set` — coerce to the record's live element type so a plain
Python list can't silently retype a `ushort[]` record to `long[]`. In
`PyPv::set`, replace the Array arm:

```rust
PvKind::Array(p) => {
    // Bound handles coerce strictly to the record's current element type
    // (the record is the authority); unbound handles fall back to
    // inference — set() on them raises Unbound in p.set() anyway.
    let v = match block_on_py(py, async { p.get().await }) {
        Ok(cur) => crate::convert::py_to_scalar_array_typed(
            value,
            crate::convert::scalar_array_type_code(&cur),
        )?,
        Err(_) => py_to_scalar_array(value)?,
    };
    block_on_py(py, p.set(v)).map_err(pv_err)
}
```

and the same replacement inside `set_async`'s Array arm (the conversion happens before `future_into_py`, exactly like today).

`pv()` — append `r#type=None` to the signature and handle the override at the top of the function body:

```rust
#[pyo3(signature = (name, initial, *, units=None, prec=None, desc=None,
                    adel=None, mdel=None, drive_limits=None, alarm_limits=None, r#type=None))]
```

body (before the existing inference `if` chain):

```rust
if let Some(t) = &r#type {
    let code = parse_scalar_type(t)?;
    // Arrays: typed waveform (same metadata-option rejection as inferred arrays).
    if initial.is_instance_of::<PyList>() || initial.is_instance_of::<PyBytes>() {
        if units.is_some()
            || prec.is_some()
            || desc.is_some()
            || adel.is_some()
            || mdel.is_some()
            || drive_limits.is_some()
            || alarm_limits.is_some()
        {
            return Err(PyTypeError::new_err(
                "metadata options (units/prec/desc/adel/mdel/drive_limits/alarm_limits) \
                 are not supported for array PVs",
            ));
        }
        let arr = crate::convert::py_to_scalar_array_typed(initial, code)?;
        return Ok(PyPv {
            kind: PvKind::Array(PvArray::waveform(name, arr)),
        });
    }
    let sv = py_to_scalar_typed(initial, code)?;
    // The four native kinds keep their monomorphic handles (calc() inputs,
    // existing repr contracts); everything else rides PvKind::Typed.
    let kind = match sv {
        ScalarValue::F64(v) => PvKind::F64(apply_opts(
            Pv::ao(name, v),
            units, prec, desc, adel, mdel, drive_limits, alarm_limits,
        )),
        ScalarValue::Bool(v) => PvKind::Bool(apply_opts(
            Pv::bo(name, v),
            units, prec, desc, adel, mdel, drive_limits, alarm_limits,
        )),
        ScalarValue::I32(v) => PvKind::I32(apply_opts(
            Pv::longout(name, v),
            units, prec, desc, adel, mdel, drive_limits, alarm_limits,
        )),
        ScalarValue::Str(v) => PvKind::Str(apply_opts(
            Pv::string_out(name, v),
            units, prec, desc, adel, mdel, drive_limits, alarm_limits,
        )),
        other => PvKind::Typed(
            apply_opts(
                Pv::<ScalarValue>::scalar_out(name, other),
                units, prec, desc, adel, mdel, drive_limits, alarm_limits,
            ),
            code,
        ),
    };
    return Ok(PyPv { kind });
}
```

(add `use pyo3::types::PyList` etc. — they are already imported locally inside `pv()`; move that `use` line to the top of the function so both branches see it.)

- [ ] **Step 4: Build and run**

```powershell
.\.venv\Scripts\maturin.exe develop
.\.venv\Scripts\python.exe tests\test_value_types.py
.\.venv\Scripts\python.exe tests\test_pv_handles.py
```
Expected: `ALL OK` for both.

- [ ] **Step 5: Commit**

```powershell
git add spvirit-py/src/pv.rs spvirit-py/tests/test_value_types.py
git commit -m "feat(py): type= on waveform/aai/aao and spvirit.pv; array sets respect record element type

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: `server.pv()` attaches to 64-bit/unsigned records

**Files:**
- Modify: `spvirit-py/src/server.rs` (`PyServer::pv`, the scalar match at lines 536–558)
- Modify: `spvirit-py/tests/test_value_types.py`

**Interfaces:**
- Consumes: `PvKind::Typed` (Task 4), `scalar_value_type_code` (Task 2), `server.pv::<ScalarValue>(&name)` (compiles via Task 1).
- Produces: `server.pv(name)` returns a Typed handle for `long`/`ubyte`/`ushort`/`uint`/`ulong` records instead of raising `KeyError`. Existing mappings (`double`/`float`→float, `byte`/`short`/`int`→int, etc.) unchanged.

- [ ] **Step 1: Write the failing test**

```python
def test_server_pv_attaches_to_unsigned_and_64bit_records():
    ul = spvirit.scalar("VTH:UL", 10, type="ulong", writable=True)
    server = spvirit.Server(pvs=[ul], port=16072, udp_port=16073,
                            listen_ip="127.0.0.1")
    server.start()
    h = server.pv("VTH:UL")           # used to raise KeyError
    assert "(ulong)" in repr(h)
    h.set(2**63 + 1)
    assert ul.get() == 2**63 + 1
    _expect(OverflowError, lambda: h.set(-1))
```

- [ ] **Step 2: Run to verify failure**

Expected: FAIL — `KeyError: "PV 'VTH:UL' has unsupported value type U64(10) for typed handles"`.

- [ ] **Step 3: Implement**

In `PyServer::pv`, replace the final scalar arm

```rust
other => {
    return Err(pyo3::exceptions::PyKeyError::new_err(format!(
        "PV '{name}' has unsupported value type {other:?} for typed handles"
    )));
}
```

with

```rust
// long / ubyte / ushort / uint / ulong — dynamically typed handle.
other => {
    let code = crate::convert::scalar_value_type_code(&other);
    let h = block_on_py(py, server.pv::<ScalarValue>(&name)).map_err(pv_err)?;
    PvKind::Typed(h, code)
}
```

- [ ] **Step 4: Build and run**

```powershell
.\.venv\Scripts\maturin.exe develop
.\.venv\Scripts\python.exe tests\test_value_types.py
.\.venv\Scripts\python.exe tests\test_pv_handles.py
```
Expected: `ALL OK` for both.

- [ ] **Step 5: Commit**

```powershell
git add spvirit-py/src/server.rs spvirit-py/tests/test_value_types.py
git commit -m "feat(py): server.pv() attaches typed handles to 64-bit and unsigned scalar records

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: ServerBuilder `type=`/`types=` kwargs

**Files:**
- Modify: `spvirit-py/src/server.rs` (builder methods `waveform`/`aai`/`aao` line ~111, `sub_array` line ~148, `nt_table` line ~163, `nt_ndarray` line ~185, `generic` line ~225, and the `py_to_pv_value` helper at the bottom)
- Modify: `spvirit-py/tests/test_value_types.py`

**Interfaces:**
- Consumes: `py_to_scalar_array_maybe_typed`, `py_to_scalar_array_typed`, `py_to_scalar_typed`, `parse_scalar_type` (Task 2).
- Produces: builder methods `waveform(name, data, *, type=None)`, `aai`, `aao` (same), `sub_array(name, data, indx=0, nelm=None, *, type=None)`, `nt_ndarray(name, data, dims, *, type=None)`, `nt_table(name, columns, *, types=None)`, `generic(name, struct_id, fields, *, types=None)`; helper `py_to_pv_value_typed(obj, ty: &str) -> PyResult<PvValue>` supporting `"double"` and `"double[]"` forms.

- [ ] **Step 1: Write the failing tests**

```python
def test_builder_typed_records():
    server = (
        spvirit.ServerBuilder()
        .waveform("VTB:WF", [0] * 3, type="ushort")
        .aai("VTB:R", [1, 2], type="uint")
        .aao("VTB:W", [0.5], type="float")
        .sub_array("VTB:SUB", [0] * 8, indx=2, nelm=4, type="short")
        .nt_table("VTB:TBL", {"n": ["a"], "c": [3]}, types={"c": "ubyte"})
        .nt_ndarray("VTB:IMG", [0] * 6, [(3, 0), (2, 0)], type="ushort")
        .generic("VTB:CFG", "my:cfg:1.0", {"gain": 2, "taps": [1, 2]},
                 types={"gain": "float", "taps": "short[]"})
        .port(16074).udp_port(16075).listen_ip("127.0.0.1")
        .build()
    )
    store = server.start_background()
    assert store.get_nt("VTB:WF").value_type == "ushort"
    assert store.get_nt("VTB:R").value_type == "uint"
    assert store.get_nt("VTB:W").value_type == "float"
    assert store.get_nt("VTB:TBL").column_types() == {"n": "string", "c": "ubyte"}
    assert store.get_nt("VTB:IMG").value_type == "ushort"
    cfg = store.get_nt("VTB:CFG")
    assert cfg["gain"] == 2.0
    assert cfg["taps"] == [1, 2]
```

- [ ] **Step 2: Run to verify failure**

Expected: FAIL — `waveform() got an unexpected keyword argument 'type'`.

- [ ] **Step 3: Implement**

Import at the top of `server.rs`:

```rust
use crate::convert::{
    decoded_to_py, py_to_scalar, py_to_scalar_array, py_to_scalar_array_maybe_typed,
    py_to_scalar_array_typed, py_to_scalar_typed, parse_scalar_type, scalar_to_py,
};
```

Builder `waveform` (aai/aao identical apart from the delegate):

```rust
/// Add a `waveform` NTScalarArray record — writable over the wire.
/// `type=` selects the element type explicitly.
#[pyo3(signature = (name, data, *, r#type=None))]
fn waveform<'py>(
    mut slf: PyRefMut<'py, Self>,
    name: String,
    data: &Bound<'py, PyAny>,
    r#type: Option<String>,
) -> PyResult<PyRefMut<'py, Self>> {
    let arr = py_to_scalar_array_maybe_typed(data, r#type.as_deref())?;
    let b = take_builder(&mut slf)?;
    slf.builder = Some(b.waveform(name, arr));
    Ok(slf)
}
```

`sub_array`:

```rust
#[pyo3(signature = (name, data, indx=0, nelm=None, *, r#type=None))]
fn sub_array<'py>(
    mut slf: PyRefMut<'py, Self>,
    name: String,
    data: &Bound<'py, PyAny>,
    indx: usize,
    nelm: Option<usize>,
    r#type: Option<String>,
) -> PyResult<PyRefMut<'py, Self>> {
    let arr = py_to_scalar_array_maybe_typed(data, r#type.as_deref())?;
    let n = nelm.unwrap_or(arr.len());
    let b = take_builder(&mut slf)?;
    slf.builder = Some(b.sub_array(name, arr, indx, n));
    Ok(slf)
}
```

`nt_table` — add `types=` (per-column dict, same lookup pattern as Task 3's `NtTable::py_new`):

```rust
#[pyo3(signature = (name, columns, *, types=None))]
fn nt_table<'py>(
    mut slf: PyRefMut<'py, Self>,
    name: String,
    columns: &Bound<'py, PyAny>,
    types: Option<&Bound<'py, pyo3::types::PyDict>>,
) -> PyResult<PyRefMut<'py, Self>> {
    let dict = columns.downcast::<pyo3::types::PyDict>().map_err(|_| {
        pyo3::exceptions::PyTypeError::new_err("columns must be a dict of {name: list}")
    })?;
    let mut cols: Vec<(String, ScalarArrayValue)> = Vec::new();
    for (key, val) in dict.iter() {
        let col_name: String = key.extract()?;
        let ty: Option<String> = match types {
            Some(d) => match d.get_item(&col_name)? {
                Some(t) => Some(t.extract()?),
                None => None,
            },
            None => None,
        };
        let col_data = py_to_scalar_array_maybe_typed(&val, ty.as_deref())?;
        cols.push((col_name, col_data));
    }
    let b = take_builder(&mut slf)?;
    slf.builder = Some(b.nt_table(name, cols));
    Ok(slf)
}
```

`nt_ndarray`:

```rust
#[pyo3(signature = (name, data, dims, *, r#type=None))]
fn nt_ndarray<'py>(
    mut slf: PyRefMut<'py, Self>,
    name: String,
    data: &Bound<'py, PyAny>,
    dims: Vec<(i32, i32)>,
    r#type: Option<String>,
) -> PyResult<PyRefMut<'py, Self>> {
    let arr = py_to_scalar_array_maybe_typed(data, r#type.as_deref())?;
    let b = take_builder(&mut slf)?;
    slf.builder = Some(b.nt_ndarray(name, arr, dims));
    Ok(slf)
}
```

`generic` — add `types=` routed through a typed variant of the helper:

```rust
#[pyo3(signature = (name, struct_id, fields, *, types=None))]
fn generic<'py>(
    mut slf: PyRefMut<'py, Self>,
    name: String,
    struct_id: String,
    fields: &Bound<'py, pyo3::types::PyDict>,
    types: Option<&Bound<'py, pyo3::types::PyDict>>,
) -> PyResult<PyRefMut<'py, Self>> {
    let mut field_vec: Vec<(String, spvirit_types::PvValue)> = Vec::new();
    for (key, val) in fields.iter() {
        let field_name: String = key.extract()?;
        let ty: Option<String> = match types {
            Some(d) => match d.get_item(&field_name)? {
                Some(t) => Some(t.extract()?),
                None => None,
            },
            None => None,
        };
        let pv_val = match ty {
            Some(t) => py_to_pv_value_typed(&val, &t)?,
            None => py_to_pv_value(&val)?,
        };
        field_vec.push((field_name, pv_val));
    }
    let b = take_builder(&mut slf)?;
    slf.builder = Some(b.generic(name, struct_id, field_vec));
    Ok(slf)
}
```

Helper next to `py_to_pv_value`:

```rust
/// Typed variant of `py_to_pv_value`: `"short"` coerces a scalar,
/// `"short[]"` coerces an array.
fn py_to_pv_value_typed(obj: &Bound<'_, PyAny>, ty: &str) -> PyResult<spvirit_types::PvValue> {
    let t = ty.trim();
    if let Some(base) = t.strip_suffix("[]") {
        let code = parse_scalar_type(base)?;
        Ok(spvirit_types::PvValue::ScalarArray(py_to_scalar_array_typed(obj, code)?))
    } else {
        let code = parse_scalar_type(t)?;
        Ok(spvirit_types::PvValue::Scalar(py_to_scalar_typed(obj, code)?))
    }
}
```

- [ ] **Step 4: Build and run**

```powershell
.\.venv\Scripts\maturin.exe develop
.\.venv\Scripts\python.exe tests\test_value_types.py
.\.venv\Scripts\python.exe tests\test_pv_handles.py
```
Expected: `ALL OK` for both.

- [ ] **Step 5: Commit**

```powershell
git add spvirit-py/src/server.rs spvirit-py/tests/test_value_types.py
git commit -m "feat(py): type=/types= kwargs on ServerBuilder record methods

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: Store put paths coerce to the record's wire type

**Files:**
- Modify: `spvirit-py/src/server.rs` (`PyStore::set_value`, `set_array_value`, `put_nt` at lines ~677–701)
- Modify: `spvirit-py/tests/test_value_types.py`

**Interfaces:**
- Consumes: `py_to_scalar_typed`, `py_to_scalar_array_typed`, `scalar_value_type_code`, `scalar_array_type_code`, `coerce_scalar_value`, `coerce_scalar_array_value` (Task 2).
- Produces: `Store.set_value`/`set_array_value`/`put_nt` never retype a record — incoming values are strictly coerced to the record's current wire type (record is the authority); a missing PV still returns `False` without raising.

- [ ] **Step 1: Write the failing tests**

```python
def test_store_set_value_respects_record_type():
    u16 = spvirit.scalar("VST:U16", 5, type="ushort", writable=True)
    server = spvirit.Server(pvs=[u16], port=16076, udp_port=16077,
                            listen_ip="127.0.0.1")
    store = server.start_background()
    assert store.set_value("VST:U16", 42) is True
    from spvirit.lowlevel import Channel
    with Channel.connect("VST:U16", "127.0.0.1:16076", timeout=5.0) as ch:
        assert ch.introspect().field("value").type_code == "uint16"
    _expect(OverflowError, lambda: store.set_value("VST:U16", 70000))
    _expect(TypeError, lambda: store.set_value("VST:U16", "x"))
    assert store.set_value("VST:NOPE", 1) is False


def test_store_set_array_value_respects_element_type():
    wf = spvirit.waveform("VST:WF", [0] * 3, type="float")
    server = spvirit.Server(pvs=[wf], port=16078, udp_port=16079,
                            listen_ip="127.0.0.1")
    store = server.start_background()
    assert store.set_array_value("VST:WF", [1, 2]) is True   # ints -> float[]
    assert wf.get() == [1.0, 2.0]
    _expect(OverflowError, lambda: store.set_array_value("VST:WF", [1e300]))
    assert store.set_array_value("VST:NOPE", [1]) is False


def test_store_put_nt_coerces_payload_value():
    u8 = spvirit.scalar("VST:U8", 1, type="ubyte", writable=True)
    server = spvirit.Server(pvs=[u8], port=16080, udp_port=16081,
                            listen_ip="127.0.0.1")
    store = server.start_background()
    # payload built without type= carries a long value; put coerces to ubyte
    assert store.put_nt("VST:U8", spvirit.NtScalar(7, units="ct")) is True
    nt = store.get_nt("VST:U8")
    assert nt.value == 7 and nt.value_type == "ubyte" and nt.units == "ct"
    _expect(OverflowError,
            lambda: store.put_nt("VST:U8", spvirit.NtScalar(300)))
```

- [ ] **Step 2: Run to verify failure**

Expected: FAIL — the `type_code == "uint16"` assertion (today `set_value` retypes the record to `int64`), and the two `_expect(OverflowError, ...)` checks.

- [ ] **Step 3: Implement**

Extend the `crate::convert` import in `server.rs` with `coerce_scalar_value, coerce_scalar_array_value, scalar_array_type_code, scalar_value_type_code` and add `use spvirit_types::NtPayload;`.

Replace the three `PyStore` methods:

```rust
/// Set a scalar value on a PV, strictly coerced to the record's current
/// wire type (the record is the authority — writes never retype it).
/// Returns True if the PV exists.
fn set_value(&self, py: Python<'_>, name: String, value: &Bound<'_, PyAny>) -> PyResult<bool> {
    let store = self.inner.clone();
    let sv = match block_on_py(py, store.get_value(&name)) {
        Some(current) => py_to_scalar_typed(value, scalar_value_type_code(&current))?,
        None => return Ok(false),
    };
    Ok(block_on_py(py, store.set_value(&name, sv)))
}

/// Set an array value on a PV, strictly coerced to the record's current
/// element type. Returns True if the PV exists.
fn set_array_value(
    &self,
    py: Python<'_>,
    name: String,
    value: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let store = self.inner.clone();
    let code = match block_on_py(py, store.get_nt(&name)) {
        Some(NtPayload::ScalarArray(nt)) => Some(scalar_array_type_code(&nt.value)),
        Some(NtPayload::NdArray(nt)) => Some(scalar_array_type_code(&nt.value)),
        Some(_) => None, // non-array record: keep inference, core decides
        None => return Ok(false),
    };
    let arr = match code {
        Some(c) => py_to_scalar_array_typed(value, c)?,
        None => py_to_scalar_array(value)?,
    };
    Ok(block_on_py(py, store.set_array_value(&name, arr)))
}

/// Write a full NT payload (NtScalar, NtScalarArray, etc.) to a PV. The
/// payload's value is strictly coerced to the record's current wire type.
/// Returns True if the PV exists.
fn put_nt(&self, py: Python<'_>, name: String, nt: &Bound<'_, PyAny>) -> PyResult<bool> {
    let payload = py_to_nt_payload(nt)?;
    let store = self.inner.clone();
    let payload = match (block_on_py(py, store.get_nt(&name)), payload) {
        (None, _) => return Ok(false),
        (Some(NtPayload::Scalar(current)), NtPayload::Scalar(mut new)) => {
            new.value = coerce_scalar_value(new.value, scalar_value_type_code(&current.value))?;
            NtPayload::Scalar(new)
        }
        (Some(NtPayload::ScalarArray(current)), NtPayload::ScalarArray(mut new)) => {
            new.value =
                coerce_scalar_array_value(new.value, scalar_array_type_code(&current.value))?;
            NtPayload::ScalarArray(new)
        }
        (_, p) => p, // table/ndarray/enum/generic or kind mismatch: core decides
    };
    Ok(block_on_py(py, store.put_nt(&name, payload)))
}
```

- [ ] **Step 4: Build and run**

```powershell
.\.venv\Scripts\maturin.exe develop
.\.venv\Scripts\python.exe tests\test_value_types.py
.\.venv\Scripts\python.exe tests\test_pv_handles.py
```
Expected: `ALL OK` for both. (If a test in `test_pv_handles.py` relied on `set_value` retyping a record, inspect it — the spec says the new strict behavior wins; adjust that test and say so in the commit message.)

- [ ] **Step 5: Commit**

```powershell
git add spvirit-py/src/server.rs spvirit-py/tests/test_value_types.py
git commit -m "feat(py): store put paths coerce strictly to the record's wire type

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: Documentation

**Files:**
- Modify: `spvirit-py/README.md`

**Interfaces:** none (docs only). Content changes:

- [ ] **Step 1: Update the README**

1. **Factory table** (line ~139): add a row
   `| spvirit.scalar(name, initial, *, type, writable=False, **opts) | per type | NTScalar <type> | writable= flag | — |`
   and mention it right below the table: "For the remaining NTScalar wire types (`long`, unsigned, `float`) use `spvirit.scalar(...)`, which covers all twelve."
2. **Rewrite the "NT scalar type coverage" subsection** (lines ~153–183): the twelve types are now all creatable — `scalar()` for scalars, `type=` on `waveform`/`aai`/`aao` for arrays; `server.pv()` no longer raises `KeyError` for `long`/unsigned records (they attach as dynamically typed handles whose `repr` shows the wire type); array element inference is unchanged when `type=` is omitted. Keep the widening table for handles minted by `server.pv()` on `double|float`, `byte|short|int`, etc. Document the strict coercion rules (OverflowError/TypeError/ValueError) once here.
3. **Array records** (line ~195): show `spvirit.waveform("SIM:WF", [0]*1024, type="ushort")` and note the empty-list default is `double[]` only when `type=` is omitted.
4. **Type-inferred creation** (line ~207): note `spvirit.pv(..., type=...)` overrides inference.
5. **Classic builder** (line ~449): update signatures — `waveform/aai/aao/sub_array/nt_ndarray` take `type=`, `nt_table`/`generic` take `types=`; drop the sentence "The builder's record methods take no metadata kwargs" only if inaccurate (metadata kwargs still absent — reword to "no *metadata* kwargs (`units=`, `prec=`, …); value types are selectable via `type=`/`types=`").
6. **Runtime store access** (line ~505): document that `set_value`/`set_array_value`/`put_nt` coerce strictly to the record's existing wire type and never retype a record.
7. **Normative Type classes** (line ~597): add `type=` to the `NtScalar` and `NtScalarArray` signatures, the `.value_type` property, and the new `NtTable(columns, *, labels=None, types=None, descriptor=None)` / `NtNdArray(value, dims, *, type=None)` constructors (they are no longer "returned by reads only"); mention `NtTable.column_types()`.

- [ ] **Step 2: Sanity-check examples**

Run every new code snippet added to the README mentally against the implemented API (names, kwarg spelling `type=`, `types=`, `writable=`). No build needed.

- [ ] **Step 3: Commit**

```powershell
git add spvirit-py/README.md
git commit -m "docs(py): document value-type selection across the Python surface

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 10: Full verification pass

- [ ] **Step 1: Run the complete test matrix**

```powershell
cd C:\spvirit
cargo test -p spvirit-server
cd spvirit-py
.\.venv\Scripts\maturin.exe develop
.\.venv\Scripts\python.exe tests\test_value_types.py
.\.venv\Scripts\python.exe tests\test_pv_handles.py
```
Expected: everything passes. `cargo test -p spvirit-server` may show pre-existing failures from the user's in-progress timestamp work in the dirty tree — compare against `git stash; cargo test -p spvirit-server; git stash pop` ONLY if failures appear, and never commit stash-related changes.

- [ ] **Step 2: Wire-level spot check against a real client tool (optional but recommended)**

Start `spvirit-py/examples/demo_server.py`-style script serving a `scalar(type="ulong")` PV and verify with `pvget`/`pvinfo` if EPICS tools are available; otherwise the `Channel.introspect()` assertions from Tasks 4/5/8 stand as the wire check.

- [ ] **Step 3: No commit** (nothing should be left uncommitted by this task; if fixes were needed, fold them into a `fix:` commit listing what broke)
