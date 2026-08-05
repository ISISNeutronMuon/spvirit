# Installation

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

There are three ways in, depending on what you want to do. None of them
require EPICS Base — Spvirit speaks PVAccess itself.

## Python

```bash
pip install spvirit
```

That is the whole story on Linux (x86_64, aarch64), macOS (Intel and Apple
Silicon), and Windows x86_64: those five platforms get prebuilt `abi3`
wheels, so there is no compiler involved. Python 3.9 or newer.

It also brings the command-line tools. `spvirit` depends on the
`spvirit-tools` package, so the twelve binaries below land on your `PATH`
in the same step — no Rust toolchain, and inside the virtualenv rather than
in `~/.cargo/bin`.

Check it:

```console
$ python -c "from importlib.metadata import version; print(version('spvirit'))"
0.1.19
```

(The module itself has no `__version__` attribute — ask the package
metadata, as above.)

On any other platform pip falls back to the sdist and builds from source,
which needs a Rust toolchain.

## Command-line tools

Twelve binaries: `spget`, `spput`, `spmonitor`, `spinfo`, `splist`,
`spsearch`, `spexplore`, `sptable`, `spserver`, `spsine`, `spdodeca`, and
`spget_compare`.

```bash
pip install spvirit-tools
```

Prebuilt wheels for the same five platforms as above, so no compiler is
involved. The binaries install into the environment's `bin` (or `Scripts`)
directory, which puts them on `PATH` whenever that environment is active.
This package is what `pip install spvirit` pulls in, so if you already have
the bindings you already have these.

Or, with a Rust toolchain:

```bash
cargo install spvirit-tools
```

[rustup](https://rustup.rs) is the usual way to get one. Stable is what CI
builds against. This route installs into `~/.cargo/bin` and is the one to
use on platforms without a prebuilt wheel.

Either way, check it:

```console
$ spget --help
```

## Rust library

Add whichever crates you need. They are strictly layered, so asking for
`spvirit-client` pulls in `spvirit-codec` and `spvirit-types` for you.

```toml
[dependencies]
spvirit-client = "0.1"   # search, connect, get, put, monitor
spvirit-server = "0.1"   # .db parsing, the Source trait, the PVA server
spvirit-codec  = "0.1"   # low-level PVAccess encode/decode
spvirit-types  = "0.1"   # the Normative Type data model
```

Most of this site uses `spvirit-client` and `spvirit-server`. You will also
want [Tokio](https://tokio.rs) — both are async.

## From source

For hacking on Spvirit itself, or to run the examples this site includes:

```bash
git clone https://github.com/ISISNeutronMuon/spvirit
cd spvirit
cargo build --release
```

The binaries land in `target/release/`. Examples run straight from the
workspace:

```bash
cargo run -p spvirit-server --example simple_server
```

### Python from source

The bindings are built with [maturin](https://www.maturin.rs/):

```bash
python -m venv .venv
source .venv/bin/activate      # Windows: .venv\Scripts\activate
pip install maturin
cd spvirit-py
maturin develop --release
```

After `maturin develop` the `spvirit` module is importable from that venv.

## A note on ports

PVAccess uses **TCP 5075** and **UDP 5076** by default, and discovery is
UDP broadcast. If a client cannot find a server that is definitely running,
the firewall is the first thing to suspect — on Windows especially, and on
any host where the two are on different subnets.

## Next

[Your first PV](first-pv.md).
