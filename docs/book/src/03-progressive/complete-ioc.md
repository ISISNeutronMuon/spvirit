# A complete IOC

<!-- verify:begin -->
> ✅ **Verified** · [`complete_ioc.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/complete_ioc.rs) · [`demo_complete_ioc.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_complete_ioc.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

One server that uses everything in Part III at once: a scanned readback
with units and a deadband, a validated setpoint, a derived PV, an array,
and an explicitly-managed alarm. It is small, but it is shaped like a real
piece of equipment rather than a demo.

The device is a vacuum system:

| PV | Record | Role |
|---|---|---|
| `VAC:PRESSURE` | `ai` | scanned readback, pumping down |
| `VAC:SETPOINT` | `ao` | target pressure, range-checked on write |
| `VAC:ERROR` | `calc` | readback minus setpoint |
| `VAC:RGA` | `aai` | a 64-point residual-gas spectrum |
| `VAC:LINK` | `ai` | controller reachability, severity set by hand |

## Rust

```rust
{{#include ../../../../spvirit-server/examples/complete_ioc.rs:build}}
```

Everything above is declarative — you describe the records and hand them
to the server. Anything the IOC needs to *do* beyond that is an ordinary
Tokio task holding the same handles:

```rust
{{#include ../../../../spvirit-server/examples/complete_ioc.rs:drive}}
```

Note the two-call construction. `PvaServer::serve()` takes one homogeneous
iterator, so the four `Pv<f64>` handles go in the first call and the
`PvArray` goes in through `.pvs()`
(`spvirit-server/src/pva_server.rs:724`, `:741`). Chain `.pvs()` as many
times as you have distinct handle types.

## Python

The same five records, with three differences worth naming:

```python
{{#include ../../../../spvirit-py/examples/demo_complete_ioc.py:build}}
```

`Server(pvs=[...])` takes **one flat list** — the two-call split above is a
Rust type-system constraint, not a protocol one, so the array handle goes
in beside the scalars.

`spvirit.calc(name, inputs, callback)` accepts no metadata keywords, so
`VAC:ERROR` cannot carry `units="mbar"` or a description the way the Rust
version does. If a computed PV needs metadata, write it through the raw-NT
layer with `store.put_nt(...)` after the server is up
([Records vs raw NT](../01-fundamentals/records-vs-raw-nt.md)).

Array PVs reject the scalar keywords (`units`, `prec`, `desc`, `adel`,
`mdel`, and both limit pairs) with `TypeError`, and `on_put`/`scan` raise
`TypeError` on them too (`spvirit-py/src/pv.rs:307`, `:365`). `VAC:RGA` is
therefore driven from a plain loop rather than a scan callback:

```python
{{#include ../../../../spvirit-py/examples/demo_complete_ioc.py:drive}}
```

Note the rejection path. Raising from `on_put` sends the exception's text
to the client, matching the Rust `Err(String)`; returning `False` also
rejects but the client sees a fixed `rejected by on_put`
(`spvirit-py/src/pv.rs:559`).

## Run it

```bash
# Terminal 1
cargo run -p spvirit-server --example complete_ioc
# or: python spvirit-py/examples/demo_complete_ioc.py
```

Discover the server, then ask it for its PV list:

```console
$ splist
GUID 0xC4960000E061B581C193C818 version 2: tcp@[ 10.64.23.134:5075 ]

$ splist 127.0.0.1:5075
VAC:ERROR
VAC:LINK
VAC:PRESSURE
VAC:RGA
VAC:SETPOINT
__pvlist
```

`splist` with no argument lists *servers*; `splist <target>` lists the PVs
on one. `__pvlist` is the server's own introspection channel — it is how
that second call works, and it appears in every listing.

Read and write:

```console
$ spget VAC:PRESSURE
VAC:PRESSURE 2026-08-04 10:35:23.578 0.000001

$ spput VAC:SETPOINT 5e-4
VAC:SETPOINT OK

$ spget VAC:ERROR
VAC:ERROR 2026-08-04 10:35:56.070 -0.0005

$ spput VAC:SETPOINT 1.0
VAC:SETPOINT ERROR protocol error: PUT failed: VAC:SETPOINT: 1 outside 1e-9..1e-3
```

The derived PV moved on its own: `on_put` accepted the setpoint, the store
propagated the change through the link graph, and `VAC:ERROR` recomputed
before the next `spget` arrived.

## What to notice

**A client PUT always advances the record's `timeStamp`.** The write
applies and the record is restamped with server time, distinct from
whatever it carried before:

```console
$ spput VAC:SETPOINT 2e-4 && spget VAC:SETPOINT
VAC:SETPOINT 2026-08-04 10:35:11.566 0.0002

$ spput VAC:SETPOINT 3e-4 && spget VAC:SETPOINT
VAC:SETPOINT 2026-08-04 10:35:57.902 0.0003
```

Each read carries the time of its own PUT, not the record's *creation*
time. `RecordInstance::apply_put`
(`spvirit-server/src/apply.rs:546`) updates `value`, `alarm`, `display`
and `control` from the client's structure and then always restamps —
with the client's `timeStamp` if the PUT carried a non-default one, so a
gateway can forward the originating acquisition time, otherwise with
server time — whether or not the value itself changed. That matches
EPICS Base, where `recGblGetTimeStampSimm()` runs unconditionally in
`process()`. Server-driven updates — `scan`, `calc`, `set` — stamp too,
which is why `VAC:PRESSURE` and `VAC:ERROR` above move on the same
rhythm as the client-written `VAC:SETPOINT`.

**The deadband is doing its job.** `VAC:PRESSURE` scans every 500 ms, but
a monitor posts roughly once a second:

```console
$ spmonitor VAC:PRESSURE
VAC:PRESSURE 2026-08-04 10:36:09.069   0
VAC:PRESSURE 2026-08-04 10:36:10.065   0
```

`.mdel(1.0e-8)` suppresses every tick whose change is smaller than that.
Half the scans move less than a nanobar and are dropped. See
[Monitors](monitors.md).

**`spget` formatting is not the value.** Once the chamber pumps below
about `1e-7` the display collapses to `0`. The wire value is intact —
`spget -F value` and `spmonitor` read the same double. For a fixed number
of digits, use the record's `PREC` field and a client that honours it, or
read the raw field.

**`Pv::calc` takes handles, not names.** The signature is
`calc(name, inputs: &[&Pv<f64>], f)` (`spvirit-server/src/pv.rs:392`), so
the input PVs must exist as handles before the derived PV is built. That
ordering constraint is what makes the link graph resolvable at
construction time instead of at first read.

**Validation belongs in `on_put`, not in `DRVL`/`DRVH`.** The drive limits
are published for GUIs; nothing enforces them. The range check here is the
only thing keeping `1.0` out of the record — and because `spput` retries a
rejected write, keep the callback idempotent
([Reacting to writes](reacting-to-writes.md)).

## Where to go next

You now have every building block. The remaining parts of this book are
reference rather than tutorial:

- [Command-line tools](../04-tools/index.md) — the `sp*` family in detail
- [Developer guide](../06-dev-guide/index.md) — internals, protocol notes,
  and the crate-level API
