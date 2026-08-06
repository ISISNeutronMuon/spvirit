# Record types

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

`RecordType` has seventeen variants
(`spvirit-server/src/types.rs:24`). Not all seventeen are reachable by
every route: the handle API, the server builder, and `.db` loading each
cover a different subset. This page is that matrix, checked against the
constructors that actually exist.

If you are not sure whether you want a record at all, read
[Records vs raw NT](../01-fundamentals/records-vs-raw-nt.md) first.

## The matrix

| Record | EPICS meaning | Handle API | Builder | `.db` | Writable |
|---|---|---|---|---|---|
| `ai` | analog in | `Pv::ai` | `.ai()` | `record(ai, …)` | no¹ |
| `ao` | analog out | `Pv::ao` | `.ao()` | `record(ao, …)` | yes |
| `bi` | binary in | `Pv::bi` | `.bi()` | `record(bi, …)` | no |
| `bo` | binary out | `Pv::bo` | `.bo()` | `record(bo, …)` | yes |
| `longin` | 32-bit integer in | `Pv::longin` | — | — | no |
| `longout` | 32-bit integer out | `Pv::longout` | — | — | yes |
| `stringin` | string in | `Pv::string_in` | `.string_in()` | `record(stringin, …)` | no |
| `stringout` | string out | `Pv::string_out` | `.string_out()` | `record(stringout, …)` | yes |
| `mbbi` | multi-bit binary in | `Pv::mbbi` | `.mbbi()` | parses, then refused² | yes³ |
| `mbbo` | multi-bit binary out | `Pv::mbbo` | `.mbbo()` | parses, then refused² | yes³ |
| `waveform` | array | `PvArray::waveform` | `.waveform()` | `record(waveform, …)` | yes |
| `aai` | array analog in | `PvArray::aai` | `.aai()` | `record(aai, …)` | no |
| `aao` | array analog out | `PvArray::aao` | `.aao()` | `record(aao, …)` | yes |
| `subArray` | window onto an array | — | `.sub_array()` | `record(subarray, …)` | no |
| `NtTable` | table payload | — | `.nt_table()` | refused² | yes⁴ |
| `NtNdArray` | image / detector frame | — | `.nt_ndarray()` | refused² | yes⁴ |
| `Generic` | arbitrary structure | — | `.generic()` | refused² | yes |

¹ An `ai` becomes writable when `SIMM` is set — the simulation-mode path
(`spvirit-server/src/types.rs:308`).

² `RecordType::from_db_name` maps twelve `.db` spellings, including `mbbi`
(also spelled `ntenum`) and `mbbo`. Those two then reach an arm in
`spvirit-server/src/db.rs:550` that prints *"is not a standard EPICS Base
record type and cannot be loaded from .db files"* and drops the record.
`mbbi` and `mbbo` **are** standard EPICS Base record types; the message is
wrong. See [Known gaps](known-gaps.md).

³ Writable in the sense that the record accepts write access. A wire PUT of
an enum index is currently dropped — see [Known gaps](known-gaps.md).

⁴ Writable via a client PUT as well as `store.put_nt()`. The `NtTable`/
`NtNdArray` arms of `RecordInstance::apply_put`
(`spvirit-server/src/apply.rs:609`) apply the wire fields and restamp the
record.

## Reading the columns

**Handle API** — `Pv<T>` and `PvArray` constructors in
`spvirit-server/src/pv.rs`. These give you a handle you keep after the
server starts, so you can `set()`, `scan()`, `calc()` and `on_put()` on it.
This is the level most of [Part III](../03-progressive/scalars.md) works at.

**Builder** — `PvaServer::builder().ai(…)` and friends
(`spvirit-server/src/pva_server.rs`). Declares records inline; you reach them
afterwards through the store rather than through a handle. The builder is
the only route to `sub_array`, `nt_table`, `nt_ndarray` and `generic`.

**`.db`** — text database files, as EPICS Base uses them. See
[Serving a .db file](../03-progressive/db-files.md) for the fields spvirit
acts on.

**Writable** — whether the server grants write access
(`RecordInstance::writable`, `spvirit-server/src/types.rs:303`). Output
record types are always writable; a handful of input types are too, for the
reasons in the footnotes. Everything else answers a PUT with
`Write access denied`.

## The derived record

`Pv::calc` (`spvirit-server/src/pv.rs:392`) is not a record type. It builds an
`ai` whose value is recomputed from other `Pv<f64>` handles whenever one of
them changes — the equivalent of a `calc` record's `CALC` expression, written
as a Rust closure. The builder spelling is `.link(output, inputs, compute)`.

## Python

The Python module exposes the same handle-API set:
`ai`, `ao`, `bi`, `bo`, `string_in`, `string_out`, `longin`, `longout`,
`mbbi`, `mbbo`, `waveform`, `aai`, `aao`, `calc`, plus the generic `pv` and
`scalar` constructors (`spvirit-py/src/lib.rs:54`). See
[Python API](python-api.md).
