# `spget`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Read one PV, print it, exit. The equivalent of EPICS Base `pvget`.

```
spget [OPTIONS] [PV]
```

Requires the `client` feature. Takes exactly one PV — pass several names
and it errors with `Unexpected argument`. Loop in the shell if you need
more than one.

## Flags

The shared client flags apply ([see the index](index.md)).
`spget` adds none of its own.

`-F`/`--fields` is the one worth knowing: it narrows the field request
sent to the server, so `-F value` fetches the value without the alarm,
timestamp, and display blocks.

## Output

```console
$ spget VAC:PRESSURE
VAC:PRESSURE 2026-08-04 10:35:23.578 0.000001
```

Three columns: name, the record's `timeStamp`, and the value. An alarm
appends severity and status:

```console
$ spget DEMO:SETPOINT
DEMO:SETPOINT  46 MAJOR READ HIHI
```

Structured payloads print inline:

```console
$ spget SIM:STATE
SIM:STATE {index=2, choices=["Idle", "Running", "Fault"]}

$ spget VAC:RGA
VAC:RGA 2026-08-04 10:35:24.531 [0.083089, 0.116549, 0.311541, ...]
```

## Gotchas

**The printed value is formatted, not raw.** Small doubles collapse:

```console
$ spget VAC:SETPOINT     # the record holds 5e-7
VAC:SETPOINT 2026-08-04 10:35:11.566   0
```

The wire value is intact. Use `-F value` or a monitor if you need to see
the number rather than a display rendering.

**Search failure looks like a timeout.** If nothing answers, you get
`Timeout("search response")` — not "PV not found". No server on the
network can distinguish the two, so neither can the client. Check the PV
name, then `splist`, then `--server`.

## See also

[Reading and writing](../03-progressive/read-write.md) does the same thing
from the Rust and Python APIs.
