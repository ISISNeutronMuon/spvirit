# Python API

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Every progressive example in [Part III](../03-progressive/scalars.md) shows
Rust and Python side by side, so the tutorial route is the one to take
first. This page is the orientation map: what the module contains, and where
the full reference lives.

The complete API reference is
[`spvirit-py/README.md`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/README.md)
— around a thousand lines covering every class, method and keyword argument.
It is not duplicated here; this page tells you which part of it you want.
Every object also carries a docstring, so `help(spvirit.ai)` and
`help(spvirit.Server)` work at the interpreter prompt.

For the **Rust** side there is generated reference documentation on docs.rs:
[types](https://docs.rs/spvirit-types/latest/spvirit_types/) ·
[codec](https://docs.rs/spvirit-codec/latest/spvirit_codec/) ·
[client](https://docs.rs/spvirit-client/latest/spvirit_client/) ·
[server](https://docs.rs/spvirit-server/latest/spvirit_server/) ·
[tools](https://docs.rs/spvirit-tools/latest/spvirit_tools/) ·
[calc](https://docs.rs/spvirit-calc/latest/spvirit_calc/). The Python
module is a thin layer over `spvirit-client` and `spvirit-server`, so when a
Python docstring is terse the Rust page for the same call is often the
fuller answer. See the [crate map](crate-map.md).

## Install

```bash
pip install spvirit
```

A compiled PyO3 extension, not a pure-Python package. Wheels ship for the
usual platforms; building from source needs a Rust toolchain. See
[Installation](../02-getting-started/install.md).

## The four layers

| Layer | Import | What it gives you |
|---|---|---|
| Typed PV handles | `spvirit.ai`, `spvirit.ao`, … | IOC-style records you keep a handle to. The one to start with. |
| Server + store | `spvirit.Server` | Runs the PVAccess server; `store` reaches records by name. |
| Client | `spvirit.get`, `spvirit.put`, `spvirit.monitor`, `spvirit.Channel` | Reading and writing other people's PVs. |
| Low level | `spvirit.lowlevel`, `spvirit.codec` | Raw frames and wire encoding, for proxies and analysers. |

## Constructors

The module-level PV constructors mirror the Rust handle API one for one:

```
ai  ao  bi  bo  string_in  string_out  longin  longout
mbbi  mbbo  waveform  aai  aao  calc  pv  scalar
```

`pv` and `scalar` are the generic forms — you pass the type explicitly rather
than getting it from the constructor name. The full type-coverage table (which
NT scalar types each constructor accepts) is in the
[README's *NT scalar type coverage* section](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/README.md#nt-scalar-type-coverage).

## Sync and async

Most operations come in both flavours: `set`/`set_async`, `get`/`get_async`,
`connect`/`connect_async` and so on. The sync forms release the GIL while
they block, so they are safe to call from a thread. The README's
*Threading and async model* section is the one to read before you mix them.

## The rule that catches everyone

**Attach callbacks before starting the server.** `on_put`, `scan` and `calc`
must be registered on a handle while it is still unbound. Once
`Server.start()` has run, the handle is bound and a late `on_put` will not
fire. The same rule holds in Rust, but Python makes it easier to trip over
because the server object is mutable and the failure is silent. See
[Reacting to writes](../03-progressive/reacting-to-writes.md) and
[Troubleshooting](troubleshooting.md).

## Examples

`spvirit-py/examples/` holds around thirty runnable scripts — one concept
each. The ones the book's chapters use directly:

| Script | Chapter |
|---|---|
| `demo_first_pv.py` | [Your first PV](../02-getting-started/first-pv.md) |
| `demo_scalars.py` | [Serving scalars](../03-progressive/scalars.md) |
| `demo_get.py`, `demo_put.py` | [Reading and writing](../03-progressive/read-write.md) |
| `demo_monitor.py` | [Monitoring changes](../03-progressive/monitors.md) |
| `demo_on_put.py` | [Reacting to writes](../03-progressive/reacting-to-writes.md) |
| `demo_scan.py`, `demo_calc.py` | [Simulating a device](../03-progressive/simulating.md) |
| `demo_waveform.py` | [Arrays and waveforms](../03-progressive/arrays.md) |
| `demo_enums.py` | [Enums and binary records](../03-progressive/enums.md) |
| `demo_alarms.py` | [Alarms and severity](../03-progressive/alarms.md) |
| `demo_table.py` | [Tables and images](../03-progressive/tables-and-images.md) |

For [Custom data sources](../03-progressive/sources.md) the Python route is
the `demo_source_*.py` family — `sensor`, `async`, `multi`, `passthrough`,
`aggregate`, `rpc`, `wildcard`. The rest of the directory — gateways, stress
tests, wire inspectors, the 10 000-PV farm — is listed in the README's
*Examples* section.

## Internals

How the bindings are put together, and why they are sync-first, is in
[Python Bindings](../06-dev-guide/05-python-bindings.md) in the developer
guide.
