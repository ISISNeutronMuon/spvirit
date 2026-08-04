# Serving scalars

<!-- verify:begin -->
> ✅ **Verified** · [`scalar_metadata.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/scalar_metadata.rs) · [`demo_scalars.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_scalars.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

The same two PVs as [Your first PV](../02-getting-started/first-pv.md), but
described properly: engineering units, display precision, a description,
alarm limits, drive limits, and a monitor deadband.

This is the difference between a number and a reading.

## Rust

```rust
{{#include ../../../../spvirit-server/examples/scalar_metadata.rs:meta}}
```

Note what this is **not** using. `PvaServer::builder().ai(name, value)`
takes a name and an initial value and nothing else — there is no
`.units()` on the builder. Metadata is set through typed `Pv<T>` handles,
then handed to `PvaServer::serve([...])`. If you need metadata in code,
that is the route.

(The other route is a `.db` file, covered in
[Loading .db files](db-files.md).)

## Python

```python
{{#include ../../../../spvirit-py/examples/demo_scalars.py:meta}}
```

Python has no such split — `spvirit.ai()` takes all of it as keyword
arguments, and `spvirit.Server(pvs=[...])` serves the handles.

## What to notice

**Metadata rides with the value.** A client reading `SIM:TEMPERATURE` gets
`degC` and `precision: 2` in the same message. That is the NTScalar
`display` structure from [Normative Types](../01-fundamentals/normative-types.md),
and it is why a PVAccess GUI can label an axis without being told to.

**Alarm limits are four numbers in one call**, ordered outward-in:
`alarm_limits(lolo, low, high, hihi)`. Crossing `low` or `high` is MINOR;
crossing `lolo` or `hihi` is MAJOR. [Alarms](alarms.md) goes into this.

**Drive limits are advertised, not enforced.** This one will catch you:

```console
$ spput SIM:SETPOINT 500
SIM:SETPOINT OK

$ spget SIM:SETPOINT
SIM:SETPOINT 2026-08-04 10:00:31.426 500
```

`drive_limits(0.0, 100.0)` populates the NTScalar `control` structure so
clients know the intended range — and that is all it does. Nothing in the
server clamps a write. If out-of-range values must be rejected, reject
them yourself in an `on_put` handler; see
[Reacting to writes](reacting-to-writes.md).

**`MDEL` and `ADEL` behave differently from the rest.** They are written
into the record's *field* table rather than the NT payload, which means
they are the two pieces of metadata you can read back QSRV-style:

```console
$ spget SIM:TEMPERATURE.MDEL
SIM:TEMPERATURE.MDEL 2026-08-04 10:02:48.038 0.5

$ spget SIM:TEMPERATURE.EGU
Error: Timeout("read header")
```

`.EGU` times out because there is no such channel. Field access serves the
dbCommon fields plus whatever a parsed `.db` file contained — and `units`
set in code goes into the payload, not the field table. The units are still
there; you just have to read the whole PV to see them. This is the gap
described in [EPICS in 10 minutes](../01-fundamentals/epics-in-10-minutes.md).

## Run it

```bash
# Terminal 1
cargo run -p spvirit-server --example scalar_metadata
# or: python spvirit-py/examples/demo_scalars.py

# Terminal 2
spget SIM:TEMPERATURE
spget SIM:TEMPERATURE.MDEL
spput SIM:SETPOINT 30
```

## Next

[Reading and writing](read-write.md).
