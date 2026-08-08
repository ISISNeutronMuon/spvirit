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
use crate::model::{Field, Kind, Link, Target, Value};

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
        let kind = set.get(id).kind;
        let (value, _sev) = fetch_link_value(set, &sdis, kind, ctx)?;
        set.get_mut(id).common.disa = value.as_i32();
    }
    let record = set.get(id);
    if record.common.disa == record.common.disv {
        let diss = record.common.diss;
        if tpro {
            ctx.trace_line(format!("{name}: disabled (DISA == DISV)"));
        }
        // A disabled record still publishes its disabled state.
        let r = set.get_mut(id);
        r.common.nsev = Severity::NoAlarm;
        r.common.nsta = Condition::NoAlarm;
        r.common.nsev.raise(diss);
        if diss != Severity::NoAlarm || r.common.stat != Condition::Disable {
            r.common.nsta = Condition::Disable;
        }
        reset_alarms(set, id, ctx);
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

/// Commit `NSEV`/`NSTA` into `SEVR`/`STAT` and clear the pass state.
///
/// This is `recGblResetAlarms`. Task 7 extends it to post an alarm monitor;
/// here it only commits.
pub(crate) fn reset_alarms(set: &mut LockSetData, id: RecordId, _ctx: &mut ProcCtx) {
    let r = set.get_mut(id);
    r.common.sevr = r.common.nsev;
    r.common.stat = r.common.nsta;
    r.common.nsev = Severity::NoAlarm;
    r.common.nsta = Condition::NoAlarm;
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

/// The type-specific processing body. Tasks 7-10 build this out.
pub(crate) fn record_body(
    set: &mut LockSetData,
    id: RecordId,
    _ctx: &mut ProcCtx,
) -> Result<(), ProcError> {
    let r = set.get_mut(id);
    r.common.udf = false;
    r.time_ns = now_ns();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::build_records;
    use crate::lockset::RecordDb;
    use spvirit_server::db::parse_db_records;
    use std::collections::HashMap;

    fn db(text: &str) -> RecordDb {
        let raw = parse_db_records(text, "t.db", &HashMap::new()).expect("parse");
        RecordDb::build(build_records(&raw).expect("build"))
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
        let d = db(
            "record(ai, \"PV:A\") {\n    field(SDIS, \"PV:S\")\n    field(DISV, \"1\")\n}\n\
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
}
