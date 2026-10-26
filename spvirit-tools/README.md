# spvirit-tools

Command-line tools for EPICS PVAccess, built on the pure-Rust
[spvirit](https://github.com/ISISNeutronMuon/spvirit) stack. No EPICS base
installation required.

## Install

```sh
pip install spvirit-tools
```

The tools also arrive as a dependency of the Python bindings, so
`pip install spvirit` puts all of them on your PATH as well.

With a Rust toolchain:

```sh
cargo install spvirit-tools
```

## Tools

Thirteen binaries ship in the default build; the pip wheel has those plus
the two packet-capture tools below. They are named `sp*` rather than `pv*` so they can sit
alongside an EPICS Base installation without shadowing `pvget`, `pvput`, and
friends.

| Tool | Kind | What it does |
| --- | --- | --- |
| `spget` | client | read a PV once |
| `spput` | client | write a PV |
| `spmonitor` | client | subscribe to changes |
| `spinfo` | client | print a PV's type structure |
| `splist` | client | discover servers and their PVs |
| `spexplore` | client, TUI | browse a server interactively |
| `spsearch` | client, TUI | passively watch PVA search traffic |
| `spsine` | client | drive a PV with a sine wave |
| `spget_compare` | offline | compare a captured GET exchange against spvirit's encoders |
| `spserver` | server | serve a `.db` file |
| `sptable` | server, TUI | an interactive spreadsheet IOC |
| `spdodeca` | server | serve a rotating wireframe as an image |
| `spgateway` | client, server | p4p-compatible PVAccess gateway |

`spload` is an internal load driver used by the `loadtest/` harness. It is
not a user-facing tool and is not in the wheel or a default build
(`--features load` builds it).

Each accepts `--help`.

### Packet capture: `spvirit-scry`

Two more binaries, folded in from the former `spvirit-scry` crate, capture
and decode live PVAccess traffic off a network interface:

| Tool | Kind | What it does |
| --- | --- | --- |
| `spvirit-scry` | capture, TUI | passively capture and decode PVA traffic |
| `spvirit-scry_influx_collector` | capture | the same capture, exported to InfluxDB and Loki |

Both are in the pip wheel. On Windows they need the
[Npcap](https://npcap.com/#download) runtime, which the wheel does not
bundle (licence); without it they print a warning and exit, and every other
tool works as usual.

From source they are behind the optional `scry` feature (not in a default
build), because they link against libpcap (Linux, `libpcap-dev`) or the
Npcap SDK (Windows, found automatically or via `NPCAP_SDK`):

```sh
cargo install spvirit-tools --features scry
```

Capturing needs `sudo`/`CAP_NET_RAW` on Linux or administrator rights on
Windows. The book's
[spvirit-scry chapter](https://isisneutronmuon.github.io/spvirit/04-tools/spvirit-scry.html)
covers flags, source fingerprinting and the collector's configuration.

## Documentation

The [spvirit book](https://isisneutronmuon.github.io/spvirit/) covers the tools
in detail, along with the client and server libraries. The
[root README](https://github.com/ISISNeutronMuon/spvirit#readme) is the shortest
route in.

## License

BSD-3-Clause.
