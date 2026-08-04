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

## 1. A client PUT does not advance the timestamp

**What happens.** Two accepted writes to the same record, seconds apart,
leave `timeStamp` unchanged while `value` moves.

**Why.** `apply_put_to_record`
(`spvirit-server/src/simple_store.rs:558`) writes the value, the alarm, the
display and the control fields, and never touches `time_stamp`. The store
stamps only at registration (`:74`) and in `put_nt` (`:109`).

**Consequence.** Server-driven updates — `scan`, `calc`/`link`, `set_value` —
do stamp, so a mixed IOC looks inconsistent: the scanned PVs move their
timestamps and the client-written ones do not. EPICS Base restamps on record
processing.

**Where it is documented.** [A complete IOC](../03-progressive/complete-ioc.md),
[Records vs raw NT](../01-fundamentals/records-vs-raw-nt.md).

## 2. An enum write is accepted and dropped

**What happens.** `spput SIM:MODE --json '{"value":{"index":2}}'` prints
`OK`. The record does not change.

**Why.** The `NtEnum` arm of `apply_put_to_record`
(`spvirit-server/src/simple_store.rs:615`) accepts a field literally named
`value` carrying a scalar integer. A wire PUT of an enum delivers `value` as
a sub-structure, so no branch matches, `changed` stays false, and the
operation reports success.

**Consequence.** The one failure mode worse than an error: a write that says
it worked. `mbbi`/`mbbo` records are read-only in practice over the wire.

**Where it is documented.** [Enums and binary records](../03-progressive/enums.md),
[`spput`](../04-tools/spput.md).

## 3. `.db` refuses record types that EPICS Base has

**What happens.** Given this file:

```
record(ai, "GAP:OK")        { field(VAL, "1.0") }
record(longin, "GAP:LONGIN"){ field(VAL, "7") }
record(mbbo, "GAP:MBBO")    { field(ZRST, "Off") field(ONST, "On") }
```

```console
$ spserver --db-file gap_test.db
INFO spserver: Loaded DB file 'gap_test.db' with 1 PVs
Record 'GAP:MBBO': type 'mbbo' is not a standard EPICS Base record type and cannot be loaded from .db files
```

One of three records loaded.

**Why.** Two separate holes. `mbbi`/`mbbo` are recognised by
`RecordType::from_db_name` and then rejected by an arm in
`spvirit-server/src/db.rs:550` whose message claims they are not standard
EPICS Base record types — which they are. `longin`/`longout` are not in
`from_db_name` at all, so they fall out through a `?`
(`spvirit-server/src/db.rs:315`) and vanish **with no message whatsoever**.

**Consequence.** A `.db` file written for a real IOC loses records, and the
diagnostic is either misleading or absent. The silent case is the dangerous
one: the count in the startup line is the only clue.

**Workaround.** Create those four types through the handle API
(`Pv::longin`, `Pv::mbbo`, …). See [Record types](record-types.md).

**Where it is documented.** [Serving a .db file](../03-progressive/db-files.md).

## 4. Alarm limits on a handle are published but never evaluated

**What happens.** `Pv::alarm_limits(lolo, low, high, hihi)` puts the limits
in the payload's `valueAlarm` structure. Severity stays `NO_ALARM` however
far the value goes past them.

**Why.** Severity computation is gated on the server-wide `compute_alarms`
flag, which defaults to `false` (`spvirit-server/src/server.rs:55`). The
handle-level limits do not turn it on.

**Consequence.** A PV that looks alarmed to a human reading the metadata and
healthy to anything that reads `severity`.

**Workaround.** `spserver --compute-alarms`, or `.compute_alarms(true)` on
the builder. That path derives MINOR from `LOW`/`HIGH` and MAJOR from
`LOLO`/`HIHI`, and it works.

**Where it is documented.** [Alarms and severity](../03-progressive/alarms.md),
[`spserver`](../04-tools/spserver.md).

## 5. `spput` delivers a rejected write twice

**What happens.** A write a validator rejects reaches the server's `on_put`
twice; an accepted write reaches it once.

**Why.** When the full EPICS-Base-style PUT flow fails, `spput` falls back to
the simple flow without saying so
(`spvirit-tools/src/bin/spvirit_put.rs:214`).

**Consequence.** Any `on_put` with a side effect — a log line, a counter, a
hardware poke — doubles up on exactly the writes you were trying to refuse.

**Workaround.** `--no-flow-fallback`, and idempotent callbacks.

**Where it is documented.** [`spput`](../04-tools/spput.md),
[Reacting to writes](../03-progressive/reacting-to-writes.md).

## 6. `ADEL` is parsed and exposed but not applied

`MDEL` gates monitor posts (`spvirit-server/src/simple_store.rs:535`).
`ADEL` — the archive deadband — is read out of the `.db` file and readable
as a field, and no posting logic consults it. A `.db` that relies on `ADEL`
behaves as though it were absent.

## 7. Wire PUT is not wired for tables, images or generic structures

The `NtTable`/`NtNdArray` arm of `apply_put_to_record`
(`spvirit-server/src/simple_store.rs:608`) logs and returns; `Generic` does
the same at `:635`. The records report themselves as writable, so a client
sees a successful PUT that changed nothing — the same failure shape as gap 2.
Write them server-side with `store.put_nt()`.

## 8. The builder has no `longin`/`longout`

`PvaServerBuilder` covers fifteen record constructors and omits these two,
which both the Rust handle API (`Pv::longin`, `Pv::longout`) and the Python
module do have. Combined with gap 3, the builder and `.db` are the only two
routes that cannot produce a `longin`. The matrix is on
[Record types](record-types.md).

## 9. `CANCEL_REQUEST` is unimplemented

The server answers PVA command `CANCEL_REQUEST` with
`"CANCEL_REQUEST command is not supported"`
(`spvirit-server/src/handler.rs:1773`). `ACL_CHANGE`, `MESSAGE`,
`MULTIPLE_DATA`, `ORIGIN_TAG` and commands 14 and 16 likewise return errors.
Clients that cancel a request rather than destroying the channel will see the
error; the common clients do not.

## What is *not* on this list

Behaviour that is deliberate and merely surprising lives in the chapters, not
here — `spget` accepting exactly one PV name, `spsine` printing nothing on
success, `discover` and `off` suppressing enumeration but not discovery,
`spget`'s value column being a display rendering rather than the wire value.
Each is a gotcha in its own tool page.

The full engineering picture — everything above plus the internal to-do list
— is in
[Current State and Roadmap](../06-dev-guide/08-current-state.md).
