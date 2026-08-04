# Tables and images

<!-- verify:begin -->
> ✅ **Verified** · [`exotic_nt.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/exotic_nt.rs) · [`demo_table.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_table.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

An `NtTable` — columnar data, like a scan result or a device inventory —
and an `NtNdArray` — a framed image.

These are the two places where spvirit steps outside the EPICS record
model. As [Records vs raw NT](../01-fundamentals/records-vs-raw-nt.md)
explains, there is no `table` record in EPICS Base. An `NtTable` is a
**payload**, not a processing entity: nothing scans it, nothing computes
alarms from it, and there is no `.db` syntax that produces one. You are
using PVAccess as a transport for structured data.

## NtTable

A table is a set of equal-length named columns plus display labels:

```
labels:  string[]         # what to show in a header row
value:
  x: double[]             # one array per column
  y: double[]
```

### Rust

```rust
{{#include ../../../../spvirit-server/examples/exotic_nt.rs:table}}
```

### Python

```python
{{#include ../../../../spvirit-py/examples/demo_table.py:table}}
```

Python's builder takes a plain `{name: list}` dict and infers each column's
wire type; `types=` overrides that per column. `labels` defaults to the
column names.

## NtNdArray

An image is flat data plus a dimension list. The dimensions carry offset,
binning and reversal so a client can reconstruct a region of interest
without a second PV.

```rust
{{#include ../../../../spvirit-server/examples/exotic_nt.rs:ndarray}}
```

The Python builder is the compact form of the same thing:

```python
builder.nt_ndarray("SIM:IMG", [0] * 16, [(4, 4), (4, 4)], type="ubyte")
```

Each tuple is `(size, full_size)` — the served extent and the extent of the
underlying frame.

## What to notice

**Neither type is writable.** The store's PUT branch for `NtTable` and
`NtNdArray` does nothing (`spvirit-server/src/simple_store.rs:608`). These
are server-to-client payloads. Drive them with `store.put_nt(...)`.

**No deadband, no alarm computation, no scanning.** All of that lives in
the record layer, which these types are not part of. Every `put_nt` posts
to every subscriber.

**`spget` prints them structurally.**

```console
$ spget SIM:TBL
SIM:TBL {x=[0.000000, 1.000000, ...], y=[0.863209, 0.334988, ...]}

$ spget SIM:IMG
SIM:IMG {ubyteValue=[0, 0, 0, ...]}
```

The `ubyteValue` field name is not decoration — NTNDArray's `value` is a
union, and the field name identifies which arm is populated. An `int16`
image would come back as `shortValue`.

**`sptable` renders tables properly.** It is the interactive viewer for
`NtTable` PVs; `spget` is only useful for a quick shape check.

**Element types are fixed at creation.** Writing an `f64` column into a
table created with `int` columns coerces to the record's type rather than
retyping the record. The record is the authority.

**Python needs lists, not numpy arrays.** Call `.tolist()` — `bytes` is
also accepted for `ubyte` data.

## Run it

```bash
# Terminal 1
cargo run -p spvirit-server --example exotic_nt
# or: python spvirit-py/examples/demo_table.py

# Terminal 2
spget SIM:TBL
sptable SIM:TBL      # interactive table viewer
spget SIM:IMG
```

## Next

[Custom data sources](sources.md).
