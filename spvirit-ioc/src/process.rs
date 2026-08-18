//! The processing algorithm, mirroring `dbProcess` in EPICS Base.
//!
//! `process` is an ordinary recursive `fn` over `&mut LockSetData`. The
//! caller holds the lock set's mutex for the whole pass, so nothing inside
//! may block: monitors and cross-set requests go into `ctx` and the caller
//! flushes them after unlocking.

use crate::alarm::{Condition, Severity};
#[cfg(test)]
use crate::ctx::MAX_DEPTH;
use crate::ctx::{ProcCtx, ProcError};
use crate::lockset::{LockSetData, RecordId};
use crate::model::{Field, Kind, Limits, Link, Omsl, Record, Target, Value};

/// Wall-clock nanoseconds since the epoch, for the record's timestamp.
///
/// Sub-project B replaces this with a TSE-aware time provider; A stamps at
/// process time, which is what `TSE = 0` means.
fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        // A clock before the epoch is not worth failing a process pass over.
        .unwrap_or(1)
}

/// Process one record.
///
/// The numbered steps are `dbProcess`'s, kept in the same order so the
/// divergences from Base are visible rather than emergent.
pub fn process(set: &mut LockSetData, id: RecordId, ctx: &mut ProcCtx) -> Result<(), ProcError> {
    let name = set.get(id).name.clone();
    let tpro = set.get(id).common.tpro;

    // 1. PACT is the recursion brake. A record already being processed —
    //    because of a link cycle, or because it is waiting on async
    //    completion — returns immediately.
    if set.get(id).common.pact {
        if tpro {
            ctx.trace_line(format!("{name}: PACT already set, returning"));
        }
        return Ok(());
    }

    ctx.push_depth(&name)?;
    let result = process_inner(set, id, ctx, &name, tpro);
    ctx.pop_depth();
    result
}

fn process_inner(
    set: &mut LockSetData,
    id: RecordId,
    ctx: &mut ProcCtx,
    name: &str,
    tpro: bool,
) -> Result<(), ProcError> {
    if tpro {
        ctx.trace_line(format!("{name}: process entered"));
    }

    // 2. Disable check. SDIS, if it is a link, supplies DISA; the record is
    //    disabled when DISA == DISV.
    let sdis = set.get(id).common.sdis.clone();
    if !matches!(sdis, Link::Constant(_)) {
        // DISA is a plain Long field regardless of the disabling record's own
        // Kind. Coercing through the record's Kind would be wrong for a
        // binary (Bi/Bo) disabler: Value::coerce_to forces any non-zero
        // source to 1, so a DISV other than 0/1 could never match.
        let (value, _sev) = fetch_link_value(set, &sdis, Kind::LongIn, ctx)?;
        set.get_mut(id).common.disa = value.as_i32();
    }
    let record = set.get(id);
    if record.common.disa == record.common.disv {
        let diss = record.common.diss;
        if tpro {
            ctx.trace_line(format!("{name}: disabled (DISA == DISV)"));
        }
        // A disabled record still publishes its disabled state, but only if
        // DISS actually raises the severity. `recGblSetSevr` is raise-only:
        // severity and condition move together, and only upward. With the
        // default DISS (NoAlarm) a disabled record is not in alarm at all —
        // SEVR/STAT stay NoAlarm.
        let r = set.get_mut(id);
        r.common.nsev = Severity::NoAlarm;
        r.common.nsta = Condition::NoAlarm;
        set_sevr(r, diss, Condition::Disable);
        let alarm_changed = reset_alarms(set, id, ctx);
        post_monitors(set, id, alarm_changed, ctx);
        return Ok(());
    }

    // 3. PACT guards the pass against re-entry through a link cycle.
    set.get_mut(id).common.pact = true;

    // 4. The type-specific body: read inputs, compute, check limits, stamp,
    //    write outputs, post monitors, then FLNK. Filled in by Tasks 7-10.
    let body = record_body(set, id, ctx);

    // 5. PACT itself is the flag; there is nothing left to decide here. As
    //    in Base, a record's own `process()` clears `prec->pact` as the last
    //    thing a synchronous pass does — `record_body` now does that
    //    directly (see its doc comment) once its own work is complete. A
    //    body that is still two-phase-pending, or that failed before
    //    reaching that point, simply leaves PACT set; `complete_async` is
    //    the only other path that clears it.
    if tpro {
        ctx.trace_line(format!("{name}: process complete"));
    }
    body
}

/// `recGblSetSevr`: raise the pending alarm state, never lower it.
///
/// Returns whether the state changed, so callers can tell an ignored
/// duplicate from a real escalation.
pub(crate) fn set_sevr(record: &mut Record, sev: Severity, cond: Condition) -> bool {
    if record.common.nsev.raise(sev) {
        record.common.nsta = cond;
        true
    } else {
        false
    }
}

/// Which limit a value has crossed, honouring HYST.
///
/// `current` is the record's committed condition: an alarm already in force
/// must stay in force until the value clears the limit by HYST, otherwise a
/// value hovering on a limit produces an unbounded stream of monitors.
fn limit_crossed(value: f64, limits: &Limits, current: Condition) -> Option<(Severity, Condition)> {
    let hyst = limits.hyst;
    let held = |cond: Condition| current == cond;

    if limits.hhsv != Severity::NoAlarm
        && (value >= limits.hihi || (held(Condition::HiHi) && value >= limits.hihi - hyst))
    {
        return Some((limits.hhsv, Condition::HiHi));
    }
    if limits.llsv != Severity::NoAlarm
        && (value <= limits.lolo || (held(Condition::LoLo) && value <= limits.lolo + hyst))
    {
        return Some((limits.llsv, Condition::LoLo));
    }
    if limits.hsv != Severity::NoAlarm
        && (value >= limits.high || (held(Condition::High) && value >= limits.high - hyst))
    {
        return Some((limits.hsv, Condition::High));
    }
    if limits.lsv != Severity::NoAlarm
        && (value <= limits.low || (held(Condition::Low) && value <= limits.low + hyst))
    {
        return Some((limits.lsv, Condition::Low));
    }
    None
}

/// Promote a record that has never been given a value to INVALID/UDF.
///
/// This is the UDF check every EPICS record type's `checkAlarms` routine
/// does for itself (e.g. `aiRecord.c`'s `if (prec->udf) { recGblSetSevr(...);
/// return; }`) — it is not part of `recGblResetAlarms`, which only commits.
/// Routed through `set_sevr` so there is exactly one raise-only
/// implementation. `pub(crate)` because every record kind must call this,
/// including `bi`/`bo`, which have no limit fields at all and so never call
/// [`check_limits`] — see [`record_body`]'s `else` branch.
pub(crate) fn check_udf(set: &mut LockSetData, id: RecordId) -> bool {
    let r = set.get_mut(id);
    if r.common.udf {
        set_sevr(r, Severity::Invalid, Condition::Udf)
    } else {
        false
    }
}

/// Apply the record's alarm limits to its current value.
///
/// Mirrors EPICS `checkAlarms`: an undefined record reports INVALID/UDF and
/// nothing else — the limit ladder is skipped entirely, even if a stale VAL
/// happens to sit outside one of the HIHI/HIGH/LOW/LOLO thresholds. Binary
/// records have no limits; their `Limits` stay at the defaults, where every
/// severity is `NoAlarm`, so the limit ladder is a no-op for them.
pub(crate) fn check_limits(set: &mut LockSetData, id: RecordId) {
    if check_udf(set, id) {
        return;
    }
    let value = set.get(id).val.as_f64();
    let current = set.get(id).common.stat;
    let crossed = limit_crossed(value, &set.get(id).limits, current);
    if let Some((sev, cond)) = crossed {
        set_sevr(set.get_mut(id), sev, cond);
    }
}

/// `recGblResetAlarms`: commit the pass's pending alarm state.
///
/// A pure commit — it does not decide alarms itself, only publishes what
/// [`check_udf`]/[`check_limits`]/`set_sevr` accumulated into NSEV/NSTA.
/// Returns whether the committed state changed.
pub(crate) fn reset_alarms(set: &mut LockSetData, id: RecordId, _ctx: &mut ProcCtx) -> bool {
    let r = set.get_mut(id);
    let changed = r.common.sevr != r.common.nsev || r.common.stat != r.common.nsta;
    r.common.sevr = r.common.nsev;
    r.common.stat = r.common.nsta;
    r.common.nsev = Severity::NoAlarm;
    r.common.nsta = Condition::NoAlarm;
    changed
}

/// Read a link's value and the severity it contributes.
///
/// A `PP` db link processes its target first — this is the ordering
/// guarantee a whole class of `.db` designs depends on. `MS` asks the
/// caller to maximise its own severity against the target's; the caller
/// applies it, because only the caller knows which of its links this was.
pub(crate) fn fetch_link_value(
    set: &mut LockSetData,
    link: &Link,
    kind: Kind,
    ctx: &mut ProcCtx,
) -> Result<(Value, Severity), ProcError> {
    match link {
        Link::Constant(v) => Ok((v.coerce_to(kind), Severity::NoAlarm)),
        // An unresolvable link contributes the record's default. The name was
        // already reported once at load; failing every pass would be noise.
        Link::Unresolved { .. } => Ok((Value::default_for(kind), Severity::NoAlarm)),
        Link::Db {
            target: Target::Id(target_id),
            field,
            process_passive,
            maximize_severity,
        } => {
            if *process_passive {
                // Passive-only in Base; A has no scan mechanism yet, so
                // every PP target is processed. Sub-project B narrows this
                // to SCAN = Passive targets.
                process(set, *target_id, ctx)?;
            }
            let value = read_field(set, *target_id, *field).coerce_to(kind);
            let severity = if *maximize_severity {
                set.get(*target_id).common.sevr
            } else {
                Severity::NoAlarm
            };
            Ok((value, severity))
        }
        Link::Db {
            target: Target::Name(name),
            ..
        } => {
            // RecordDb::build resolves every name; an unresolved one here is
            // a bug in the builder, not bad user input.
            unreachable!("unresolved target '{name}' reached process()")
        }
    }
}

/// Write one field of a record in the same lock set.
///
/// Writing `.PROC` forces a process pass — that is the field's only
/// meaning. Writing any other field sets it; whether that triggers
/// processing is the caller's decision.
///
/// This is the write-side counterpart to [`fetch_link_value`]'s read side —
/// the entry point a future client put (CA/PVA) or an output record's OUT
/// write calls. Task 9's `output_body` is its first production caller.
///
/// The `.VAL` branch reuses [`post_monitors`] rather than duplicating its
/// MDEL/ADEL comparison. A raw field write is not itself a process pass, so
/// nothing else would otherwise queue this record's monitor or advance its
/// `prev_val`/`prev_archive_val` reference points — this mirrors Base's
/// `dbPut`, which posts the field's monitor immediately on a direct write
/// rather than waiting for the target to be processed. `alarm_changed` is
/// always `false` here because a raw write cannot itself change the alarm
/// state. When the caller (`output_body`) goes on to reprocess this record
/// for a `PP` link, `record_body`'s own `post_monitors` call sees a
/// `prev_val` that already reflects this write, so it only posts again if
/// reprocessing produces a genuine alarm transition — the value change
/// itself is not posted twice.
pub(crate) fn write_field(
    set: &mut LockSetData,
    id: RecordId,
    field: Field,
    value: Value,
    ctx: &mut ProcCtx,
) -> Result<(), ProcError> {
    if field == Field::Proc {
        return process(set, id, ctx);
    }
    let kind = set.get(id).kind;
    match field {
        Field::Val => {
            let r = set.get_mut(id);
            r.val = value.coerce_to(kind);
            r.common.udf = false;
            post_monitors(set, id, false, ctx);
        }
        Field::Disa => set.get_mut(id).common.disa = value.as_i32(),
        Field::Disv => set.get_mut(id).common.disv = value.as_i32(),
        Field::Hihi => set.get_mut(id).limits.hihi = value.as_f64(),
        Field::High => set.get_mut(id).limits.high = value.as_f64(),
        Field::Low => set.get_mut(id).limits.low = value.as_f64(),
        Field::Lolo => set.get_mut(id).limits.lolo = value.as_f64(),
        // SEVR and STAT are engine-owned outputs, not client inputs.
        Field::Sevr | Field::Stat => {}
        Field::Proc => unreachable!("handled above"),
    }
    Ok(())
}

/// Read one field of a record in the same lock set.
pub(crate) fn read_field(set: &LockSetData, id: RecordId, field: Field) -> Value {
    let r = set.get(id);
    match field {
        Field::Val => r.val,
        Field::Sevr => Value::Long(r.common.sevr as i32),
        Field::Stat => Value::Long(r.common.stat as i32),
        Field::Disa => Value::Long(r.common.disa),
        Field::Disv => Value::Long(r.common.disv),
        Field::Hihi => Value::Double(r.limits.hihi),
        Field::High => Value::Double(r.limits.high),
        Field::Low => Value::Double(r.limits.low),
        Field::Lolo => Value::Double(r.limits.lolo),
        // Reading PROC is meaningless; writing it forces processing.
        Field::Proc => Value::Long(0),
    }
}

/// The type-specific processing body.
///
/// Input records read INP and store it. Output records take a desired value
/// from DOL when OMSL says so, then write it through OUT. Both then check
/// limits or UDF, stamp the time, and commit their alarm state.
///
/// This is also where PACT is owned. `process_inner` sets `common.pact`
/// before calling this function; mirroring Base, where a record type's own
/// `process()` clears `prec->pact` unconditionally at the bottom of the
/// function regardless of the status its device support returned, this
/// function clears PACT on *every* path that finishes handling the pass —
/// whether the input/output side succeeded or failed. A `TooDeep` (or any
/// other) error from `input_body`/`output_body` is not a two-phase-pending
/// body; it is this pass giving up. Leaving PACT set in that case would be
/// silent and permanent: `process()`'s own brake (`if common.pact { return
/// Ok(()) }`) means every later call on that record would report success
/// while doing nothing, forever, with no error and no event. So the failure
/// branch below clears PACT explicitly, by hand — not by falling through a
/// bare `?` and letting the clear be skipped as a side effect of control
/// flow, which is how that bug arose the first time.
///
/// On the success path, PACT is cleared *after* `forward_link` runs, not
/// before — mirroring Base's `dbProcess`, where `recGblFwdLink` executes
/// while `prec->pact` is still `TRUE` and only then is it cleared. This is
/// what makes PACT the brake for a cycle built purely out of FLNK edges:
/// if the forward chain re-enters this record before the clear, the
/// re-entrant `process()` call sees PACT set and returns immediately
/// instead of recursing. Clearing PACT before firing the forward link (as
/// an earlier version of this function did) leaves FLNK-only cycles
/// unguarded — they recurse until `MAX_DEPTH` reports `TooDeep` instead of
/// terminating gracefully after one bounce, because by the time the cycle
/// re-enters, PACT has already been cleared. PP-link cycles were never
/// affected by that mistake: a PP input link re-enters via
/// `fetch_link_value`'s nested `process()` call, which happens earlier in
/// this same function, well before either PACT-clear site.
///
/// The one thing that is allowed to leave PACT set past this function is a
/// future async body returning `AsyncOutcome::Pending` *by design*: see the
/// hook point described below, which is a distinct, deliberate return, not
/// an error.
///
/// This is also the declared seam for two-phase (async) device support,
/// which sub-project A does not implement: the hook point a future
/// `AsyncSupport` implementation would need is inside `input_body` (INP) or
/// `output_body` (OUT), above, at the point the value is fetched or
/// written. An implementation whose `start()` reports
/// `AsyncOutcome::Pending` must return `Ok(())` from that call site without
/// having finished its work, so it takes the success branch here (not the
/// error branch) while genuinely being incomplete — a distinction this
/// function cannot make on its own until B builds it; today, sub-project A
/// has no such body, so every path through here is either a real success or
/// a real failure. How a record binds to an `AsyncSupport` implementation (a
/// registry, a per-record slot, or something else) is left undecided here;
/// see [`AsyncSupport`]'s own doc comment for why.
pub(crate) fn record_body(
    set: &mut LockSetData,
    id: RecordId,
    ctx: &mut ProcCtx,
) -> Result<(), ProcError> {
    let kind = set.get(id).kind;
    let fetched = if kind.is_output() {
        output_body(set, id, ctx)
    } else {
        input_body(set, id, ctx)
    };
    if let Err(err) = fetched {
        // The pass did not complete, and it is not coming back on its own —
        // this is not the deliberate "operation still outstanding" case, so
        // PACT must not be left set for `process()`'s brake to swallow every
        // later pass on this record. See this function's doc comment.
        set.get_mut(id).common.pact = false;
        return Err(err);
    }
    set.get_mut(id).time_ns = now_ns();
    if is_analogue(kind) {
        check_limits(set, id);
    } else {
        // bi/bo carry no limit fields at all (EPICS gives them ZSV/OSV/COSV
        // state alarms instead, out of scope for this plan), so they never
        // reach `check_limits` — and therefore never reach the `check_udf`
        // inside it. Every kind must still promote a never-processed record
        // to INVALID/UDF, so bi/bo call `check_udf` directly here.
        check_udf(set, id);
    }
    let alarm_changed = reset_alarms(set, id, ctx);
    post_monitors(set, id, alarm_changed, ctx);
    // Fire the forward link while PACT is still set — this is what makes
    // PACT the brake for FLNK cycles: if the chain re-enters this record,
    // `process()` sees PACT set and returns immediately instead of
    // recursing. Base does the same at the bottom of each record's own
    // `process()`: `recGblFwdLink` runs before `prec->pact = FALSE`. The clear below is the direct analogue
    // of that `prec->pact = FALSE`, and it must run unconditionally —
    // whether `forward_link` succeeded or errored — or a failure here would
    // strand PACT set for the rest of the IOC's life.
    let result = forward_link(set, id, ctx);
    set.get_mut(id).common.pact = false;
    result
}

/// Queue the record's value monitor, if the change warrants one.
///
/// A post happens when the alarm state changed, or when the value moved
/// further than MDEL from the last posted value. ADEL governs the archive
/// stream the same way; `spvirit` publishes one monitor stream, so ADEL only
/// advances its own reference for now and sub-project E revisits it against
/// a real archiver.
pub(crate) fn post_monitors(
    set: &mut LockSetData,
    id: RecordId,
    alarm_changed: bool,
    ctx: &mut ProcCtx,
) {
    let record = set.get(id);
    let moved = (record.val.as_f64() - record.prev_val.as_f64()).abs();
    let value_changed = record.val != record.prev_val;
    let past_mdel = moved > record.limits.mdel || (value_changed && record.limits.mdel == 0.0);

    if !alarm_changed && !past_mdel {
        return;
    }

    let payload = record.to_payload();
    let name = record.name.clone();
    // MLST and ALST each advance only when their own deadband was crossed,
    // evaluated independently — mirroring aiRecord::monitor in Base, where
    // the MDEL and ADEL checks are two separate `if` statements, neither
    // nested under the other or under whether an alarm caused the post.
    // Advancing MLST unconditionally on every post (including an
    // alarm-only one where `past_mdel` is false) would lose the reference
    // point for the next MDEL comparison: e.g. MDEL = 10, VAL 0 -> 9 with
    // an alarm change posts but must leave MLST at 0, so a later move to
    // 18 is correctly seen as a 18-unit move past MDEL rather than a
    // 9-unit move that gets suppressed.
    let archived =
        (record.val.as_f64() - record.prev_archive_val.as_f64()).abs() > record.limits.adel;
    let value = record.val;

    let r = set.get_mut(id);
    if past_mdel {
        r.prev_val = value;
    }
    if archived {
        r.prev_archive_val = value;
    }
    r.prev_sevr = r.common.sevr;
    r.prev_stat = r.common.stat;

    ctx.post(&name, payload);
}

/// `recGblFwdLink`: process the forward link target.
///
/// This runs after the record's own monitors are queued, so a client sees
/// this record's update before the downstream record's — one of the
/// ordering guarantees the conformance suite pins down.
fn forward_link(set: &mut LockSetData, id: RecordId, ctx: &mut ProcCtx) -> Result<(), ProcError> {
    let flnk = set.get(id).common.flnk.clone();
    match flnk {
        Link::Db {
            target: Target::Id(target_id),
            ..
        } => process(set, target_id, ctx),
        Link::Constant(_) | Link::Unresolved { .. } => Ok(()),
        Link::Db {
            target: Target::Name(name),
            ..
        } => {
            unreachable!("unresolved FLNK target '{name}' reached process()")
        }
    }
}

/// Only `ai`/`ao`/`longin`/`longout` carry HIHI/HIGH/LOW/LOLO limit fields
/// and run the limit ladder in [`check_limits`], exactly as their EPICS Base
/// counterparts do. `bi`/`bo` have no limit fields at all — see
/// [`record_body`]'s `else` branch for how they still get UDF promotion.
fn is_analogue(kind: Kind) -> bool {
    matches!(kind, Kind::Ai | Kind::Ao | Kind::LongIn | Kind::LongOut)
}

fn input_body(set: &mut LockSetData, id: RecordId, ctx: &mut ProcCtx) -> Result<(), ProcError> {
    let kind = set.get(id).kind;
    let inp = set.get(id).inp.clone();
    // A CONSTANT link is a no-op during processing — `dbGetLink` returns
    // immediately for `plink->type == CONSTANT` without touching the
    // destination. Constant links are applied once, at init time, by
    // `recGblInitConstantLink` (PINI-time initialisation is Task 13's); on
    // every later process pass a soft record with a constant (including a
    // never-configured, i.e. default) INP simply keeps whatever value a
    // direct write last gave it. See the SDIS check in `process_inner` for
    // the same idiom.
    // UDF is cleared unconditionally here, not on some success condition —
    // and that is deliberate, not a shortcut. `aiRecord::process` clears UDF
    // on a successful read (`if (status == 0) prec->udf = FALSE;`), and in
    // this plan every input path *is* a success: `fetch_link_value` returns
    // `Ok` even for `Link::Unresolved` (the record's default, reported once
    // at load rather than failing every pass) and constant links are a
    // processing-time no-op rather than a fetch that can fail. There is no
    // failure path for "clear on success" to diverge from "clear
    // unconditionally" against — do not add a conditional here expecting to
    // "fix" a bug; there isn't one. See
    // `a_record_reports_invalid_udf_before_the_first_process_pass` /
    // `_and_no_alarm_after_it` below for the transition this produces.
    if !matches!(inp, Link::Constant(_)) {
        let (value, link_sev) = fetch_link_value(set, &inp, kind, ctx)?;
        let r = set.get_mut(id);
        r.val = value;
        r.common.udf = false;
        if link_sev != Severity::NoAlarm {
            set_sevr(r, link_sev, Condition::Link);
        }
    } else {
        set.get_mut(id).common.udf = false;
    }
    Ok(())
}

fn output_body(set: &mut LockSetData, id: RecordId, ctx: &mut ProcCtx) -> Result<(), ProcError> {
    let kind = set.get(id).kind;

    // OMSL = closed_loop means the record is driven by DOL rather than by
    // whatever a client last wrote. As with INP above, a CONSTANT DOL is a
    // processing-time no-op — it was already applied once, at init time, by
    // `recGblInitConstantLink` (see `build.rs::init_constant`). Re-reading it
    // on every pass would clobber a supervisory-then-switched-to-closed-loop
    // record back to its constant default instead of leaving it to whatever
    // a real (non-constant) DOL link supplies.
    if set.get(id).omsl == Omsl::ClosedLoop {
        let dol = set.get(id).dol.clone();
        if !matches!(dol, Link::Constant(_)) {
            let (value, link_sev) = fetch_link_value(set, &dol, kind, ctx)?;
            let r = set.get_mut(id);
            r.val = value;
            if link_sev != Severity::NoAlarm {
                set_sevr(r, link_sev, Condition::Link);
            }
        }
    }
    set.get_mut(id).common.udf = false;

    // Write through OUT. A constant OUT is the "no hardware attached" case
    // every soft record has; there is nothing to write.
    let out = set.get(id).out.clone();
    let value = set.get(id).val;
    match out {
        Link::Db {
            target: Target::Id(target_id),
            field,
            process_passive,
            ..
        } => {
            write_field(set, target_id, field, value, ctx)?;
            if process_passive {
                process(set, target_id, ctx)?;
            }
        }
        Link::Unresolved { .. } | Link::Constant(_) => {}
        Link::Db {
            target: Target::Name(name),
            ..
        } => {
            unreachable!("unresolved OUT target '{name}' reached process()")
        }
    }
    Ok(())
}

/// What a device-support `start` reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncOutcome {
    /// The operation finished inside `start`; processing continues.
    Complete,
    /// The operation is outstanding. The record keeps PACT set and the
    /// device calls [`complete_async`] when it finishes.
    Pending,
}

/// Device support that may take longer than a processing pass.
///
/// This is sub-project A's *declared contract* for sub-project B, not live
/// machinery: A ships no implementation of it, and nothing in A's
/// `record_body` calls `start()` — there is no call site to wire one into
/// yet. See [`record_body`]'s doc comment for exactly which hook point
/// (inside `input_body`/`output_body`, immediately before the PACT clear at
/// the end of `record_body`) a future implementation must use, and what a
/// `Pending` outcome requires of it. Deciding how a record binds to a
/// specific `AsyncSupport` implementation — a registry, a per-record slot,
/// how that lookup stays deterministic across a `.db` — is design work
/// owned by the source-tier spec and sub-project B, not by this trait: A
/// declares the gap rather than filling it in unspecified. [`complete_async`]
/// exists so B's scan threads have a defined completion path today, and so
/// the PACT semantics it depends on are covered by tests written against
/// the engine that owns them, rather than retro-fitted later.
pub trait AsyncSupport: Send + Sync {
    fn start(&self, record: &str, ctx: &mut ProcCtx) -> AsyncOutcome;
}

/// Finish a record that returned from its body with PACT still set.
///
/// The second half of processing — value, limits, monitors, forward link —
/// runs now, via the ordinary [`record_body`], which owns clearing PACT
/// itself once it reaches the end of a synchronous pass (see its doc
/// comment). A record that is not active is left alone: a duplicate
/// completion callback must not process the record twice.
pub fn complete_async(
    set: &mut LockSetData,
    id: RecordId,
    ctx: &mut ProcCtx,
) -> Result<(), ProcError> {
    if !set.get(id).common.pact {
        return Ok(());
    }
    let name = set.get(id).name.clone();
    ctx.push_depth(&name)?;
    let result = record_body(set, id, ctx);
    ctx.pop_depth();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockset::RecordDb;
    use crate::test_support::db;
    use spvirit_types::{NtPayload, ScalarValue};

    fn event_names(ctx: &mut ProcCtx) -> Vec<String> {
        ctx.take_events().into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn processing_posts_a_monitor_for_the_record() {
        let d = db("record(ai, \"PV:A\") {\n    field(INP, \"1\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            process(set, id, &mut ctx).expect("process succeeds")
        });
        assert_eq!(event_names(&mut ctx), vec!["PV:A".to_string()]);
    }

    #[test]
    fn flnk_posts_after_the_records_own_monitor() {
        let d = db(
            "record(ai, \"PV:A\") {\n    field(INP, \"1\")\n    field(FLNK, \"PV:B\")\n}\n\
                    record(ai, \"PV:B\") {\n    field(INP, \"2\")\n}\n",
        );
        let a = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            process(set, a, &mut ctx).expect("process succeeds")
        });
        assert_eq!(
            event_names(&mut ctx),
            vec!["PV:A".to_string(), "PV:B".to_string()],
            "the forward link must fire after this record's monitors"
        );
    }

    #[test]
    fn mdel_suppresses_a_change_smaller_than_the_deadband() {
        // NOTE: the brief's original fixture mutated `record.inp` between
        // process() calls to simulate an input change, and its first
        // assertion claimed "First pass always posts: the record was UDF."
        // Both are wrong. A CONSTANT link (which an absent INP is) is a
        // no-op during processing (Task 8), so re-assigning `.inp` never
        // reaches VAL — the mutation could not have moved the value on any
        // of the later passes. And the "always posts" claim is false in
        // this engine *and* in EPICS Base: MLST/ALST (this engine's
        // prev_val/prev_archive_val) have no DBD default and start at the
        // kind's zero, independently of whatever `recGblInitConstantLink`
        // seeds VAL to. A record with no INP loads with VAL == prev_val
        // == 0.0, so its first pass has no delta to report and posts
        // nothing — matching Base exactly. See
        // `processing_posts_a_monitor_for_the_record` below for the
        // complementary case (a non-zero seeded VAL, which *does* post on
        // pass 1).
        //
        // The fix mutates VAL directly through the lock set, the idiom
        // Task 7's `hyst_holds_an_alarm_until_the_value_clears_the_deadband`
        // already uses — sound here because this record's INP is constant,
        // so `record_body` never overwrites a directly-written VAL.
        let d = db("record(ai, \"PV:A\") {\n    field(MDEL, \"1\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| process(set, id, &mut ctx).expect("process"));
        assert!(
            event_names(&mut ctx).is_empty(),
            "no INP, no alarm: the first pass has nothing to report"
        );
        // 0.5 is inside MDEL of the last-posted reference, which is still
        // 0.0 (nothing has posted yet).
        d.with_set(id.set, |set| {
            set.get_mut(id).val = Value::Double(0.5);
            process(set, id, &mut ctx).expect("process");
        });
        assert!(
            event_names(&mut ctx).is_empty(),
            "MDEL must suppress the post"
        );
        // 2.0 exceeds MDEL measured from that same reference.
        d.with_set(id.set, |set| {
            set.get_mut(id).val = Value::Double(2.0);
            process(set, id, &mut ctx).expect("process");
        });
        assert_eq!(event_names(&mut ctx).len(), 1, "a change past MDEL posts");
    }

    #[test]
    fn an_alarm_change_posts_regardless_of_mdel() {
        // As above: the value is moved by writing VAL directly, not by
        // reassigning the (constant, process-time-inert) INP link. The move
        // is 2.0, far inside MDEL's 1000, so a post here can only be
        // attributed to the alarm transition, not to MDEL.
        let d = db("record(ai, \"PV:A\") {\n    field(MDEL, \"1000\")\n\
                    field(HIHI, \"1\")\n    field(HHSV, \"MAJOR\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| process(set, id, &mut ctx).expect("process"));
        let _ = ctx.take_events();
        d.with_set(id.set, |set| {
            set.get_mut(id).val = Value::Double(2.0);
            process(set, id, &mut ctx).expect("process");
        });
        assert_eq!(
            event_names(&mut ctx).len(),
            1,
            "an alarm transition must post even inside MDEL"
        );
    }

    #[test]
    fn an_alarm_only_post_does_not_advance_mlst() {
        // Pins the Base `aiRecord::monitor` contract: MLST (prev_val) only
        // advances when the value delta itself exceeded MDEL, never merely
        // because a post happened for some other reason (here, an alarm
        // transition). MDEL = 10, and the first move (0 -> 9) is
        // alarm-driven only (9 < 10, so it must NOT be seen as an MDEL-
        // worthy move). If MLST wrongly advanced to 9 on that post, the
        // second move (9 -> 18, an 18-unit move from the *original*
        // reference of 0) would be measured as only 9 units from the
        // wrongly-advanced reference and suppressed — silently losing a
        // real MDEL-worthy change. Base computes |18 - 0| = 18 > 10 and
        // posts; a buggy implementation computes |18 - 9| = 9 <= 10 and
        // suppresses.
        let d = db("record(ai, \"PV:A\") {\n    field(MDEL, \"10\")\n\
                    field(HIHI, \"9\")\n    field(HHSV, \"MAJOR\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        // No INP, so VAL and MLST both start at 0.0: the first pass has
        // nothing to report.
        d.with_set(id.set, |set| process(set, id, &mut ctx).expect("process"));
        assert!(
            event_names(&mut ctx).is_empty(),
            "no change on the first pass"
        );
        // 0 -> 9 crosses HIHI (alarm-only post; the 9-unit move itself is
        // not past MDEL's 10).
        d.with_set(id.set, |set| {
            set.get_mut(id).val = Value::Double(9.0);
            process(set, id, &mut ctx).expect("process");
        });
        assert_eq!(
            event_names(&mut ctx).len(),
            1,
            "the alarm transition at VAL=9 must post"
        );
        // 9 -> 18: no further alarm change (still MAJOR/HiHi), but the move
        // from the *original* MLST reference of 0.0 is 18, past MDEL's 10.
        d.with_set(id.set, |set| {
            set.get_mut(id).val = Value::Double(18.0);
            process(set, id, &mut ctx).expect("process");
        });
        assert_eq!(
            event_names(&mut ctx).len(),
            1,
            "MLST must not have advanced on the alarm-only post: this move is past MDEL \
             measured from the original reference of 0.0"
        );
    }

    #[test]
    fn the_posted_payload_carries_the_records_alarm_state() {
        let d = db("record(ai, \"PV:A\") {\n    field(INP, \"11\")\n\
                    field(HIHI, \"10\")\n    field(HHSV, \"MAJOR\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| process(set, id, &mut ctx).expect("process"));
        let events = ctx.take_events();
        match &events[0].1 {
            NtPayload::Scalar(s) => {
                assert_eq!(s.value, ScalarValue::F64(11.0));
                assert_eq!(s.alarm_severity, 2);
                assert_eq!(s.alarm_message, "HIHI");
            }
            other => panic!("expected a scalar payload, got {other:?}"),
        }
    }

    #[test]
    fn a_disabled_record_does_not_fire_its_forward_link() {
        // Base does not run FLNK for a disabled record either. PV:B's INP
        // is a non-zero constant, so if `process_inner`'s disabled branch
        // wrongly called `forward_link`, PV:B would process and post its
        // own monitor — this discriminates the guard, not just presence.
        let d = db(
            "record(ai, \"PV:A\") {\n    field(DISA, \"1\")\n    field(DISV, \"1\")\n\
                    field(DISS, \"MAJOR\")\n    field(FLNK, \"PV:B\")\n}\n\
                    record(ai, \"PV:B\") {\n    field(INP, \"2\")\n}\n",
        );
        let a = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            process(set, a, &mut ctx).expect("process succeeds")
        });
        assert_eq!(
            event_names(&mut ctx),
            vec!["PV:A".to_string()],
            "a disabled record posts its own monitor but must not fire FLNK"
        );
    }

    #[test]
    fn a_disabled_record_still_posts_its_disabled_state() {
        let d = db("record(ai, \"PV:A\") {\n    field(DISA, \"1\")\n\
                    field(DISV, \"1\")\n    field(DISS, \"MAJOR\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| process(set, id, &mut ctx).expect("process"));
        let events = ctx.take_events();
        assert_eq!(events.len(), 1, "a disable transition is observable");
        match &events[0].1 {
            NtPayload::Scalar(s) => assert_eq!(s.alarm_message, "DISABLE"),
            other => panic!("expected a scalar payload, got {other:?}"),
        }
    }

    #[test]
    fn processing_clears_udf_and_stamps_the_time() {
        let d = db("record(ai, \"PV:A\") {\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            assert!(set.get(id).common.udf, "a fresh record is UDF");
            process(set, id, &mut ctx).expect("process succeeds");
            assert!(!set.get(id).common.udf, "processing clears UDF");
            assert!(set.get(id).time_ns > 0, "processing stamps the time");
        });
    }

    #[test]
    fn pact_is_clear_again_after_a_synchronous_pass() {
        let d = db("record(ai, \"PV:A\") {\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            process(set, id, &mut ctx).expect("process succeeds");
            assert!(
                !set.get(id).common.pact,
                "a synchronous record ends with PACT clear"
            );
        });
    }

    #[test]
    fn a_record_already_active_returns_without_reprocessing() {
        let d = db("record(ai, \"PV:A\") {\n    field(TPRO, \"1\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            set.get_mut(id).common.pact = true;
            set.get_mut(id).time_ns = 0;
            process(set, id, &mut ctx).expect("an active record is not an error");
            assert_eq!(set.get(id).time_ns, 0, "the body must not have run");
            assert!(
                set.get(id).common.pact,
                "PACT is left as the async pass set it"
            );
        });
        assert!(
            ctx.trace.iter().any(|l| l.contains("PACT")),
            "TPRO must record the recursion brake, got {:?}",
            ctx.trace
        );
    }

    #[test]
    fn a_disabled_record_raises_disable_at_diss_and_skips_the_body() {
        let d = db("record(ai, \"PV:A\") {\n    field(DISA, \"1\")\n\
                    field(DISV, \"1\")\n    field(DISS, \"MAJOR\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            process(set, id, &mut ctx).expect("process succeeds");
            let r = set.get(id);
            assert_eq!(r.common.sevr, Severity::Major);
            assert_eq!(r.common.stat, Condition::Disable);
            assert_eq!(r.time_ns, 0, "a disabled record does not process");
        });
    }

    #[test]
    fn a_disabled_record_with_default_diss_is_not_in_alarm() {
        let d = db("record(ai, \"PV:A\") {\n    field(DISA, \"1\")\n    field(DISV, \"1\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            process(set, id, &mut ctx).expect("process succeeds");
            let r = set.get(id);
            assert_eq!(
                r.common.sevr,
                Severity::NoAlarm,
                "DISS defaults to NoAlarm, which raises nothing"
            );
            assert_eq!(
                r.common.stat,
                Condition::NoAlarm,
                "disabled but not in alarm: STAT must stay NoAlarm"
            );
        });
    }

    #[test]
    fn a_disabled_record_with_default_diss_does_not_oscillate_across_passes() {
        let d = db("record(ai, \"PV:A\") {\n    field(DISA, \"1\")\n    field(DISV, \"1\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            for pass in 0..3 {
                process(set, id, &mut ctx).expect("process succeeds");
                let r = set.get(id);
                assert_eq!(
                    r.common.sevr,
                    Severity::NoAlarm,
                    "pass {pass}: SEVR must stay NoAlarm"
                );
                assert_eq!(
                    r.common.stat,
                    Condition::NoAlarm,
                    "pass {pass}: STAT must stay NoAlarm, not oscillate"
                );
            }
        });
    }

    #[test]
    fn a_disabled_record_with_diss_major_stays_stable_across_passes() {
        let d = db("record(ai, \"PV:A\") {\n    field(DISA, \"1\")\n\
                    field(DISV, \"1\")\n    field(DISS, \"MAJOR\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            for pass in 0..3 {
                process(set, id, &mut ctx).expect("process succeeds");
                let r = set.get(id);
                assert_eq!(r.common.sevr, Severity::Major, "pass {pass}: SEVR");
                assert_eq!(r.common.stat, Condition::Disable, "pass {pass}: STAT");
            }
        });
    }

    #[test]
    fn disa_not_equal_to_disv_leaves_the_record_enabled() {
        let d = db("record(ai, \"PV:A\") {\n    field(DISA, \"0\")\n    field(DISV, \"1\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            process(set, id, &mut ctx).expect("process succeeds");
            assert_eq!(set.get(id).common.stat, Condition::NoAlarm);
            assert!(set.get(id).time_ns > 0, "an enabled record processes");
        });
    }

    #[test]
    fn sdis_supplies_disa_when_it_is_a_link() {
        // DISS is set to MAJOR (rather than left at its NoAlarm default) so
        // that the disabled state is observable in STAT/SEVR: with the
        // raise-only rule, a disabled record with DISS == NoAlarm is not in
        // alarm, so it would not distinguish "disabled" from "enabled" here.
        let d = db(
            "record(ai, \"PV:A\") {\n    field(SDIS, \"PV:S\")\n    field(DISV, \"1\")\n\
                    field(DISS, \"MAJOR\")\n}\n\
                    record(longin, \"PV:S\") {\n    field(VAL, \"1\")\n}\n",
        );
        let a = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            process(set, a, &mut ctx).expect("process succeeds");
            let r = set.get(a);
            assert_eq!(r.common.disa, 1, "DISA must be fetched through SDIS");
            assert_eq!(r.common.stat, Condition::Disable);
        });
    }

    #[test]
    fn the_depth_cap_reports_the_record_rather_than_overflowing_the_stack() {
        let d = db("record(ai, \"PV:A\") {\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        for _ in 0..MAX_DEPTH {
            ctx.push_depth("outer").expect("within the cap");
        }
        let err = d
            .with_set(id.set, |set| process(set, id, &mut ctx))
            .expect_err("processing at the cap must fail");
        assert!(matches!(err, ProcError::TooDeep { .. }), "got {err:?}");
    }

    fn limited(val: &str, extra: &str) -> (RecordDb, RecordId) {
        let text = format!(
            "record(ai, \"PV:A\") {{\n    field(VAL, \"{val}\")\n\
             field(HIHI, \"10\")\n    field(HIGH, \"5\")\n\
             field(LOW, \"-5\")\n    field(LOLO, \"-10\")\n\
             field(HHSV, \"MAJOR\")\n    field(HSV, \"MINOR\")\n\
             field(LSV, \"MINOR\")\n    field(LLSV, \"MAJOR\")\n{extra}}}\n"
        );
        let d = db(&text);
        let id = d.lookup("PV:A").expect("PV:A exists");
        (d, id)
    }

    fn sevr_stat_after_process(d: &RecordDb, id: RecordId) -> (Severity, Condition) {
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            process(set, id, &mut ctx).expect("process succeeds");
            (set.get(id).common.sevr, set.get(id).common.stat)
        })
    }

    #[test]
    fn a_value_above_hihi_is_major_hihi() {
        let (d, id) = limited("11", "");
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::Major, Condition::HiHi)
        );
    }

    #[test]
    fn a_value_above_high_is_minor_high() {
        let (d, id) = limited("7", "");
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::Minor, Condition::High)
        );
    }

    #[test]
    fn a_value_below_lolo_is_major_lolo() {
        let (d, id) = limited("-11", "");
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::Major, Condition::LoLo)
        );
    }

    #[test]
    fn a_value_inside_the_limits_clears_the_alarm() {
        let (d, id) = limited("0", "");
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::NoAlarm, Condition::NoAlarm)
        );
    }

    #[test]
    fn a_zero_severity_limit_does_not_alarm() {
        let d = db("record(ai, \"PV:A\") {\n    field(VAL, \"99\")\n\
                    field(HIHI, \"10\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::NoAlarm, Condition::NoAlarm),
            "HHSV defaults to NO_ALARM, so HIHI alone must not alarm"
        );
    }

    #[test]
    fn hyst_holds_an_alarm_until_the_value_clears_the_deadband() {
        let (d, id) = limited("7", "    field(HYST, \"2\")\n");
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::Minor, Condition::High)
        );
        // 4.0 is below HIGH but within HYST of it: the alarm must persist.
        d.with_set(id.set, |set| set.get_mut(id).val = Value::Double(4.0));
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::Minor, Condition::High)
        );
        // 2.0 clears HIGH - HYST = 3.0.
        d.with_set(id.set, |set| set.get_mut(id).val = Value::Double(2.0));
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::NoAlarm, Condition::NoAlarm)
        );
    }

    #[test]
    fn severity_only_ever_rises_within_one_pass() {
        let d = db("record(ai, \"PV:A\") {\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        d.with_set(id.set, |set| {
            let r = set.get_mut(id);
            assert!(set_sevr(r, Severity::Minor, Condition::Soft));
            assert!(
                !set_sevr(r, Severity::NoAlarm, Condition::Calc),
                "a lower severity must not overwrite"
            );
            assert_eq!(r.common.nsta, Condition::Soft);
            assert!(set_sevr(r, Severity::Major, Condition::Calc));
            assert_eq!(r.common.nsta, Condition::Calc);
        });
    }

    #[test]
    fn an_undefined_record_is_invalid_udf() {
        // A record whose body never ran keeps UDF; checking its alarms
        // and committing them must report INVALID/UDF, as EPICS does for a
        // never-processed PV. The promotion is `check_limits`'s job (via
        // `check_udf`), not `reset_alarms`'s: `reset_alarms` only commits.
        let d = db("record(ai, \"PV:A\") {\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            set.get_mut(id).common.udf = true;
            check_limits(set, id);
            reset_alarms(set, id, &mut ctx);
            assert_eq!(set.get(id).common.sevr, Severity::Invalid);
            assert_eq!(set.get(id).common.stat, Condition::Udf);
        });
    }

    // --- pinning the UDF -> INVALID/UDF transition around a process pass -
    //
    // Fix round 1: the reviewer flagged that `record_body` clears UDF
    // unconditionally on every pass and asked for the transition itself to
    // be pinned in both directions — before the first `process()` and after
    // it — rather than trusting the surrounding tests to imply it.

    #[test]
    fn a_record_reports_invalid_udf_before_the_first_process_pass() {
        // A record that has never been processed keeps the UDF default a
        // fresh build gives it (see `build.rs`'s `init_constant`: with no
        // INP there is nothing to seed VAL from, so UDF stays set). There is
        // no "peek without processing" API — `check_limits` (via
        // `check_udf`) and `reset_alarms` are themselves the accessor that
        // computes and commits observable alarm state, so this test drives
        // them directly instead of a full `process()` pass. This is the same
        // pair `an_undefined_record_is_invalid_udf` above exercises; the
        // difference here is that UDF is never forced — it is the record's
        // ordinary, never-processed default.
        let d = db("record(ai, \"PV:A\") {\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            assert!(
                set.get(id).common.udf,
                "a never-processed record is UDF by default"
            );
            check_limits(set, id);
            reset_alarms(set, id, &mut ctx);
            assert_eq!(set.get(id).common.sevr, Severity::Invalid);
            assert_eq!(set.get(id).common.stat, Condition::Udf);
        });
    }

    #[test]
    fn a_record_reports_no_alarm_and_clear_udf_after_the_first_process_pass() {
        // The complement: one full `process()` pass reads (or, with no INP,
        // no-ops on) the input side, clears UDF, and the limit ladder then
        // commits NO_ALARM. Together with the test above, this pins the
        // transition `record_body`'s unconditional `udf = false` produces —
        // the point the reviewer's Important finding was about.
        let d = db("record(ai, \"PV:A\") {\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            process(set, id, &mut ctx).expect("process succeeds");
            assert!(!set.get(id).common.udf, "processing clears UDF");
            assert_eq!(set.get(id).common.sevr, Severity::NoAlarm);
            assert_eq!(set.get(id).common.stat, Condition::NoAlarm);
        });
    }

    #[test]
    fn an_undefined_record_reports_udf_not_a_limit_alarm() {
        // EPICS's checkAlarms returns immediately after the UDF check,
        // skipping the limit ladder entirely. A record whose stale VAL sits
        // above HIHI must still come out INVALID/UDF, not MAJOR/HiHi.
        let (d, id) = limited("11", "");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            set.get_mut(id).common.udf = true;
            check_limits(set, id);
            reset_alarms(set, id, &mut ctx);
            assert_eq!(set.get(id).common.sevr, Severity::Invalid);
            assert_eq!(set.get(id).common.stat, Condition::Udf);
        });
    }

    // --- reset_alarms's `changed` contract -------------------------------
    //
    // Fix round 1: these were missing entirely. A `reset_alarms` that
    // returned `true` unconditionally, `false` unconditionally, or computed
    // `changed` from severity alone (ignoring the condition) would have
    // passed the whole suite without these.

    #[test]
    fn reset_alarms_reports_true_when_the_pending_state_differs_from_committed() {
        let d = db("record(ai, \"PV:A\") {\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            let r = set.get_mut(id);
            r.common.sevr = Severity::NoAlarm;
            r.common.stat = Condition::NoAlarm;
            r.common.nsev = Severity::Minor;
            r.common.nsta = Condition::Soft;
            assert!(
                reset_alarms(set, id, &mut ctx),
                "pending NSEV/NSTA differ from committed SEVR/STAT: this must report a change"
            );
            assert_eq!(set.get(id).common.sevr, Severity::Minor);
            assert_eq!(set.get(id).common.stat, Condition::Soft);
        });
    }

    #[test]
    fn reset_alarms_reports_false_when_recommitting_the_same_state() {
        let d = db("record(ai, \"PV:A\") {\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            let r = set.get_mut(id);
            r.common.sevr = Severity::Minor;
            r.common.stat = Condition::Soft;
            r.common.nsev = Severity::Minor;
            r.common.nsta = Condition::Soft;
            assert!(
                !reset_alarms(set, id, &mut ctx),
                "committing the same severity and condition again must not report a change"
            );
        });
    }

    #[test]
    fn reset_alarms_reports_true_when_only_the_condition_differs() {
        // Severity alone is not enough to compute `changed`: two different
        // conditions can share a severity (e.g. HIGH and SOFT are both
        // Minor), and a condition change is still an observable change.
        let d = db("record(ai, \"PV:A\") {\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            let r = set.get_mut(id);
            r.common.sevr = Severity::Minor;
            r.common.stat = Condition::Soft;
            r.common.nsev = Severity::Minor;
            r.common.nsta = Condition::Calc;
            assert!(
                reset_alarms(set, id, &mut ctx),
                "the condition changed even though the severity did not; this must report a change"
            );
            assert_eq!(set.get(id).common.stat, Condition::Calc);
        });
    }

    // --- exact-boundary and ladder-order coverage -------------------------

    #[test]
    fn a_value_exactly_at_hihi_alarms_hihi() {
        let (d, id) = limited("10", "");
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::Major, Condition::HiHi)
        );
    }

    #[test]
    fn a_value_exactly_at_high_alarms_high() {
        let (d, id) = limited("5", "");
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::Minor, Condition::High)
        );
    }

    #[test]
    fn a_value_exactly_at_low_alarms_low() {
        let (d, id) = limited("-5", "");
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::Minor, Condition::Low)
        );
    }

    #[test]
    fn a_value_exactly_at_lolo_alarms_lolo() {
        let (d, id) = limited("-10", "");
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::Major, Condition::LoLo)
        );
    }

    #[test]
    fn crossing_both_hihi_and_high_yields_hihi_by_ladder_order_alone() {
        // HSV is given the higher severity here (MAJOR vs HHSV's MINOR): if
        // the outcome were decided by comparing severities rather than by
        // the ladder returning on its first match, this would come out
        // MAJOR/High instead of MINOR/HiHi.
        let text = "record(ai, \"PV:A\") {\n    field(VAL, \"11\")\n\
                     field(HIHI, \"10\")\n    field(HIGH, \"5\")\n\
                     field(HHSV, \"MINOR\")\n    field(HSV, \"MAJOR\")\n}\n";
        let d = db(text);
        let id = d.lookup("PV:A").expect("PV:A exists");
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::Minor, Condition::HiHi),
            "the ladder must return on HIHI before it ever reaches HIGH"
        );
    }

    #[test]
    fn crossing_both_lolo_and_low_yields_lolo_by_ladder_order_alone() {
        let text = "record(ai, \"PV:A\") {\n    field(VAL, \"-11\")\n\
                     field(LOW, \"-5\")\n    field(LOLO, \"-10\")\n\
                     field(LSV, \"MAJOR\")\n    field(LLSV, \"MINOR\")\n}\n";
        let d = db(text);
        let id = d.lookup("PV:A").expect("PV:A exists");
        assert_eq!(
            sevr_stat_after_process(&d, id),
            (Severity::Minor, Condition::LoLo),
            "the ladder must return on LOLO before it ever reaches LOW"
        );
    }

    // --- Task 8: input links -----------------------------------------------

    // The brief's original fixtures for these two tests tried to detect
    // "did the target process?" by inspecting the VALUE the reader got.
    // That evidence is unsound in principle for sub-project A: under either
    // PP or NPP, `A` ends up reading `B.VAL` either way, and there are no
    // device inputs yet, so processing a soft record's own body never
    // changes its own VAL. The fixtures only appeared to discriminate
    // because they relied on a constant INP being (re-)applied at *process*
    // time — precisely the non-EPICS behaviour Task 8's later rulings
    // removed (a CONSTANT link is a no-op during processing; it is applied
    // once, at load, in `build.rs`). With that fixed, both fixtures'
    // "B starts at its default" precondition became false (B is seeded to
    // 5.0 at load) and the tests could no longer tell PP from NPP by value.
    //
    // The fix moves the evidence to the TARGET's side, onto state that
    // processing demonstrably changes regardless of device support:
    // `record_body` clears UDF (and stamps `time_ns`) as a side effect of
    // being processed at all. `PV:B` has no INP, so per the init-constant
    // rule it loads with the default VAL and UDF set; the test then writes
    // `B.VAL` directly (standing in for an external caput) without going
    // through `record_body`, so UDF stays set until something processes B.
    // PP must clear it; NPP must not. This pair differs in exactly one
    // input (the link modifier) and exactly one assertion (B's UDF), so
    // mentally swapping the PP/NPP branches in `fetch_link_value` turns both
    // red: skip-on-PP loses `!B.udf` in the first test, and process-on-NPP
    // gains `!B.udf` (failing the `B.udf` assertion) in the second.

    #[test]
    fn a_pp_input_processes_its_target_before_reading() {
        let d = db("record(ai, \"PV:A\") {\n    field(INP, \"PV:B PP\")\n}\n\
                    record(ai, \"PV:B\") {\n}\n");
        let a = d.lookup("PV:A").expect("PV:A exists");
        let b = d.lookup("PV:B").expect("PV:B exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            set.get_mut(b).val = Value::Double(5.0);
            assert!(
                set.get(b).common.udf,
                "B starts UDF: no INP has ever supplied it a value"
            );
            process(set, a, &mut ctx).expect("process succeeds");
            assert_eq!(
                set.get(a).val.as_f64(),
                5.0,
                "A read B's directly-written value through the link"
            );
            assert!(
                !set.get(b).common.udf,
                "PP must have processed B as a side effect: UDF is cleared"
            );
            assert!(
                set.get(b).time_ns > 0,
                "PP must have processed B as a side effect: time_ns is stamped"
            );
        });
    }

    #[test]
    fn an_npp_input_reads_without_processing_the_target() {
        let d = db("record(ai, \"PV:A\") {\n    field(INP, \"PV:B NPP\")\n}\n\
                    record(ai, \"PV:B\") {\n}\n");
        let a = d.lookup("PV:A").expect("PV:A exists");
        let b = d.lookup("PV:B").expect("PV:B exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            set.get_mut(b).val = Value::Double(5.0);
            process(set, a, &mut ctx).expect("process succeeds");
            assert_eq!(
                set.get(a).val.as_f64(),
                5.0,
                "A still reads B's value even when NPP does not process it"
            );
            assert!(
                set.get(b).common.udf,
                "NPP must not have processed B: UDF stays set"
            );
        });
    }

    #[test]
    fn ms_propagates_the_targets_severity_to_the_reader() {
        let d = db(
            "record(ai, \"PV:A\") {\n    field(INP, \"PV:B PP MS\")\n}\n\
                    record(ai, \"PV:B\") {\n    field(INP, \"11\")\n\
                    field(HIHI, \"10\")\n    field(HHSV, \"MAJOR\")\n}\n",
        );
        let a = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            process(set, a, &mut ctx).expect("process succeeds");
            assert_eq!(
                set.get(a).common.sevr,
                Severity::Major,
                "MS must carry B's MAJOR up to A"
            );
            assert_eq!(
                set.get(a).common.stat,
                Condition::Link,
                "the propagated condition is LINK, not the target's own"
            );
        });
    }

    #[test]
    fn nms_does_not_propagate_severity() {
        let d = db(
            "record(ai, \"PV:A\") {\n    field(INP, \"PV:B PP NMS\")\n}\n\
                    record(ai, \"PV:B\") {\n    field(INP, \"11\")\n\
                    field(HIHI, \"10\")\n    field(HHSV, \"MAJOR\")\n}\n",
        );
        let a = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            process(set, a, &mut ctx).expect("process succeeds");
            assert_eq!(set.get(a).common.sevr, Severity::NoAlarm);
        });
    }

    #[test]
    fn a_link_to_a_dot_field_reads_that_field() {
        let d = db(
            "record(ai, \"PV:A\") {\n    field(INP, \"PV:B.SEVR NPP\")\n}\n\
                    record(ai, \"PV:B\") {\n}\n",
        );
        let a = d.lookup("PV:A").expect("PV:A exists");
        let b = d.lookup("PV:B").expect("PV:B exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            set.get_mut(b).common.sevr = Severity::Major;
            process(set, a, &mut ctx).expect("process succeeds");
            assert_eq!(set.get(a).val.as_f64(), 2.0, "MAJOR reads as 2");
        });
    }

    #[test]
    fn a_pp_cycle_terminates_via_pact() {
        let d = db("record(ai, \"PV:A\") {\n    field(INP, \"PV:B PP\")\n}\n\
                    record(ai, \"PV:B\") {\n    field(INP, \"PV:A PP\")\n}\n");
        let a = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            process(set, a, &mut ctx).expect("PACT must break the cycle, not the stack");
        });
        assert!(ctx.depth() == 0, "the depth counter must unwind cleanly");
    }

    #[test]
    fn a_pp_self_link_terminates_via_pact_not_the_depth_cap() {
        // A record whose INP is a PP link to itself is the degenerate
        // one-node cycle. `process()` checks PACT *before* `push_depth`
        // (step 1 runs ahead of the depth-counted call to `process_inner`),
        // so the recursive `process()` call this self-link makes finds PACT
        // already set and returns immediately without ever pushing a second
        // depth frame. Termination is therefore via the PACT brake, not the
        // MAX_DEPTH cap: TPRO tracing below shows "process entered" exactly
        // once, not once per recursion up to MAX_DEPTH, and "PACT already
        // set" firing on that same first (and only) re-entry.
        let d =
            db("record(ai, \"PV:A\") {\n    field(INP, \"PV:A PP\")\n    field(TPRO, \"1\")\n}\n");
        let a = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            process(set, a, &mut ctx).expect("PACT must break the self-link, not the stack");
            assert!(
                !set.get(a).common.pact,
                "PACT must be clear again once the outer pass unwinds"
            );
        });
        assert!(ctx.depth() == 0, "the depth counter must unwind cleanly");
        let entered = ctx
            .trace
            .iter()
            .filter(|l| l.contains("process entered"))
            .count();
        assert_eq!(
            entered, 1,
            "PACT must stop the recursion on its first re-entry, not run it out to MAX_DEPTH"
        );
        assert!(
            ctx.trace
                .iter()
                .any(|l| l.contains("PACT already set, returning")),
            "the self-link's recursive process() call must observe PACT already set"
        );
    }

    #[test]
    fn a_constant_link_is_a_no_op_during_processing() {
        // `dbGetLink` returns immediately for a CONSTANT link; it does not
        // touch the destination. A soft `ai` with no INP therefore keeps
        // whatever a direct write (e.g. caput) last gave it, across any
        // number of process passes, rather than being clobbered back to the
        // link's default zero.
        let d = db("record(ai, \"PV:A\") {\n}\n");
        let a = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            set.get_mut(a).val = Value::Double(42.0);
            process(set, a, &mut ctx).expect("process succeeds");
            assert_eq!(set.get(a).val.as_f64(), 42.0, "VAL must survive pass 1");
            process(set, a, &mut ctx).expect("process succeeds");
            assert_eq!(set.get(a).val.as_f64(), 42.0, "VAL must survive pass 2");
        });
    }

    #[test]
    fn writing_dot_proc_forces_a_process_pass() {
        let d = db("record(ai, \"PV:A\") {\n}\n");
        let a = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            write_field(set, a, Field::Proc, Value::Long(1), &mut ctx).expect("write succeeds");
            assert!(set.get(a).time_ns > 0, "writing PROC processes the record");
        });
    }

    // --- Task 9: type-specific bodies ---------------------------------------

    // The brief's original fixtures for these two tests gave DOL a bare
    // numeric literal ("9"), which `build.rs::link` parses as a *constant*
    // link. A constant DOL is init-seeded into VAL unconditionally at load
    // (`build.rs::init_constant`, ungated by OMSL — see
    // `a_specified_numeric_dol_seeds_val_and_clears_udf_for_an_output_record`
    // in `build.rs`) and is then a no-op on every later process pass
    // regardless of OMSL (the same CONSTANT-link idiom `input_body` uses for
    // INP). So under a constant DOL, supervisory and closed_loop are
    // indistinguishable in this engine, just as they are in EPICS Base:
    // `aoRecord.c`'s `init_record` seeds VAL from a constant DOL ungated by
    // OMSL, and `dbGetLink` on a CONSTANT link during `process` writes
    // nothing either way. That made the original fixture unsound in
    // principle — the same failure mode Task 8's PP/NPP fixtures had — not
    // just for this implementation but for any correct one. The fix moves
    // DOL to a real DB link (`PV:SRC`, a plain `ai` record) so the two tests
    // actually exercise different code paths: supervisory never reads
    // `PV:SRC` at all; closed_loop reads it and overwrites VAL. The tests'
    // names and asserted intent are unchanged.

    #[test]
    fn an_output_record_in_supervisory_mode_keeps_its_own_value() {
        let d = db("record(ao, \"PV:O\") {\n    field(VAL, \"3\")\n\
                    field(DOL, \"PV:SRC\")\n    field(OMSL, \"supervisory\")\n}\n\
                    record(ai, \"PV:SRC\") {\n    field(VAL, \"9\")\n}\n");
        let id = d.lookup("PV:O").expect("PV:O exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            process(set, id, &mut ctx).expect("process succeeds");
            assert_eq!(set.get(id).val.as_f64(), 3.0, "supervisory ignores DOL");
        });
    }

    #[test]
    fn an_output_record_in_closed_loop_takes_its_value_from_dol() {
        let d = db("record(ao, \"PV:O\") {\n    field(VAL, \"3\")\n\
                    field(DOL, \"PV:SRC\")\n    field(OMSL, \"closed_loop\")\n}\n\
                    record(ai, \"PV:SRC\") {\n    field(VAL, \"9\")\n}\n");
        let id = d.lookup("PV:O").expect("PV:O exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            process(set, id, &mut ctx).expect("process succeeds");
            assert_eq!(set.get(id).val.as_f64(), 9.0, "closed_loop follows DOL");
        });
    }

    #[test]
    fn an_output_record_writes_its_value_through_out() {
        let d = db("record(ao, \"PV:O\") {\n    field(VAL, \"4\")\n\
                    field(OUT, \"PV:T.VAL\")\n}\n\
                    record(ai, \"PV:T\") {\n}\n");
        let o = d.lookup("PV:O").expect("PV:O exists");
        let t = d.lookup("PV:T").expect("PV:T exists");
        let mut ctx = ProcCtx::new();
        d.with_set(o.set, |set| {
            process(set, o, &mut ctx).expect("process succeeds");
            assert_eq!(set.get(t).val.as_f64(), 4.0, "OUT must write the target");
        });
    }

    #[test]
    fn an_npp_out_write_posts_a_monitor_naming_the_target() {
        // OUT with no PP suffix is NPP (the default link mode). Nothing
        // reprocesses PV:T, so if write_field doesn't post the monitor
        // itself, no subscriber ever hears about the change -- this is
        // Finding 1's core bug.
        let d = db("record(ao, \"PV:O\") {\n    field(VAL, \"4\")\n\
                    field(OUT, \"PV:T.VAL\")\n}\n\
                    record(ai, \"PV:T\") {\n}\n");
        let o = d.lookup("PV:O").expect("PV:O exists");
        let mut ctx = ProcCtx::new();
        d.with_set(o.set, |set| {
            process(set, o, &mut ctx).expect("process succeeds");
        });
        let names = event_names(&mut ctx);
        assert!(
            names.contains(&"PV:T".to_string()),
            "the NPP OUT write must post a monitor naming the target, got {names:?}"
        );
    }

    #[test]
    fn an_npp_out_write_advances_the_targets_prev_val_for_the_next_mdel_check() {
        // PV:T has MDEL = 10. The first OUT write moves it 0 -> 15, which
        // is past MDEL, so it posts and (per post_monitors' existing
        // contract) advances prev_val to 15. If write_field left prev_val
        // stale at 0.0 instead (Finding 1's bug), a second write moving
        // PV:T from 15 to 20 -- only a 5-unit move from the correct
        // reference, well inside MDEL -- would instead be measured from the
        // stale 0.0 reference as a 20-unit move and wrongly posted.
        let d = db("record(ao, \"PV:O\") {\n    field(VAL, \"15\")\n\
                    field(OUT, \"PV:T.VAL\")\n}\n\
                    record(ai, \"PV:T\") {\n    field(MDEL, \"10\")\n}\n");
        let o = d.lookup("PV:O").expect("PV:O exists");
        let t = d.lookup("PV:T").expect("PV:T exists");
        let mut ctx = ProcCtx::new();
        d.with_set(o.set, |set| {
            process(set, o, &mut ctx).expect("process succeeds");
            assert_eq!(
                set.get(t).prev_val.as_f64(),
                15.0,
                "a past-MDEL write must advance PV:T's prev_val, not leave it stale"
            );
        });
        let _ = ctx.take_events();
        // A second OUT write moving PV:T from 15 to 20 is only a 5-unit
        // move from the correctly-advanced reference, inside MDEL's 10, so
        // it must be suppressed.
        d.with_set(o.set, |set| {
            set.get_mut(o).val = Value::Double(20.0);
            process(set, o, &mut ctx).expect("process succeeds");
        });
        let names = event_names(&mut ctx);
        assert!(
            !names.contains(&"PV:T".to_string()),
            "a sub-MDEL move from the correctly-advanced prev_val must be suppressed, got {names:?}"
        );
    }

    #[test]
    fn a_pp_out_write_posts_exactly_once() {
        // OUT with PP: output_body calls write_field (which posts the
        // target's monitor itself under Finding 1's fix) and then
        // reprocesses the target because process_passive is set. The
        // target's own process() must not repost the same value change --
        // only one monitor should reach the subscriber for one write.
        let d = db("record(ao, \"PV:O\") {\n    field(VAL, \"4\")\n\
                    field(OUT, \"PV:T.VAL PP\")\n}\n\
                    record(ai, \"PV:T\") {\n}\n");
        let o = d.lookup("PV:O").expect("PV:O exists");
        let t = d.lookup("PV:T").expect("PV:T exists");
        let mut ctx = ProcCtx::new();
        d.with_set(o.set, |set| {
            process(set, o, &mut ctx).expect("process succeeds");
        });
        let names = event_names(&mut ctx);
        let target_posts = names.iter().filter(|n| *n == "PV:T").count();
        assert_eq!(
            target_posts, 1,
            "a PP OUT write must post exactly one monitor for the target, got {names:?}"
        );
        d.with_set(o.set, |set| {
            assert_eq!(set.get(t).prev_val.as_f64(), 4.0);
        });
    }

    #[test]
    fn a_binary_record_stores_zero_or_one() {
        let d = db("record(bi, \"PV:B\") {\n    field(INP, \"7\")\n}\n");
        let id = d.lookup("PV:B").expect("PV:B exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            process(set, id, &mut ctx).expect("process succeeds");
            assert_eq!(
                set.get(id).val,
                Value::Enum(1),
                "any non-zero input means 1"
            );
        });
    }

    #[test]
    fn a_long_record_rounds_a_double_input() {
        let d = db(
            "record(longin, \"PV:L\") {\n    field(INP, \"PV:S NPP\")\n}\n\
                    record(ai, \"PV:S\") {\n    field(VAL, \"2.6\")\n}\n",
        );
        let l = d.lookup("PV:L").expect("PV:L exists");
        let mut ctx = ProcCtx::new();
        d.with_set(l.set, |set| {
            process(set, l, &mut ctx).expect("process succeeds");
            assert_eq!(set.get(l).val, Value::Long(3));
        });
    }

    #[test]
    fn a_binary_record_does_not_apply_numeric_limits() {
        let d = db("record(bi, \"PV:B\") {\n    field(INP, \"1\")\n\
                    field(HIHI, \"0\")\n    field(HHSV, \"MAJOR\")\n}\n");
        let id = d.lookup("PV:B").expect("PV:B exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            process(set, id, &mut ctx).expect("process succeeds");
            assert_eq!(
                set.get(id).common.sevr,
                Severity::NoAlarm,
                "binary records have no analogue limits"
            );
        });
    }

    // --- discriminators for the corrected `is_analogue` set -----------------
    //
    // Ruling: `is_analogue` is `Ai | Ao | LongIn | LongOut`, matching EPICS
    // (longinRecord/longoutRecord carry HIHI/HIGH/LOW/LOLO and run the same
    // checkAlarms ladder as ai/ao; only bi/bo have no limit fields at all).
    // Neither of these was pinned by the brief's own tests, so a later
    // narrowing of `is_analogue` back to `{Ai, Ao}` would pass every test
    // above but silently drop limit checking for longin/longout.

    #[test]
    fn a_longin_record_crossing_hihi_is_limit_checked() {
        // INP is a constant (numeric literal), so it seeds VAL = 20 at load
        // and is a no-op on every later process pass (see
        // `a_constant_link_is_a_no_op_during_processing`); the limit ladder
        // is what must catch the crossing, not the input read.
        let d = db("record(longin, \"PV:L\") {\n    field(INP, \"20\")\n\
                    field(HIHI, \"10\")\n    field(HHSV, \"MAJOR\")\n}\n");
        let id = d.lookup("PV:L").expect("PV:L exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            process(set, id, &mut ctx).expect("process succeeds");
            assert_eq!(
                set.get(id).common.sevr,
                Severity::Major,
                "longin must run the same limit ladder as ai/ao"
            );
            assert_eq!(set.get(id).common.stat, Condition::HiHi);
        });
    }

    #[test]
    fn a_never_processed_binary_record_is_invalid_udf() {
        // bi/bo never reach `check_limits` (they have no limit fields), so
        // `record_body` must call `check_udf` directly for them — otherwise
        // a never-processed binary record would never be promoted to
        // INVALID/UDF at all. There is no "peek without processing" API (see
        // the equivalent ai-kind tests above), so this drives `check_udf`
        // directly, the same way those do.
        let d = db("record(bi, \"PV:B\") {\n}\n");
        let id = d.lookup("PV:B").expect("PV:B exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            assert!(
                set.get(id).common.udf,
                "a never-processed record is UDF by default"
            );
            check_udf(set, id);
            reset_alarms(set, id, &mut ctx);
            assert_eq!(
                set.get(id).common.sevr,
                Severity::Invalid,
                "bi must still get UDF promotion despite having no limit fields"
            );
            assert_eq!(set.get(id).common.stat, Condition::Udf);
        });
    }

    // --- Task 11: async completion -----------------------------------------

    #[test]
    fn completing_an_async_record_clears_pact_and_posts() {
        // Inverting the `if !set.get(id).common.pact { return Ok(()); }`
        // guard's negation (i.e. always running the body) would still pass
        // this test, since PACT is set here — this test alone only proves
        // the "PACT set" arm; its no-op sibling below covers the other arm.
        let d = db("record(ai, \"PV:A\") {\n    field(INP, \"3\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            // Simulate a body that returned with the operation outstanding.
            set.get_mut(id).common.pact = true;
            complete_async(set, id, &mut ctx).expect("completion succeeds");
            assert!(!set.get(id).common.pact, "completion clears PACT");
            assert_eq!(set.get(id).val.as_f64(), 3.0, "the body ran on completion");
        });
        assert_eq!(event_names(&mut ctx), vec!["PV:A".to_string()]);
    }

    #[test]
    fn completing_a_record_that_is_not_active_is_a_no_op() {
        // Inverts the same guard the other way: PACT is left clear (the
        // record's ordinary default), so if the `if !pact { return }` check
        // were removed or inverted, `record_body` would run, `time_ns` would
        // become nonzero and a monitor would post — both assertions below
        // would fail.
        let d = db("record(ai, \"PV:A\") {\n    field(INP, \"3\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            complete_async(set, id, &mut ctx).expect("no-op succeeds");
            assert_eq!(set.get(id).time_ns, 0, "nothing should have processed");
        });
        assert!(ctx.take_events().is_empty());
    }

    #[test]
    fn an_async_completion_fires_the_forward_link() {
        // If `complete_async` finished the record via some partial path that
        // skipped `forward_link` (rather than the full `record_body`), PV:B
        // would never process and this event list would be just ["PV:A"].
        let d = db(
            "record(ai, \"PV:A\") {\n    field(INP, \"1\")\n    field(FLNK, \"PV:B\")\n}\n\
                    record(ai, \"PV:B\") {\n    field(INP, \"2\")\n}\n",
        );
        let a = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(a.set, |set| {
            set.get_mut(a).common.pact = true;
            complete_async(set, a, &mut ctx).expect("completion succeeds");
        });
        assert_eq!(
            event_names(&mut ctx),
            vec!["PV:A".to_string(), "PV:B".to_string()]
        );
    }

    #[test]
    fn an_ordinary_process_still_returns_early_while_pact_is_set() {
        // Confirms the PACT brake and the completion path do not fight: a
        // record whose PACT is set by an async body still bounces off the
        // Task 6 brake in `process()`, even though `complete_async` can
        // finish that same record. Without this test, a change that made
        // `complete_async` clear PACT *before* running the body (letting a
        // reentrant `process()` call slip through and double-process) would
        // not be caught by the three tests above alone.
        let d = db("record(ai, \"PV:A\") {\n    field(INP, \"3\")\n}\n");
        let id = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        d.with_set(id.set, |set| {
            set.get_mut(id).common.pact = true;
            set.get_mut(id).time_ns = 0;
            process(set, id, &mut ctx).expect("an active record is not an error");
            assert_eq!(
                set.get(id).time_ns,
                0,
                "process() must not have run the body"
            );
            assert!(
                set.get(id).common.pact,
                "PACT is left set for the completion path"
            );
        });
        assert!(
            ctx.take_events().is_empty(),
            "process() must not post while PACT is held"
        );
    }

    #[test]
    fn a_too_deep_error_does_not_strand_pact_for_later_passes() {
        // Fix round 2: a `TooDeep` error from `input_body` (via a PP link's
        // nested `process()` call) must not leave PACT stuck on the record
        // whose pass failed. If it did, `process()`'s own PACT brake would
        // report every later call on that record as an unremarkable `Ok(())`
        // while doing nothing — a silent, permanent stall, strictly worse
        // than the recursion error it came from. That is the bug this test
        // exists to catch; it must go red if `record_body` reverts to
        // clearing PACT only via a bare `?` on `input_body`/`output_body`.
        // PV:B's INP is a nonzero constant so it seeds VAL = 5.0 at load
        // (constant links are a load-time init, not a per-pass fetch — see
        // `a_constant_link_is_a_no_op_during_processing`), giving both
        // records a real MDEL-worthy delta from their zero `prev_val` on
        // their first completed pass. Without this, both would start and
        // stay at VAL == prev_val == 0.0 and neither would post, the same
        // "no INP, no alarm" case `mdel_suppresses_a_change_smaller_than_the_deadband`
        // documents — which would make the final event-list assertion below
        // pass vacuously instead of proving the recovered pass actually ran.
        let d = db("record(ai, \"PV:A\") {\n    field(INP, \"PV:B PP\")\n}\n\
                    record(ai, \"PV:B\") {\n    field(INP, \"5\")\n}\n");
        let a = d.lookup("PV:A").expect("PV:A exists");
        let mut ctx = ProcCtx::new();
        // Fill the depth budget to one below the cap: PV:A's own
        // `push_depth` still succeeds (so `process_inner` enters, sets
        // PV:A's PACT, and calls into `input_body`), but the nested
        // `process()` call PV:A's PP link makes on PV:B pushes the depth to
        // exactly the cap and fails.
        for _ in 0..MAX_DEPTH - 1 {
            ctx.push_depth("outer").expect("within the cap");
        }
        let first = d.with_set(a.set, |set| process(set, a, &mut ctx));
        assert!(
            matches!(first, Err(ProcError::TooDeep { .. })),
            "got {first:?}"
        );
        // Drain this test's artificial frames back to zero. `process()`'s
        // own push/pop pair around PV:A (and the failed push for PV:B, which
        // never incremented the depth) already left the real bookkeeping
        // balanced; only the setup above needs undoing.
        for _ in 0..MAX_DEPTH - 1 {
            ctx.pop_depth();
        }
        // The assertion that matters: an ordinary pass on PV:A afterwards
        // must actually run, not be silently swallowed by a stuck PACT.
        d.with_set(a.set, |set| {
            process(set, a, &mut ctx)
                .expect("a plain pass must succeed once the depth budget is free");
            assert!(
                set.get(a).time_ns > 0,
                "PV:A must have actually processed, not been swallowed by a stuck PACT"
            );
            assert!(
                !set.get(a).common.pact,
                "a completed synchronous pass must leave PACT clear"
            );
        });
        assert_eq!(
            event_names(&mut ctx),
            vec!["PV:B".to_string(), "PV:A".to_string()],
            "the recovered pass must run to completion, including the PP-processed target"
        );
    }
}
