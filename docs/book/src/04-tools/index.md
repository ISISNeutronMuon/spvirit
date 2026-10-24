# Command-line tools

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

`spvirit-tools` ships sixteen binaries: the fifteen below plus `spload`, an
internal load driver for the `loadtest/` harness that is not documented here.
Thirteen are in the default build and the Python wheel; the two `scry` tools
and `spload` are opt-in features. They are named `sp*` rather than
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
| [`spget_compare`](spget-compare.md) | offline | compare a captured GET exchange against spvirit's encoders |
| [`spserver`](spserver.md) | server | serve a `.db` file |
| [`sptable`](sptable.md) | server, TUI | an interactive spreadsheet IOC |
| [`spdodeca`](spdodeca.md) | server | serve a rotating wireframe as an image |
| [`spgateway`](spgateway.md) | client, server | p4p-compatible PVAccess gateway |
| [`spvirit-scry`](spvirit-scry.md) | capture, TUI | passively capture and decode PVA traffic |
| [`spvirit-scry_influx_collector`](spvirit-scry.md) | capture | the same capture, exported to InfluxDB and Loki |

The last two need the `scry` feature and libpcap (Linux) or the Npcap SDK
(Windows); they are not part of the Python wheel.

## Building them

The binaries are gated behind Cargo features. A plain
`cargo build -p spvirit-tools` uses the default features (`client`, `server`,
`tui`) and builds the thirteen default tools; ask for everything with:

```bash
cargo build -p spvirit-tools --all-features
```

`--all-features` includes `scry`, so it needs libpcap or the Npcap SDK
(see [spvirit-scry](spvirit-scry.md)). Without them, the default features
build every tool except the `scry` pair and `spload`.

| Feature | Tools it enables |
|---|---|
| `client` | `spget`, `spput`, `spmonitor`, `spinfo`, `splist`, `spsine`, `spget_compare` |
| `server` | `spserver`, `spdodeca` |
| `client` + `server` | `spgateway` |
| `client` + `tui` | `spexplore`, `spsearch` |
| `server` + `tui` | `sptable` |
| `scry` (needs libpcap / Npcap SDK) | `spvirit-scry`, `spvirit-scry_influx_collector` |
| `load` (not in the default build) | `spload` |

## Flags every client shares

The client tools that query a server (`spget`, `spput`, `spmonitor`,
`spinfo`, `splist`, `spsine`, `spexplore`, and `spload`) share one
connection-option helper. (`spget_compare` reads a file and takes none of
them.) Rather than repeat the table on each page, it is here:

| Flag | Meaning |
|---|---|
| `-w`, `--timeout` | timeout in seconds |
| `--server` | talk to `ip:port` directly, skipping search |
| `--search-addr` | search target IP. If set, it is the only search target. If not set, the targets are the `EPICS_PVA_ADDR_LIST` entries plus the auto-broadcast addresses (unless `EPICS_PVA_AUTO_ADDR_LIST=NO`). |
| `--bind-addr` | local IP to bind the search socket to |
| `--name-server` | PVA name server `host:port`. One value on the command line; `EPICS_PVA_NAME_SERVERS` may hold several, and its entries are added to the flag value. |
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
`EPICS_PVA_NAME_SERVERS`. Explicit `--search-addr` and `--bind-addr`
replace the environment value. `--name-server` is added to
`EPICS_PVA_NAME_SERVERS`, not substituted. `--no-broadcast` can only disable
broadcast.

## A server to try them against

Every page below assumes something is serving. The quickest option:

```bash
cargo run -p spvirit-server --example complete_ioc
```

That is the [capstone IOC](../03-progressive/complete-ioc.md), which
publishes a scalar, a setpoint, a derived PV, and an array.
