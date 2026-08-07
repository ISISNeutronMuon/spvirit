# spvirit-types & spvirit-codec — Foundation Crates

These two crates are the foundation of the workspace: `spvirit-types` is the pure
data model, `spvirit-codec` is the wire format. Everything else (`-client`,
`-server`, `-tools`, `-py`) depends on both.

```
spvirit-types  (NtPayload, NtScalar, ScalarValue, PvValue, …)   ← zero dependencies
      │
      ▼
spvirit-codec  (PVA wire codec + PVD codec + connection state tracker)
      │             re-exports spvirit_types at crate root (lib.rs:51)
      ▼
spvirit-client / spvirit-server / spvirit-tools / spvirit-py
```

Deps: `spvirit-types` has none; `spvirit-codec` uses only `spvirit-types`,
`hex`, `tracing`. Edition 2024 throughout.

## spvirit-types

The entire crate is one file, `spvirit-types/src/lib.rs` (~617 lines): pure
structs/enums for the Normative Type (NT) data model, plus builder methods and
validation. No I/O, no wire format.

| Type | Location | Role |
|---|---|---|
| `ScalarValue` | lib.rs:9 | Tagged union of the twelve NTScalar value types (Bool, I8–I64, U8–U64, F32, F64, Str) |
| `ScalarArrayValue` | lib.rs:25 | Array counterpart; `len()`, `element_size_bytes()`, `type_label()` at lib.rs:40–95 |
| `NtAlarm` / `NtTimeStamp` / `NtDisplay` / `NtControl` | lib.rs:98–147 | Normative sub-structures |
| `NtScalar` | lib.rs:150 | The big one: value + flattened alarm/display/control/valueAlarm/units + **optional `time_stamp`** (lib.rs:185) |
| `NtScalarArray` | lib.rs:352 | Array payload |
| `NtTable` / `NtTableColumn` | lib.rs:373–405 | Table; `validate()` checks column-length equality |
| `NtNdArray` + `NdCodec`/`NdDimension`/`NtAttribute` | lib.rs:408–504 | Image/detector model; `validate()` checks dims × element size vs `uncompressed_size` |
| `NtEnum` | lib.rs:524 | index + choices; `selected()` |
| `PvValue` | lib.rs:560 | **Recursive** value tree (Scalar/ScalarArray/Structure) so this crate can represent arbitrary structures without depending on the codec |
| `NtPayload` | lib.rs:570 | Top-level union: `Scalar/ScalarArray/Table/NdArray/Enum/Generic{struct_id, fields}` — the primary hand-off type between server/client and codec |

`NtScalar::update_alarm_from_value` (lib.rs:285) computes alarm severity from
the HIHI/HIGH/LOW/LOLO limits — this is the server's alarm engine, but note it
lives here in the types crate.

### Footgun: `NtScalar.time_stamp` is `Option`

`None` means the encoder stamps `SystemTime::now()` **at encode time**
(non-deterministic — it breaks monitor delta detection of `secondsPastEpoch`
and makes tests flaky). `Some` is stable. Documented at lib.rs:179–184. The
server now stamps timestamps on every mutation and (in-flight change) at
store-entry time precisely because of this.

## spvirit-codec

| File | ~Lines | Contents |
|---|---|---|
| `lib.rs` | 51 | Module decls + curated re-exports (also re-exports `spvirit_types`) |
| `encode_common.rs` | 28 | `encode_size` (PVA varint) + `encode_string` |
| `error.rs` | 108 | `DecodeError` (the eleven typed decode failures) + `DecodeResult` |
| `segment.rs` | 315 | `SegmentReassembler` / `SegmentOutcome`: sans-io reassembly of segmented PVA messages |
| `monitor.rs` | 621 | **Monitor deltas**: `MonitorUpdate`, `MonitorLayout`, the three bitset-layout body decoders, the lenient scoring heuristic |
| `epics_decode.rs` | 2046 | **PVA wire-format decode**: header, control flags, ~20 command payload structs, `PvaPacket::decode_payload` dispatch, `PvaOpPayload`, `DecodeMode` |
| `spvd_decode.rs` | 1842 | **pvData (PVD) introspection + value decode**: `TypeCode`, `FieldDesc`, `StructureDesc`, `DecodedValue`, `DecodeLimits`, `PvdDecoder` (incl. introspection registry, bitset decode) |
| `spvd_encode.rs` | 2571 | **pvData encode / NT serialization**: struct-desc encoding, per-NT-type encoders, bitset/delta/monitor encoding, PvRequest encode/decode, projection/filtering |
| `spvirit_encode.rs` | 1372 | **PVA wire-format encode**: `encode_header`, all request/response builders (search, create-channel, op init/data/status, monitor, beacon) |
| `spvirit_state.rs` | 1358 | **Connection state tracker**: CID↔SID↔PV-name mapping, operation states, search cache, snapshots/stats (used by the sniffing/diagnostic tools) |

### How the wire protocol works

**Framing.** Every PVA message is an 8-byte header (`magic 0xCA` / version /
flags / command / payload_length) + payload. Decode entry point:
`PvaPacket::new` → `decode_payload` (epics_decode.rs:206), which dispatches on
the command byte (0=Beacon, 1=ConnValidation, 3=Search, 4=SearchResp,
7=CreateChannel, 8=DestroyChannel, 9=ConnValidated, 10–14/16/20=Op,
15=DestroyRequest, 17=GetField, 18=Message, 21=CancelRequest, 22=OriginTag …).
Encode entry point: `encode_header` (spvirit_encode.rs:66); each
`encode_*_response/request` builds a payload then prepends the header.

**Byte order** is decided per-packet by header flag bit 7. Every integer
read/write branches on `is_be` — there is no abstraction layer, the
`if is_be {…} else {…}` pattern is repeated everywhere. Connections cache
their order in `ConnectionState.is_be` (defaults little-endian).

**Sizes/strings** use the PVA varint: 1 byte < 254, `0xFE` + u32 above, `0xFF`
= null. **Warning:** four near-identical size codecs exist
(`encode_common::encode_size`, `epics_decode::decode_size`,
`spvd_encode::encode_size_pvd`, `spvirit_encode::encode_size_pva`) — if you
change one, check the others.

**Introspection / type descriptors.** PVD structures are described by
`FieldDesc`/`StructureDesc` trees. Parse path: `PvdDecoder::parse_field_desc`
(spvd_decode.rs:402) → `parse_type_desc` → `parse_structure_desc`. Tag bytes:
`0x80` structure, `0x81` union, `0x82` variant (+`0x08` for array forms),
scalar/array mode bits `& 0x18`, base type `& 0xE7`. Encode side:
`spvd_encode::encode_structure_desc` (spvd_encode.rs:33) plus per-NT descriptor
builders (`nt_scalar_desc`:274, `nt_scalar_array_desc`:708, `nt_table_desc`:764,
`nt_ndarray_desc`:933, `nt_enum_desc`:1191, dispatcher `nt_payload_desc`:1304).

**The introspection registry is stateful.** `0xFD` = "full type with id"
(parse + cache under a u16 key), `0xFE` = "only id" (look up cached type).
The registry lives inside a `RefCell` in `PvdDecoder` (spvd_decode.rs:327) —
**a single `PvdDecoder` instance must be reused across a connection's packets**
or `0xFE` references won't resolve. This is easy to get wrong.

**Bitsets (monitor deltas).** Bit 0 = whole structure; field bits start at
bit 1; nested structs consume a contiguous bit block (`count_structure_fields`,
spvd_decode.rs:1204, flattens the count). Decoding is **spec-exact by
default** — changed bitset, data, overrun bitset — via
`PvdDecoder::decode_monitor_update` (monitor.rs:116), which returns a
`MonitorUpdate` (monitor.rs:19): the decoded value, both raw bitsets, the
bytes consumed, and the bit-indexed field paths captured at decode time
(`changed_paths`:65, `overrun_paths`:71, `overrun_fields`:58 — the last takes
the descriptor explicitly). `PvaOpPayload::decode_with_field_desc`
(epics_decode.rs:1473) selects the policy with a `DecodeMode`
(epics_decode.rs:1512); every call site in the workspace passes
`DecodeMode::Strict`. `DecodeMode::Lenient` retains the old
try-every-layout scoring heuristic — `decode_monitor_update_lenient`
(monitor.rs:140) reports the winning `MonitorLayout` (monitor.rs:38) — for
mid-stream captures and peers that disagree on the ordering. It is public API
for out-of-tree consumers; nothing in the workspace selects it. The
single-bitset decoders `decode_structure_with_bitset` (spvd_decode.rs:1005)
and `pub(crate) decode_structure_with_bitset_body` (:1035) are what
`monitor.rs` builds on. Encode side: `encode_nt_payload_delta`
(spvd_encode.rs:1916), `compute_changed_bits`:1812,
`encode_structure_bitset`:464.

The path order in `flatten_field_paths` (monitor.rs:99) must stay in step with
`count_structure_fields`, which numbers the bits: depth-first, self then
nested. If the two diverge, overrun bits map to the wrong field names.

**Op payloads.** `PvaOpPayload::new` (epics_decode.rs:1371) handles client vs
server field-offset differences, the conditional status prefix, PV-name
extraction, and parses introspection on INIT responses. `decoded_value` is
filled later once a `field_desc` is known.

**Value decode** (`PvdDecoder::decode_value`, spvd_decode.rs:820) is recursive
over `FieldType`. Every array count is checked before anything is allocated
(`check_array_count`, spvd_decode.rs:797): a count above its `DecodeLimits`
ceiling is `DecodeError::ArrayTooLarge`, and a count that cannot fit in the
bytes that remain is `DecodeError::CountExceedsBuffer`. The limit is checked
first, so a corrupt length that violates both is reported as `ArrayTooLarge`.
**Nothing is silently truncated.** `DecodeLimits` (spvd_decode.rs:21) defaults
to 4 000 000 elements for scalar arrays and 65 536 each for string, structure,
union and variant arrays; override per decoder with `PvdDecoder::with_limits`
(spvd_decode.rs:336) and read them back with `limits()`.

**Segmentation is reassembled by the codec.** The flags byte carries
first/middle/last segment bits, decoded into `PvaControlFlags`
(epics_decode.rs:75). `SegmentReassembler` (segment.rs:34) turns a run of them
back into one message: `push` (segment.rs:65) takes an 8-byte header plus its
payload and answers `SegmentOutcome::Pending`, `::Complete` — the first
segment's header with the segment bits cleared and `payload_length` rewritten
to the concatenated total — or `::Control`, since control frames are legal
between the segments of one message and leave the in-progress state alone. It
is sans-io and holds one message at a time, so it needs **one instance per
connection** — the same lifetime rule as `PvdDecoder`. The ceiling is
`DEFAULT_MAX_MESSAGE_BYTES` (segment.rs:7, 256 MiB), overridable with
`with_max_bytes` (segment.rs:46). `spvirit-client`'s `read_packet`
(transport.rs:63) and the server handler (handler.rs:834) both drive one.

**Emission is still not implemented.** Nothing in the workspace splits an
oversized outgoing message into segments; the server sends one large frame
regardless of size. Roadmap item 6 in
[Current State and Roadmap](08-current-state.md).

### Connection state tracker (`spvirit_state.rs`)

`PvaStateTracker` is fed protocol events (`on_search`,
`on_create_channel_request/response`, `on_op_init_request/response`,
`on_op_activity`, `on_destroy_channel`, …) and maintains CID↔SID↔PV-name
maps, per-operation `field_desc`s, a search cache, TTL-based cleanup (5 min /
40 k channels default) and snapshot/stats reporting. PV-name resolution
(`resolve_pv_name`, spvirit_state.rs:741) is deliberately best-effort for
mid-stream packet captures — the single-channel fallback is disabled when
multiple ops exist to avoid mis-attribution on multiplexed connections
(Phoebus). Used by the diagnostic TUI tools, not by the server/client
runtimes.

### Data-flow asymmetry (important)

NT types flow **into** the encoder (`spvd_encode` takes `spvirit-types`
structs → wire bytes + `StructureDesc`). The decoder emits `DecodedValue`, a
separate codec-local tree — **there is no `DecodedValue → NtPayload` reverse
mapping in these crates**; each consumer interprets `DecodedValue` itself
(see `spvirit-server/src/convert.rs`, `spvirit-py/src/convert.rs`).

### Known issues & sharp edges

- `PvaHeader::new` panics on < 8 bytes; use `try_new` for untrusted input.
- `StructureArray` null elements decode to an empty-struct placeholder, not a
  true null — lossy.
- `decoded_to_scalar_value`-style truthy-first conversions live in consumers;
  see the server chapter for the bool-coercion bug that every `PvScalar` impl
  works around.
- **No captured-packet fixture corpus.** All tests build byte arrays inline or
  round-trip encode→decode. There is no regression corpus of real EPICS
  traffic — consider adding one (e.g. captures from pvxs/p4p/PVAccessJava)
  before making codec changes.
- There are **no TODO/FIXME markers** in either crate; incomplete areas are
  only discoverable by reading. The list above is the current known set.

### Tests

All inline `#[cfg(test)]` modules at the bottom of each file: spvd_encode 18,
spvirit_encode 20, spvirit_state 15, spvd_decode 18, epics_decode 10,
monitor 10, segment 10, error 3, types 2.
Round-trip oriented (encode → decode → assert equality). The state-tracker
tests (spvirit_state.rs:1023–1350) double as documentation of the intended
state-machine semantics. Run with `cargo test -p spvirit-codec -p spvirit-types`.
