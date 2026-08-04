# spvirit-py — Python Bindings

PyO3 bindings exposing the client, server, typed PV handles, NT payloads,
dynamic sources, and the low-level channel/codec to Python. Distributed on
PyPI as `spvirit`; the complete *user* guide is `spvirit-py/README.md` (~32 KB,
the primary user doc). This chapter covers the internals.

## Packaging

- `Cargo.toml`: package `spvirit-py`, **versioned independently of the Rust
  crates** (0.1.15 vs 0.1.18 — see chapter 07). `[lib] name = "spvirit"`,
  `crate-type = ["cdylib"]`. pyo3 0.24 (`extension-module`, `abi3-py39` — one
  wheel for Python 3.9+), `pyo3-async-runtimes` **hard-pinned =0.24.0**,
  tokio 1.47.
- `pyproject.toml`: maturin backend, distribution name `spvirit`,
  version `dynamic` (from Cargo.toml).
- **There is no Python source package** — the whole API is the compiled
  module plus two Rust-registered submodules `spvirit.codec` and
  `spvirit.lowlevel` (injected into `sys.modules` at import).
- **No `.pyi` type stubs** — a known gap for a binding whose selling point is
  a typed API.
- Stale artifacts present on disk but **gitignored, not committed**:
  `dist/spvirit-0.1.9.tar.gz` and `.venv/` (both under `.gitignore`; the
  build/test commands assume `.venv/` exists locally).

Dev loop:

```powershell
cd C:\spvirit\spvirit-py
.\.venv\Scripts\maturin.exe develop          # debug build, importable in the venv
.\.venv\Scripts\python.exe tests\test_pv_handles.py
```

## Module map (`spvirit-py/src/`, ~6,300 LOC)

| File | LOC | Purpose |
|---|---|---|
| `lib.rs` | 81 | `#[pymodule]` — the authoritative index of the public surface; registers classes, 16 factory functions, submodules; initializes the runtime bridge |
| `runtime.rs` | 64 | Shared Tokio runtime (`LazyLock`); `block_on_py` (releases GIL, re-entrancy aware via `block_in_place`); `future_into_py` (asyncio bridge) |
| `errors.rs` | 138 | Exception hierarchy under `SpviritError`; `TimeoutError`/`IoError` dual-inherit builtins |
| `convert.rs` | 605 | Value conversion layer, including the typed (strict) coercion layer (see below) |
| `pv.rs` | 997 | `PyPv` + `PvKind`, all factories, on_put/scan/calc bridging — the central file |
| `server.rs` | 835 | `PyServerBuilder`, `PyServer`, `PyStore` |
| `client.rs` | 595 | `PyClientBuilder`, `PyClient`, `PySubscription`, discovery |
| `nt.rs` | 740 | NT wrapper classes, `NtPayload`↔Python bridging |
| `source.rs` | 617 | Python-defined dynamic `Source`s: `PyPvInfo`, `PyNotifier`, `PySourceAdapter`, sync/async bridging |
| `channel.rs` | 635 | `spvirit.lowlevel.Channel` — persistent single-PV TCP connection |
| `codec.rs` | 492 | `spvirit.codec` — FieldDesc/StructureDesc wrappers, packet decode helpers |
| `discovery.rs` | 300 | `spvirit.lowlevel` search/discover/pvlist functions |
| `packet.rs` | 172 | `spvirit.lowlevel.Packet` — owned PVA frame |

## Threading model (read before touching)

Three design comments are the real documentation — read them first:

1. **`runtime.rs:29–42`** — one shared multi-thread Tokio runtime.
   `block_on_py` releases the GIL and detects being already on a runtime
   worker (uses `block_in_place`), so a Python callback running inside the
   runtime can call `pv.set()`/client ops without deadlock. Covered by
   `test_on_put_can_set_other_pvs` and `test_client_usable_inside_callbacks`.
2. **`pv.rs:370–382`** — scan closures run synchronously inside the async
   scan task and therefore **cannot** block-on other PV reads; `None`/raised
   exception falls back to the closure's cached last value.
3. **`source.rs:319–326`** — `async def` source methods are submitted to a
   dedicated long-lived asyncio event loop on its own Python thread
   (`run_coroutine_threadsafe`), then `.result()` blocks with the GIL
   released — this avoids the nested-`run_until_complete` deadlock.

The "sync-only for phase 1" headers on `server.rs:1`/`client.rs:1` are
historical — `PyPv` and `Channel` have async variants now; top-level
Server/Client ops remain blocking. Reconcile the comments when convenient.

## The API surface, mapped to Rust

- **`PvKind`** (pv.rs:29): `F64/Bool/I32/Str/Array` wrapping the server
  crate's `Pv<f64>/Pv<bool>/Pv<i32>/Pv<String>/PvArray`, plus
  `Typed(Pv<ScalarValue>, TypeCode)` — a dynamically typed scalar covering
  all twelve NTScalar wire types, backing `spvirit.scalar()` and any
  `long`/unsigned handle minted by `server.pv()`.
- **Factories**: `ai/ao/bi/bo/string_in/string_out/longin/longout` generated
  by the `pv_ctor!` macro (pv.rs:604), each with keyword-only
  `units/prec/desc/adel/mdel/drive_limits/alarm_limits`; `mbbi`/`mbbo`
  hand-written; `waveform/aai/aao` build `PvKind::Array` and accept a
  keyword-only `type=` for the element type; `scalar(name, initial, *, type,
  writable=False, **opts)` covers all twelve NTScalar wire types via
  `PvKind::Typed`; `calc` bridges `callback(list[float]) -> float`; `pv()`
  infers record type from the initial value (**bool checked before int** —
  `isinstance(True, int)` is True) or, with `type=`, picks the wire type
  explicitly.
- **Callbacks**: `on_put(pv, value)` runs *before* apply; returning `False`
  or raising rejects the PUT on the wire; handle-driven `pv.set()` bypasses
  it. `scan` works as call or decorator; must be attached **before** serving
  (afterwards is a silent no-op — the core only logs a tracing warning).
  `calc` exceptions/non-float returns post `0.0` (asymmetric with scan's
  cache fallback — documented in the README).
- **`Server`**: primary constructor `Server(pvs=[...], sources=[...],
  **kwargs)`; `run()` blocks, `start()` spawns a `std::thread` running
  `RUNTIME.block_on(server.run())`. `server.pv(name)` sniffs the stored NT
  payload to mint a typed handle — it attaches to **any** served record,
  including `long`/unsigned/`float`-distinct ones, which now mint a
  dynamically typed `PvKind::Typed` handle (server.rs:583); it still raises
  `KeyError` for NTTable/NTNDArray/generic-structure records, which have no
  handle representation (use `Store` for those).
- **`Store`**: name-keyed runtime access (`get_value/get_nt/set_value/
  set_array_value/put_nt/pv_names`), all through `block_on_py`. `set_value`/
  `set_array_value`/`put_nt` coerce strictly to the record's *existing* wire
  type and never retype a scalar or scalar-array record (`put_nt` on
  `NtTable`/`NtNdArray` replaces the payload wholesale instead).
- **`Client`**: `get/put/monitor/info/pvlist/subscribe`; `PySubscription`
  runs on a spawned task, has `is_active/error/close`, context-manager
  support, and a `Drop` that aborts the task.
- **NT classes**: `NtScalar`/`NtScalarArray` constructible with an optional
  `type=` picking the wire value type explicitly; `NtTable(columns, *,
  labels=None, types=None, descriptor=None)` and `NtNdArray(value, dims, *,
  type=None)` are now constructible too (previously read-only, returned only
  by `Store.get_nt`). `Enum` and `Generic` payloads cross the boundary as
  plain dicts (nt.rs:709, 716).
- **Dynamic sources**: any duck-typed object with `claim/get/put/names`
  (+optional `rpc/subscribe/on_start`), sync or async.
  `PvInfo.nt_scalar("double", writable)` declares precise wire types via
  type strings — parsed by `parse_type_code`, now centralized in
  `convert.rs` (source.rs:42 imports it) and shared with the rest of the
  typed API.

## The conversion layer

`convert.rs` has two parallel paths:

- **Inference path** (unchanged, used when no `type=` is given): full-fidelity
  **Rust → Python** (every width preserved; `U8` arrays become `bytes`), but
  lossy **Python → Rust** — `py_to_scalar` (convert.rs:199): `bool→Bool`,
  `int→I64`, `float→F64`, `str→Str`; `py_to_scalar_array` (convert.rs:221):
  `bytes→U8`, empty list → `F64`, otherwise sniffs the first element.
- **Typed (strict) path** (convert.rs:411 onward — `py_to_scalar_typed`,
  `py_to_scalar_array_typed`, `coerce_scalar_value`,
  `coerce_scalar_array_value`): used whenever a `type=`/`types=` string is
  given, or when coercing against an existing record's wire type. Every
  Python value is checked strictly against the requested `TypeCode`:
  out-of-range raises `OverflowError`, wrong-kind raises `TypeError`, an
  unrecognized type string raises `ValueError`. This is what makes all
  twelve NTScalar wire types (`byte/short/int32/ubyte/ushort/uint32/uint64/
  float32/...`) reachable from Python — see `spvirit.scalar()`, the `type=`/
  `types=` kwargs across NT classes, factories, and `ServerBuilder`, and
  chapter 08 for how this landed.

## Tests and examples

- **No Rust unit tests in this crate** (it's a cdylib); the Rust-side tests
  for handle plumbing live in `spvirit-server` (`cargo test -p spvirit-server pv::`).
- **Python tests**: `tests/test_pv_handles.py` (422 LOC) and
  `tests/test_value_types.py` (324 LOC, covers the typed-conversion layer:
  `NtScalar`/`NtScalarArray`/`NtTable`/`NtNdArray` `type=`/`types=`,
  `spvirit.scalar()`, `server.pv()` on long/unsigned records, strict
  coercion errors) — **plain-assert scripts, not pytest**; functions named
  `test_*` collected by a `main()` loop. Each server test uses a unique port
  pair (`test_pv_handles.py`: 15075–15206; `test_value_types.py`:
  16060–16081, within the reserved 16060–16099 range). Run directly with the
  venv's python after `maturin develop`.
- **Examples**: 24 scripts under `spvirit-py/examples/` — start with
  `demo_pv_handles.py` (typed handles), `demo_server.py`, the
  `demo_source_*.py` family (dynamic sources), `demo_channel*.py`
  (low-level), `demo_10k_farm.py`/`demo_stress.py` (scale).
