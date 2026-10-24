# Architecture Overview

## What this project is

Spvirit is a from-scratch Rust implementation of the EPICS **PVAccess**
protocol: wire codec, client, server (softIOC-like), CLI tools, and Python
bindings. It interoperates with EPICS Base, p4p/pvxs, and PVAccessJava.

If you are new to EPICS, read
[EPICS in 10 minutes](../01-fundamentals/epics-in-10-minutes.md) and
[Normative Types](../01-fundamentals/normative-types.md) first. This guide
assumes that vocabulary.

## Crate dependency graph

```
                 spvirit-types        (pure NT data model, zero deps)
                       │
                 spvirit-codec        (PVA wire format + PVD codec + segment reassembly)
                    ┌──┴──────────┐
             spvirit-client   spvirit-server
                    │              ├──────── spvirit-ioc   (scan/process; server-only)
                    └──────┬───────┘
                    ┌──────┴───────┐
             spvirit-gateway   spvirit-py    (both consume client + server; py also uses spvirit-ioc)
             (proxy engine)    (PyO3 bindings)
                    │
             spvirit-tools
             (CLI binaries; client + server + gateway)
```

The authoritative dependency graph, including `spvirit-calc` (which stands
apart), is the mermaid diagram on the [Crate map](../05-reference/crate-map.md).

- **spvirit-types** — `ScalarValue`, `NtScalar`, `NtPayload`, etc. Everything
  the wire carries, as plain Rust data. Chapter 02.
- **spvirit-codec** — encode/decode for both protocol layers, the PVA message
  layer (headers, commands) and the PVD data layer (introspection
  descriptors, values, bitsets), plus sans-io segment reassembly. Chapter 02.
- **spvirit-client** — search/discovery, channel lifecycle, get/put/monitor/
  info. Chapter 04.
- **spvirit-server** — the `Source` provider model, `SimplePvStore` record
  store, protocol runtime (UDP search, TCP handler, beacons, monitors), `.db`
  parser, and the typed `Pv<T>` handle layer. Chapter 03.
- **spvirit-ioc** — the higher-level IOC layer built on the server: scan and
  process infrastructure. Depends on the server (and codec/types), not the
  client.
- **spvirit-gateway** — the PVAccess proxy engine behind the `spgateway`
  binary: it consumes both the client (to reach upstream servers) and the
  server (to face downstream clients). Chapter 04 (`spgateway`).
- **spvirit-tools** — 13 CLI binaries in a default build (`spget`, `spput`,
  `spmonitor`, `spexplore`, `spserver`, `spgateway`, …), plus `spload` behind
  the non-default `load` feature and `spvirit-scry` /
  `spvirit-scry_influx_collector` behind the non-default `scry` feature (needs
  Npcap/libpcap), and the workspace's integration/interop test suite. Depends on `spvirit-gateway` for the
  gateway binary. Chapter 04.
- **spvirit-py** — PyO3 bindings mirroring the handle API in Python,
  plus Python-defined dynamic sources and a low-level channel/codec surface.
  Chapter 05.

## The two protocol layers

PVAccess is two nested encodings, and the codec keeps them in separate
modules:

1. **PVA message layer** (`epics_decode.rs` / `spvirit_encode.rs`): 8-byte
   header (magic 0xCA, flags carrying byte order + segmentation, command
   byte, payload length), ~20 command types (Search, CreateChannel, the Op
   family for GET/PUT/MONITOR/RPC, Beacon, …).
2. **PVD data layer** (`spvd_decode.rs` / `spvd_encode.rs`): self-describing
   structured data — type descriptors (`FieldDesc`/`StructureDesc`) with a
   per-connection introspection cache, values (`DecodedValue`), and bitsets
   for delta updates.

Key asymmetry to internalize: **NT types flow into the encoder; the decoder
emits `DecodedValue`**, a separate tree. Consumers (server, client, py) each
convert `DecodedValue` to what they need — there is no shared reverse
mapping.

## Server data flow (the picture to keep in your head)

```
                     ┌────────────── PvaServer::run ──────────────┐
 client SEARCH ──► UDP responder          TCP handler ◄── client TCP
                        │                  │       ▲
                        │            decode PUT    │ frames (ConnWriter)
                        ▼                  ▼       │
                  SourceRegistry ──► SimplePvStore ──► MonitorRegistry
                  (priority list)    (records, MDEL,     (self-contained
                   builtin @0        validators, links,   frames, pipeline
                   ioc @5 (if any)   on_put, timestamps)  credits)
                   record-fields @10
                   user sources)           ▲
                                           │ set_value / set (internal writes)
                              scan tasks / Pv<T> handles / links
```

Two write entry points converge on the store: wire PUTs (via the handler →
registry → `Source::put`) and internal writes (scan callbacks, `Pv::set`,
link evaluation → `store.set_value`). A third, for sources whose values
change upstream (gateway, group), is a per-PV pump task that drains
`Source::subscribe` — started at monitor init for any source that does not
self-notify (`Source::pushes_own_updates`). All three end at
`MonitorRegistry::notify_monitors`, which builds a self-contained,
fully-filtered frame per subscriber (the stored delta baseline serves only as
a change detector, never as wire output) and hands the bytes to each
connection's `ConnWriter` — a two-lane flat-combining writer that coalesces
monitor frames on a latest-per-ioid lane while control replies take a
lossless FIFO lane.

## Two API levels, everywhere

The project consistently offers an IOC-style record level and a raw-NT level
(the [Records vs raw NT](../01-fundamentals/records-vs-raw-nt.md) page is the
user-facing version of this):

| | Record level | Raw NT level |
|---|---|---|
| Rust server | `Pv<T>` handles, builder methods, `.db` files | `put_nt`/`get_nt`, hand-built `RecordInstance`, custom `Source` |
| Python | `spvirit.ai(...)` etc., `Server(pvs=[...])` | `Store.put_nt`, `NtScalar`/`NtTable` classes, dynamic sources + `Notifier` |
| Behavior | alarms computed, timestamps stamped, MDEL applied | caller owns metadata; every post goes out |

Keep new features consistent with this split: record-level conveniences
should be sugar over the NT level, not a parallel implementation.

## Design invariants worth knowing before changing anything

1. **Timestamps**: an `NtScalar` with `time_stamp: None` is stamped with
   `now()` at *encode* time, which breaks monitor deltas. A `None`
   table/NDArray timestamp encodes as epoch 0, which the Archiver Appliance
   rejects. The store stamps missing (`None`/epoch-0) timestamps on entry,
   and every mutation path stamps update time; keep it that way.
2. **The introspection registry is per-connection state** — decoders must be
   reused across a connection's packets (0xFE type refs). The same lifetime
   rule applies to `SegmentReassembler`: one per connection, because it holds
   the message currently being reassembled.
3. **First-claim-wins source priority** — `claim` must be cheap and
   idempotent because `get`/`put`/`subscribe` re-claim.
4. **on_put (post-apply, can't reject) vs PUT validator (pre-apply, can
   reject)** are different mechanisms; the Python handle-level `Pv.on_put`
   maps to the validator, and the builder-level `ServerBuilder.on_put(name,
   cb)` maps to the post-apply callback and cannot reject.
5. **Monitor bitset ordering is spec-exact by default** — changed bitset,
   data, overrun bitset — and every workspace call site uses it. The old
   try-every-layout scoring heuristic still exists behind
   `DecodeMode::Lenient` for mid-stream captures of implementations that
   disagree; the packet-capture tool `spvirit-scry` uses it
   (`spvirit-tools/src/scry/utils/packet_processor.rs`), so do not delete it
   as dead code.
6. **Bool coercion trap**: `decoded_to_scalar_value` checks truthiness before
   numeric types; typed paths override `from_decoded` to avoid it.
