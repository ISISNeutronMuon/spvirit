# Spvirit

Spvirit is a pure-Rust implementation of the EPICS **PVAccess** protocol —
client, server, wire codec, command-line tools, and Python bindings — with no
dependency on an EPICS base installation.

## Quickstart

Install. This one command brings the Python module *and* the twelve `sp*`
command-line tools; there is no compiler and no Rust toolchain involved on
Linux, macOS, or Windows x86_64.

```bash
pip install spvirit
```

Save this as `first.py` — it is a complete soft IOC serving three PVs:

```python
{{#include ../../../spvirit-py/examples/demo_first_pv.py:serve}}
```

Run it, and read a PV from a second terminal:

```bash
python first.py            # terminal 1 — it blocks, serving
```

```console
$ spget SIM:TEMPERATURE
SIM:TEMPERATURE 2026-08-06 09:38:00.729 22.5

$ spput SIM:SETPOINT 30
SIM:SETPOINT OK

$ spget SIM:SETPOINT
SIM:SETPOINT 2026-08-06 09:38:00.729  30
```

If you see those three lines you have a working PVAccess server. Nobody
configured a port, an address, or a permission: the client broadcast the PV
name, the server answered, and `SIM:TEMPERATURE` refused the write it should
refuse because `ai` is an input record.

The same thing in Rust is
[Your first PV](02-getting-started/first-pv.md); every chapter of this book
shows both languages.

## Where to go next

- Never met EPICS? → [EPICS in 10 minutes](01-fundamentals/epics-in-10-minutes.md)
- Know EPICS, want a soft IOC? → [Your first PV](02-getting-started/first-pv.md)
- Want the whole tour? → [Serving scalars](03-progressive/scalars.md) and the
  twelve chapters after it
- Looking up a function? → **[API reference](#api-reference)** below

## How the site is laid out

- **[Fundamentals](01-fundamentals/what-is-spvirit.md)** — what EPICS is, what
  Normative Types are, and the one distinction worth understanding before you
  write anything: [records vs raw NT](01-fundamentals/records-vs-raw-nt.md).
- **[Getting started](02-getting-started/install.md)** — install, then a PV
  served and read in under a page.
- **[Progressive examples](03-progressive/scalars.md)** — thirteen chapters,
  Rust and Python side by side, ending in
  [a complete IOC](03-progressive/complete-ioc.md).
- **[Command-line tools](04-tools/index.md)** — a page per `sp*` binary, with
  real captured output.
- **[Reference](05-reference/crate-map.md)** — the crate map, the
  [record-type matrix](05-reference/record-types.md), the
  [Python API](05-reference/python-api.md),
  [troubleshooting](05-reference/troubleshooting.md), and the
  [known gaps](05-reference/known-gaps.md).
- **[Developer guide](06-dev-guide/index.md)** — internals, with
  file-and-line citations, for people changing spvirit itself.

## API reference

This book is the tutorial. The exhaustive, generated API documentation lives
on docs.rs, one page per crate:

| Crate | API docs |
|---|---|
| `spvirit-types` | [docs.rs/spvirit-types](https://docs.rs/spvirit-types/latest/spvirit_types/) |
| `spvirit-codec` | [docs.rs/spvirit-codec](https://docs.rs/spvirit-codec/latest/spvirit_codec/) |
| `spvirit-client` | [docs.rs/spvirit-client](https://docs.rs/spvirit-client/latest/spvirit_client/) |
| `spvirit-server` | [docs.rs/spvirit-server](https://docs.rs/spvirit-server/latest/spvirit_server/) |
| `spvirit-tools` | [docs.rs/spvirit-tools](https://docs.rs/spvirit-tools/latest/spvirit_tools/) |
| `spvirit-calc` | [docs.rs/spvirit-calc](https://docs.rs/spvirit-calc/latest/spvirit_calc/) |

For Python there is no generated site; the
[Python API](05-reference/python-api.md) chapter is the reference, and every
object carries a docstring, so `help(spvirit.ai)` works at the prompt.

## Project

- **Licence** — BSD-3-Clause. See [Licence and support](licence.md).
- **Source and issues** —
  [github.com/ISISNeutronMuon/spvirit](https://github.com/ISISNeutronMuon/spvirit)
- **Contributing** — [Licence and support](licence.md) has the short version;
  the [Developer guide](06-dev-guide/index.md) has the long one.

Every code sample on this site is included verbatim from a file in the
repository that is compiled by CI. The badge at the top of each chapter links
to that source and to the test that checks it — including the quickstart
above:

<!-- verify:begin -->
> ✅ **Verified** · [`demo_first_pv.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_first_pv.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->
