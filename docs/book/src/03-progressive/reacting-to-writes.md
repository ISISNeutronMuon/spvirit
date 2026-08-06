# Reacting to writes

<!-- verify:begin -->
> ✅ **Verified** · [`on_put.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/on_put.rs) · [`on_put_reject.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/on_put_reject.rs) · [`demo_on_put.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_on_put.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

A writable PV that runs your code when a client writes to it — and, in the
form that supports it, refuses writes it does not like.

## Rust

```rust
{{#include ../../../../spvirit-server/examples/on_put.rs:callback}}
```

That is the **builder** form, and it is worth reading its signature
carefully:

```rust
Fn(&str, &DecodedValue)
```

It returns `()`. It runs *after* the value has been applied, and it has no
way to say no. It is a notification hook, not a validator.

To reject a write, use the **typed handle** form instead:

```rust
Fn(&Pv<T>, T) -> Result<(), String>
```

```rust
{{#include ../../../../spvirit-server/examples/on_put_reject.rs:reject}}
```

`Err(msg)` rejects the PUT on the wire and the client's `put` fails:

```console
$ spput SIM:SETPOINT 500
SIM:SETPOINT ERROR protocol error: PUT failed: SIM:SETPOINT outside 0..100: 500

$ spget SIM:SETPOINT
SIM:SETPOINT 2026-08-04 10:08:05.123  30
```

The rejected value never reached the record.
`Ok(())` accepts it. You also get the value already converted to `T`
instead of a raw `DecodedValue`.

The two forms are not interchangeable, and the builder form's inability to
reject is the single most common surprise in this API. If you are
validating, use handles.

## Python

```python
{{#include ../../../../spvirit-py/examples/demo_on_put.py:callback}}
```

Python has one form and it can reject: returning `False` — or raising —
rejects the PUT, and anything else accepts it. The callback runs **before**
the value is applied.

```console
$ spput SIM:SETPOINT 30
SIM:SETPOINT OK

$ spput SIM:SETPOINT 500
SIM:SETPOINT ERROR protocol error: PUT failed: rejected by on_put
```

## What to notice

**Attach callbacks before serving.** `on_put`, `scan`, and `calc` must be
attached to a PV before it is handed to `Server(...)` / `PvaServer::serve`.
Attaching afterwards is a **silent no-op** — the core logs a warning and
carries on. Nothing raises, nothing fails; your callback simply never runs.
This is true in both languages.

**Validation is the only enforcement you get.** Drive limits are advisory
(see [Serving scalars](scalars.md)), so `on_put` is where range checking
actually happens. If a PV must not accept 500, write that rule here.

**The callback is not a place to block.** It runs on the server's runtime.
Long work belongs in a task you spawn from it.

**A single client write can invoke your callback more than once.** `spput`
tries the full PUT flow first and silently falls back to the simple flow if
that fails, so a *rejected* write arrives at the server twice and your
callback logs twice:

```console
$ spput SIM:SETPOINT 700
SIM:SETPOINT ERROR protocol error: PUT failed: rejected by on_put

# server log
SIM:SETPOINT was set to 700.0
SIM:SETPOINT was set to 700.0
```

Accepted writes run once; only the retry path doubles up. Pass
`--no-flow-fallback` to suppress it. The general rule holds regardless of
client: **`on_put` callbacks should be idempotent**, and side effects that
must happen exactly once do not belong in one.

**Array PVs do not support `on_put` or `scan` in Python.** Calling either
on an array PV raises `TypeError`. Drive arrays with `pv.set(...)` from
your own loop instead — see [Arrays and waveforms](arrays.md).

## Run it

```bash
# Terminal 1
cargo run -p spvirit-server --example on_put
# or: python spvirit-py/examples/demo_on_put.py

# Terminal 2
spput SIM:SETPOINT 30
spput SIM:SETPOINT 500
```

`on_put` only *observes* — both writes succeed, and the interesting output
is in terminal 1:

```console
$ spput SIM:SETPOINT 30
SIM:SETPOINT OK

$ spput SIM:SETPOINT 500
SIM:SETPOINT OK
```

```console
# terminal 1
SIM:SETPOINT was set to Structure([("value", Float64(30.0))])
SIM:SETPOINT was set to Structure([("value", Float64(500.0))])
```

The callback receives the whole submitted structure, not a bare number —
a client may write `value` alone or several fields at once.

Now run `on_put_reject` instead, which returns an error for out-of-range
values:

```bash
cargo run -p spvirit-server --example on_put_reject
```

```console
$ spput SIM:SETPOINT 30
SIM:SETPOINT OK

$ spput SIM:SETPOINT 500
SIM:SETPOINT ERROR protocol error: PUT failed: SIM:SETPOINT outside 0..100: 500
Error: Protocol("PUT failed: SIM:SETPOINT outside 0..100: 500")
```

Your message crosses the wire verbatim, so write it for whoever is holding
the terminal. `spput` also exits non-zero, which is why it prints that
second `Error:` line — useful in a script.

Terminal 1 logs only the write it let through:

```console
SIM:SETPOINT accepted 30
```

## Next

[Simulating a device](simulating.md).
