# Current State, In-Flight Work, and Roadmap

> Snapshot taken **2026-07-16**, revised **2026-07-17** after Effort B
> landed. Reconcile against `git log`/`git status` before acting on anything
> here — this chapter goes stale fastest.

## Repository state at handover

- Branch `main`, **15 commits ahead of `origin/main`** (unpushed, as of this
  revision) — the design spec/plan for Python NT value-type selection, the
  NTTable-metadata/timestamp fix, and the full Effort B implementation
  (`7c5bc9b` through `d85473b`, latest `fix(py): final-review fixes —
  strict types= key validation, doc accuracy`).
- **Working tree is clean** — the three files that were mid-edit at the
  2026-07-16 snapshot (`spvirit-codec/src/spvd_encode.rs`,
  `spvirit-server/src/simple_store.rs`, `spvirit-server/src/types.rs`) have
  since been committed as part of Effort A below. There are no uncommitted
  changes to reconcile.

### Effort A — NTTable metadata + store-entry timestamps (committed: `0ac87d5`)

Purpose: make static/NTTable PVs archivable by the EPICS Archiver Appliance
(it rejects epoch-0 events and NPEs on structures without a top-level
`timeStamp`).

- `spvd_encode.rs`: `nt_table_desc` now includes `descriptor`, `alarm`,
  `timeStamp` fields; `encode_nt_table_full` encodes them (defaulting when
  `None`); new round-trip test `nt_table_wire_format_carries_metadata`.
  Encode order must match descriptor field order — that's the invariant.
- `types.rs`: new `RecordInstance::stamp_missing_timestamps()` fills missing
  timestamps per payload family (NdArray stamps `data_time_stamp` too;
  Generic skipped).
- `simple_store.rs`: calls it in `SimplePvStore::new` and `insert`, plus a
  test asserting all record families get `seconds_past_epoch > 0` on store
  entry. Purely additive; caller-supplied timestamps are preserved.

### Effort B — Python NT value-type selection (landed)

Delivered as a 10-task TDD plan with per-task commits.

Goal (achieved): let Python select any of the twelve NTScalar wire types
instead of collapsing `int→I64`/`float→F64`. Architecture: shared
type-string parser + strict coercion layer in `spvirit-py/src/convert.rs`
(`parse_scalar_type`, `py_to_scalar_typed`, `py_to_scalar_array_typed`,
`coerce_scalar_value`, `coerce_scalar_array_value`); keyword-only `type=`/
`types=` params across NT classes, factories, and `ServerBuilder`, and a new
`spvirit.scalar()` factory backed by `PvKind::Typed(Pv<ScalarValue>,
TypeCode)`; store put paths coerce to the record's existing wire type.
Errors: `ValueError` (unknown type string), `OverflowError` (out of range),
`TypeError` (wrong kind) — matches `spvirit-py/README.md`, the authoritative
user-facing doc.

**Status**: all 10 tasks are committed (`git log` `0819a18`..`d85473b`).
`spvirit-py/tests/test_value_types.py` (324 LOC, ports 16060–16081) now
exists and exercises the full surface: `NtScalar`/`NtScalarArray`
`type=`/`.value_type`, `NtTable`/`NtNdArray` constructors, `spvirit.scalar()`,
`server.pv()` on `long`/unsigned records (no more `KeyError`), `waveform`/
`aai`/`aao`/`pv()` with `type=`, `ServerBuilder` `type=`/`types=` kwargs, and
`Store.set_value`/`set_array_value`/`put_nt` strict coercion. Build:
`.\.venv\Scripts\maturin.exe develop`; tests:
`cargo test -p spvirit-server pv::` and
`.\.venv\Scripts\python.exe tests\test_value_types.py`.

## Known gaps and latent bugs (triage list)

Cross-referenced from the per-crate chapters.

**Protocol/codec**
- No TLS anywhere (top roadmap item in README).
- No segmentation *reassembly* in the codec (consumers roll their own).
- Monitor bitset overrun ordering is heuristic (three variants + scoring).
- No packet-capture regression corpus; four duplicated size codecs.
- Array decode caps silently truncate; string/struct truncation can desync.

**Server**
- CANCEL_REQUEST unimplemented (some clients send it).
- PUT to `Generic` not wired into `RecordInstance::apply_put` (only `put_nt`
  works); `NtTable`/`NtNdArray` PUTs are wired and restamp like any other
  record.
- `.db` parser: one-statement-per-line only; cannot load
  longin/longout/mbbi/mbbo/table/ndarray/generic (the repo's only two
  `TODO(follow-up)` markers: `types.rs:45`, `db.rs:546`).
- `Pv::alarm_limits()` sets only the wire metadata fields, not the fields the
  alarm engine reads — `.alarm_limits()` + `compute_alarms(true)` does not
  auto-alarm. `compute_alarms` defaults to false.
- ADEL parsed but not enforced; beacon change counter only bumps on protocol
  PUTs.

**Client/tools**
- Structured puts not fully surfaced (README caveat; `put_encode.rs` is more
  capable than the high-level API exposes).
- Epoch disambiguation heuristic in `format.rs` (UNIX vs 1990 epoch by
  closeness to now).
- Hardcoded cid=1/ioid=1 in single-shot paths — hazard if ever multiplexed.
- spserver: NtTable/NtNdArray DOL links are no-ops; `demo/docker_compose.yml`
  is an empty placeholder.

**Python**
- No `.pyi` type stubs.
- Stale artifacts present but gitignored (not committed):
  `spvirit-py/dist/spvirit-0.1.9.tar.gz`, `spvirit-py/.venv/`.
- "sync-only for phase 1" file headers are outdated.
- ~~The type-collapsing limitation~~ — fixed by Effort B: all twelve
  NTScalar wire types are now selectable from Python via `type=`/`types=`
  and `spvirit.scalar()`, with strict coercion. Remaining Python-side gaps:
  `on_put`/`scan` still unsupported on array PVs; widened `byte`/`short`/
  `int` handles (the plain `int` `PvKind`) are not range-checked on write —
  only `spvirit.scalar(type=...)`/`store.set_value` enforce strict range
  checks on those types (see the README's "Widened `int` handles are not
  range-checked" caveat).

**Process**
- No clippy/rustfmt CI gate; no MSRV; no CONTRIBUTING.md; crates.io releases
  are manual with a history of version-numbering slips.

## Roadmap (from README + observed direction)

1. TLS support in client (and eventually server).
2. Structured put payloads surfaced in the high-level client API.
3. More complete softIOC behaviours and record processing in the server
   (record types in `.db`, table/ndarray PUT, CANCEL_REQUEST).
4. ~~Finish the Python value-types work (Effort B)~~ — done; see above.
5. Quality infrastructure: packet-capture regression corpus, benchmarks,
   lint gate in CI, `.pyi` stubs.
