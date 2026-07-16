# Python surface: explicit NT value-type selection

**Date:** 2026-07-16
**Status:** Approved design

## Problem

The Python surface collapses value types: `py_to_scalar` maps every Python
`int` to `I64` and every `float` to `F64`; `py_to_scalar_array` does the same
for lists. Users cannot create an NTScalar of wire type `float`, `short`,
`ubyte`, etc. from Python, even though:

- the Rust store (`ScalarValue` / `ScalarArrayValue`) supports all twelve
  NTScalar value types end-to-end, and
- dynamic sources already declare precise types via `PvInfo` type strings
  (`"double"`, `"int"`, `"ushort"`, ...).

The handle machinery has the same gap deeper down: `PvScalar` is implemented
only for `f64`/`bool`/`i32`/`String`, `PvKind` has five variants, and
`server.pv()` raises `KeyError` for `long`/unsigned/`float` records.

## Scope (all four surfaces)

1. NT payload classes (`NtScalar`, `NtScalarArray`, plus new constructors for
   `NtTable`, `NtNdArray`).
2. Handle factories: a new generic `spvirit.scalar(...)`, element `type=` on
   `waveform`/`aai`/`aao`, `type=` override on `spvirit.pv(...)`.
3. Classic `ServerBuilder` methods.
4. Store put paths (`set_value`, `set_array_value`, `put_nt`) — coerce to the
   record's existing wire type.

## Type vocabulary and shared parser

Promote the type-string parser from `spvirit-py/src/source.rs` into
`spvirit-py/src/convert.rs` (single definition, `source.rs` re-uses it). It
maps the established `PvInfo` convention to `TypeCode`:

| Canonical | Aliases |
|---|---|
| `boolean` | `bool` |
| `byte` | `int8`, `i8` |
| `short` | `int16`, `i16` |
| `int` | `int32`, `i32` |
| `long` | `int64`, `i64` |
| `ubyte` | `uint8`, `u8` |
| `ushort` | `uint16`, `u16` |
| `uint` | `uint32`, `u32` |
| `ulong` | `uint64`, `u64` |
| `float` | `float32`, `f32` |
| `double` | `float64`, `f64` |
| `string` | `str` |

Every new parameter is keyword-only: `type=` for a single value, `types=`
(dict) where per-column / per-field. Unknown strings raise `ValueError`.
`type=None` (the default) keeps today's inference everywhere — the change is
fully backward compatible.

## Strict coercion layer (`convert.rs`)

New functions:

- `py_to_scalar_typed(obj, TypeCode) -> PyResult<ScalarValue>`
- `py_to_scalar_array_typed(obj, TypeCode) -> PyResult<ScalarArrayValue>`

Rules (strict — chosen over saturate/wrap):

- `int` → any integer type when in range, else `OverflowError`;
  `int` → `float`/`double` allowed.
- `float` → `double` always; → `float` with f32 precision loss allowed, but a
  finite f64 whose magnitude overflows f32 raises `OverflowError` (NaN/±inf
  pass through). `float` → integer types only when integral (`2.0` ok,
  `2.5` → `TypeError`).
- `bool` → `boolean` only (checked before int, as Python `bool` is an `int`
  subclass). `str` → `string` only.
- `bytes` → `byte[]`/`ubyte[]` arrays only; other element types reject
  `bytes` with `TypeError`.
- Anything else → `TypeError`.
- Arrays apply the scalar rule per element; the first failing element aborts
  with its error. `type=` resolves the empty-list ambiguity (today an empty
  list is forced to `double[]`).

## NT payload classes (`nt.rs`)

- `NtScalar(value, *, type=None, units=..., ...)` — existing signature plus
  `type=`.
- `NtScalarArray(value, *, type=None)`.
- New constructor `NtTable(columns, *, labels=None, types=None,
  descriptor=None)` — `columns` is `dict[str, list|bytes]`; `labels` defaults
  to the column names; `types` is an optional per-column dict of type
  strings.
- New constructor `NtNdArray(value, dims, *, type=None)` — `value` is
  `list|bytes`, `dims` a list of sizes (offsets default 0); `type` is the
  element type.

## Generic scalar handle and `PvKind::Typed`

New factory:

```python
spvirit.scalar(name, initial, *, type, writable=False,
               units=None, prec=None, desc=None, adel=None, mdel=None,
               drive_limits=None, alarm_limits=None)
```

- Covers all twelve NTScalar types; `type` is required here (no inference).
- `writable=False` serves the read-only (input) flavor, `True` the writable
  (output) flavor, matching the existing in/out factory pairs.
- Backed by a new `PvKind::Typed(Pv<ScalarValue>, TypeCode)` variant — one
  new match arm per method (`set`, `get`, async variants, `set_alarm`,
  `on_put`, `scan`) instead of eight monomorphized kinds.
- Record mapping reuses existing `RecordType`s by family: floats → `Ai`/`Ao`,
  integers (all widths/signs) → `LongIn`/`LongOut`, `boolean` → `Bi`/`Bo`,
  `string` → `StringIn`/`StringOut`. The `NtScalar` payload's `ScalarValue`
  variant carries the precise wire type; RTYP stays a sensible record name.
- `set`/`scan` returns/`on_put` inputs coerce strictly against the stored
  `TypeCode`; `get` returns the natural Python type (`int`/`float`/`bool`/
  `str`). `repr` shows the wire type, e.g. `<spvirit.Pv 'X' (ulong)>`.
- `waveform`/`aai`/`aao` gain `type=` (element type) applied via
  `py_to_scalar_array_typed`.
- `spvirit.pv(name, initial, *, type=None, ...)`: when `type` is given it
  overrides inference — scalar types delegate to `scalar(..., writable=True)`
  semantics; `list`/`bytes` initials use the typed waveform path.

## Core change (`spvirit-server/src/pv.rs`)

Identity `impl PvScalar for ScalarValue`:

- `into_scalar` = identity; `from_scalar` = `Some`.
- `from_decoded` maps `DecodedValue` variants 1:1 onto `ScalarValue`
  variants (`Int32` → `I32`, ...), sidestepping the documented truthy-bool
  pitfall in `decoded_to_scalar_value`. The Python layer then re-coerces to
  the record's `TypeCode` inside its closures (on_put validator, scan
  bridge), so wire puts of a narrower/wider int are accepted or rejected by
  the same strict rules.

Bonus: `server.pv()` maps `long`/unsigned/`float` scalar records onto the
`Typed` kind instead of raising `KeyError`; the four existing kinds keep
their current mapping (no behavior change for existing code).

## Classic builder (`server.rs`)

- `type=` on `.waveform`, `.aai`, `.aao`, `.sub_array`, `.nt_ndarray`.
- `types=` (per-column / per-field dict) on `.nt_table` and `.generic`.

## Store put paths (`server.rs` Store)

`set_value`, `set_array_value`, and `put_nt` coerce the incoming value to the
**record's existing wire type** using the strict rules (today they replace
the stored variant with I64/F64). No `type=` parameter — the record is the
authority. Mismatches raise `TypeError`/`OverflowError` instead of silently
retyping the record.

## Errors

- `ValueError` — unknown type string.
- `OverflowError` — numeric value out of range for the requested type.
- `TypeError` — wrong value kind (str into int, fractional float into int,
  bytes into non-byte array, mixed-type list, ...).

## Testing

- Rust-side unit tests for the coercion matrix (each TypeCode × accepting /
  overflow / wrong-kind inputs), scalar and array.
- Python tests: construct each NT payload type with `type=`, serve via
  `scalar()`/typed `waveform()`, and verify the wire type through
  `Client.get`/`info` introspection (struct field type string) plus value
  round-trip; store-path coercion (in-range accepted, out-of-range raises,
  record type unchanged after put); `server.pv()` attach to unsigned/64-bit
  records; builder kwargs.

## Documentation

Update `spvirit-py/README.md`: new `scalar()` factory row/section, `type=`
kwargs, NT constructor signatures, and shrink the "NT scalar type coverage"
caveat block (the `KeyError` and "cannot be created from factories"
limitations disappear).

