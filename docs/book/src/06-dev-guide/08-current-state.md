# Current State, In-Flight Work, and Roadmap

> Revised for the **0.4.1** release (2026-10-26). This chapter goes stale
> fastest; reconcile against `git log` before acting on anything here.

## What 0.4.1 adds

- The `spvirit-tools` pip wheel ships `spvirit-scry` and
  `spvirit-scry_influx_collector` (the wheel build enables the `scry`
  feature).
- On Windows, scry delay-loads `wpcap.dll` and warns when Npcap is missing.
  The Npcap runtime itself is not bundled.

## What 0.4.0 adds

- A client resolution cache (bounded at 16,384 names) and connection pool,
  with retry-once for get/put/info.
- `MonitorHandle` cancellation.
- Gateway idle-monitor teardown and upstream-disconnect propagation.
- An error (not success) for refused PUTs: an unrepresentable value, an
  out-of-range enum index, a Generic record.
- Working enum wire PUTs.
- Evaluated handle alarm limits.
- A codec introspection node budget (16 nodes per input byte, at most
  1,048,576 per decode).
- IOC scan threads capped at 64 distinct periods, reaped and woken at
  shutdown.
- IOC client writes that accept real PVA clients' PUTs.
- `spload` moved out of the pip wheel.
- `RunningServer::local_addrs` for port-0 servers.

### NTTable metadata and store-entry timestamps (`0ac87d5`)

Purpose: make static/NTTable PVs archivable by the EPICS Archiver Appliance
(it rejects epoch-0 events and NPEs on structures without a top-level
`timeStamp`).

- `spvd_encode.rs`: `nt_table_desc` now includes `descriptor`, `alarm`,
  `timeStamp` fields; `encode_nt_table_full` encodes them (defaulting when
  `None`); new round-trip test `nt_table_wire_format_carries_metadata`.
  Encode order must match descriptor field order; that is the invariant.
- `types.rs`: new `RecordInstance::stamp_missing_timestamps()` fills missing
  timestamps per payload family (NdArray stamps `data_time_stamp` too;
  Generic skipped).
- `simple_store.rs`: calls it in `SimplePvStore::new` and `insert`, plus a
  test asserting all record families get `seconds_past_epoch > 0` on store
  entry. Purely additive; caller-supplied timestamps are preserved.

### Python NT value-type selection (`0819a18`..`d85473b`)

Purpose: let Python select any of the twelve NTScalar wire types
instead of collapsing `int→I64`/`float→F64`. Architecture: shared
type-string parser + strict coercion layer in `spvirit-py/src/convert.rs`
(`parse_scalar_type`, `py_to_scalar_typed`, `py_to_scalar_array_typed`,
`coerce_scalar_value`, `coerce_scalar_array_value`); keyword-only `type=`/
`types=` params across NT classes, factories, and `ServerBuilder`, and a new
`spvirit.scalar()` factory backed by `PvKind::Typed(Pv<ScalarValue>,
TypeCode)`; store put paths coerce to the record's existing wire type.
Errors: `ValueError` (unknown type string), `OverflowError` (out of range),
`TypeError` (wrong kind), matching `spvirit-py/README.md`, the authoritative
user-facing doc.

`spvirit-py/tests/test_value_types.py` (341 LOC, ports 16060–16083) covers: `NtScalar`/`NtScalarArray`
`type=`/`.value_type`, `NtTable`/`NtNdArray` constructors, `spvirit.scalar()`,
`server.pv()` on `long`/unsigned records (no more `KeyError`), `waveform`/
`aai`/`aao`/`pv()` with `type=`, `ServerBuilder` `type=`/`types=` kwargs, and
`Store.set_value`/`set_array_value`/`put_nt` strict coercion. Build:
`.\.venv\Scripts\maturin.exe develop`; tests:
`cargo test -p spvirit-server pv::` and
`.\.venv\Scripts\python.exe tests\test_value_types.py`.

## Known gaps and latent bugs (triage list)

Cross-referenced from the per-crate chapters.

**Crate audit, 2026-08-29 (in 0.4.0).** A per-crate audit of the workspace
led to fixes for its high- and medium-severity findings. The main ones:

- The server monitor writer is the two-lane flat-combining `ConnWriter`
  (`spvirit-server/src/conn_writer.rs`). It coalesces monitor frames on a
  latest-per-ioid lane and keeps control replies on a lossless FIFO lane,
  building self-contained per-subscriber frames (the stored delta serves only
  as a change detector).
- Client-supplied pipeline windows (`nfree`) are clamped to
  `MAX_PIPELINE_WINDOW = 4096` at every assignment from client input (server
  `handler.rs` and `monitor.rs`, and the `spserver` binary).
- Python `Notifier.notify` delivers monitor updates in order rather than via
  fire-and-forget spawns.
- `spvirit-calc` uses `wrapping_rem` to avoid an `i32::MIN % -1` panic.
- The IOC layer prunes dead `field_subs`.

**Codec gaps, 2026-08-07 (`241f4f4`..`ef7d587`).** Three of the
protocol/codec entries below are struck through: segmentation reassembly,
the heuristic monitor bitset ordering, and silent array truncation.

- New codec surface: `error.rs` (`DecodeError`/`DecodeResult`), `segment.rs`
  (`SegmentReassembler`), `monitor.rs` (`MonitorUpdate`, `MonitorLayout`),
  `DecodeLimits` on `PvdDecoder`, and `DecodeMode` on
  `PvaOpPayload::decode_with_field_desc`.
- Client, server, spserver, the tools and the Python bindings all use the
  new APIs.
- Breaking: `PvdDecoder` returns `DecodeResult` rather than `Option`, and
  monitor callbacks receive a `&MonitorUpdate` rather than a bare value.
- Not done: segment emission (roadmap item 6).

**Protocol/codec**
- No TLS anywhere (top roadmap item in README).
- ~~No segmentation *reassembly* in the codec (consumers roll their own)~~:
  fixed by the codec-gaps work in `spvirit-codec/src/segment.rs`'s
  `SegmentReassembler`, driven by the client transport, the server handler
  and spserver. Emission is still missing (roadmap item 6).
- ~~Monitor bitset overrun ordering is heuristic (three variants +
  scoring)~~: fixed. `decode_monitor_update` is spec-exact (changed bitset,
  data, overrun bitset) and is what every workspace call site uses. The
  scoring heuristic survives only behind `DecodeMode::Lenient`, for
  packet-capture consumers (`spvirit-scry` uses it).
- No packet-capture regression corpus; four duplicated size codecs.
- ~~Array decode caps silently truncate; string/struct truncation can
  desync~~: fixed. Over-limit counts are `DecodeError::ArrayTooLarge` and
  counts that cannot fit the remaining buffer are
  `DecodeError::CountExceedsBuffer`, both raised before allocation. Limits
  are configurable via `DecodeLimits`.

**Server**
- CANCEL_REQUEST unimplemented (some clients send it).
- PUT to `Generic` not wired into `RecordInstance::apply_put`: a client PUT
  is refused with an error (only `put_nt` works); `NtTable`/`NtNdArray`/
  `NtEnum` PUTs are wired and restamp like any other record.
- `.db` parser: one-statement-per-line only; cannot load
  longin/longout/mbbi/mbbo/table/ndarray/generic (the repo's only two
  `TODO(follow-up)` markers: `types.rs:45`, `db.rs:562`).
- `compute_alarms` defaults to false; with it on, `.db` limits and
  `Pv::alarm_limits()` both drive severity (evaluated on write, not on
  definition).
- ADEL parsed but not enforced; beacon change counter only bumps on protocol
  PUTs.

**Client/tools**
- Structured puts not fully surfaced (README caveat; `put_encode.rs` is more
  capable than the high-level API exposes).
- Epoch disambiguation heuristic in `format.rs` (UNIX vs 1990 epoch by
  closeness to now).
- The one-shot `establish_channel`/`pvget` path still uses cid 1 on its own
  connection; `PvaClient` operations allocate cids/ioids per pooled
  connection (`PooledConn`).
- spserver: NtTable/NtNdArray DOL links are no-ops; `demo/docker_compose.yml`
  is an empty placeholder.

**Python**
- No `.pyi` type stubs.
- Stale artifacts present but gitignored (not committed):
  `spvirit-py/dist/spvirit-0.1.9.tar.gz`, `spvirit-py/.venv/`.
- "sync-only for phase 1" file headers are outdated.
- ~~The type-collapsing limitation~~: fixed by the value-type selection work
  above. All twelve
  NTScalar wire types are now selectable from Python via `type=`/`types=`
  and `spvirit.scalar()`, with strict coercion. Remaining Python-side gaps:
  `on_put`/`scan` are still unsupported on array PVs. Writes through a
  widened `byte`/`short` handle are now refused rather than wrapped: an
  unrepresentable value raises `TypeError`.

**Process**
- No clippy/rustfmt CI gate; no MSRV; no CONTRIBUTING.md; crates.io releases
  are manual with a history of version-numbering slips.

## Roadmap (from README + observed direction)

1. TLS support in client (and eventually server).
2. Structured put payloads surfaced in the high-level client API.
3. More complete softIOC behaviours and record processing in the server
   (record types in `.db`, table/ndarray PUT, CANCEL_REQUEST).
4. ~~Finish the Python value-types work~~: done; see above.
5. Quality infrastructure: packet-capture regression corpus, benchmarks,
   lint gate in CI, `.pyi` stubs.
6. Segmentation *emission* in the encoder. Reassembly landed in
   `spvirit-codec` with the codec-gaps work above, but nothing in the
   workspace splits an oversized outgoing message into segments; the server
   sends one large frame regardless of size. Needed before spvirit can serve large
   NTNDArray images to clients that expect segmented delivery, and it would
   also let the reassembler be tested end-to-end against our own server
   instead of only against synthetic buffers.
