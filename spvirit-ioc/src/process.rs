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
use crate::model::{Field, Kind, Limits, Link, Record, Target, Value};

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
        let _ = reset_alarms(set, id, ctx);
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
/// including the ones (bi/bo/longin/longout) that never call
/// [`check_limits`] — see Task 9's binding note.
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
/// happens to sit outside a configured limit. Binary records have no
/// limits; their `Limits` stay at the defaults, where every severity is
/// `NoAlarm`, so the limit ladder is a no-op for them.
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
/// Task 8 adds full input-link semantics, including PP processing and MS
/// severity propagation. Until then, `Db` links read the target's field
/// directly with no side effects.
pub(crate) fn fetch_link_value(
    set: &mut LockSetData,
    link: &Link,
    kind: Kind,
    _ctx: &mut ProcCtx,
) -> Result<(Value, Severity), ProcError> {
    match link {
        Link::Constant(v) => Ok((v.coerce_to(kind), Severity::NoAlarm)),
        // An unresolvable link contributes the record's default. The name was
        // already reported once at load; failing every pass would be noise.
        Link::Unresolved { .. } => Ok((Value::default_for(kind), Severity::NoAlarm)),
        Link::Db {
            target: Target::Id(target_id),
            field,
            ..
        } => {
            let value = read_field(set, *target_id, *field);
            Ok((value.coerce_to(kind), Severity::NoAlarm))
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

/// The type-specific processing body. Tasks 8-10 build this out further;
/// Task 7 adds alarm-limit checking, which every numeric record needs
/// regardless of kind (binary records simply have every limit severity at
/// its `NoAlarm` default, so `check_limits` is a no-op for them).
pub(crate) fn record_body(
    set: &mut LockSetData,
    id: RecordId,
    ctx: &mut ProcCtx,
) -> Result<(), ProcError> {
    set.get_mut(id).common.udf = false;
    set.get_mut(id).time_ns = now_ns();
    check_limits(set, id);
    let _ = reset_alarms(set, id, ctx);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockset::RecordDb;
    use crate::test_support::db;

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
}
