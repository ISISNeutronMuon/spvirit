# `sptable` — Interactive Spreadsheet IOC — Design

> Status: approved for planning. Date: 2026-07-21.

## Summary

A new ratatui TUI binary, `sptable`, that spawns a PVAccess server in-process
and presents a spreadsheet where **each row is an independent PV**. The user
adds rows to create PVs, edits cells to set live values, and deletes rows to
remove PVs — all against a running server, with monitor clients seeing updates
live. This is a developer/testing tool in the same family as `spexplore` and
`spsearch`.

## Goals

- Spawn a running PVAccess server and dynamically **add** PVs at runtime.
- Support all **12 NTScalar wire types** (double, float, int64/32/16/8,
  uint64/32/16/8, bool, string) **and array** variants of each.
- **Edit** a PV's value inline; the write goes through the full posting
  pipeline so connected monitors update live.
- **Delete** a PV at runtime.
- Reflect **external writes** (from `spput` etc.) back into the grid.
- In-memory only — no persistence in v1.

## Non-goals (v1)

- Persistence / save-load of the grid (explicitly deferred).
- Alarms, alarm limits, MDEL/ADEL configuration per row.
- NTTable / NTEnum / NTNDArray row kinds (scalars + scalar-arrays only).
- Links / calc / on_put callbacks.
- Editing PV *type* after creation (delete + re-add instead).

## Architecture

A new binary in `spvirit-tools`: source `src/spvirit_table.rs`, installed as
`sptable` via the `[[bin]]` table (matching the `spvirit_*.rs` → `sp*`
convention). Uses the existing `tui` feature (ratatui 0.29 + color-eyre).

Two layers:

1. **`ServerHandle`** — a thin async wrapper around `RunningServer` and its
   `store()`. The single choke point for all PV mutation:
   `add_scalar`, `add_array`, `set_scalar`, `set_array`, `remove`,
   `read_value` (for refresh). Headless and unit-testable independent of the
   TUI.
2. **TUI app** — a synchronous ratatui event loop on the main thread. Async
   store calls are driven through a manually-built Tokio runtime via
   `block_on`, exactly as the other client tools do (`CommonClientArgs`
   pattern). The grid is the source of truth for *display*; the store is the
   source of truth for *what is served*.

### Server-crate addition

The server crate has runtime `insert` (`SimplePvStore::insert`,
simple_store.rs:108) but no public "add a typed record to a running server"
convenience and no `remove`. Add both — they are generally useful, not
tool-specific:

```rust
// spvirit-server/src/pva_server.rs — impl RunningServer
pub async fn add_scalar(&self, name: &str, value: ScalarValue, writable: bool)
    -> Pv<ScalarValue>;
pub async fn add_array(&self, name: &str, value: ScalarArrayValue, writable: bool)
    -> PvArray;

// spvirit-server/src/simple_store.rs — impl SimplePvStore
pub async fn remove(&self, name: &str) -> bool;   // true if a record was removed
```

- `add_scalar` picks the record family from the `ScalarValue` variant +
  `writable` flag via the existing `scalar_family_record_type` (pv.rs:542),
  builds the record with `make_scalar_record`/`make_output_record`, calls
  `store.insert`, then mints a bound handle (`Pv::attach`).
- `add_array` mirrors this using the array record constructor
  (`make_array_record`) + `PvArray::attach`.
- The wire type is carried by the `ScalarValue` / `ScalarArrayValue` variant,
  so all 12 types fall out for free.

### The "records are never removed" invariant

`Pv::set` (pv.rs:588-596) contains a benign-TOCTOU comment asserting records
are never removed from `SimplePvStore`. Adding `remove` weakens this. Handling:

- Update that comment to acknowledge removal is now possible.
- The race is only theoretical **in this tool**: the TUI drives add/edit/remove
  serially from one thread, so a `set` never races a `remove` it issued.
- If a `set` targets a just-removed record, `set_value` returns `false` and
  `get_value` returns `None`, so `Pv::set` returns `PvError::NotFound` — a
  clean, correct outcome. `Source::put` already tolerates missing records.

No behavioural regression; the change is additive and the existing tests stay
green.

## Grid model

Each row is one PV with these columns:

| Column  | Meaning |
|---------|---------|
| Name    | PV name (channel name) |
| Kind    | `scalar` or `array` |
| Type    | one of the 12 wire types |
| R/W     | read-only (`ai`/`aai` family) or writable (`ao`/`aao`) |
| Value   | scalar rendered per type; array rendered comma-separated |
| Clients | live subscriber count for the PV (best-effort; see below) |

`Clients` is sourced from the store's per-PV subscriber list. If exposing that
cleanly adds friction, it is the one column that may be dropped in v1 without
affecting core function.

## Interaction

Keybindings (single-key, ratatui event loop):

- `a` — add row. A small modal input flow collects: name → kind
  (scalar/array) → type (12-way pick) → R/W → initial value. On commit,
  `ServerHandle::add_scalar`/`add_array`.
- `e` / `Enter` on a row — edit the Value cell inline. On commit,
  `set_scalar`/`set_array` (full posting pipeline → monitors update).
- `d` — delete the selected row → `ServerHandle::remove`.
- `j`/`k` / ↑/↓ — navigate rows.
- `q` / `Esc` (at top level) — quit; `RunningServer::abort()`.
- A status line shows the last action, parse/validation errors, and the bound
  server address:port (so the user knows where to point `spget`/`spmonitor`).

### Value parsing

- Scalars: parsed per the row's wire type; out-of-range → error in the status
  line, cell unchanged. Reuse the strict-coercion semantics from the Python
  value-types work (unknown/out-of-range/wrong-kind → distinct messages).
- Arrays: comma-separated tokens, each parsed into the element type; any bad
  token rejects the whole edit with an inline error.

## Data flow

- **Add**: `add_scalar/array` → `store.insert` → record is now claimable; a
  client searching afterward finds it (UDP-search responder and `claim` read
  the live map).
- **Edit**: `set_scalar/array` → MDEL gate → monitor registry pushes deltas to
  connected clients.
- **External write** (e.g. `spput`): normal `Source::put` path updates the
  record; the TUI **polls `store.get_value` for visible rows on each render
  tick** so external writes appear in the grid.
- **Delete**: `store.remove` → subsequent `claim` returns `None`; existing
  connected clients see their channel drop on next operation (standard EPICS
  behaviour — no proactive channel teardown in v1).

## Error handling

- Duplicate PV name on add → rejected with a status-line message (do not
  silently replace).
- Empty/invalid name → rejected.
- Value parse/range errors → status line, no store mutation.
- Store operations returning `false`/`None` → surfaced, never panic.

## Testing

- **Server crate** (`cargo test -p spvirit-server`): unit tests for
  `add_scalar` (record inserted, claimable, correct wire type across a
  representative set of the 12 types, `writable` maps to the right family),
  `add_array` (element type + writability), and `remove` (record gone, `claim`
  → `None`, `get_value` → `None`, and `Pv::set` on the removed handle →
  `NotFound`). Placed alongside the existing `pva_server.rs` / `simple_store.rs`
  suites.
- **Tool level** (`spvirit-tools/tests/protocol`): a scenario exercising the
  headless `ServerHandle` layer (no ratatui) — add a PV, GET it via the
  in-process client, edit it, GET again, delete it, confirm the channel no
  longer resolves. Follows the `frame_harness`/`scenario_harness` pattern.
- **Ratatui rendering** is not unit-tested, consistent with `spexplore` and
  `spsearch`.

## Files touched

- `spvirit-server/src/pva_server.rs` — `RunningServer::add_scalar/add_array`.
- `spvirit-server/src/simple_store.rs` — `SimplePvStore::remove`; comment fix.
- `spvirit-server/src/pv.rs` — update the "never removed" TOCTOU comment.
- `spvirit-tools/src/spvirit_table.rs` — new binary (`ServerHandle` + TUI).
- `spvirit-tools/Cargo.toml` — `[[bin]]` entry for `sptable`.
- `spvirit-tools/tests/protocol/…` — headless scenario test.
- Docs: a short section in `docs/dev-guide/04-client-and-tools.md` tools table.
