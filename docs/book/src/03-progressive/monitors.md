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

The callback is handed a `&MonitorUpdate`, not a bare value: `update.value`
is the decoded delta, and the update also carries which fields changed and
which the server dropped (see [Overruns](#overruns) below). It returns a
`ControlFlow`: `Continue(())` to keep going, `Break(())` to unsubscribe.
`pvmonitor` runs until the callback breaks or the connection drops.

`MonitorOptions::pipelined(q)` asks the server for flow control with a
queue depth of `q` — useful on high-rate PVs where a slow consumer would
otherwise fall behind.

## Python

```python
{{#include ../../../../spvirit-py/examples/demo_monitor.py:monitor}}
```

The Python callback receives a `spvirit.lowlevel.MonitorUpdate` too —
`update.value` is the decoded structure as a dict, so an NTScalar's number is
`update.value["value"]`.

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

## Overruns

Every update carries two bitsets: **changed**, the fields this delta actually
contains, and **overrun**, the fields for which the server dropped at least
one earlier update before sending this one. An overrun means you are seeing
the latest value but not every value. Raising the queue depth with
`MonitorOptions::pipelined(q)`, or doing less work in the callback, is the fix:
a pipelined subscription is delivered losslessly, so it is never conflated.

> **spvirit's own server does not populate the overrun bitset.** Under load it
> conflates a non-pipelined subscriber's queued updates down to the latest value
> per subscription — dropping the intermediate ones — and still sends an *empty*
> overrun bitset, so `has_overrun()` stays `false` even when frames were
> silently dropped. The bits remain meaningful against servers that do set them
> (a real EPICS IOC, or pvxs), and pipelining a spvirit subscription avoids the
> drops entirely. So treat the overrun API as "believe it when it fires," not as
> a promise that a quiet stream lost nothing.

In Rust, `update.changed` and `update.overrun` are the raw bitset bytes;
`changed_paths()` and `overrun_paths()` resolve them to dotted field names,
and `has_overrun()` is the cheap check:

```rust,ignore
let cb = |update: &MonitorUpdate| {
    if update.has_overrun() {
        eprintln!("dropped updates for: {}", update.overrun_paths().join(", "));
    }
    println!("{}", update.value);
    ControlFlow::Continue(())
};
```

In Python the resolution is already done: `.changed` and `.overrun` are lists
of dotted paths and `.has_overrun` is a property.

```python
def on_update(update):
    if update.has_overrun:
        print("dropped updates for:", update.overrun)
    print(update.value["value"])
    return True
```

The path `"<whole structure>"` appears when bit 0 is set — the server is
reporting the whole value rather than naming individual fields.

## From the command line

```console
$ spmonitor SIM:TEMPERATURE
```

When the server reports overruns — a real EPICS IOC or pvxs, not spvirit's own
server (see the note above) — `spmonitor` prints them to stderr, one line per
affected update, so they never contaminate the value stream on stdout:

```console
SOME:IOC:PV: overrun on value, alarm.severity
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

`demo_scan.py` updates ten times a second, so terminal 2 fills immediately:

```console
$ spmonitor SIM:TEMPERATURE
SIM:TEMPERATURE 2026-08-06 09:25:29.221 21.677613
SIM:TEMPERATURE 21.734644
SIM:TEMPERATURE 21.8092
SIM:TEMPERATURE 21.89275
SIM:TEMPERATURE 21.956506
SIM:TEMPERATURE 22.051046
SIM:TEMPERATURE 22.151852
SIM:TEMPERATURE 22.240413
SIM:TEMPERATURE 2026-08-06 09:25:30.028 22.347997
SIM:TEMPERATURE 22.440441
SIM:TEMPERATURE 22.534399
...
```

`spmonitor` prints the timestamp only on the first update of each
wall-clock second — that is a display convenience, not a change in the
data. Every update carries a full timestamp on the wire. Counting the
lines between two timestamps is a quick way to see your actual update rate.

Stop it with `Ctrl-C`.

## Next

[Discovery and introspection](discovery.md).
