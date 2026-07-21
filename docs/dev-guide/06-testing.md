# Testing Guide

## The four layers

| Layer | Where | Run with |
|---|---|---|
| Rust unit tests | inline `#[cfg(test)]` modules in every crate's `src/` | `cargo test --all` |
| In-process protocol/integration tests | `spvirit-tools/tests/` (19 test files + shared harnesses, ~33 test fns) | `cargo test -p spvirit-tools` |
| Cross-implementation interop tests | `spvirit-tools/tests/interop_*.rs` + `tests/interop/` | env-gated; see below |
| Python tests | `spvirit-py/tests/test_pv_handles.py` + `test_value_types.py` | run the file directly with the venv python |

There are **no benchmarks** (no `benches/`, no criterion) and **no
golden-file/packet-capture fixtures** — codec tests are inline-bytes and
round-trip based. Both are worth adding.

## Rust unit tests

Every crate keeps tests at the bottom of each source file. Notable suites:

- `spvirit-codec`: spvd_encode (18), spvirit_encode (20), spvirit_state (15 —
  these double as the state-machine spec), epics_decode (9), spvd_decode (7).
- `spvirit-server`: simple_store (MDEL, timestamps, all 12 array element
  types, put_nt, validator rejection), pv (constructors, attach guards),
  pva_server (builder wiring, links), monitor (delta-frame semantics), db,
  record_fields, group.
- `spvirit-client`: ~39 tests (search target math, parsing, round-trips).

## In-process protocol tests (`spvirit-tools/tests/`)

Two harnesses under `tests/protocol/`:

- `frame_harness.rs` — `TestServer`/`TestSession`: spawns actual workspace
  binaries (locates `target/<profile>/`) and speaks raw frames.
- `scenario_harness.rs` — `ScenarioSession`: higher-level
  connect/handshake/get/put helpers.

Used by `spvirit_protocol_*` (codec, lifecycle, unsupported-command
handling), `spvirit_pvlist`, `spvirit_nt_codec`/`spvirit_nt_lifecycle`,
`spvirit_record_array`, `spvirit_monitor_fields` (nested pvRequest
selection), `spvirit_monitor_pipeline` (flow control), `ioc_fields`
(`PV.FIELD`/`FIELD$`), `pv_handle_api`, `spvirit_search_resilience`,
`spvirit_get` (incl. read-only/simulation PUT authorization).

## Interop tests (against real EPICS implementations)

`tests/interop/harness.rs` provides `ProcessGuard`, `LocalServerFixture`,
free-port helpers, and sets `EPICS_PVA_ADDR_LIST=127.0.0.1`,
`EPICS_PVA_AUTO_ADDR_LIST=NO`, `EPICS_PVA_BROADCAST_PORT` for hermetic runs.

All interop tests are **env-gated and skip silently** unless enabled:

| Suite | Needs | Enable |
|---|---|---|
| p4p (pvxs) | `pip install p4p` | `PVA_TEST_P4P=1`, `P4P_PROVIDER_CMD` → `tests/interop/p4p_server.py`, `P4P_TEST_SERVER=127.0.0.1:5075` |
| EPICS Base | EPICS Base install | its own enable vars (see `interop_epics_base.rs`) |
| PVAccessJava | Gradle project | see `interop_pvaccess_java.rs` |
| Generic external server | any PVA server | `PVA_TEST_SERVER`, `PVA_TEST_PV`, `PVA_TEST_MONITOR`, `PVA_TEST_*` addr/port vars |

CI runs the p4p matrix on every push/PR (see chapter 07). The command:

```bash
PVA_TEST_P4P=1 P4P_PROVIDER_CMD="python spvirit-tools/tests/interop/p4p_server.py" \
  cargo test -p spvirit-tools --test interop_tool_matrix -- p4p_provider_matrix
```

## Python tests

Plain-assert style, deliberately not pytest: `test_*` functions collected by
a `main()` loop at the bottom of the file. Conventions:

- Every test that starts a `Server` uses its **own unique port pair**
  (`test_pv_handles.py`: 15075–15206; `test_value_types.py`: 16060–16081,
  within its reserved 16060–16099 range). Pick unused ranges for new files.
- Build first: `.\.venv\Scripts\maturin.exe develop` (debug is fine).
- Run: `.\.venv\Scripts\python.exe tests\test_pv_handles.py` → expect `ALL OK`.

## Conventions for new work

- TDD is the house style: the plan documents in `docs/superpowers/plans/`
  write failing tests first, verify failure, implement, verify pass, commit
  per task. Follow the same rhythm.
- Timestamps in tests: always set explicit `NtTimeStamp`s on payloads you
  assert against — a `None` timestamp is stamped at *encode* time and makes
  monitor-delta assertions flaky (see chapter 02).
- When touching the codec, remember the four duplicated size codecs and the
  absence of a capture corpus — add a round-trip test in the same file as the
  change, and consider checking real captures from pvxs/p4p if the change is
  wire-visible.
