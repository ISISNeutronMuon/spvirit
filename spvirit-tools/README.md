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

Twelve binaries. They are named `sp*` rather than `pv*` so they can sit
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
| `spget_compare` | offline | replay a captured GET frame |
| `spserver` | server | serve a `.db` file |
| `sptable` | server, TUI | an interactive spreadsheet IOC |
| `spdodeca` | server | serve a rotating wireframe as an image |

Each accepts `--help`.

## Documentation

The [spvirit book](https://isisneutronmuon.github.io/spvirit/) covers the tools
in detail, along with the client and server libraries. The
[root README](https://github.com/ISISNeutronMuon/spvirit#readme) is the shortest
route in.

## License

BSD-3-Clause.
