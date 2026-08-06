# Records vs raw NT

<!-- verify:begin -->
> ✅ **Verified** · [`nt_put_get.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/nt_put_get.rs) · [`demo_nt_access.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_nt_access.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Everything on the wire is a Normative Type, but Spvirit gives you two levels
at which to work — and it pays to know which one you are on. Choosing wrong
is the most common source of "why is my timestamp zero" and "why does my
monitor fire on every tick".

## The comparison

|  | IOC-style records | Raw NT payloads |
|---|---|---|
| You create them with | `Pv<T>` handles (`Pv::ai(...)` …), builder methods (`.ai()`, `.waveform()` …), `.db` files | `NtScalar`/`NtScalarArray`/… built by hand; hand-built `RecordInstance`; custom `Source` impls |
| You read/write | plain values: `pv.set(21.5)`, `store.set_value(...)` | whole payloads: `store.put_nt(...)` / `get_nt(...)`, `Notifier` posts |
| Alarm state | computed for you from HIHI/HIGH/LOW/LOLO limits (`compute_alarms`), or `pv.set_alarm(...)` | you set `alarm` on every payload yourself |
| Timestamps | stamped automatically on every update, client PUT included | yours to fill in — an explicit `timeStamp` is honoured, a zero one is stamped for you |
| Display/control metadata (EGU, PREC, limits) | record fields, visible QSRV-style (`PV.EGU`, `PV.DESC`, …) | whatever you put in the payload, each update |
| Monitor deadbands (MDEL/ADEL) | applied by the server | not applied — every `put_nt`/notify posts |
| Best for | soft IOCs, simulators, anything that should feel like an EPICS record | gateways/bridges, tables, images, PVs whose metadata changes per update |

## Rule of thumb

Stay IOC-style — `Pv<T>` handles first, `.db` files for existing databases —
until you need per-update control of the metadata, or a payload shape the
record layer does not model. Then drop to `put_nt`/`get_nt`, hand-built
records, or a custom `Source`.

The two mix freely in one server. `store.get_nt()` returns the full payload
of an IOC-style record too, so you can start with records and reach through
to the raw layer for the one PV that needs it.

## The three consequences that bite

**Deadbands only exist at the record level.** `MDEL` and `ADEL` are applied
by the server when an IOC-style record's value is set. A `put_nt` bypasses
that entirely: every post goes out. If you have a 1 kHz raw-NT source and a
monitor client, you are sending 1000 updates a second, and no amount of
`MDEL` in a `.db` file will change that.

**Timestamps are automatic at both levels, and the rule is the same one.**
Post a payload — or PUT a record field — with a zero or absent `timeStamp`
and the server stamps it with the current time; supply a real `timeStamp`
and it is honoured instead. That is deliberate, so a gateway can forward
the *originating* acquisition time rather than the time it happened to
relay the value.

At the record level this restamp happens on every accepted PUT, whether or
not the value itself changed — `RecordInstance::apply_put`
(`spvirit-server/src/apply.rs:546`) applies `value`, `alarm`, `display` and
`control`, then always calls `set_time_stamp`. Server-side updates —
`set_value`, a `scan` callback, a `.link()` recomputation — restamp the same
way. EPICS Base does the same: `recGblGetTimeStampSimm()` runs
unconditionally in `process()`, independent of whether the value moved.
[Reading and writing](../03-progressive/read-write.md) shows it happening.

**Alarms are only computed at the record level.** `compute_alarms` walks the
`HIHI`/`HIGH`/`LOW`/`LOLO` thresholds and sets severity. Raw payloads get the
severity you put in them, which for a hand-built `NtScalar::from_value` is
`NO_ALARM` — a silently un-alarming PV.

## Building a payload by hand

This is what the raw level actually looks like. In Rust, `NtScalar` starts
from a `ScalarValue` and the `with_*` builders each consume and return the
payload, so they chain; fields without a builder are plain `pub` fields you
assign:

```rust
{{#include ../../../../spvirit-server/examples/nt_put_get.rs:builder}}
```

Then the payload goes to the store as a whole, and comes back as a whole:

```rust
{{#include ../../../../spvirit-server/examples/nt_put_get.rs:putget}}
```

Python has no builder chain — the same fields are keyword arguments on the
`NtScalar` constructor:

```python
{{#include ../../../../spvirit-py/examples/demo_nt_access.py:builder}}
```

and the read side:

```python
{{#include ../../../../spvirit-py/examples/demo_nt_access.py:getnt}}
```

`NtScalarArray`, `NtTable` and `NtNdArray` follow the same shape. The Python
constructors are `spvirit.NtScalar`, `spvirit.NtScalarArray`,
`spvirit.NtTable`, `spvirit.NtNdArray`, plus `Alarm`, `TimeStamp`, `Display`
and `Control` for the substructures.

Note what neither example gets for free: no deadband, no computed alarm.
Both are record-level services, and the payload you built is not a record.
That is the trade the table above describes.

## Which one am I on?

If you called `.ai()`, `.ao()`, `Pv::ai()`, or loaded a `.db`, you are on the
record level. If you implemented the `Source` trait or called `put_nt`, you
are on the raw level. If you called `store.set_value()` on a record, you are
still on the record level — that is the record API, and it applies deadbands
and stamps time.

Both are covered in Part III: records throughout, and the raw level in
[Tables and images](../03-progressive/tables-and-images.md) (payload types
the record layer does not model) and [Custom data
sources](../03-progressive/sources.md) (serving PVs without a record store
at all).
