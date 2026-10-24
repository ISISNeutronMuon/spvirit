# Troubleshooting

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Real failures, in rough order of how often they happen.

## A client cannot find the server

```console
$ spget DEMO:TEMP
Error: Timeout("search response")
```

`PvGetError::Timeout` carries the stage it gave up at
(`spvirit-client/src/types.rs:63`), and the stage tells you where to look.

**`"search response"`** — nothing answered the UDP broadcast. Either no
server is running, or the search never reached it. Check, in this order:

1. Is the server up? `splist` with no arguments broadcasts and lists every
   server that answers.
2. Same subnet? PVA searches go out on UDP **5076** by broadcast. Across a
   router, set `EPICS_PVA_ADDR_LIST` to the server's address, or pass
   `--server ip:port` (an IP literal; a hostname is not resolved) to skip the
   search. For a hostname, put it in `EPICS_PVA_ADDR_LIST` or
   `--name-server`. (`splist` ignores `--server`; give it the address as its
   positional target.)
3. Firewall? Windows blocks inbound UDP on a new binary the first time it
   binds. The server starts fine and answers nothing.
4. Multi-homed host? The search leaves on one interface. `spsearch` shows
   what is actually arriving — if it shows nothing, you are on the wrong NIC.

**`"read header"`, `"name server connect"`, `"name server handshake"`** —
something answered, and then the TCP leg failed. A stale
`EPICS_PVA_NAME_SERVERS` entry is tried in parallel with the UDP broadcast,
so it only bites when broadcast is disabled (`--no-broadcast`,
`EPICS_PVA_AUTO_ADDR_LIST=NO`) or the server is not reachable by broadcast;
the error you see is the last strategy to fail. Unset it, or pass
`--server ip:port`, to rule it out.

The client reads four environment variables: `EPICS_PVA_ADDR_LIST`,
`EPICS_PVA_AUTO_ADDR_LIST`, `EPICS_PVA_NAME_SERVERS`, and
`EPICS_PVA_ENABLE_GET_FIELD_FALLBACK` (the last only affects `splist`'s
GET_FIELD fallback, off by default). `spserver` also reads
`EPICS_PVA_CONN_TMO` for its idle-connection timeout.

## The server is found but the PV is not

A server answers a search only for names it serves, so a timeout on one PV
while another works means the name is wrong. Start the server in the
foreground and read the startup line: it reports how many PVs came out of
the `.db` file.

A `.db` file containing an `mbbi`, `mbbo`, `longin` or `longout` record does
not load at all: `spserver` exits with an error naming the first such record.
See [Known gaps](known-gaps.md).

## `Write access denied`

```console
$ spput VAC:PRESSURE 1e-5
VAC:PRESSURE ERROR protocol error: PUT init error: Write access denied
```

The record type is an input type. `ai`, `bi`, `stringin`, `longin`, `mbbi`,
`aai` and `subArray` refuse writes; their output counterparts accept them. The
matrix is on [Record types](record-types.md).

If you meant it to be writable, you wanted `ao` rather than `ai`. If you
want a simulated readback you can also poke, set `SIMM` — an `ai` in
simulation mode is writable (`spvirit-server/src/types.rs:359`).

## The write is accepted and nothing changes

**A validator rejected it.** Then you get an error, not silence — check the
exit status. `spput` returns non-zero on a rejected write.

An enum PUT with an out-of-range index, a PUT to an `mbbi`, and a PUT to a
`Generic` record are all refused with an error too — none of them reports
success without a change. [Known gaps](known-gaps.md).

## A monitor looks frozen

The value is moving but no updates arrive. Almost always the deadband:
`should_post_update` (`spvirit-server/src/simple_store.rs:644`) suppresses
the *post*, not the store, when the record is a numeric scalar, `MDEL` is
greater than zero, the severity has not changed, and the delta is under
`MDEL`.

Confirm with `spget` — if the value has moved and the monitor has not, that
is the deadband. Set `MDEL` to 0 to post everything.

Two things that are *not* the cause: alarm transitions always post regardless
of the deadband, and `ADEL` is parsed and exposed but not wired into any
posting logic.

## Python: `on_put` never fires

You attached it after the PV was handed to `Server(...)`. A handle is
unbound until `Server(...)` is constructed, and bound after it; `on_put`,
`scan` and `calc` must be registered before that. Nothing is raised to
Python; the core only logs a warning.

Register everything before you start:

```python
temp = spvirit.ao("DEMO:SETPOINT", 20.0)
temp.on_put(lambda pv, v: print("set to", v))   # before Server(...)
server = spvirit.Server(pvs=[temp])               # binds the handle
server.start()
```

## `on_put` fires twice for one write

Only for a *rejected* write, and only from `spput`. When the full PUT flow
fails, `spput` silently retries with the simple flow
(`spvirit-tools/src/bin/spvirit_put.rs:240-249`), so the server sees the write
twice. Pass `--no-flow-fallback` to suppress the retry — and write `on_put`
callbacks to be idempotent regardless.

## Alarm limits are set but the severity stays `NO_ALARM`

Limits — from `Pv::alarm_limits` or a `.db` file — are only evaluated when
the server-level `compute_alarms` flag is on, and it is off by default
(`spvirit-server/src/server.rs:64`) — `spserver --compute-alarms`, or
`.compute_alarms(true)` on the builder. The limits are not applied to the
initial value: severity changes on the first write. See
[Alarms and severity](../03-progressive/alarms.md).

## `cargo build` produced no binaries

`spvirit-tools` gates every binary behind a feature. A build with
`--no-default-features` builds the library and nothing else. See
[Crate map](crate-map.md).

## `spget` prints `0` for a value that is not zero

That column is a display rendering, not the wire value: `5e-7` prints as
`0`. Neither `-F value` nor `--json` shows the full-precision double (they
use the same formatter). Use `spget --raw` for the payload bytes, or read the
PV from a client library.
