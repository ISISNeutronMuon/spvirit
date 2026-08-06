# Alarms and severity

<!-- verify:begin -->
> ✅ **Verified** · [`alarms.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/alarms.rs) · [`demo_alarms.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_alarms.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

PVs that tell a client not just *what* the value is but whether it should
be worried about it.

## The alarm structure

Every `NtScalar` carries an alarm block alongside its value:

```
alarm:
  severity: int      # 0 NONE, 1 MINOR, 2 MAJOR, 3 INVALID
  status:   int      # EPICS status code
  message:  string   # free text
```

Severity is the part clients act on. A control screen turns a widget yellow
on MINOR and red on MAJOR; an archiver may record the transition even when
the value itself is inside the deadband.

**Alarm transitions always post.** The MDEL deadband gates value changes
only — a severity change reaches every subscriber regardless of how small
the value moved (`spvirit-server/src/simple_store.rs:535`).

## Two ways to get a severity

| | How | Evaluated by |
|---|---|---|
| **Computed** | `LOW`/`HIGH`/`LOLO`/`HIHI` in a `.db` file, plus `.compute_alarms(true)` | the server, on every write |
| **Explicit** | `set_alarm(severity, status, message)` | you |

### Computed, from a `.db` file

```epics
record(ao, "DEMO:SETPOINT") {
    field(LOW,  "10")     # MINOR below this
    field(HIGH, "40")     # MINOR above this
    field(LOLO, "5")      # MAJOR below this
    field(HIHI, "45")     # MAJOR above this
}
```

With `.compute_alarms(true)` the server re-evaluates on every write:

```console
$ spput DEMO:SETPOINT 41 && spget DEMO:SETPOINT
DEMO:SETPOINT  41 MINOR READ HIGH

$ spput DEMO:SETPOINT 46 && spget DEMO:SETPOINT
DEMO:SETPOINT  46 MAJOR READ HIHI

$ spput DEMO:SETPOINT 3 && spget DEMO:SETPOINT
DEMO:SETPOINT   3 MAJOR READ LOLO

$ spput DEMO:SETPOINT 25 && spget DEMO:SETPOINT
DEMO:SETPOINT  25
```

`compute_alarms` defaults to **`false`**. Without it the limits are
published but never compared against anything.

### Explicit

## Rust

```rust
{{#include ../../../../spvirit-server/examples/alarms.rs:limits}}
```

```rust
{{#include ../../../../spvirit-server/examples/alarms.rs:manual}}
```

## Python

Same two halves. `alarm_limits=(lolo, low, high, hihi)` is a keyword
argument rather than a builder call:

```python
{{#include ../../../../spvirit-py/examples/demo_alarms.py:limits}}
```

```python
{{#include ../../../../spvirit-py/examples/demo_alarms.py:manual}}
```

## What to notice

**`.alarm_limits()` is published but not evaluated.** This is the trap in
this chapter. Calling `.alarm_limits(lolo, low, high, hihi)` on a `Pv`
handle — or passing `alarm_limits=(...)` in Python — fills in the
`valueAlarm` structure, and clients can read those limits back with
`spinfo`. But the value is never compared against them, **even with
`.compute_alarms(true)`**:

```console
$ spget SIM:PRESSURE
SIM:PRESSURE  50

$ spput SIM:PRESSURE 95 && spget SIM:PRESSURE
SIM:PRESSURE  95          # no MINOR, despite high = 90
```

The cause is two parallel sets of fields. `Pv::alarm_limits` writes
`nt.value_alarm_*` (`spvirit-server/src/pv.rs:329`), while the evaluator
`update_alarm_from_value` reads `nt.alarm_low`/`alarm_high`/`alarm_lolo`/
`alarm_hihi` (`spvirit-types/src/lib.rs:285`) — and only the `.db` loader
populates those (`spvirit-server/src/db.rs:360`).

So today: **computed alarms require a `.db` file.** For handle-built PVs,
call `set_alarm` yourself, as the examples above do.

**INVALID is for "I don't know", not "bad value".** Severity 3 means the
reading cannot be trusted — the device is unreachable, the scan threw, the
link is down. A value that is simply too high is MAJOR, not INVALID. This
matters because clients treat INVALID as "ignore this number".

**A failing `calc` posts `0.0` with severity NONE.** It looks like a
genuine reading of zero. If a computed PV must distinguish broken from
zero, set INVALID explicitly — see [Simulating a device](simulating.md).

**`set_alarm` bypasses the deadband and does not evaluate links.** It is a
direct write to the alarm block.

## Run it

```bash
# Terminal 1
cargo run -p spvirit-server --example alarms
# or: python spvirit-py/examples/demo_alarms.py

# Terminal 2
spget SIM:LINK          # INVALID device unreachable
spmonitor SIM:PRESSURE
```

## Next

[Serving a .db file](db-files.md).
