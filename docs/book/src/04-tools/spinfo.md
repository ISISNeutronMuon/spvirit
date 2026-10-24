# `spinfo`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Print a PV's type structure without fetching its value. The equivalent of
EPICS Base `pvinfo`.

```
spinfo [OPTIONS] [PV]
```

Requires the `client` feature. Uses the `CMD_GET_FIELD` (`0x11`) protocol
command, so the server answers with an introspection description and no
data.

## Flags beyond the shared set

| Flag | Meaning |
|---|---|
| `-f`, `--field PATH` | inspect a sub-field, e.g. `value` or `alarm.severity` |
| `-t`, `--terse` | one-line type summary instead of the tree |

## Output

```console
$ spinfo VAC:SETPOINT
VAC:SETPOINT:
struct epics:nt/NTScalar:1.0
value: double
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
```

## What it is good for

**Confirming the normative type.** The first line is the type ID. If a
client library refuses a PV, this tells you whether the server is really
sending `epics:nt/NTScalar:1.0` or something else.

**Seeing the alarm limits.** The `valueAlarm` block above exists because
the record was built with `.alarm_limits(...)`. The limits are on the wire,
and `spinfo` proves it. Whether the server also *evaluates* them depends on
`compute_alarms`: when it is on (`spserver --compute-alarms` or
`.compute_alarms(true)`), every write is compared against these same limits
and drives the severity; it is off by default, and then the limits are only
published. `spinfo` shows the limits either way, which is how you tell
metadata from computed severity. See [Alarms](../03-progressive/alarms.md).

**Working out what `-F` can ask for.** The dotted paths in `spget -F` and
`spmonitor -F` are exactly the paths in this tree.
