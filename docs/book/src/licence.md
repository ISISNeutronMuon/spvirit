# Licence and support

## Licence

Spvirit is released under the **BSD 3-Clause License**.

> Copyright (c) 2026, Mateusz Leputa for UKRI STFC ISIS Neutron and Muon
> Facility

In short: use it, modify it, redistribute it, ship it in a commercial
product. You must keep the copyright notice and the licence text in source
and binary redistributions, and you may not use the names of the copyright
holder or its contributors to endorse products derived from it without
written permission. There is no warranty.

That is a summary and not the licence. The authoritative text is
[`LICENSE`](https://github.com/ISISNeutronMuon/spvirit/blob/main/LICENSE) in
the repository root, and it applies to every crate in the workspace and to
the Python packages built from them.

## Getting help

| You want to | Go to |
|---|---|
| Report a bug, or ask a question | [GitHub issues](https://github.com/ISISNeutronMuon/spvirit/issues) |
| Work out why a PV will not connect | [Troubleshooting](05-reference/troubleshooting.md) |
| Check whether something is unimplemented rather than broken | [Known gaps](05-reference/known-gaps.md) |
| Look up a Rust API | [docs.rs](https://docs.rs/spvirit-server/latest/spvirit_server/), or the [crate map](05-reference/crate-map.md) |
| Look up a Python API | [Python API](05-reference/python-api.md) |

Before filing a bug, [Known gaps](05-reference/known-gaps.md) is worth two
minutes — it lists the behaviours that are deliberately absent, so you can
tell "not implemented" from "regression". A useful report includes the
spvirit version (`python -c "from importlib.metadata import version;
print(version('spvirit'))"` or `cargo tree -p spvirit-server`), the platform,
and the smallest server and client pair that reproduces it.

## Contributing

Contributions are welcome through pull requests on
[GitHub](https://github.com/ISISNeutronMuon/spvirit).

The short version:

```bash
git clone https://github.com/ISISNeutronMuon/spvirit && cd spvirit
cargo build
cargo test --all          # must be green before you open a PR
cargo fmt --all
cargo clippy --all-targets
```

Three conventions specific to this repository:

1. **Chapters cite code; they do not copy it.** Every code block in this book
   is an `{{#include}}` pointing at a real file under `spvirit-*/examples/`,
   anchored with `// ANCHOR: name` / `// ANCHOR_END: name`. If you add a
   snippet, add the example file and the anchor, not a copy of the source.

2. **`docs/book/verify.toml` is the manifest.** Each chapter declares the
   example files, anchors, and CLI tools it cites.
   `spvirit-tools/tests/docs_verify.rs` checks that they all exist, that
   every include is declared, and that every shipped example and tool is
   documented somewhere. Run it:

   ```bash
   cargo test -p spvirit-tools --test docs_verify
   ```

   The "✅ Verified" badge at the top of each chapter is generated, not
   typed. After changing what a chapter cites, regenerate:

   ```bash
   UPDATE_DOCS=1 cargo test -p spvirit-tools --test docs_verify
   ```

3. **Every example has a Python counterpart, and vice versa.** Part III is
   dual-language throughout; a new Rust example wants a matching
   `spvirit-py/examples/demo_*.py`.

Every page of this book has an **edit** link in its top-right corner that
opens the source file directly on GitHub — the fastest route for a typo or a
correction.

The [Developer guide](06-dev-guide/index.md) is the long version: the crate
graph, per-crate internals with file-and-line citations, the test suites and
how to run the EPICS interop ones, and the release process.

## Citing spvirit

If spvirit is used in published work, cite the repository:

> Leputa, M. *Spvirit: EPICS PVAccess in pure Rust.* UKRI STFC ISIS Neutron
> and Muon Facility. <https://github.com/ISISNeutronMuon/spvirit>

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->
