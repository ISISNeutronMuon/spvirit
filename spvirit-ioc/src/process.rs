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

    // 5. A synchronous record is done. An async one returned from the body
    //    with PACT still set; sub-project B's completion path clears it.
    let still_async = set.get(id).common.pact && body.is_ok() && is_async_pending(set, id);
    if !still_async {
        set.get_mut(id).common.pact = false;
    }
    if tpro {
        ctx.trace_line(format!("{name}: process complete"));
    }
    body
}

/// Whether the body left an asynchronous operation outstanding.
///
/// A has no async devices, so this is always false. Sub-project B replaces
/// the body with one that can return `AsyncOutcome::Pending`.
fn is_async_pending(_set: &LockSetData, _id: RecordId) -> bool {
    false
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
/// Note for Task 10: the `.VAL` branch below sets the field and clears UDF
/// only — it does not touch `prev_val` or post any monitor. Monitor
/// bookkeeping (comparing against `prev_val`/`MDEL`/`ADEL` and queuing the
/// update) is Task 10's territory, not this one's.
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
    let r = set.get_mut(id);
    match field {
        Field::Val => {
            r.val = value.coerce_to(kind);
            r.common.udf = false;
        }
        Field::Disa => r.common.disa = value.as_i32(),
        Field::Disv => r.common.disv = value.as_i32(),
        Field::Hihi => r.limits.hihi = value.as_f64(),
        Field::High => r.limits.high = value.as_f64(),
        Field::Low => r.limits.low = value.as_f64(),
        Field::Lolo => r.limits.lolo = value.as_f64(),
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
pub(crate) fn record_body(
    set: &mut LockSetData,
    id: RecordId,
    ctx: &mut ProcCtx,
) -> Result<(), ProcError> {
    let kind = set.get(id).kind;
    if kind.is_output() {
        output_body(set, id, ctx)?;
    } else {
        input_body(set, id, ctx)?;
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
    forward_link(set, id, ctx)
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
    let archived =
        (record.val.as_f64() - record.prev_archive_val.as_f64()).abs() > record.limits.adel;
    let value = record.val;

    let r = set.get_mut(id);
    r.prev_val = value;
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
}
