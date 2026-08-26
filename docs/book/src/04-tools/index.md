# Command-line tools

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

`spvirit-tools` ships thirteen binaries. They are named `sp*` rather than
`pv*` so they can sit alongside an EPICS Base installation without
shadowing `pvget`, `pvput`, and friends.

| Tool | Kind | What it does |
|---|---|---|
| [`spget`](spget.md) | client | read a PV once |
| [`spput`](spput.md) | client | write a PV |
| [`spmonitor`](spmonitor.md) | client | subscribe to changes |
| [`spinfo`](spinfo.md) | client | print a PV's type structure |
| [`splist`](splist.md) | client | discover servers and their PVs |
| [`spexplore`](spexplore.md) | client, TUI | browse a server interactively |
| [`spsearch`](spsearch.md) | client, TUI | passively watch PVA search traffic |
| [`spsine`](spsine.md) | client | drive a PV with a sine wave |
| [`spget_compare`](spget-compare.md) | offline | replay a captured GET frame |
| [`spserver`](spserver.md) | server | serve a `.db` file |
| [`sptable`](sptable.md) | server, TUI | an interactive spreadsheet IOC |
| [`spdodeca`](spdodeca.md) | server | serve a rotating wireframe as an image |
| [`spgateway`](spgateway.md) | client, server | p4p-compatible PVAccess gateway |

## Building them

The binaries are gated behind Cargo features, so a plain
`cargo build -p spvirit-tools` produces none of them:

```bash
cargo build -p spvirit-tools --all-features
```

| Feature | Tools it enables |
|---|---|
| `client` | `spget`, `spput`, `spmonitor`, `spinfo`, `splist`, `spsine`, `spget_compare` |
| `server` | `spserver`, `spdodeca` |
| `client` + `server` | `spgateway` |
| `client` + `tui` | `spexplore`, `spsearch` |
| `server` + `tui` | `sptable` |

## Flags every client shares

The seven `client` tools take the same connection options, because they
share one argument-parser helper. Rather than repeat the table on each
page, it is here:

| Flag | Meaning |
|---|---|
| `-w`, `--timeout` | timeout in seconds |
| `--server` | talk to `ip:port` directly, skipping search |
| `--search-addr` | search target IP; defaults to `EPICS_PVA_ADDR_LIST` or broadcast |
| `--bind-addr` | local IP to bind the search socket to |
| `--name-server` | PVA name server `host:port`; repeatable via `EPICS_PVA_NAME_SERVERS` |
| `--udp-port` | search port (default 5076) |
| `--tcp-port` | default server port (default 5075) |
| `--no-broadcast` | disable UDP broadcast/multicast search; same as `EPICS_PVA_AUTO_ADDR_LIST=NO` |
| `--authnz-user` | override the AuthNZ user sent at connect |
| `--authnz-host` | override the AuthNZ host sent at connect |
| `-F`, `--fields` | comma-separated dotted field paths to request; empty means all |
| `-d`, `--debug` | verbose protocol logging |

`spsearch` is the exception — it never opens a TCP channel, so it takes
only `--udp-port`, `--bind-addr`, and `--debug`.

## Environment variables

The standard EPICS PVAccess variables are honoured:
`EPICS_PVA_ADDR_LIST`, `EPICS_PVA_AUTO_ADDR_LIST`,
`EPICS_PVA_NAME_SERVERS`. Explicit flags win over the environment.

## A server to try them against

Every page below assumes something is serving. The quickest option:

```bash
cargo run -p spvirit-server --example complete_ioc
```

That is the [capstone IOC](../03-progressive/complete-ioc.md), which
publishes a scalar, a setpoint, a derived PV, and an array.
