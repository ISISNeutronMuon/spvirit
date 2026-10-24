# Known gaps

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Divergences between what spvirit does and what an EPICS user would expect,
found while writing this book and confirmed by running the code. Each one is
documented where it bites, in the relevant chapter; this page collects them
so you can scan the list before you spend an afternoon on one.

Nothing here is a plan. These are findings, not commitments.

## 1. `.db` refuses record types that EPICS Base has

**What happens.** Given this file:

```
record(ai, "GAP:OK") {
  field(VAL, "1.0")
}
record(longin, "GAP:LONGIN") {
  field(VAL, "7")
}
record(mbbo, "GAP:MBBO") {
  field(ZRST, "Off")
  field(ONST, "On")
}
```

```console
$ spserver --db-file gap_test.db
Error: "DB load error: record 'GAP:LONGIN' of type 'longin' is not yet supported by the .db loader; create it through the host API instead"
```

The whole load aborts at the first unsupported record; none of the three
records is served.

**Why.** `to_record` in `spvirit-server/src/db.rs` has no construction for
`mbbi`/`mbbo` (which `RecordType::from_db_name` recognises) or for
`longin`/`longout` (which it does not), and the typed loader turns either
case into an error naming the record and its type.

**Consequence.** A `.db` file written for a real IOC that contains any of
these four types does not load at all. The error names the first offending
record, so the failure is loud rather than silent, but the remaining
records are not served either.

**Workaround.** Create those four types through the handle API
(`Pv::longin`, `Pv::mbbo`, …). See [Record types](record-types.md).

**Where it is documented.** [Serving a .db file](../03-progressive/db-files.md).

## 2. `spput` delivers a rejected write twice

**What happens.** A write a validator rejects reaches the server's `on_put`
twice; an accepted write reaches it once.

**Why.** When the full EPICS-Base-style PUT flow fails, `spput` falls back to
the simple flow without saying so
(`spvirit-tools/src/bin/spvirit_put.rs:237`).

**Consequence.** Any `on_put` with a side effect — a log line, a counter, a
hardware poke — doubles up on exactly the writes you were trying to refuse.

**Workaround.** `--no-flow-fallback`, and idempotent callbacks.

**Where it is documented.** [`spput`](../04-tools/spput.md),
[Reacting to writes](../03-progressive/reacting-to-writes.md).

## 3. `ADEL` is parsed and exposed but not applied

`MDEL` gates monitor posts (`should_post_update`,
`spvirit-server/src/simple_store.rs:571`).
`ADEL` — the archive deadband — is read out of the `.db` file and readable
as a field, and no posting logic consults it. A `.db` that relies on `ADEL`
behaves as though it were absent.

## 4. Wire PUT is refused for generic structures

The `Generic` arm of `RecordInstance::apply_put`
(`spvirit-server/src/apply.rs`) has no wire PUT path, so a client PUT to a
generic record is answered with a PUT error and the record is left as it
was. `NtTable`, `NtNdArray` and `NtEnum` are wired. Write a generic record
server-side with `store.put_nt()`.

## 5. The builder has no `longin`/`longout`

`PvaServerBuilder` covers fifteen record constructors and omits these two,
which both the Rust handle API (`Pv::longin`, `Pv::longout`) and the Python
module do have. Combined with gap 1, the builder and `.db` are the only two
routes that cannot produce a `longin`. The matrix is on
[Record types](record-types.md).

## 6. `CANCEL_REQUEST` is unimplemented

The server answers PVA command `CANCEL_REQUEST` with
`"CANCEL_REQUEST command is not supported"`
(`spvirit-server/src/handler.rs:1795`). `ACL_CHANGE`, `MESSAGE`,
`MULTIPLE_DATA`, `ORIGIN_TAG` and commands 14 and 16 likewise return errors.
Clients that cancel a request rather than destroying the channel will see the
error; the common clients do not.

## 7. Large values are sent as one frame, never segmented

spvirit reassembles segmented messages it receives — `SegmentReassembler` in
`spvirit-codec/src/segment.rs:34`, driven by the client transport and the
server handler — but it never emits them. `encode_header`
(`spvirit-codec/src/spvirit_encode.rs:66`) never sets the segmentation bits,
and no encode path splits a payload, so a 4 MB NTNDArray goes out as a single
PVA frame with a four-byte payload length where EPICS Base would split it.
Clients that accept large unsegmented frames — pvxs, p4p, PVAJava — read it
fine; a client that enforces a maximum frame size will not. Tracked as
roadmap item 6 in
[Current State and Roadmap](../06-dev-guide/08-current-state.md).

## Gateway (M1): passthrough only

`spgateway` in M1 is a *passthrough* gateway — it resolves, reads, writes, and
monitors PVs across networks, with `readOnly`/`pvlist`/ACF access control
enforced, but the observability layers (metrics, audit, hot reload) are not
yet wired. The divergences below are deliberate for this milestone and are
documented in full on the [`spgateway`](../04-tools/spgateway.md) page.

**Representation.** The proxy round-trips through spvirit's decoded value model,
which does not carry every PVAccess shape losslessly. An array-of-structure
value (e.g. an `NTNDArray` `dimension`) is proxied as an index-keyed structure
rather than a true array; `union`/`any` fields degrade; non-finite floats
(`NaN`/`Inf`) become JSON `null` on the put path; and deeply nested structure
IDs are best-effort.

**Resolution.** Only the first token of a client's `addrlist` is used. A p4p
config with `autoaddrlist:false` plus an explicit unicast `addrlist` now does
perform the unicast search (an earlier defect where it searched nothing at all
is fixed), but it still emits a subnet broadcast alongside the unicast probe —
the underlying `spvirit-client` search branch cannot be changed from the
gateway. Malformed `addrlist`/`interface` entries degrade silently rather than
erroring.

**Control plane.** `names()` (what a downstream `pvlist` sees) reports only the
names this gateway has already claimed, not a true fan-out of each upstream's
`pvlist` — the gateway resolves names dynamically and caches no upstream
`SocketAddr` in M1. `rpc` forwarding is not implemented: there is no general
client RPC entry point yet, so an RPC to the gateway returns
`"gateway RPC forwarding is not implemented in M1"`.

**Access control and ops.** `readOnly`, `pvlist`, and `.acf` access control
*are* enforced (see the [`spgateway`](../04-tools/spgateway.md) page) — this
is no longer a passthrough-only gap. What remains unwired: the `x-spvirit`
metrics, audit, hot-reload, and rate-limit blocks are parsed and validated
but not consumed, and RPC forwarding to upstream servers is not implemented.
The per-client `acf-client` field is likewise parsed and referentially
validated against the `clients[]` list but never consulted at runtime.
Ctrl-C shutdown is immediate: outstanding requests are hard-cancelled rather
than drained gracefully.

## What is *not* on this list

Behaviour that is deliberate and merely surprising lives in the chapters, not
here — `mbbi` refusing client writes (an input record, read-only over the
wire as in EPICS; drive it with `pv.set(index)`), `spget` accepting exactly
one PV name, `spsine` printing nothing on success, `discover` and `off` suppressing enumeration but not discovery,
`spget`'s value column being a display rendering rather than the wire value.
Each is a gotcha in its own tool page.

The full engineering picture — everything above plus the internal to-do list
— is in
[Current State and Roadmap](../06-dev-guide/08-current-state.md).
