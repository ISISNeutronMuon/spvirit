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
DEMO:SETPOINT 2026-08-06 09:14:58.052  46 MAJOR READ HIHI
```

Structured payloads print inline:

```console
$ spget SIM:STATE
SIM:STATE 2026-08-06 09:14:32.958 {index=0, choices=["Idle", "Running", "Error"]}

$ spget VAC:RGA
VAC:RGA 2026-08-06 09:35:20.331 [0.909297, 0.808496, 0.675463, ...]
```

The array above is elided for the page; `spget` prints **every** element,
however many there are. A 1024-point waveform is one very long line.

## Gotchas

**The printed value is formatted, not raw.** Small doubles collapse:

```console
$ spget VAC:SETPOINT     # the record holds 5e-7
VAC:SETPOINT 2026-08-06 09:35:33.545   0
```

The wire value is intact — but neither `-F value` nor `--json` will show it
to you, because both go through the same formatter:

```console
$ spget -F value VAC:SETPOINT
VAC:SETPOINT   0

$ spget --json VAC:SETPOINT
{"alarm":"alarm=OK status=NO_ALARM(0)","pv":"VAC:SETPOINT",
 "timestamp":"2026-08-06 09:35:33.545","units":null,
 "value":"value=0.000000, ts=1786008933"}
```

`spget --raw` dumps the payload bytes, which does contain the true double;
otherwise read the PV from a client library rather than a CLI.

**Search failure looks like a timeout.** If nothing answers, you get
`Timeout("search response")` — not "PV not found". No server on the
network can distinguish the two, so neither can the client. Check the PV
name, then `splist`, then `--server`.

## See also

[Reading and writing](../03-progressive/read-write.md) does the same thing
from the Rust and Python APIs.
