# Simulating a device

<!-- verify:begin -->
> ✅ **Verified** · [`scan_callback.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/scan_callback.rs) · [`linked_calc.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/linked_calc.rs) · [`demo_scan.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_scan.py) · [`demo_calc.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_calc.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

PVs that change on their own — a periodic **scan**, and **computed** PVs
that recalculate whenever their inputs move. Together these are most of
what a test double needs.

## Periodic scanning

### Rust

```rust
{{#include ../../../../spvirit-server/examples/scan_callback.rs:sim}}
```

### Python

```python
{{#include ../../../../spvirit-py/examples/demo_scan.py:sim}}
```

`@temp.scan(period=0.1)` is the decorator form; `temp.scan(0.1, fn)` is the
same thing as a call.

## Computed PVs

### Rust

```rust
{{#include ../../../../spvirit-server/examples/linked_calc.rs:links}}
```

`.link(output, inputs, compute)` recomputes `output` whenever any of
`inputs` changes. There is also `Pv::calc(name, &[&inputs], f)` on the
handle side, which is the same idea with `f64` types filled in for you.

### Python

```python
{{#include ../../../../spvirit-py/examples/demo_calc.py:links}}
```

## What to notice

**Scan and calc fail differently, and neither one shouts.**

| | On a callback exception |
|---|---|
| `scan` | logs the error, re-posts the **last value the scan produced** (or the type default — `0.0`/`False`/`0`/`""` — if it has never produced one) |
| `calc` | logs the error, posts **`0.0`** |

So a `calc` that starts throwing pins its PV at zero, which looks exactly
like a real reading of zero. If a computed PV must distinguish "broken"
from "zero", set an alarm severity explicitly — see [Alarms](alarms.md).

**A scan returning `None` does not mean "leave it alone".** It re-posts the
last value *that scan* produced. It does not read the PV's current value,
so if something else called `pv.set(...)` in between, returning `None` will
overwrite that with the scan's own cached value. Return a real value, or
call `pv.set()` and let the scan return `None` deliberately.

**Computed PVs are read-only.** `.link()` and `calc` produce `ai` records.
Writing to one either fails or is immediately overwritten on the next
recomputation.

**Recomputation is change-driven, not periodic.** Nothing happens until an
input changes. A `calc` over two inputs that never move costs nothing.

**Deadbands apply to the output too.** If a computed PV has an `mdel`, its
subscribers see the deadbanded stream even though the recomputation ran.

## Run it

```bash
# Terminal 1
cargo run -p spvirit-server --example linked_calc

# Terminal 2
spput CALC:A 10
spput CALC:B 3
spget CALC:SUM       # 13
spget CALC:PROD      # 30
spget CALC:MEAN      # 6.5
spmonitor CALC:SUM   # live updates as A or B change
```

```console
$ spput CALC:A 10
CALC:A OK

$ spput CALC:B 3
CALC:B OK

$ spget CALC:SUM
CALC:SUM 2026-08-06 09:13:28.993  13

$ spget CALC:PROD
CALC:PROD 2026-08-06 09:13:28.993  30

$ spget CALC:MEAN
CALC:MEAN 2026-08-06 09:13:28.993 6.5
```

All three derived PVs carry the *same* timestamp, because one write to
`CALC:B` recomputed all of them in the same pass.

Leave `spmonitor CALC:SUM` running first, then do the two puts from a third
terminal, and you can watch the recomputation happen:

```console
$ spmonitor CALC:SUM
CALC:SUM 2026-08-06 09:27:29.216   0
CALC:SUM 2026-08-06 09:27:34.375  10
CALC:SUM 2026-08-06 09:27:35.262  13
CALC:SUM 2026-08-06 09:27:36.132  23
```

`0` is the initial value delivered on connect, `10` follows `spput CALC:A
10`, `13` follows `spput CALC:B 3`, and `23` is a later `spput CALC:A 20`.
Each input write produces exactly one output update.

Or the scan pair:

```bash
python spvirit-py/examples/demo_scan.py     # terminal 1
spmonitor SIM:TEMPERATURE                   # terminal 2
```

```console
SIM:TEMPERATURE 2026-08-06 09:25:29.221 21.677613
SIM:TEMPERATURE 21.734644
SIM:TEMPERATURE 21.8092
SIM:TEMPERATURE 21.89275
...
SIM:TEMPERATURE 2026-08-06 09:25:30.028 22.347997
```

Ten updates a second, forever, with no client asking for them. The
timestamp is reprinted only when the wall-clock second rolls over.

## Next

[Arrays and waveforms](arrays.md).
