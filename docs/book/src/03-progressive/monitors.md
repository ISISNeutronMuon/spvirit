# Monitoring changes

<!-- verify:begin -->
> ✅ **Verified** · [`pvmonitor.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-client/examples/pvmonitor.rs) · [`demo_monitor.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_monitor.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

A subscription: one request, then updates pushed by the server for as long
as you care to listen. This is what PVAccess is actually for — polling a PV
in a loop is almost always the wrong answer.

## Rust

```rust
{{#include ../../../../spvirit-client/examples/pvmonitor.rs:core}}
```

The callback returns a `ControlFlow`: `Continue(())` to keep going,
`Break(())` to unsubscribe. `pvmonitor` runs until the callback breaks or
the connection drops.

`MonitorOptions::pipelined(q)` asks the server for flow control with a
queue depth of `q` — useful on high-rate PVs where a slow consumer would
otherwise fall behind.

## Python

```python
{{#include ../../../../spvirit-py/examples/demo_monitor.py:monitor}}
```

`client.monitor(...)` **blocks** until the callback returns `False` or
raises. For a non-blocking version use `client.subscribe(...)`, which
returns a `Subscription` you can `close()`, and which runs the callback on
a background thread:

```python
sub = client.subscribe("SIM:TEMPERATURE", on_update)
...
sub.close()
```

If a subscription ends on a network error, `sub.error` holds the message
and `sub.is_active` becomes `False` — worth checking, because a silently
dead subscription looks exactly like a quiet PV.

## From the command line

```console
$ spmonitor SIM:TEMPERATURE
```

## What to notice

**The first update is the current value.** A subscription delivers the
present state immediately, then changes. You do not need a GET before a
monitor.

**Monitors respect the record's `MDEL`.** This is the single most useful
thing on this page. A record with `mdel=1.0` posts an update only when the
value has moved at least 1.0 from the **last posted** value — not from the
last set value. Writing 0.1, 0.2, 0.3, 5.0, 5.1, 5.2, 20.0 to such a PV
delivers three updates:

```text
posted to monitor: [0.0, 5.0, 20.0]
```

`0.0` is the initial value on subscribe; `5.0` and `20.0` each cleared the
deadband. The intermediate writes landed in the record — a GET would show
`5.2` — they just were not broadcast.

**`MDEL` defaults to 0, meaning no deadband.** A record you never gave an
`mdel` posts every change. On a 1 kHz PV with a hundred subscribers, that
is a decision, so make it deliberately.

**Severity changes always get through.** The deadband is bypassed when the
alarm severity changes, so a PV crossing into MAJOR is never silently
swallowed by a large `MDEL`.

**`ADEL` is not the same thing.** `ADEL` is the archive deadband; it is
parsed, stored, and served over field access, but PVAccess monitors use
`MDEL`.

**Deadbands are a record-level feature.** A raw-NT source posting with
`put_nt` has no `MDEL` to consult, and every post goes out. See
[Records vs raw NT](../01-fundamentals/records-vs-raw-nt.md).

## Run it

```bash
# Terminal 1
python spvirit-py/examples/demo_scan.py

# Terminal 2
cargo run -p spvirit-client --example pvmonitor -- SIM:TEMPERATURE
# or
python spvirit-py/examples/demo_monitor.py
# or
spmonitor SIM:TEMPERATURE
```

## Next

[Reacting to writes](reacting-to-writes.md).
