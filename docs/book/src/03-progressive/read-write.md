# Reading and writing

<!-- verify:begin -->
> ✅ **Verified** · [`pvget.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-client/examples/pvget.rs) · [`pvput.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-client/examples/pvput.rs) · [`demo_get.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_get.py) · [`demo_put.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_put.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

Client code that reads a PV and writes one, plus the CLI equivalents you
will reach for far more often than you expect.

## Rust

### Read

```rust
{{#include ../../../../spvirit-client/examples/pvget.rs:core}}
```

### Write

```rust
{{#include ../../../../spvirit-client/examples/pvput.rs:core}}
```

Both are `async` and both need a Tokio runtime. `PvaClient::builder()`
takes the network settings — `.port()`, `.udp_port()`, timeouts — and
`.build()` gives you a client you can reuse for many operations. Building
one client per PV works, but you pay for discovery every time.

The `_fields` variants take a list of dotted paths and send them as a
**pvRequest**, so the server returns only that subtree.

## Python

### Read

```python
{{#include ../../../../spvirit-py/examples/demo_get.py:get}}
```

### Write

```python
{{#include ../../../../spvirit-py/examples/demo_put.py:put}}
```

The Python client is blocking — no `await`, no event loop. Both `get` and
`put` take the same optional `fields` argument as their Rust counterparts.

## From the command line

```console
$ spget SIM:TEMPERATURE
SIM:TEMPERATURE 2026-08-04 09:46:31.420 22.5

$ spput SIM:SETPOINT 30
SIM:SETPOINT OK
```

`spget` formats the payload for humans; `spget --fields value,alarm.severity`
narrows it. For everything the tools take, see
[Command-line tools](../04-tools/index.md).

## What to notice

**A GET returns the whole structure, not a number.** In Rust
`result.value` is a `DecodedValue`; in Python `result.value` is a nested
dict, so the number itself is `result.value["value"]`. Everything else —
alarm, timeStamp, display, control, valueAlarm — arrived in the same
message and cost you nothing extra.

**A PUT targets a field, and that field defaults to `value`.** Python's
`put(pv, v)` is `put(pv, v, fields=["value"])`. That is why writing to a PV
does not wipe its alarm state or its units: you addressed one leaf of the
structure.

**Writing to an input record fails at the protocol level.**

```console
$ spput SIM:TEMPERATURE 99
SIM:TEMPERATURE ERROR protocol error: PUT init error: Write access denied
```

The refusal comes back on the PUT *init* exchange, before any value is
sent. The record type alone decides it.

**A client PUT does not restamp the record.** Server-side updates —
`store.set_value()`, a `scan` callback, a `.link()` recomputation — set the
record's `timeStamp` to now. A PUT arriving from a client changes the value
and leaves the timestamp where it was:

```console
$ spget SIM:SETPOINT
SIM:SETPOINT 2026-08-04 10:00:31.426 500
$ spput SIM:SETPOINT 42 && spget SIM:SETPOINT
SIM:SETPOINT OK
SIM:SETPOINT 2026-08-04 10:00:31.426  42
```

EPICS Base would restamp on record processing, so this is a divergence
worth knowing about — if you are timestamping on the strength of a PUT,
stamp it yourself in an `on_put` handler.

## Run it

```bash
# Terminal 1
cargo run -p spvirit-server --example scalar_metadata

# Terminal 2
cargo run -p spvirit-client --example pvget -- SIM:TEMPERATURE
cargo run -p spvirit-client --example pvput -- SIM:SETPOINT 30
python spvirit-py/examples/demo_put.py
```

## Next

[Monitoring changes](monitors.md).
