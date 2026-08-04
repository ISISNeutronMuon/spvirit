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
(`spvirit-client/src/types.rs:64`), and the stage tells you where to look.

**`"search response"`** — nothing answered the UDP broadcast. Either no
server is running, or the search never reached it. Check, in this order:

1. Is the server up? `splist` with no arguments broadcasts and lists every
   server that answers.
2. Same subnet? PVA searches go out on UDP **5076** by broadcast. Across a
   router, set `EPICS_PVA_ADDR_LIST` to the server's address, or pass
   `--server host:port` to skip the search entirely.
3. Firewall? Windows blocks inbound UDP on a new binary the first time it
   binds. The server starts fine and answers nothing.
4. Multi-homed host? The search leaves on one interface. `spsearch` shows
   what is actually arriving — if it shows nothing, you are on the wrong NIC.

**`"read header"`, `"name server connect"`, `"name server handshake"`** —
something answered, and then the TCP leg failed. The commonest cause is a
stale `EPICS_PVA_NAME_SERVERS` pointing at a name server that is gone: the
client goes straight to TCP, skips the broadcast, and waits. Unset it and
retry before blaming the server.

The client reads five environment variables:
`EPICS_PVA_ADDR_LIST`, `EPICS_PVA_AUTO_ADDR_LIST`, `EPICS_PVA_NAME_SERVERS`,
`EPICS_PVA_CONN_TMO`, and `EPICS_PVA_ENABLE_GET_FIELD_FALLBACK`.

## The server is found but the PV is not

A server answers a search only for names it serves, so a timeout on one PV
while another works means the name is wrong — a typo, or a record that
failed to load. Start the server in the foreground and read the startup
line: it reports how many PVs came out of the `.db` file.

If the record is `mbbi` or `mbbo` and you loaded it from a `.db` file, it was
dropped and a message went to stderr. If it is `longin` or `longout`, it was
dropped with **no message at all** — the PV count in the startup line is your
only clue. See [Known gaps](known-gaps.md).

## `Write access denied`

```console
$ spput VAC:PRESSURE 1e-5
VAC:PRESSURE ERROR protocol error: PUT init error: Write access denied
```

The record type is an input type. `ai`, `bi`, `stringin`, `longin`, `aai`
and `subArray` refuse writes; their output counterparts accept them. The
matrix is on [Record types](record-types.md).

If you meant it to be writable, you wanted `ao` rather than `ai`. If you
want a simulated readback you can also poke, set `SIMM` — an `ai` in
simulation mode is writable (`spvirit-server/src/types.rs:308`).

## The write is accepted and nothing changes

Three different causes, and they look identical from the client:

**A validator rejected it.** Then you get an error, not silence — check the
exit status. `spput` returns non-zero on a rejected write.

**It is an enum.** A wire PUT of an `NtEnum` index currently reports success
and changes nothing. [Known gaps](known-gaps.md).

**It is an `NtTable` or `NtNdArray`.** The wire-PUT arm for those logs and
returns (`spvirit-server/src/simple_store.rs:608`). Write them server-side
with `store.put_nt()`.

## A monitor looks frozen

The value is moving but no updates arrive. Almost always the deadband:
`should_post_update` (`spvirit-server/src/simple_store.rs:535`) suppresses
the *post*, not the store, when the record is a numeric scalar, `MDEL` is
greater than zero, the severity has not changed, and the delta is under
`MDEL`.

Confirm with `spget` — if the value has moved and the monitor has not, that
is the deadband. Set `MDEL` to 0 to post everything.

Two things that are *not* the cause: alarm transitions always post regardless
of the deadband, and `ADEL` is parsed and exposed but not wired into any
posting logic.

## The timestamp never advances

A client PUT does not restamp the record. Server-side updates do — a `scan`
callback, a `.link()` recomputation, `set_value` — but a value that arrived
over the wire keeps whatever timestamp the record already had
(`apply_put_to_record`, `spvirit-server/src/simple_store.rs:558`). EPICS Base
would restamp on record processing, so this is a divergence. It is on
[Known gaps](known-gaps.md).

## Python: `on_put` never fires

You attached it after `Server.start()`. Handles are unbound until the server
starts and bound afterwards; `on_put`, `scan` and `calc` must be registered
while the handle is still unbound. There is no error — the callback simply
sits there.

Register everything before you start:

```python
temp = spvirit.ao("DEMO:SETPOINT", 20.0)
temp.on_put(lambda pv, v: print("set to", v))   # before
server = spvirit.Server([temp])
server.start()                                   # not after
```

## `on_put` fires twice for one write

Only for a *rejected* write, and only from `spput`. When the full PUT flow
fails, `spput` silently retries with the simple flow
(`spvirit-tools/src/bin/spvirit_put.rs:214`), so the server sees the write
twice. Pass `--no-flow-fallback` to suppress the retry — and write `on_put`
callbacks to be idempotent regardless.

## Alarm limits are set but the severity stays `NO_ALARM`

Limits set through `Pv::alarm_limits` are published in the payload's
`valueAlarm` structure but never evaluated. Computed severity comes from the
server-level `compute_alarms` flag, which is off by default
(`spvirit-server/src/server.rs:55`) — `spserver --compute-alarms`, or
`.compute_alarms(true)` on the builder. See
[Alarms and severity](../03-progressive/alarms.md).

## `cargo build` produced no binaries

`spvirit-tools` gates every binary behind a feature. A build with
`--no-default-features` builds the library and nothing else. See
[Crate map](crate-map.md).

## `spget` prints `0` for a value that is not zero

That column is a display rendering, not the wire value: `5e-7` prints as
`0`. Use `spget -F value` to see the field as decoded.
