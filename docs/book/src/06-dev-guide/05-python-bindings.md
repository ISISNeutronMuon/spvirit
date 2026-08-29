# spvirit-py — Python Bindings

PyO3 bindings exposing the client, server, typed PV handles (tier 2), the
IOC engine (tier 3), NT payloads, dynamic sources, and the low-level
channel/codec to Python. Distributed on PyPI as `spvirit`; the complete
*user* guide is `spvirit-py/README.md` (the primary user doc). This chapter
covers the internals.

## Packaging

- `Cargo.toml`: package `spvirit-py`, **`version.workspace = true`** — it now
  shares the single workspace version (0.3.4 at time of writing) with every
  Rust crate; it is *no longer* versioned independently. `edition = "2024"`.
  `[lib] name = "spvirit"`, `crate-type = ["cdylib", "rlib"]` — the `rlib` is
  there only so `cargo test -p spvirit-py --no-default-features --features
  test-embed` can link a standalone test binary; the maturin build only ever
  produces/uses the `cdylib`. pyo3 0.24 (`abi3-py39` — one wheel for Python
  3.9+), `pyo3-async-runtimes` **hard-pinned =0.24.0**, tokio 1.47.1, plus
  `serde_json` and `tracing`. Depends on `spvirit-ioc` (tier 3).
- **Features**: `default = ["extension-module"]` (pyo3 links no libpython; the
  hosting interpreter supplies the C API). `test-embed = ["pyo3/auto-initialize"]`
  is test-only, for the `#[cfg(test)]` unit tests that need a real embedded
  interpreter inside the `cargo test` binary (e.g. the asyncio-bridge race
  test in `source.rs` and the ordered-notify tests in that file).
- `pyproject.toml`: maturin backend, distribution name `spvirit`, version
  `dynamic` (from Cargo.toml, so it tracks the workspace version). Pins the
  runtime dependency `spvirit-tools == 0.1.22`.
- **There is no Python source package** — the whole API is the compiled
  module plus three Rust-registered submodules `spvirit.codec`,
  `spvirit.lowlevel` and `spvirit.ioc` (each injected into `sys.modules` at
  import so `from spvirit.ioc import ao` works).
- **No `.pyi` type stubs** — a known gap for a binding whose selling point is
  a typed API.
- `.venv/` is gitignored (the build/test commands assume it exists locally);
  any `dist/` wheels/sdists on disk are gitignored build output, not committed.

Dev loop:

```powershell
cd C:\spvirit\spvirit-py
.\.venv\Scripts\maturin.exe develop          # debug build, importable in the venv
.\.venv\Scripts\python.exe tests\test_pv_handles.py
```

## Module map (`spvirit-py/src/`, ~7,600 LOC)

| File | LOC | Purpose |
|---|---|---|
| `lib.rs` | 117 | `#[pymodule]` — the authoritative index of the public surface; registers classes, 16 factory functions, submodules; initializes the runtime bridge; pins `threading._main_thread` at import |
| `runtime.rs` | 64 | Shared Tokio runtime (`LazyLock`); `block_on_py` (releases GIL, re-entrancy aware via `block_in_place`); `future_into_py` (asyncio bridge) |
| `errors.rs` | 139 | Exception hierarchy under `SpviritError`; `TimeoutError`/`IoError` dual-inherit builtins |
| `convert.rs` | 573 | Value conversion layer, including the typed (strict) coercion layer (see below) |
| `pv.rs` | 995 | `PyPv` + `PvKind`, all factories, on_put/scan/calc bridging — the central tier-2 file |
| `server.rs` | 1,144 | `PyServerBuilder`, `PyServer`, `PyStore`, `PyEventDecorator` (lifecycle/event hooks) |
| `client.rs` | 583 | `PyClientBuilder`, `PyClient`, `PyGetResult`, `PyDiscoveredServer`, `PySubscription`, discovery |
| `nt.rs` | 740 | NT wrapper classes, `NtPayload`↔Python bridging |
| `source.rs` | 1,225 | Python-defined dynamic `Source`s: `PyNotifier`, `PySourceAdapter`, sync/async bridging, field-tier plumbing, and embedded ordered-notify/asyncio unit tests |
| `ioc.rs` | 366 | Tier 3: `spvirit.ioc.*` record constructors, `PyRecordSpec`, and `spvirit.Ioc` (the IOC engine/store) |
| `channel.rs` | 636 | `spvirit.lowlevel.Channel` — persistent single-PV TCP connection |
| `codec.rs` | 500 | `spvirit.codec` — FieldDesc/StructureDesc wrappers, packet decode helpers |
| `discovery.rs` | 300 | `spvirit.lowlevel` search/discover/pvlist functions |
| `packet.rs` | 172 | `spvirit.lowlevel.Packet` — owned PVA frame |
| `monitor_update.rs` | 88 | `spvirit.lowlevel.MonitorUpdate` — value + changed/overrun dotted paths handed to monitor callbacks; `dispatch_monitor_update` |

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
3. **`source.rs:324–331`** — `async def` source methods are submitted to a
   dedicated long-lived asyncio event loop on its own Python thread
   (`run_coroutine_threadsafe`), then `.result()` blocks with the GIL
   released — this avoids the nested-`run_until_complete` deadlock.
4. **`source.rs:250–284`** — `Notifier.notify` delivers monitor updates
   **synchronously and in order**. The in-runtime branch used to
   `handle.spawn(...)` the delivery fire-and-forget, which let `notify(v2)`
   overtake `notify(v1)` (computing v2's changed-bit delta against the wrong
   snapshot) and silently dropped delivery panics. It now blocks on delivery
   — via `block_in_place` when already on a runtime worker, plain `block_on`
   otherwise — so each call returns only once its update has been applied.
   Covered by the ordered-notify tests at the foot of `source.rs`.

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
- **`Server`**: constructor is **fully keyword-only** —
  `Server(*, pvs=None, ioc=None, db_file=None, db_string=None, sources=None,
  port=None, udp_port=None, listen_ip=None, advertise_ip=None,
  compute_alarms=None, beacon_period=None)` (server.rs:695). `ioc=` takes a
  `spvirit.Ioc` (tier 3); `db_file=`/`db_string=` load `.db` text as ordinary
  tier-2 records (deliberately *not* routed through the IOC engine, so an
  existing user's records are not silently upgraded to a self-processing IOC).
  `run()` blocks, `start()` spawns a `std::thread` running
  `RUNTIME.block_on(server.run())`. `server.pv(name)` sniffs the stored NT
  payload to mint a typed handle — it attaches to **any** served record,
  including `long`/unsigned/`float`-distinct ones, which now mint a
  dynamically typed `PvKind::Typed` handle (server.rs:840); it still raises
  `KeyError` for NTTable/NTNDArray/generic-structure records, which have no
  handle representation (use `Store` for those).
- **`Store`**: name-keyed runtime access (`get_value/get_nt/set_value/
  set_array_value/put_nt/pv_names`), all through `block_on_py`. `set_value`/
  `set_array_value`/`put_nt` coerce strictly to the record's *existing* wire
  type and never retype a scalar or scalar-array record (`put_nt` on
  `NtTable`/`NtNdArray` replaces the payload wholesale instead).
- **`Client`**: `get/put/monitor/info/pvlist/subscribe`; `PySubscription`
  runs on a spawned task, has `is_active/error/close`, context-manager
  support, and a `Drop` that aborts the task. **Monitor callbacks now receive a
  `MonitorUpdate`, not the raw value** — both `monitor` and `subscribe` (and
  `Channel.monitor`) route the callback through
  `monitor_update::dispatch_monitor_update` (client.rs:370, 449). The callback
  gets `.value` (the decoded value, as before), plus `.changed`/`.overrun`
  (dotted field paths) and `.has_overrun`; returning `False` unsubscribes,
  any other/`None` return continues.
- **`MonitorUpdate`** (`spvirit.lowlevel.MonitorUpdate`, frozen): built by
  `PyMonitorUpdate::from_update` from a `spvirit_client::MonitorUpdate`. Its
  `changed`/`overrun` getters return `Vec<String>` copies of the resolved
  paths (the underlying decoded-value bitsets). A tier-3 change to back the
  paths with `Arc<[String]>` is **deferred and not implemented** — the class
  hands out plain `list[str]`.
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
  `convert.rs:223` (source.rs:44 imports it) and shared with the rest of the
  typed API.
- **Tier 3 (`ioc.rs`)**: the IOC layer. `spvirit.ioc.ai/ao/bi/bo/longin/longout`
  build a `RecordSpec` (`PyRecordSpec`) using **verbatim EPICS field names**
  (`EGU`, `PREC`, `DESC`, `DRVL`/`DRVH`, `HIHI`, …) passed as `**fields`, *not*
  tier 2's `units=`/`drive_limits=` spellings — passing a tier-2 spelling
  raises `ValueError` (ioc.rs:79). A spec is pending until handed to
  `spvirit.Ioc(records=[...])` (or `db_file=`/`db_string=`, exactly one);
  before binding its handle methods raise `RuntimeError` ("Unbound"). Once
  bound, `rec.get()`/`rec.set(v)`/`rec.set_async(v)` and `rec["EGU"]` (field
  read) work; `rec["FIELD"] = …` and `rec.on_put` **always raise** — field
  writes are deferred to sub-project B, and tier-3 records permanently exclude
  `on_put` (it would run Python inside `process()` under a lock set). `Ioc`
  exposes `record_names()` and `run(*, port=None, udp_port=None)` (a
  `Server(ioc=self).run()` shortcut); `add_record` always raises (records are
  fixed at build). A bound spec belongs to exactly one engine — reusing it
  raises `RuntimeError`.

## The conversion layer

`convert.rs` has two parallel paths:

- **Inference path** (unchanged, used when no `type=` is given): full-fidelity
  **Rust → Python** (every width preserved; `U8` arrays become `bytes`), but
  lossy **Python → Rust** — `py_to_scalar` (convert.rs:167): `bool→Bool`,
  `int→I64`, `float→F64`, `str→Str`; `py_to_scalar_array` (convert.rs:189):
  `bytes→U8`, empty list → `F64`, otherwise sniffs the first element.
- **Typed (strict) path** (convert.rs:380 onward — `py_to_scalar_typed`,
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

- **Some Rust `#[cfg(test)]` unit tests exist** — notably the asyncio-bridge
  race test and the ordered/synchronous-notify tests at the foot of
  `source.rs`. They need a real embedded interpreter, so they run only under
  the test-only feature: `cargo test -p spvirit-py --no-default-features
  --features test-embed`. The rest of the handle-plumbing tests live in
  `spvirit-server` (`cargo test -p spvirit-server pv::`).
- **Python tests** (`tests/`, ~1,500 LOC total) — **plain-assert scripts, not
  pytest**; functions named `test_*` collected by a `main()` loop, run directly
  with the venv's python after `maturin develop`:
  - `test_pv_handles.py` (422 LOC) — typed handles.
  - `test_value_types.py` (341 LOC) — the typed-conversion layer:
    `NtScalar`/`NtScalarArray`/`NtTable`/`NtNdArray` `type=`/`types=`,
    `spvirit.scalar()`, `server.pv()` on long/unsigned records, strict
    coercion errors.
  - `test_ioc_host_api.py` (200 LOC) — tier-3 `spvirit.Ioc`/`spvirit.ioc.*`
    host API.
  - `test_lifecycle_events.py` (283 LOC) — `on_start`/`on_event` server hooks.
  - `test_monitor_overrun.py` (91 LOC) — `MonitorUpdate.changed`/`overrun`.
  - `test_source_fields.py` (160 LOC) — dynamic-source field tiers.

  Each server test uses a unique port pair from a reserved range (e.g.
  `test_pv_handles.py`: 15075–15206; `test_value_types.py`: 16060–16099).
- **Examples**: ~38 scripts under `spvirit-py/examples/` — start with
  `demo_pv_handles.py` (typed handles), `demo_server.py`, the
  `demo_source_*.py` family (dynamic sources), `demo_channel*.py`
  (low-level), `demo_complete_ioc.py`/`demo_db_file.py` (tier-3 IOC), and
  `demo_10k_farm.py`/`demo_stress.py` (scale).
