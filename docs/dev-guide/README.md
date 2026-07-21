# Spvirit Developer Guide

Internal handover documentation for developers taking over Spvirit — a
pure-Rust implementation of the EPICS PVAccess protocol (client, server,
codec, CLI tools, Python bindings).

This guide covers **internals**: architecture, per-crate deep dives with
file/line references, testing, release process, and the exact state of
in-flight work. For the *user-facing* API story, read the top-level
[`README.md`](../../README.md) (Rust) and
[`spvirit-py/README.md`](../../spvirit-py/README.md) (Python) first — both
are comprehensive and kept current.

## Chapters

| # | Chapter | Read it when |
|---|---|---|
| 01 | [Architecture Overview](01-architecture.md) | Day one — the crate graph, the two protocol layers, server data flow, and the invariants everything relies on |
| 02 | [spvirit-types & spvirit-codec](02-types-and-codec.md) | Before touching the data model or wire format |
| 03 | [spvirit-server](03-server.md) | Before touching the server: Source model, store, protocol runtime, handles, alarms/deadbands |
| 04 | [spvirit-client & spvirit-tools](04-client-and-tools.md) | Before touching search/get/put/monitor or the CLI tools |
| 05 | [spvirit-py](05-python-bindings.md) | Before touching the Python bindings — especially the threading model |
| 06 | [Testing Guide](06-testing.md) | Before writing tests; how to run the interop suites |
| 07 | [Build, CI, and Release](07-build-ci-release.md) | Before releasing anything; repo conventions |
| 08 | [Current State & Roadmap](08-current-state.md) | **First**, if you're picking up work — uncommitted changes, the in-flight Python value-types plan, known-gaps triage list |

## Day-one checklist

```bash
git clone https://github.com/ISISNeutronMuon/spvirit && cd spvirit
cargo build --release
cargo test --all                                   # should be green

# See it work — two terminals:
cargo run -p spvirit-server --example simple_server
cargo run -p spvirit-client --example pvget -- SIM:TEMPERATURE

# Python bindings:
cd spvirit-py && python -m venv .venv
.venv\Scripts\Activate.ps1                         # Windows; source .venv/bin/activate elsewhere
pip install maturin && maturin develop
python tests/test_pv_handles.py                    # expect ALL OK
```

Then:

1. Read chapter 08 and run `git status` / `git log origin/main..main` —
   there is uncommitted work and unpushed commits at handover.
2. Read `.superpowers/sdd/progress.md` — the ledger of known follow-ups and
   latent bugs from previous development rounds.
3. Skim the top-level README's "Key Concepts" if EPICS/PVAccess is new to
   you, then chapter 01 here.

## Orientation in 60 seconds

Six crates, strict layering: `types` (pure data) → `codec` (wire format) →
`client`/`server` → `tools` (CLIs + integration tests) and `py` (PyO3).
Everything on the wire is a Normative Type; the server offers an IOC-style
record level (records, alarms, deadbands, auto-timestamps) as sugar over a
raw-NT level (`put_nt`/`get_nt`, custom `Source` providers). Default ports:
TCP 5075, UDP 5076. Interop is validated against EPICS Base, p4p/pvxs, and
PVAccessJava.

## How work is planned here

Substantial features follow a spec → plan → TDD execution workflow:
design specs in [`docs/superpowers/specs/`](../superpowers/specs/), checkbox
implementation plans in [`docs/superpowers/plans/`](../superpowers/plans/)
(exact commands, per-task commits, Conventional Commit messages). One such
plan — Python NT value-type selection — is mid-flight; see chapter 08 before
touching `spvirit-py` or `spvirit-server/src/pv.rs`.

## Where to get answers

- Protocol questions: the [pvAccess Protocol Specification](https://docs.epics-controls.org/en/latest/pv-access/protocol.html)
  and [pvxs](https://epics-base.github.io/pvxs/) (the reference
  implementation this project most closely mirrors).
- "Why is this code like this": `.superpowers/sdd/` contains per-task briefs,
  reports and review diffs from previous development — good archaeology.
- Wire debugging: `spsearch` (search traffic TUI), `spget --raw` /
  `spmonitor --raw` (hex dumps), `spget_compare` (byte-compare against
  captures), and the related
  [spvirit-scry](https://crates.io/crates/spvirit-scry) capture tool.
