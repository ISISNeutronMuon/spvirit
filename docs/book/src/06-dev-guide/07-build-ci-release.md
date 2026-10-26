# Build, CI, and Release

## Building

```bash
cargo build --release          # whole workspace
cargo test --all               # all Rust tests
```

Python bindings (from `spvirit-py/`, venv assumed at `.venv/`):

```powershell
.\.venv\Scripts\maturin.exe develop            # dev build into the venv
maturin build --release                        # wheel → target/wheels/
```

Facts a new team should know:

- All crates are **edition 2024**; there is **no `rust-toolchain.toml`, no
  MSRV declaration, no committed `.cargo/config.toml`** (it's gitignored for
  local patch overrides), and **no clippy/rustfmt config** — CI uses
  `dtolnay/rust-toolchain@stable` and formatting is by convention
  (`cargo fmt` before committing; history shows manual `style:` commits).
- `[workspace.dependencies]` covers only the internal `spvirit-*` crates.
  External deps (tokio, serde_json, ...) are declared per crate, so their
  versions can drift. Check them when you bump a shared dep like tokio.
- Feature flags: `spvirit-tools` has `default = ["client","server","tui"]`
  plus the opt-in `scry` and `load`, and every binary declares
  `required-features`. `spvirit-py` has `extension-module` (default) and
  `test-embed`.
- `target/package/spvirit-*-<version>/` directories are `cargo package`
  artifacts — never edit those copies.

## CI (`.github/workflows/ci.yml`)

Runs on push and PR to `main`, two jobs on ubuntu-latest:

1. **build-and-test**: `cargo build --release`, `cargo test --all`, the
   spvirit-py embedded-interpreter tests (`cargo test -p spvirit-py
   --no-default-features --features test-embed`, after installing
   `libpython3-dev`), the `docs_verify` suite, and `mdbook build docs/book`.
2. **p4p-interop** — installs `p4p` (Python 3.12), builds, runs the p4p
   provider matrix (`interop_tool_matrix -- p4p_provider_matrix`) with
   `PVA_TEST_P4P=1` etc.

`.github/workflows/pages.yml` builds the book and deploys it to GitHub Pages
on pushes to `main` that touch `docs/**`, `README.md` or the workflow itself.

**There is no clippy or rustfmt gate in CI.** If you want one, add it — but
be aware the codebase has never been held to a CI lint bar.

## Releases

Two independent release tracks:

### Rust crates (manual)

- All nine crates (eight Rust library/binary crates plus `spvirit-py`)
  inherit one version from `[workspace.package]` in the root `Cargo.toml`
  (currently **0.4.1**). Internal dependencies are declared once under
  `[workspace.dependencies]`, so a release is a one-line version bump plus
  those six pins.
- Published to crates.io **manually via `cargo-release`** (the `chore:
  Release` commits) — there is **no workflow for this**; no release-plz.
- History includes a version-numbering hiccup (`chore: bump versions past the
  unrecorded 0.1.15 / 0.1.12 releases`) — when bumping, keep crate versions
  and path-dep pins consistent, and verify what's actually on crates.io
  before choosing the next number.

### Python wheels (automated)

- `spvirit-py` shares the workspace version (0.4.1). maturin reads the
  wheel version from its `Cargo.toml` (`dynamic = ["version"]`).
  `pyproject.toml` pins `spvirit-tools == <workspace version>` by hand, and
  `spvirit-tools/tests/version_pin.rs` fails the build if the pin drifts.
- Release trigger: push a tag matching **`spvirit-py-v*`** →
  `.github/workflows/release-python.yml` builds wheels on a 5-platform
  matrix (linux x86_64 + aarch64, windows x64, macOS x86_64 + aarch64) plus
  an sdist, and publishes to PyPI via **trusted publishing (OIDC,
  environment `pypi`)** — only on the tag push, not on manual dispatch.
- The same tag also builds `spvirit-tools` wheels (the thirteen default-build
  CLI binaries packaged as wheel scripts, `bindings = "bin"`) on the same
  five-platform matrix, plus an sdist. A step checks each tools wheel's
  contents (`spvirit-tools/scripts/verify_wheel.py`: every default tool
  present, and neither `spload` nor the `scry` binaries). `spvirit-tools` is
  published **before** `spvirit`, because `spvirit` depends on it by exact
  version.
- PyPI package names: `spvirit` and `spvirit-tools`.

### Release checklist (suggested)

1. Working tree clean; `cargo test --all` green; Python suite green.
2. Rust: bump `[workspace.package] version` and the six
   `[workspace.dependencies]` pins in the root `Cargo.toml`. Then
   `cargo release`, or `cargo publish` in dependency order: types → codec →
   calc → client → server → ioc → gateway → tools.
3. Python: update the `spvirit-tools == X.Y.Z` pin in
   `spvirit-py/pyproject.toml` (`version_pin.rs` checks it), commit, tag
   `spvirit-py-vX.Y.Z`, push the tag, and watch the workflow.
   `spvirit-py/Cargo.toml` inherits the version, so do not edit it.
4. Update the README's version references if any.

## Conventions

- **Conventional Commits**: `feat(py):`, `fix(server):`, `docs:`, `chore:
  Release`, `!` for breaking changes. Merges via GitHub PRs.
- Planning workflow: substantial features get a design spec and a task-by-task
  TDD plan (checkbox steps, exact commands, per-task commit messages) before
  any code is written. Those working documents stay local and are not
  committed — the durable record is `docs/book/src/06-dev-guide/`, the commit history, and
  the tests.
- README has a **GenAI Usage Log** table documenting which parts were
  AI-assisted — keep it updated (transparency requirement).
- License: BSD-3-Clause, ISIS Neutron and Muon Source.
- No CONTRIBUTING.md yet; contribution guidance is informal ("PRs welcome").

## Operational notes

- Default ports: TCP **5075** (data), UDP **5076** (search/beacon).
- The server binds UDP 5076 with SO_REUSEADDR/SO_REUSEPORT so it can coexist
  with other PVA servers on the host.
- Primary dev environment has been **Windows** (PowerShell commands,
  `.venv\Scripts\...` paths); CI is Linux. Watch for path and
  socket-semantics differences (the client deliberately treats SO_REUSEADDR
  as Unix-only).
- Interop test matrix validated against EPICS Base, p4p/pvxs, and
  PVAccessJava.
