# Records vs raw NT

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Everything on the wire is a Normative Type, but Spvirit gives you two levels
at which to work — and it pays to know which one you are on. Choosing wrong
is the most common source of "why is my timestamp zero" and "why does my
monitor fire on every tick".

## The comparison

|  | IOC-style records | Raw NT payloads |
|---|---|---|
| You create them with | `Pv<T>` handles (`Pv::ai(...)` …), builder methods (`.ai()`, `.waveform()` …), `.db` files | `NtScalar`/`NtScalarArray`/… built by hand; hand-built `RecordInstance`; custom `Source` impls |
| You read/write | plain values: `pv.set(21.5)`, `store.set_value(...)` | whole payloads: `store.put_nt(...)` / `get_nt(...)`, `Notifier` posts |
| Alarm state | computed for you from HIHI/HIGH/LOW/LOLO limits (`compute_alarms`), or `pv.set_alarm(...)` | you set `alarm` on every payload yourself |
| Timestamps | stamped automatically on every post | yours to fill in — an explicit `timeStamp` is honoured, a zero one is stamped for you |
| Display/control metadata (EGU, PREC, limits) | record fields, visible QSRV-style (`PV.EGU`, `PV.DESC`, …) | whatever you put in the payload, each update |
| Monitor deadbands (MDEL/ADEL) | applied by the server | not applied — every `put_nt`/notify posts |
| Best for | soft IOCs, simulators, anything that should feel like an EPICS record | gateways/bridges, tables, images, PVs whose metadata changes per update |

## Rule of thumb

Stay IOC-style — `Pv<T>` handles first, `.db` files for existing databases —
until you need per-update control of the metadata, or a payload shape the
record layer does not model. Then drop to `put_nt`/`get_nt`, hand-built
records, or a custom `Source`.

The two mix freely in one server. `store.get_nt()` returns the full payload
of an IOC-style record too, so you can start with records and reach through
to the raw layer for the one PV that needs it.

## The three consequences that bite

**Deadbands only exist at the record level.** `MDEL` and `ADEL` are applied
by the server when an IOC-style record's value is set. A `put_nt` bypasses
that entirely: every post goes out. If you have a 1 kHz raw-NT source and a
monitor client, you are sending 1000 updates a second, and no amount of
`MDEL` in a `.db` file will change that.

**Timestamps are only automatic at the record level** — though the raw level
is forgiving. Post a payload with a zero `timeStamp` and the server stamps it
for you; post one with a real `timeStamp` and it is honoured. That is
deliberate, so a gateway can forward the *originating* timestamp rather than
the time it happened to relay the value.

**Alarms are only computed at the record level.** `compute_alarms` walks the
`HIHI`/`HIGH`/`LOW`/`LOLO` thresholds and sets severity. Raw payloads get the
severity you put in them, which for a hand-built `NtScalar::from_value` is
`NO_ALARM` — a silently un-alarming PV.

## Which one am I on?

If you called `.ai()`, `.ao()`, `Pv::ai()`, or loaded a `.db`, you are on the
record level. If you implemented the `Source` trait or called `put_nt`, you
are on the raw level. If you called `store.set_value()` on a record, you are
still on the record level — that is the record API, and it applies deadbands
and stamps time.

Both are covered in Part III: records throughout, and the raw level in
[Custom sources](../03-progressive/17-sources.md).
