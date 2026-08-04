# Enums and binary records

<!-- verify:begin -->
> ✅ **Verified** · [`exotic_nt.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/exotic_nt.rs) · [`demo_enums.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_enums.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

PVs whose value is one of a fixed set of named states — the EPICS `mbbi`
and `mbbo` records, carried on the wire as an `NtEnum`.

## What an enum record is

An `NtEnum` value is not a string. It is a structure:

```
value:
  index:   int          # which choice is selected
  choices: string[]     # the labels, in order
```

The record stores the **index**. The labels ride along so a client can
render "Running" without a separate lookup. This is why `spget` prints
`{index=2, choices=["Idle", "Running", "Fault"]}` rather than `Fault`.

| Record | Direction | EPICS field names for choices |
|---|---|---|
| `bi` / `bo` | in / out | `ZNAM`, `ONAM` (exactly two) |
| `mbbi` / `mbbo` | in / out | `ZRST`…`FFST` (up to sixteen) |

`bi`/`bo` are *not* `NtEnum` in spvirit — they are `NtScalar` booleans, and
`spget` prints `true`/`false`. `ZNAM`/`ONAM` name the two states in a `.db`
file. If you need the labels on the wire, use `mbbi`/`mbbo`.

## Rust

```rust
{{#include ../../../../spvirit-server/examples/exotic_nt.rs:enums}}
```

## Python

```python
{{#include ../../../../spvirit-py/examples/demo_enums.py:enums}}
```

## What to notice

**Enum records ignore the scalar metadata options.** `units`, `prec`,
`adel`, `mdel`, and the limit setters do not apply — only `desc` is
accepted. An `NtEnum` has no engineering units to carry.

**`.db` files cannot define enum records.** `mbbi` and `mbbo` parse, then
fail at construction with:

```text
Record 'DEMO:STATE': type 'mbbi' is not a standard EPICS Base record type
and cannot be loaded from .db files
```

The message is misleading — `mbbi` *is* a standard EPICS Base record type;
it is spvirit's `.db` loader that does not build it
(`spvirit-server/src/db.rs:550`). Build enum records in code.

**Writing to an `mbbo` from a client does not work today.** The record is
advertised writable and the PUT is accepted on the wire — `spput` prints
`OK`, the Python client's `put()` returns without raising — but the value
does not change:

```console
$ spput SIM:MODE --json '{"value":{"index":2}}'
SIM:MODE OK

$ spget SIM:MODE
SIM:MODE {index=0, choices=["Standby", "Acquire", "Calibrate"]}
```

The store's enum PUT branch (`spvirit-server/src/simple_store.rs:616`)
matches only a bare integer under `value`, but the NTEnum wire format nests
the index one level deeper as `value.index`, so the update is dropped
silently. **Drive enum records server-side with `pv.set(index)`** and treat
them as read-only from the client until this is fixed.

**Out-of-range indices are rejected, not clamped.** The store checks
`idx < 0 || idx >= choices.len()` and leaves the value alone.

## Run it

```bash
# Terminal 1
cargo run -p spvirit-server --example exotic_nt
# or: python spvirit-py/examples/demo_enums.py

# Terminal 2
spget SIM:STATE
spmonitor SIM:STATE
```

## Next

[Alarms and severity](alarms.md).
