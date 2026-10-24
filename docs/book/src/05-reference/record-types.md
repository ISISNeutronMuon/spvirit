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
| `mbbi` | multi-bit binary in | `Pv::mbbi` | `.mbbi()` | parses, then refused² | no |
| `mbbo` | multi-bit binary out | `Pv::mbbo` | `.mbbo()` | parses, then refused² | yes³ |
| `waveform` | array | `PvArray::waveform` | `.waveform()` | `record(waveform, …)` | yes |
| `aai` | array analog in | `PvArray::aai` | `.aai()` | `record(aai, …)` | no |
| `aao` | array analog out | `PvArray::aao` | `.aao()` | `record(aao, …)` | yes |
| `subArray` | window onto an array | — | `.sub_array()` | `record(subarray, …)` | no |
| `NtTable` | table payload | — | `.nt_table()` | refused² | yes⁴ |
| `NtNdArray` | image / detector frame | — | `.nt_ndarray()` | refused² | yes⁴ |
| `Generic` | arbitrary structure | — | `.generic()` | refused² | yes⁵ |

¹ An `ai` becomes writable when `SIMM` is set — the simulation-mode path
(`spvirit-server/src/types.rs:359`).

² `RecordType::from_db_name` recognises thirteen `.db` spellings (twelve
record types, with `mbbi` also accepted as `ntenum`), including `mbbi` and
`mbbo`. The `.db` loader does not construct those
two yet: loading a file that contains one fails with *"record '…' of type
'mbbi' is not yet supported by the .db loader; create it through the host API
instead"*. See [Known gaps](known-gaps.md).

³ A wire PUT selects the choice by index (`{"value":{"index":2}}`); an
out-of-range index is refused with a PUT error.

⁴ The `NtTable` arm of `RecordInstance::apply_put_fields`
(`spvirit-server/src/apply.rs:675`) applies the wire fields, and `apply_put`
restamps the record. An `NtNdArray` PUT applies the metadata fields, but the
image `value` a client sends is not applied (see [Known gaps](known-gaps.md)).
Both are writable through `store.put_nt()` too.

⁵ Writable through `store.put_nt()` only. A client PUT to a generic record is
refused with a PUT error — see [Known gaps](known-gaps.md).

## Reading the columns

**Handle API** — `Pv<T>` and `PvArray` constructors in
`spvirit-server/src/pv.rs`. These give you a handle you keep after the
server starts, so you can `set()`, `scan()`, `calc()` and `on_put()` on it.
This is the level most of [Part III](../03-progressive/scalars.md) works at.

**Builder** — `PvaServer::builder().ai(…)` and friends
(`spvirit-server/src/pva_server.rs`). Declares records inline; you reach them
afterwards through the store rather than through a handle. The builder is
the only declare-up-front route to `nt_table`, `nt_ndarray` and `generic`
(`subArray` also loads from `.db`, and `PvaServer::add_table` adds a table at
runtime).

**`.db`** — text database files, as EPICS Base uses them. See
[Serving a .db file](../03-progressive/db-files.md) for the fields spvirit
acts on.

**Writable** — whether the server grants write access
(`RecordInstance::writable`, `spvirit-server/src/types.rs:354`). Output
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
`scalar` constructors (`spvirit-py/src/lib.rs:84`). See
[Python API](python-api.md).
