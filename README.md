# Spvirit

[![crates.io (spvirit-types)](https://img.shields.io/crates/v/spvirit-types?label=spvirit-types)](https://crates.io/crates/spvirit-types)
[![crates.io (spvirit-codec)](https://img.shields.io/crates/v/spvirit-codec?label=spvirit-codec)](https://crates.io/crates/spvirit-codec)
[![crates.io (spvirit-client)](https://img.shields.io/crates/v/spvirit-client?label=spvirit-client)](https://crates.io/crates/spvirit-client)
[![crates.io (spvirit-server)](https://img.shields.io/crates/v/spvirit-server?label=spvirit-server)](https://crates.io/crates/spvirit-server)
[![crates.io (spvirit-tools)](https://img.shields.io/crates/v/spvirit-tools?label=spvirit-tools)](https://crates.io/crates/spvirit-tools)
[![License](https://img.shields.io/crates/l/spvirit-types)](LICENSE)

*/ˈspɪrɪt/ of the Machine*

Spvirit is a pure-Rust implementation of the EPICS **PVAccess** protocol —
client, server, wire codec, command-line tools, and Python bindings — with no
dependency on an EPICS base installation. It is not yet production ready, but
it is available for anyone to use and contribute to.

📖 **[Read the documentation](https://isisneutronmuon.github.io/spvirit/)** —
fundamentals, progressive examples in Rust and Python, and a page per CLI tool.

Key areas of development in the near future include:
- Expanding `spvirit-server` with more complete softIOC behaviours and record processing.
- TLS support and structured put payloads in the client.
- Segmentation emission in the encoder, so large values (NTNDArray images) can
  be sent as segmented messages rather than one oversized frame.

## Why Rust?

Because why not, admittedly I just wanted to learn Rust and this seemed like a
fun project with a moderately useful outcome.

## The crates

| Crate | What it is |
|---|---|
| `spvirit-types` | Shared data model for PVAccess Normative Types (NT). |
| `spvirit-codec` | PVAccess protocol encode/decode and connection state tracking. |
| `spvirit-client` | Client library — search, connect, get, put, monitor. |
| `spvirit-server` | Server library — `.db` parsing, `Source` trait, PVAccess server runtime. |
| `spvirit-tools` | The `sp*` command-line tools. |
| `spvirit-py` | Python bindings via PyO3 — client and server APIs from Python. |

`spvirit-client` and `spvirit-server` do not depend on each other. The full
layering, versions and feature flags are in the
[crate map](https://isisneutronmuon.github.io/spvirit/05-reference/crate-map.html).

## Hello, PV

Three records, served over PVAccess.

```rust
use spvirit_server::PvaServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = PvaServer::builder()
        .ai("SIM:TEMPERATURE", 22.5)   // input  — read-only to clients
        .ao("SIM:SETPOINT", 25.0)      // output — clients may write
        .bo("SIM:ENABLE", false)
        .build();
    server.run().await?;
    Ok(())
}
```

```python
import spvirit

temp = spvirit.ai("SIM:TEMPERATURE", 22.5)
setpoint = spvirit.ao("SIM:SETPOINT", 25.0)
enable = spvirit.bo("SIM:ENABLE", False)

spvirit.Server(pvs=[temp, setpoint, enable]).run()
```

Then, from another terminal, `spget SIM:TEMPERATURE`.

Walked through step by step in
[Your first PV](https://isisneutronmuon.github.io/spvirit/02-getting-started/first-pv.html).

## Install

```bash
pip install spvirit             # the Python module, plus the sp* tools
pip install spvirit-tools       # just the sp* command-line tools
cargo install spvirit-tools     # same tools, via a Rust toolchain
```

`spvirit` depends on `spvirit-tools`, so the first line gets you both: the
importable module and the twelve binaries on your `PATH`, no compiler needed.

To use the libraries from your own Rust project, add the layer you need:

```toml
[dependencies]
spvirit-client = "0.1"
spvirit-server = "0.1"
```

Building from source, and the feature flags that gate the binaries, are covered
in [Installation](https://isisneutronmuon.github.io/spvirit/02-getting-started/install.html).

## Tools

| spvirit tool | EPICS Base equivalent | Description |
|---|---|---|
| `spget` | `pvget` | Fetch the current value of a PV |
| `spput` | `pvput` | Write a value to a PV |
| `spmonitor` | `pvmonitor` | Subscribe to a PV and print value changes |
| `spinfo` | `pvinfo` | Display field/metadata information for a PV |
| `splist` | `pvlist` | List all available PVs on discovered servers |
| `spserver` | `softIoc` | Not fully one-to-one — a demo, but it parses some `.db` vocabulary |
| `spgateway` | `pvagw` | PVAccess gateway — proxy PVs between networks from a p4p-schema config |
| `sptable` | | Interactive TUI IOC — build and drive records live |
| `spexplore` | | Interactive TUI to browse servers, select PVs, and monitor values |
| `spsearch` | | TUI showing PV search network traffic for diagnostics |
| `spsine` | | Continuously write a sine wave to a PV (demo/testing) |
| `spget_compare` | | Compare `pvget` results between spvirit and EPICS Base |
| `spdodeca` | | Server publishing a rotating 3D dodecahedron as an NTNDArray PV |

A page per tool, with real captured output, is at
[Command-line tools](https://isisneutronmuon.github.io/spvirit/04-tools/index.html).

## Examples

The repository carries runnable examples for every concept — nearly thirty
Rust examples under `spvirit-{codec,client,server}/examples/` and over thirty
Python scripts under `spvirit-py/examples/`. Each is walked through in
[Progressive examples](https://isisneutronmuon.github.io/spvirit/03-progressive/scalars.html),
Rust and Python side by side.

```bash
cargo run -p spvirit-server --example simple_server   # terminal 1
spget SIM:TEMPERATURE                                  # terminal 2
```

## Integration test matrix

I have tested the tools in this repo against the following EPICS PVAccess servers:
- EPICS
- p4p (pvxs under the hood)
- PvAccessJava

## Contributing

Internals — the codec, the record model, the server runtime, the bindings —
are documented with file-and-line citations in the
[developer guide](https://isisneutronmuon.github.io/spvirit/06-dev-guide/index.html).
Known divergences from EPICS Base behaviour are collected in
[Known gaps](https://isisneutronmuon.github.io/spvirit/05-reference/known-gaps.html).

## Related Projects

- [spvirit-scry](https://crates.io/crates/spvirit-scry) — A Rust tool for capturing and analyzing pvAccess EPICS packets.

## References

I used the following libraries and repos as refernce materials for PVAccess protocol:

- [pvxs](https://epics-base.github.io/pvxs/)
- [pvAccess Protocol Specification](https://docs.epics-controls.org/en/latest/pv-access/protocol.html)
- [EPICS Base](https://github.com/epics-base/epics-base)
- [PVAshark](https://github.com/george-mcintyre/pvashark)

## GenAI Usage Log

| Section / Area | What Was Done With AI | Plans Ahead |
|---|---|---|
| `spvirit-types` | Hand coded, few types completed with AI, the prettified with AI | keep the same, fairly complete |
| `spvirit-codec` | Most was hand-coded, some restructuring and prettifying was done with AI.  | keep the same, bring in any common helpers, maybe write a siplified API for users |
| `spvirit-tools` | Mostly AI generated, manually coded parts of Put and Get then let the Agents build on top. Client and server logic has been split out into `spvirit-client` and `spvirit-server` crates. | The APIs are now split idiomatically. Continued refinement of high-level convenience functions for put and monitor. |
| `PvaClient` / `PvaServer` | High-level builder-pattern APIs (`PvaClient::builder()`, `PvaServer::builder()`) designed with AI assistance. Wraps protocol-level operations into ergonomic one-liners for get, put, monitor, info, and typed server records. | Extend with more record types, structured put payloads, and TLS support. |
| Testing | I wrote some basic tests, then used GenAI agents to generate more tests and test cases, which I then manually curated and edited. | Suite is fairly comprehensive so I will keep it as is. |
| Documentation | The [documentation site](https://isisneutronmuon.github.io/spvirit/) was drafted with AI, with every code sample included verbatim from a compiled example and every claim checked against the source. | Keep it verified by CI — `cargo test -p spvirit-tools --test docs_verify`. |

## Licence

Licensed under the terms in [LICENSE](LICENSE).
