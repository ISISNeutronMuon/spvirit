# spvirit-types & spvirit-codec — Foundation Crates

These two crates are the foundation of the workspace: `spvirit-types` is the pure
data model, `spvirit-codec` is the wire format. Everything else (`-client`,
`-server`, `-tools`, `-py`) depends on both.

```
spvirit-types  (NtPayload, NtScalar, ScalarValue, PvValue, …)   ← zero dependencies
      │
      ▼
spvirit-codec  (PVA wire codec + PVD codec + segment reassembly)
      │             re-exports spvirit_types at crate root (lib.rs:46)
      ▼
spvirit-client / spvirit-server / spvirit-tools / spvirit-py
```

Deps: `spvirit-types` has none; `spvirit-codec` uses only `spvirit-types`,
`hex`, `tracing`. Edition 2024 throughout.

## spvirit-types

The entire crate is one file, `spvirit-types/src/lib.rs` (~734 lines): pure
structs/enums for the Normative Type (NT) data model, plus builder methods and
validation. No I/O, no wire format.

| Type | Location | Role |
|---|---|---|
| `ScalarValue` | lib.rs:9 | Tagged union of the twelve NTScalar value types (Bool, I8–I64, U8–U64, F32, F64, Str) |
| `ScalarArrayValue` | lib.rs:25 | Array counterpart; `len()`, `element_size_bytes()`, `type_label()` at lib.rs:40–99 |
| `NtAlarm` / `NtTimeStamp` / `NtDisplay` / `NtControl` | lib.rs:102–130 | Normative sub-structures |
| `NtScalar` | lib.rs:132 | The big one: value + flattened alarm/display/control/valueAlarm/units + **optional `time_stamp`** (lib.rs:167) |
| `NtScalarArray` | lib.rs:345 | Array payload |
| `NtTable` / `NtTableColumn` | lib.rs:366–405 | Table; `validate()` checks column-length equality |
| `NtNdArray` + `NdCodec`/`NdDimension`/`NtAttribute` | lib.rs:408–523 | Image/detector model; `validate()` checks dims × element size vs `uncompressed_size` |
| `NtEnum` | lib.rs:543 | `index: i32` + `choices: Vec<String>`; `selected()` |
| `PvValue` | lib.rs:579 | **Recursive** value tree (Scalar/ScalarArray/Structure) so this crate can represent arbitrary structures without depending on the codec |
| `NtPayload` | lib.rs:589 | Top-level union: `Scalar/ScalarArray/Table/NdArray/Enum/Generic{struct_id, fields}` — the primary hand-off type between server/client and codec |

`NtScalar::update_alarm_from_value` (lib.rs:281) computes alarm severity from
the HIHI/HIGH/LOW/LOLO limits — this is the server's alarm engine, but note it
lives here in the types crate.

### Footgun: `NtScalar.time_stamp` is `Option`

`None` means the encoder stamps `SystemTime::now()` **at encode time**
(non-deterministic — it breaks monitor delta detection of `secondsPastEpoch`
and makes tests flaky). `Some` is stable. Documented at lib.rs:161–167. The
server stamps timestamps on every mutation and at store-entry time
(`stamp_missing_timestamps`, `spvirit-server/src/types.rs:457`) precisely
because of this.

## spvirit-codec

| File | ~Lines | Contents |
|---|---|---|
| `lib.rs` | 46 | Module decls + curated re-exports (also re-exports `spvirit_types`) |
| `encode_common.rs` | 65 | `encode_size` (PVA varint) + `encode_string` |
| `error.rs` | 257 | `DecodeError` (the twelve typed decode failures) + `DecodeResult`, **plus** `Utf8Policy` and the shared `decode_size_prefixed` / `decode_string_prefixed` core (single source of truth for size-prefix + string decode) |
| `segment.rs` | 343 | `SegmentReassembler` / `SegmentOutcome`: sans-io reassembly of segmented PVA messages |
| `monitor.rs` | 685 | **Monitor deltas**: `MonitorUpdate`, `MonitorLayout`, the three bitset-layout body decoders, the lenient scoring heuristic |
| `epics_decode.rs` | 2295 | **PVA wire-format decode**: header, control flags, ~20 command payload structs, `PvaPacket::decode_payload` dispatch, `PvaOpPayload`, `DecodeMode` |
| `spvd_decode.rs` | 2730 | **pvData (PVD) introspection + value decode**: `TypeCode`, `FieldDesc`, `StructureDesc`, `DecodedValue`, `DecodeLimits`, the introspection node budget, `PvdDecoder` (incl. introspection registry, bitset decode) |
| `spvd_encode.rs` | 4401 | **pvData encode / NT serialization**: struct-desc encoding, per-NT-type encoders, bitset/delta/monitor encoding, PvRequest encode/decode, projection/filtering |
| `spvirit_encode.rs` | 1863 | **PVA wire-format encode**: `encode_header`, all request/response builders (search, create-channel, op init/data/status, monitor, beacon) |

### How the wire protocol works

**Framing.** Every PVA message is an 8-byte header (`magic 0xCA` / version /
flags / command / payload_length) + payload. Decode entry point:
`PvaPacket::new` → `decode_payload` (epics_decode.rs:222), which dispatches on
the command byte (0=Beacon, 1=ConnValidation, 3=Search, 4=SearchResp,
7=CreateChannel, 8=DestroyChannel, 9=ConnValidated, 10–14/16/20=Op,
15=DestroyRequest, 17=GetField, 18=Message, 21=CancelRequest, 22=OriginTag …).
Encode entry point: `encode_header` (spvirit_encode.rs:98); each
`encode_*_response/request` builds a payload then prepends the header.

**Byte order** is decided per-packet by header flag bit 7. Every integer
read/write branches on `is_be` — there is no abstraction layer, the
`if is_be {…} else {…}` pattern is repeated everywhere. Byte order lives
with each consumer's connection state (for example the server's
per-connection `is_be`, or `spvirit-tools`' scry `ConnectionState.is_be`,
which defaults to little-endian); the codec itself is stateless about it.

**Sizes/strings** use the PVA varint: 1 byte < 254, `0xFE` + u32 above, `0xFF`
= null. **Decode is now consolidated:** `error::decode_size_prefixed`
(error.rs:116) and `error::decode_string_prefixed` (error.rs:153) are the
single source of truth. Both `epics_decode::decode_size`/`decode_string`
(epics_decode.rs:335/346) and `PvdDecoder::decode_size`/`decode_string`
(spvd_decode.rs:493/502) are thin wrappers over them — the split that remains
is the deliberate `Utf8Policy` one: framing decodes `Lossy` (passive observer),
structured pvData values decode `Strict` (a non-UTF-8 value is a real error).
**Encode has one public size encoder**
(`spvirit_encode::encode_size_pva` at spvirit_encode.rs:16, which delegates to
`encode_common::encode_size` at encode_common.rs:5). `spvd_encode` keeps
private `encode_size_pvd` / `encode_string_pvd` aliases of the `encode_common`
functions, so a change to `encode_common` covers all three names.

**Introspection / type descriptors.** PVD structures are described by
`FieldDesc`/`StructureDesc` trees. Parse path: `PvdDecoder::parse_field_desc`
(spvd_decode.rs:507) → `parse_type_desc` → `parse_structure_desc`. Tag bytes:
`0x80` structure, `0x81` union, `0x82` variant (+`0x08` for array forms),
scalar/array mode bits `& 0x18`, base type `& 0xE7`. Encode side:
`spvd_encode::encode_structure_desc` (spvd_encode.rs:23) plus per-NT descriptor
builders (`nt_scalar_desc`:275, `nt_scalar_array_desc`:759, `nt_table_desc`:820,
`nt_ndarray_desc`:999, `nt_enum_desc`:1265, dispatcher `nt_payload_desc`:1388).

**The introspection registry is stateful.** `0xFD` = "full type with id"
(parse + cache under a u16 key), `0xFE` = "only id" (look up cached type).
The registry lives inside a `RefCell` in `PvdDecoder` (spvd_decode.rs:432) —
**a single `PvdDecoder` instance must be reused across a connection's packets**
or `0xFE` references won't resolve. This is easy to get wrong.

**Introspection is bounded by a node budget.** Because `0xFE` splices a
stored tree in for three bytes, a chain of registry references can expand
exponentially. Each top-level decode may therefore resolve at most
`max_struct_fields + 16 × input bytes` type nodes, capped at 1,048,576
(`NODES_PER_BYTE`, `MAX_DECODE_NODES`, `node_ceiling`, spvd_decode.rs:370–405).
At about 25 nodes per NTScalar, the largest legitimate message under that
ceiling is roughly 40K NTScalar-sized elements. A message that exceeds the
budget is rejected with `DecodeError::Malformed` (no separate variant).

**Bitsets (monitor deltas).** Bit 0 = whole structure; field bits start at
bit 1; nested structs consume a contiguous bit block (`count_structure_fields`,
spvd_decode.rs:1457, flattens the count). Decoding is **spec-exact by
default** — changed bitset, data, overrun bitset — via
`PvdDecoder::decode_monitor_update` (monitor.rs:125), which returns a
`MonitorUpdate` (monitor.rs:20): the decoded value, both raw bitsets, the
bytes consumed, and the bit-indexed field paths captured at decode time
(`changed_paths`:69, `overrun_paths`:75, `overrun_fields`:62 — the last takes
the descriptor explicitly). `PvaOpPayload::decode_with_field_desc`
(epics_decode.rs:1540) selects the policy with a `DecodeMode`
(epics_decode.rs:1579); every server and client call site passes
`DecodeMode::Strict`. `DecodeMode::Lenient` retains the old
try-every-layout scoring heuristic — `decode_monitor_update_lenient`
(monitor.rs:163) reports the winning `MonitorLayout` (monitor.rs:42) — for
mid-stream captures and peers that disagree on the ordering. It is public
API; in the workspace only `spvirit-scry`'s packet processor selects it
(`spvirit-tools/src/scry/utils/packet_processor.rs`). The
single-bitset decoders `decode_structure_with_bitset` (spvd_decode.rs:1240)
and `pub(crate) decode_structure_with_bitset_body` (:1270) are what
`monitor.rs` builds on. Encode side: `encode_nt_payload_delta`
(spvd_encode.rs:2602), `compute_changed_bits`:2499,
`encode_structure_bitset`:341.

The path order in `flatten_field_paths` (monitor.rs:106) must stay in step with
`count_structure_fields`, which numbers the bits: depth-first, self then
nested. If the two diverge, overrun bits map to the wrong field names.

**Op payloads.** `PvaOpPayload::new` (epics_decode.rs:1442) handles client vs
server field-offset differences, the conditional status prefix, PV-name
extraction, and parses introspection on INIT responses. `decoded_value` is
filled later once a `field_desc` is known.

**Value decode** (`PvdDecoder::decode_value`, spvd_decode.rs:1044) is recursive
over `FieldType`. Every array count is checked before anything is allocated
(`check_array_count`, spvd_decode.rs:958): a count above its `DecodeLimits`
ceiling is `DecodeError::ArrayTooLarge`, and a count that cannot fit in the
bytes that remain is `DecodeError::CountExceedsBuffer`. The limit is checked
first, so a corrupt length that violates both is reported as `ArrayTooLarge`.
**Nothing is silently truncated.** `DecodeLimits` (spvd_decode.rs:23) defaults
to 4 000 000 elements for scalar arrays and 65 536 each for string, structure,
union and variant arrays; override per decoder with `PvdDecoder::with_limits`
(spvd_decode.rs:473) and read them back with `limits()`.

**Segmentation is reassembled by the codec.** The flags byte carries
first/middle/last segment bits, decoded into `PvaControlFlags`
(epics_decode.rs:52). `SegmentReassembler` (segment.rs:34) turns a run of them
back into one message: `push` (segment.rs:77) takes an 8-byte header plus its
payload and answers `SegmentOutcome::Pending`, `::Complete` — the first
segment's header with the segment bits cleared and `payload_length` rewritten
to the concatenated total — or `::Control`, since control frames are legal
between the segments of one message and leave the in-progress state alone. It
is sans-io and holds one message at a time, so it needs **one instance per
connection** — the same lifetime rule as `PvdDecoder`. The ceiling is
`DEFAULT_MAX_MESSAGE_BYTES` (segment.rs:7, 256 MiB), overridable with
`with_max_bytes` (segment.rs:50). `spvirit-client`'s `read_packet`
(transport.rs:400) and the server handler (handler.rs:1568) both drive one.

**Emission is still not implemented.** Nothing in the workspace splits an
oversized outgoing message into segments; the server sends one large frame
regardless of size. Roadmap item 6 in
[Current State and Roadmap](08-current-state.md).

### Data-flow asymmetry (important)

NT types flow **into** the encoder (`spvd_encode` takes `spvirit-types`
structs → wire bytes + `StructureDesc`). The decoder emits `DecodedValue`, a
separate codec-local tree — **there is no `DecodedValue → NtPayload` reverse
mapping in these crates**; each consumer interprets `DecodedValue` itself
(see `spvirit-server/src/convert.rs`, `spvirit-py/src/convert.rs`).

### Known issues & sharp edges

- `PvaHeader::new` (epics_decode.rs:124) and `PvaPacket::new`
  (epics_decode.rs:195) panic on input shorter than the 8-byte header — the
  former via `.expect(...)`. Use the fallible `PvaHeader::try_new`
  (epics_decode.rs:128) / `PvaPacket::try_new` (epics_decode.rs:205), which
  return `None`, on any network-ingress path where the length is
  attacker-controlled (e.g. raw UDP datagrams).
- `StructureArray` null elements decode to an empty-struct placeholder, not a
  true null — lossy.
- `decoded_to_scalar_value`-style truthy-first conversions live in consumers;
  see the server chapter for the bool-coercion bug that every `PvScalar` impl
  works around.
- **No real-traffic capture corpus.** The codec's own tests build byte arrays
  inline or round-trip encode→decode. The only binary fixtures are nine spec
  vectors in `spvirit-tools/tests/protocol/spec_vectors/`. Consider adding
  captures from pvxs/p4p/PVAccessJava before making codec changes.
- There are **no TODO/FIXME markers** in either crate; incomplete areas are
  only discoverable by reading. The list above is the current known set.

### Tests

All inline `#[cfg(test)]` modules at the bottom of each file: spvd_encode 39,
spvd_decode 38, spvirit_encode 25, epics_decode 19, monitor 12, segment 11,
error 6, encode_common 1, types 7.
Round-trip oriented (encode → decode → assert equality). Run with
`cargo test -p spvirit-codec -p spvirit-types`.
