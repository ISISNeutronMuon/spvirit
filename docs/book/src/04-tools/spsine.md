# `spsine`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Drive a PV with a sine wave by writing to it at a fixed rate. A load
generator and a "make something move" button, not part of any IOC.

```
spsine [OPTIONS] [PV]
```

Requires the `client` feature. It is a *client* — the PV must already
exist on a server, and must be writable.

## Flags beyond the shared set

| Flag | Default | Meaning |
|---|---|---|
| `--freq HZ` | 1 | sine frequency |
| `--rate N` | 10 | writes per second |
| `--amp A` | 1 | amplitude |
| `--offset O` | 0 | vertical offset |
| `--phase RAD` | 0 | phase in radians |
| `--duration SECS` | 0 | run time; 0 means forever |

The value written is `offset + amp * sin(2π · freq · t + phase)`.

## Running it

```console
$ spsine DEMO:SETPOINT --freq 0.5 --rate 2 --amp 10 --offset 20 --duration 3
$ spget DEMO:SETPOINT
DEMO:SETPOINT 2026-08-04 10:40:21.801 29.993539
```

It prints nothing. Silence is success — watch the PV with
[`spmonitor`](spmonitor.md) in another terminal if you want to see it
move.

## Gotchas

**`--rate` is the write rate, `--freq` is the waveform.** Set the rate to
at least a few times the frequency or you will sample the sine into
something that does not look like one.

**Each write is a full PUT.** `spsine` uses the same client path as
`spput`, so a server-side `on_put` callback fires on every sample. At
`--rate 100` that is a hundred callbacks a second — which is precisely
what makes this useful for load testing, and a nuisance if you forgot.

**It writes; it does not serve.** If nothing answers the search you get a
timeout. To *generate* a moving PV rather than drive an existing one, use
a scanned record ([Simulating a device](../03-progressive/simulating.md))
or [`sptable`](sptable.md)'s `:anim` command.
