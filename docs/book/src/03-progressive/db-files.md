# Serving a `.db` file

<!-- verify:begin -->
> ✅ **Verified** · [`db_file.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/db_file.rs) · [`example.db`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/example.db) · [`demo_db_file.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_db_file.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

A soft IOC defined the EPICS way — as a database file rather than as code.

This is the point where spvirit stops being "a PVAccess library" and starts
being "an IOC you can drop into an existing control system". The same file
an EPICS IOC would load, spvirit serves.

## The database

```epics
{{#include ../../../../spvirit-server/examples/example.db}}
```

A `.db` file is a list of `record(type, "NAME") { field(FIELD, "value") }`
blocks. Fields spvirit does not model are ignored rather than rejected, so
a database written for a real IOC generally loads unchanged — you just get
fewer behaviours than EPICS Base would give you.

## Rust

```rust
{{#include ../../../../spvirit-server/examples/db_file.rs:load}}
```

`.db_string(content)` takes the same syntax from a string, which is
convenient in tests.

## Python

`db_file=` and `db_string=` are keyword arguments on `Server`, and mix
freely with `pvs=[...]` handles in the same server:

```python
{{#include ../../../../spvirit-py/examples/demo_db_file.py:load}}
```

A `.db` record arrives without a handle. `Server.pv(name)` mints one,
typed from the record's wire type:

```python
{{#include ../../../../spvirit-py/examples/demo_db_file.py:handle}}
```

**That handle is already bound**, which is the catch worth knowing before
you plan around it: `on_put`, `scan` and `calc` are only honoured on an
*unbound* handle, so attaching one to a `.db` record does nothing (the core
logs a warning and carries on). A record that needs a write validator or a
scan callback has to be declared in code with `spvirit.ao(...)` rather than
loaded from the file. The same is true in Rust — `PvaServer::pv()` attaches,
it does not re-declare. See [Reacting to writes](reacting-to-writes.md).

## From the command line

No code at all:

```bash
spserver --db spvirit-server/examples/example.db
```

## Fields that do something

| Field | Effect |
|---|---|
| `VAL` | initial value |
| `DESC` | description, served in `display.description` |
| `EGU` | engineering units |
| `PREC` | display precision |
| `LOPR` / `HOPR` | display limits (a GUI hint; nothing enforces them) |
| `LOW` / `HIGH` | MINOR alarm limits — **evaluated**, given `.compute_alarms(true)` |
| `LOLO` / `HIHI` | MAJOR alarm limits — same |
| `MDEL` | monitor deadband |
| `ADEL` | archive deadband |
| `DRVL` / `DRVH` | drive limits — **advisory only**, spvirit does not clamp |
| `SCAN` | `"1 second"` etc., for periodic reprocessing |
| `INP` | input link, for scanned records that copy another PV |
| `FTVL` / `NELM` | element type and count, for array records |
| `ZNAM` / `ONAM` | the two state names of a `bi`/`bo` |
| `INDX` / `MALM` | window offset and max length, for `subArray` |

## What to notice

**`.db` is the only route to computed alarms.** As
[Alarms](alarms.md) explains, the handle API's `.alarm_limits()` publishes
limits without evaluating them. `LOW`/`HIGH`/`LOLO`/`HIHI` in a `.db` file
are evaluated. If you want the server to derive severity from the value,
this is how.

**Input records refuse writes.** `ai`, `bi`, `stringin`, `aai` are
read-only on the wire, and the refusal is explicit rather than silent:

```console
$ spput DEMO:TEMP 30
DEMO:TEMP ERROR protocol error: PUT init error: Write access denied
```

Use the `o` variants — `ao`, `bo`, `stringout`, `aao`, `waveform` — for
anything a client should set.

**`mbbi`/`mbbo` cannot be loaded from `.db`.** They parse and are then
rejected at construction. Build enum records in code — see
[Enums](enums.md).

**`longin`/`longout` are not recognised by the `.db` parser either.** They
exist in the handle API only (`spvirit-server/src/types.rs:48`).

**Loading is best-effort per record.** A record spvirit cannot build logs
to stderr and is skipped; the rest of the file still serves. Check the log
rather than assuming every PV made it.

## Run it

```bash
# Terminal 1
cargo run -p spvirit-server --example db_file
# or: python spvirit-py/examples/demo_db_file.py

# Terminal 2
splist
spget DEMO:TEMP
spput DEMO:SETPOINT 46
spget DEMO:SETPOINT        # MAJOR HIHI
```

## Next

[Tables and images](tables-and-images.md).
