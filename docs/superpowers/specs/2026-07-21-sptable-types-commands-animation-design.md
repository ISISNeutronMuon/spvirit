# sptable — enum/table types, `:` command system, pattern expansion, animation

**Status:** Design approved (2026-07-21)
**Builds on:** [`2026-07-21-sptable-interactive-ioc-design.md`](2026-07-21-sptable-interactive-ioc-design.md) — the existing `sptable` binary (`spvirit-tools/src/bin/spvirit_table.rs`).

## Goal

Extend `sptable` from a scalar/array spreadsheet IOC into a richer interactive simulator:

1. **Two new PV kinds** — NTEnum and NTTable — creatable at runtime.
2. **A vim-style `:` command line** with a full verb set, shorthands, and `:help`.
3. **Pattern (brace) expansion** so one command creates/deletes/animates many PVs.
4. **Animation** — background generators drive PV values over time so data is live, not static.

The existing modal `a`/`e`/`d` wizard is kept; the command line is an additive power-user surface.

---

## 1. Data model

Replace `PvRow`'s flat `kind: Kind` + `ty: WireType` with a single spec enum — the clean extension point for kinds whose type shape differs:

```rust
enum PvSpec {
    Scalar(WireType),
    Array(WireType),
    Enum  { choices: Vec<String> },
    Table { columns: Vec<(String, WireType)> },
}

struct PvRow {
    name: String,
    writable: bool,
    display: String,   // last known value, formatted
    spec: PvSpec,
}
```

Animation state is **not** stored on the row — the shared animator map (§4.2) is the single source of truth, keyed by PV name. Rendering the `~` marker and the selected-row status queries that map.

Grid Kind/Type columns derive from `spec`:

| spec | Kind col | Type col |
|---|---|---|
| `Scalar(t)` | `scalar` | `t.label()` (e.g. `int32`) |
| `Array(t)` | `array` | `t.label()` |
| `Enum{..}` | `enum` | `enum` |
| `Table{..}` | `table` | `table` |

---

## 2. Runtime server primitives (`spvirit-server`)

The record builders for enum (mbbi/mbbo) and table (NTTable) exist today only on `ServerBuilder`; add runtime equivalents on `RunningServer`, mirroring the existing `add_scalar`/`add_array` (TDD). They build a `RecordInstance` and `store.insert(...)`.

- `RunningServer::add_enum(&self, name: &str, choices: Vec<String>, index: i32, writable: bool)`
  — `writable` selects `RecordType::Mbbo` else `Mbbi`; `RecordData::NtEnum { nt: NtEnum::new(index, choices), .. }`.
- `RunningServer::add_table(&self, name: &str, columns: Vec<(String, ScalarArrayValue)>, writable: bool)`
  — builds `RecordData::NtTable` (labels + `NtTableColumn`s), `omsl` supervisory.

Value updates route through the **existing general setter** `SimplePvStore::put_nt(name, NtPayload)`:
- enum edit → rebuild `NtEnum::new(new_index, choices)` → `put_nt(name, NtPayload::Enum(..))`.
- table edit → `put_nt(name, NtPayload::Table(..))`.

Scalars/arrays keep using `set_value` / `set_array_value`.

---

## 3. The `:` command line

`:` from Browse enters `Mode::Command { buf }`. On Enter, the buffer is parsed by a **pure** function kept separate from execution (unit-testable, like `parse_scalar`):

```rust
enum Command {
    Add   { names: Vec<String>, spec: SpecInput, writable: bool, value: String },
    Set   { names: Vec<String>, value: String },
    Del   { names: Vec<String> },      // empty = selected row
    Rename{ old: String, new: String },
    Access{ names: Vec<String>, writable: bool },   // :ro / :rw
    Anim  { names: Vec<String>, spec: AnimSpec },   // §4
    Stop  { names: Vec<String> },                   // empty = selected row
    Rate  { hz: f64 },
    Source{ path: String },
    Help,
    Quit,
}

fn parse_command(line: &str) -> Result<Command, String>;
```

`names` are produced by pattern expansion (§3.2) so every name-taking verb is bulk-capable.

### 3.1 Verbs & shorthands

| Full | Short | Form |
|---|---|---|
| `:add` | `:a` | `:add <name> <typespec> [ro\|rw] <value…>` |
| `:set` | `:s` | `:set <name> <value…>` |
| `:del` | `:d` | `:del [<name>]` (bare = selected row) |
| `:rename` | `:mv` | `:rename <old> <new>` |
| `:ro` / `:rw` | — | `:ro <name>` / `:rw <name>` |
| `:anim` | — | `:anim <name> <fn> [k=v…]` |
| `:stop` | — | `:stop [<name>]` |
| `:rate` | — | `:rate <hz>` |
| `:source` | `:so` | `:source <path>` |
| `:help` | `:h` | `:help` |
| `:quit` | `:q` | `:quit` |

- **Access defaults to `rw`** when the token is omitted.
- `:rename` preserves spec + current value (remove + re-add). `:ro`/`:rw` recreate the record with the new writable flag, preserving value.
- `:set` on an enum accepts a **choice name or an integer index** (`:set X ON` == `:set X 1`).
- `:set`/edit on an **animated** PV stops its animator first (§4).

### 3.2 typespec

Canonical grid label is always the long form; input accepts aliases via an extended `WireType::from_token`:

| Type | Accepts | Type | Accepts |
|---|---|---|---|
| bool | `b` `bool` | uint8 | `u8` `uint8` |
| int8 | `i8` `int8` | uint16 | `u16` `uint16` |
| int16 | `i16` `int16` | uint32 | `u32` `uint32` |
| int32 | `i32` `int` `int32` | uint64 | `u64` `uint64` |
| int64 | `i64` `long` `int64` | float | `f32` `float` |
| double | `f64` `double` | string | `s` `str` `string` |

Kind is encoded in the typespec token:
- scalar: a bare type label — `i32`, `double`, …
- array: type label + `[]` — `i32[]`, `f64[]`.
- `enum`: value field is `CHOICE1,CHOICE2,… [index]` (index optional, default 0).
- `table`: value field is space-separated `col:type=v1,v2,…` specs, e.g. `id:i32=1,2,3 x:f64=0.5,1.5`.

```rust
enum SpecInput {          // resolved by the executor into PvSpec + typed value
    Scalar(WireType),
    Array(WireType),
    Enum,
    Table,
}
```

### 3.3 Pattern (brace) expansion

Pure `expand_pattern(&str) -> Result<Vec<String>, String>`, run in front of the single-name path for every name-taking verb. Modeled on bash brace expansion:

| Pattern | Expands to |
|---|---|
| `{1..8}` | `1 2 … 8` |
| `{8..1}` | descending |
| `{0..100..10}` | stepped |
| `{01..12}` | zero-padded, width from the bounds |
| `{A,B,C}` | literal list |

Multiple braces → **cartesian product** (bash semantics):
`SECTOR{1..4}:PSU{A,B}` → 8 names. `RING:BPM{01..99}` → 99 zero-padded names.

Rules:
- **Same initial value** for every generated PV (the "spin up N channels at 0" case). Distinct per-PV values are out of scope — use `:set` or a `:source` file.
- **Safety cap 1000** (a `const`): an expansion exceeding it errors with the count (`would create 100000 PVs (cap 1000)`); nothing is created.
- **Collisions skip-and-report**: existing names are left untouched; status shows `added 6, skipped 2 (exist)`, so re-running a pattern is safe.
- `:del`/`:anim`/`:stop` accept the same patterns.

### 3.4 `:source`

Reads a file, one command per line; blank lines and `#`-comment lines ignored; each line goes through `parse_command` + execution. Errors are collected and summarised in the status (`sourced 40 cmds, 1 error: line 12 …`). Lets a user script a full IOC layout.

---

## 4. Animation

A background tick drives PV values so data is live.

### 4.1 Generators — pure `sample`

```rust
enum Generator { Sine, Ramp, Triangle, Square, Noise, Walk, Count, Cycle }

struct Animator {
    gen: Generator,
    params: AnimParams,     // resolved key=value with defaults
    start: Instant,         // t = now - start, for continuous phase
    state: AnimState,       // walk position / count / cycle index
}

fn sample(anim: &mut Animator, spec: &PvSpec, t: f64) -> TypedValue;
```

| Generator | Params (defaults) | Behavior |
|---|---|---|
| `sine` | `amp=1 offset=0 period=10 phase=0` | `offset + amp·sin(2π·t/period + phase)` |
| `ramp` | `min=0 max=1 period=10` | sawtooth, wraps |
| `triangle` | `min=0 max=1 period=10` | linear up/down |
| `square` | `lo=0 hi=1 period=10 duty=0.5` | toggles |
| `noise` | `min=0 max=1` | uniform random each tick |
| `walk` | `start=0 step=0.1 min max` | random walk, clamped to `[min,max]` |
| `count` | `start=0 step=1 [wrap=N]` | increments per tick |
| `cycle` | `period=1` | **enum only** — advances choice index |

Applicability:
- **Numeric scalars** (all int/uint/float families): sample is rounded for integer types; out-of-range is clamped to the type.
- **bool**: driven by `square`/nonzero.
- **enum**: `cycle` only.
- **array / table / string**: not animatable — `:anim` on them is a clear error.

### 4.2 Engine — server-side tick task

Not UI-coupled. On `ServerHandle::start`, spawn a Tokio task on the existing runtime holding shared state:

```rust
type Animators = Arc<Mutex<HashMap<String, Animator>>>;
```

Every tick (default **10 Hz**; `--rate <hz>` CLI flag + `:rate` command) the task locks the map, samples each animator at `t = now − start`, and writes via the store (`set_value` / `put_nt`). Values update **even while a modal is open**, and monitors fire at real cadence — a genuine simulator. The UI thread only mutates the map (add/remove animators) and keeps reading current values for display.

Shutdown: the task is aborted alongside the server in `main`.

### 4.3 UI reflection

- Scalar rows already refresh from the store each Browse tick; **extend refresh to enum rows** (read index → `i (choice)`).
- **Animated rows are marked with a leading `~`** in the Value column (`~0.83`); no new column.
- The **selected row's animator + params** are shown in the status bar (`SIM:X ~ sine amp=5 period=2`).

### 4.4 Interaction rules

- Manual `:set`/edit on an animated PV **removes its animator first** (status notes it) — manual control wins.
- `:del` and `:stop` remove the animator; deleting a PV drops its map entry.

---

## 5. Wizard changes

Kept for discoverability; gains an **`[e]num`** branch (name → choices → index → access). **Table is command-line-only** — a per-column typed-array modal chain is disproportionately painful; documented as a deliberate cut. `:help` and the title-bar hint advertise the command line for tables and bulk work.

Title-bar hint updated: `a add · e edit · d del · : cmd · ? help · q quit`.

---

## 6. Testing

Pure surfaces get unit tests; the async loop/tick stay manual-smoke (consistent with the existing binary and `spexplore`/`spsearch`).

- **Server crate (TDD):** `add_enum` and `add_table` — record inserted, exact type preserved, gettable; writable→mbbo/aao vs read-only→mbbi/aai.
- **`parse_command`:** every verb + shorthand, typespec aliases, access default, enum/table value forms, malformed input errors.
- **`expand_pattern`:** ranges (asc/desc/step), zero-padding width, list, cartesian product, the 1000 cap, malformed braces.
- **`sample`:** each generator at known `t` (sine at quarter periods, ramp wrap, square duty, count/walk state advance, cycle index wrap); integer rounding + range clamp.
- **Optional wire test:** extend `sptable_dynamic.rs` with an enum add + `:set`-by-choice over the wire.

---

## Module layout

To keep `spvirit_table.rs` from ballooning, split the pure logic into sibling modules under the binary (declared with `#[path]` or a small `bin/` submodule dir):

- `parse` — `WireType::from_token`, `parse_scalar`/`parse_array`, `parse_command`, `SpecInput`, typespec parsing.
- `pattern` — `expand_pattern`.
- `anim` — `Generator`, `Animator`, `sample`.
- `spvirit_table.rs` — model (`PvSpec`, `PvRow`, `App`, `Mode`), `ServerHandle`, rendering, input handling, the tick task, `main`.

Each pure module carries its own `#[cfg(test)]` tests.

---

## Out of scope (YAGNI)

- Per-PV distinct initial values in a pattern (use `:set` / `:source`).
- Table/NDArray animation; NDArray creation of any kind.
- Command history / tab-completion in the command line.
- Persistence — still fully in-memory.
