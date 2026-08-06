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

## Choosing the wire type

`ai`/`ao`/`bi`/`bo`/`longin`/`longout`/`string_in`/`string_out` (Rust) and
`ai`/`ao`/`bo`/`longin`/`longout`/`string_in`/`string_out` (Python) each
fix the NTScalar wire type to one of `double`, `boolean`, `int` or
`string`. PVAccess defines twelve NTScalar value types in total —
`boolean`, `byte`, `short`, `int`, `long`, their unsigned variants
(`ubyte`, `ushort`, `uint`, `ulong`), `float`, `double`, and `string` — and
reaching the other eight needs an explicit type selection.

### Rust

```rust
{{#include ../../../../spvirit-server/examples/scalar_metadata.rs:types}}
```

`Pv::<ScalarValue>::scalar_out`/`scalar_in` build a record whose wire type
is whatever `ScalarValue` variant `initial` holds — `scalar_out` for a
writable PV, `scalar_in` for read-only.

### Python

```python
{{#include ../../../../spvirit-py/examples/demo_scalars.py:types}}
```

`spvirit.scalar(name, initial, *, type, writable=False, **opts)` picks the
wire type by name (or alias, e.g. `"u16"`/`"i32"`); `writable=True` serves
the output flavor. The full type-name/alias table and the value coercion
rules (overflow, widening, narrowing) are in
[`spvirit-py`'s README, *NT scalar type coverage*
section](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/README.md#nt-scalar-type-coverage)
— the same reference the [Python API](../05-reference/python-api.md) page
points to for `pv`/`scalar`.

## Run it

```bash
# Terminal 1
cargo run -p spvirit-server --example scalar_metadata
# or: python spvirit-py/examples/demo_scalars.py

# Terminal 2
spget SIM:TEMPERATURE
spget SIM:TEMPERATURE.MDEL
spput SIM:SETPOINT 30
spinfo SIM:GAIN
spinfo SIM:STATUS
```

Terminal 2 prints:

```console
$ spget SIM:TEMPERATURE
SIM:TEMPERATURE 2026-08-06 09:13:06.435 22.5

$ spget SIM:TEMPERATURE.MDEL
SIM:TEMPERATURE.MDEL 2026-08-06 09:13:06.707 0.5

$ spput SIM:SETPOINT 30
SIM:SETPOINT OK
```

`spinfo` prints the type of every field. Only the third line differs
between the two records — `SIM:GAIN` is a `ushort`, `SIM:STATUS` a `ubyte`
— and the remaining forty-odd lines are the same NTScalar skeleton in both:

```console
$ spinfo SIM:GAIN
SIM:GAIN:
struct epics:nt/NTScalar:1.0
value: ushort
alarm: structure
  severity: int
  status: int
  message: string
timeStamp: structure
  secondsPastEpoch: long
  nanoseconds: int
  userTag: int
display: structure
  limitLow: double
  limitHigh: double
  description: string
  units: string
  precision: int
  form: structure
    index: int
    choices: string[]
control: structure
  limitLow: double
  limitHigh: double
  minStep: double
valueAlarm: structure
  active: boolean
  lowAlarmLimit: double
  lowWarningLimit: double
  highWarningLimit: double
  highAlarmLimit: double
  lowAlarmSeverity: int
  lowWarningSeverity: int
  highWarningSeverity: int
  highAlarmSeverity: int
  hysteresis: ubyte

$ spinfo SIM:STATUS
SIM:STATUS:
struct epics:nt/NTScalar:1.0
value: ubyte
...
```

That is the point of the narrow types: the wire carries two bytes for a
`ushort` and one for a `ubyte`, but the surrounding structure — alarm,
timestamp, display, control, valueAlarm — is identical whatever the value
type is.

## Next

[Reading and writing](read-write.md).
