# `spmonitor`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Subscribe to one or more PVs and print every update until interrupted.

```
spmonitor [OPTIONS] [PV ...]
```

Requires the `client` feature. Unlike `spget`, this one **does** take
several PV names.

## Flags beyond the shared set

| Flag | Meaning |
|---|---|
| `--raw` | print the raw hex payload |
| `--json` | print JSON instead of the default line format |
| `--pipeline N` | enable monitor pipelining with queue size `N` (0 = off) |

Pipelining lets the server send ahead without waiting for an acknowledgement
per update. It matters for high-rate PVs on a slow link; leave it off
otherwise.

## Output

```console
$ spmonitor VAC:PRESSURE
VAC:PRESSURE 2026-08-04 10:36:09.069   0
VAC:PRESSURE 2026-08-04 10:36:10.065   0
```

Same three columns as `spget`, one line per posted update. Ctrl-C to stop.

## Gotchas

**You see posts, not writes.** A record whose `MDEL` deadband swallows a
change posts nothing, so a monitor can look frozen while the value moves.
The capstone IOC scans at 2 Hz and posts at about 1 Hz for exactly this
reason. See [Monitors](../03-progressive/monitors.md).

**Alarm transitions always post,** regardless of the deadband
(`spvirit-server/src/simple_store.rs:535`). A severity change is never
suppressed.

**The first update is the current value.** A subscription delivers one
immediate post at connect, then only changes. Do not treat the first line
as an event.

## See also

[Monitoring changes](../03-progressive/monitors.md) for the library APIs
behind this tool.
