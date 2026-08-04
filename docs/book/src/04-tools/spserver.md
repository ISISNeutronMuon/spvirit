# `spserver`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Serve an EPICS database file over PVAccess. No code, no build step — the
soft-IOC equivalent of `softIoc -d my.db`.

```
spserver [OPTIONS]
```

Requires the `server` feature.

## Flags

| Flag | Default | Meaning |
|---|---|---|
| `--db-file PATH` | — | EPICS `.db` file to load |
| `--listen-addr ADDR` | `0.0.0.0` | address to bind |
| `--tcp-port PORT` | 5075 | PVA TCP port |
| `--udp-port PORT` | 5076 | PVA search port |
| `--reload-interval SECS` | 2 | how often to re-read the `.db` file |
| `--advertise-addr ADDR` | — | address to put in search responses |
| `--beacon-period SECS` | — | beacon interval |
| `--beacon-addr IP:PORT` | — | beacon target |
| `--conn-timeout SECS` | — | idle connection timeout |
| `--compute-alarms` | off | derive severity from `LOW`/`HIGH`/`LOLO`/`HIHI` |
| `--pvlist-mode MODE` | `list` | `off`, `discover`, or `list` |
| `--pvlist-max N` | 1024 | cap on names returned by a listing |
| `--pvlist-allow-pattern RE` | — | regex filter on names exposed by a listing |
| `--debug` | off | verbose protocol logging |

## Running it

```console
$ spserver --db-file spvirit-server/examples/example.db --compute-alarms
INFO spserver: Loaded DB file 'spvirit-server/examples/example.db' with 4 PVs
INFO spserver: Starting PVA server: udp=0.0.0.0:5076 tcp=0.0.0.0:5075
     reload=2s pvlist_mode=List pvlist_max=1024 filter=<none>
```

Every connection and operation is logged:

```text
INFO spserver: TCP connection 1 from 127.0.0.1:52125
INFO spserver: Conn 1: channel 'DEMO:SETPOINT' cid=1 sid=2
INFO spserver: Conn 1: put init pv='DEMO:SETPOINT' ioid=1
```

That startup line is worth reading. It tells you the effective ports and
listing policy, which is faster than guessing when a client cannot find
anything.

## `--compute-alarms`

Off by default. With it on, `LOW`/`HIGH` produce MINOR and `LOLO`/`HIHI`
produce MAJOR on every write:

```console
$ spput DEMO:SETPOINT 46 && spget DEMO:SETPOINT
DEMO:SETPOINT  46 MAJOR READ HIHI
```

This is currently the *only* route to computed alarms — limits set through
the `Pv` handle API are published but never evaluated. See
[Alarms](../03-progressive/alarms.md).

## Reload

The `.db` file is re-read every `--reload-interval` seconds, so editing it
in place changes the served records without a restart. Set the interval
higher on a slow filesystem.

## Listing policy

`--pvlist-mode` controls whether clients may enumerate:

| Mode | Search | `splist <target>` |
|---|---|---|
| `list` (default) | answered | full list |
| `discover` | answered | refused, "RPC list endpoint disabled" |
| `off` | answered | refused |

Note that `discover` and `off` do not hide the server — a broadcast search
still finds it and named PVs still read and write. They only suppress
enumeration.

## See also

[Serving a `.db` file](../03-progressive/db-files.md) covers the database
syntax and which fields spvirit acts on.
