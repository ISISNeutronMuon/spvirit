# Your first PV

<!-- verify:begin -->
> ✅ **Verified** · [`simple_server.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/simple_server.rs) · [`pvget.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-client/examples/pvget.rs) · [`demo_first_pv.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_first_pv.py) · [`demo_get.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_get.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

A server holding three PVs, and a client that reads one back. Two terminals.

The three records are deliberately of different kinds — one you can only
read, two you can write — because that distinction is the first thing worth
internalising.

## Rust

### The server

```rust
{{#include ../../../../spvirit-server/examples/simple_server.rs:records}}
```

`PvaServer::builder()` collects records, `.build()` freezes them into a
server, and `server.run().await` binds the sockets and serves forever. The
full example adds a background task that walks `SIM:TEMPERATURE` toward
`SIM:SETPOINT`, but the four lines above are already a working IOC-like
server.

### The client

```rust
{{#include ../../../../spvirit-client/examples/pvget.rs:core}}
```

That sits inside `#[tokio::main] async fn main`, with `pv` a `String` — both
the client and the server are async, so you need a Tokio runtime.

## Python

### The server

```python
{{#include ../../../../spvirit-py/examples/demo_first_pv.py:serve}}
```

### The client

```python
{{#include ../../../../spvirit-py/examples/demo_get.py:get}}
```

The Python client is **blocking** — `client.get(...)` returns a value, no
`await`, no event loop. `result.value` is the whole NTScalar as a nested
dict, which is why the value itself is `result.value["value"]`.

## Run it

```bash
# Terminal 1
cargo run -p spvirit-server --example simple_server

# Terminal 2
cargo run -p spvirit-client --example pvget -- SIM:TEMPERATURE
```

Or with the Python pair:

```bash
python spvirit-py/examples/demo_first_pv.py     # terminal 1
python spvirit-py/examples/demo_get.py          # terminal 2
```

The two halves mix freely: the Rust client reads the Python server, `spget`
reads either, and so does `pvget` from EPICS Base.

## What to notice

**`ai` is an input; `ao` and `bo` are outputs.** Input and output are named
from the *server's* point of view, so an input record is read-only to
clients. Try it:

```console
$ spput SIM:SETPOINT 30
SIM:SETPOINT OK

$ spput SIM:TEMPERATURE 99
SIM:TEMPERATURE ERROR protocol error: PUT init error: Write access denied
```

The server enforces that from the record type alone — you did not configure
any permissions.

**You get more than a number back.** A raw `pvget` prints the entire
NTScalar: value, alarm, timeStamp, display, control, valueAlarm. The
timestamp is there because the record layer stamped it for you.

```console
$ spget SIM:TEMPERATURE
SIM:TEMPERATURE 2026-08-04 09:46:31.420 22.5
```

`spget` renders that structure for humans; the example client prints it
whole. Both received exactly the same bytes.

**Nobody configured a port or an address.** The client broadcast the PV name
and the server answered. If that step fails, see the note on ports in
[Installation](install.md).

## Next

[Serving scalars](../03-progressive/scalars.md) — engineering units,
precision, limits, and the metadata that makes a PV readable.
