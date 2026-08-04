# Arrays and waveforms

<!-- verify:begin -->
> ✅ **Verified** · [`waveform.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/waveform.rs) · [`demo_waveform.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_waveform.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

A 1024-point spectrum, updated ten times a second. Same shape as a
detector trace, a scope capture, or an image row.

## Rust

Serve it:

```rust
{{#include ../../../../spvirit-server/examples/waveform.rs:serve}}
```

Update it:

```rust
{{#include ../../../../spvirit-server/examples/waveform.rs:update}}
```

`store.set_array_value(name, ScalarArrayValue::F64(v))` replaces the whole
array. `ScalarArrayValue` has a variant per element type — `F64`, `I32`,
`U8`, `Bool`, `Str`, and the rest — and the variant is fixed when the
record is created.

## Python

```python
{{#include ../../../../spvirit-py/examples/demo_waveform.py:serve}}
```

Note `server.start()` rather than `server.run()`. `run()` blocks forever;
`start()` returns so the loop below it can drive the PV.

## The three array record types

| Builder | Record | Clients may write |
|---|---|---|
| `.waveform(name, data)` | `waveform` | yes |
| `.aai(name, data)` | `aai` | no |
| `.aao(name, data)` | `aao` | yes |

Pick `aai` for anything a client should only read — a detector spectrum, a
computed histogram. Pick `aao` or `waveform` for a lookup table or a
scan trajectory a client is meant to load.

`.sub_array(name, data, indx, nelm)` serves a window into a larger array,
which is the EPICS `subArray` record.

## What to notice

**Python array setters take a `list`.** A numpy array is not a list —
call `.tolist()` first:

```python
spectrum.set(my_numpy_array.tolist())
```

The one exception is `U8` arrays, which also accept `bytes`. `type=`
selects the element type explicitly when the Python values are ambiguous:
`spvirit.waveform("IMG", data, type="ushort")`.

**`on_put` and `scan` are not available on array PVs in Python.** Both
raise `TypeError`. Drive an array from your own loop with `pv.set(...)`,
as the example above does.

**Arrays carry their own timestamp handling.** Unlike `NtScalar` — where a
`None` timestamp makes the encoder stamp at encode time — array payloads
encode the timestamp they hold verbatim. The server stamps on every
`set_array_value`, so this only matters if you build payloads by hand at
the raw-NT level.

**Every update sends the whole array.** There is no delta encoding. A
1024-point `f64` waveform at 10 Hz is about 80 kB/s per subscriber. `MDEL`
does not help here — the deadband gate only applies to numeric *scalars*,
so an array posts on every change.

## Run it

```bash
# Terminal 1
cargo run -p spvirit-server --example waveform
# or: python spvirit-py/examples/demo_waveform.py

# Terminal 2
spget SIM:SPECTRUM
spmonitor SIM:SPECTRUM
```

## Next

[Enums and binary records](enums.md).
