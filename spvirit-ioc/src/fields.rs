//! `.FIELD` access over the IOC's typed record model.
//!
//! `SimplePvStore` answers field reads out of the raw `.db` string map it
//! kept. This crate threw those strings away at build time in favour of a
//! typed model, so the mapping is written out here instead: one table from a
//! record to a value, and a parallel one from a record *kind* to a field
//! kind. The second exists so `Source::claim` can answer a channel search
//! without taking a lock set — Base's `dbNameToAddr`/`dbGetField` split.
//!
//! `the_kind_table_agrees_with_the_value_table_for_every_kind_and_field`
//! pins the two together; without it a cheap claim could advertise a type
//! the get then contradicts.

use crate::model::{Kind, Link, Omsl, Record, Value};
use spvirit_server::record_fields::{FieldKind, dbcommon_default, dbcommon_default_value};
use spvirit_types::ScalarValue;

/// Every field the IOC serves from its own model, in a fixed order.
///
/// Fields outside this list fall through to [`dbcommon_default_value`].
pub const IOC_FIELDS: &[&str] = &[
    "NAME", "RTYP", "DTYP", "VAL", "DESC", "EGU", "SCAN", "PINI", "PHAS", "PACT", "DISA", "DISV",
    "DISS", "SDIS", "FLNK", "INP", "OUT", "DOL", "OMSL", "TSE", "TPRO", "UDF", "SEVR", "STAT",
    "PROC", "HIHI", "HIGH", "LOW", "LOLO", "HHSV", "HSV", "LSV", "LLSV", "HYST", "MDEL", "ADEL",
];

/// Resolves a link target's slot id back to a record name.
///
/// [`Link::Db`]'s `target` holds a `RecordId` once `RecordDb::build` has
/// resolved it, and `Target::name()` returns `None` in exactly that case, so
/// only a caller holding the whole database can name it. `IocSource` passes
/// a closure over the name map it builds at load time — no lock set needed.
pub type TargetNames<'a> = &'a dyn Fn(&crate::lockset::RecordId) -> Option<String>;

/// Render a link field the way `dbpr` prints it: target, field, and both
/// modifiers, however terse the `.db` was.
///
/// [`Link`] has no `Display` impl and deliberately so: this is the only place
/// that needs a textual form, and the form is a wire value rather than a
/// diagnostic.
pub fn render_link(link: &Link, names: TargetNames) -> String {
    match link {
        Link::Constant(Value::Double(v)) => format!("{v}"),
        Link::Constant(Value::Long(v)) => format!("{v}"),
        Link::Constant(Value::Enum(v)) => format!("{v}"),
        Link::Unresolved { name } => name.clone(),
        Link::Db {
            target,
            field,
            process_passive,
            maximize_severity,
        } => {
            let name = match target {
                crate::model::Target::Name(n) => n.clone(),
                crate::model::Target::Id(id) => {
                    names(id).unwrap_or_else(|| "<unresolved>".to_string())
                }
            };
            let mut s = name;
            s.push('.');
            s.push_str(&format!("{field:?}").to_ascii_uppercase());
            if *process_passive {
                s.push_str(" PP");
            } else {
                s.push_str(" NPP");
            }
            if *maximize_severity {
                s.push_str(" MS");
            } else {
                s.push_str(" NMS");
            }
            s
        }
    }
}

/// Resolve `field` on `record`.
///
/// `names` names the targets of `record`'s own db links — see
/// [`TargetNames`]. The IOC's own model is consulted first, then
/// [`dbcommon_default_value`] — the same fallback tier 2 uses — then `None`.
pub fn record_field_value(record: &Record, field: &str, names: TargetNames) -> Option<ScalarValue> {
    let s = |v: &str| Some(ScalarValue::Str(v.to_string()));
    let i = |v: i32| Some(ScalarValue::I32(v));
    let d = |v: f64| Some(ScalarValue::F64(v));
    let c = &record.common;
    let l = &record.limits;
    match field {
        "NAME" => s(&record.name),
        "RTYP" => s(record.kind.db_name()),
        "DTYP" => s("Soft Channel"),
        "VAL" => Some(match record.val {
            Value::Double(v) => ScalarValue::F64(v),
            Value::Long(v) => ScalarValue::I32(v),
            Value::Enum(v) => ScalarValue::U16(v),
        }),
        "DESC" => s(&c.desc),
        "EGU" => s(&record.egu),
        "SCAN" => s(&c.scan_raw),
        "PINI" => s(if c.pini { "YES" } else { "NO" }),
        "PHAS" => i(c.phas),
        "PACT" => i(i32::from(c.pact)),
        "DISA" => i(c.disa),
        "DISV" => i(c.disv),
        "DISS" => s(c.diss.epics_string()),
        "SDIS" => s(&render_link(&c.sdis, names)),
        "FLNK" => s(&render_link(&c.flnk, names)),
        "INP" => s(&render_link(&record.inp, names)),
        "OUT" => s(&render_link(&record.out, names)),
        "DOL" => s(&render_link(&record.dol, names)),
        "OMSL" => s(match record.omsl {
            Omsl::Supervisory => "supervisory",
            Omsl::ClosedLoop => "closed_loop",
        }),
        "TSE" => i(c.tse),
        "TPRO" => i(i32::from(c.tpro)),
        "UDF" => i(i32::from(c.udf)),
        "SEVR" => s(c.sevr.epics_string()),
        "STAT" => s(c.stat.epics_string()),
        // PROC reads as zero and writes trigger processing; writes are B's.
        // This arm is an equivalent mutant under `cargo mutants`: PROC's
        // dbCommon default (DBCOMMON_DEFAULTS, "0") is the same literal
        // value, so deleting this arm and falling through to
        // `dbcommon_default_value` produces an identical answer for every
        // input. No value-based test can distinguish the two; see Task 11's
        // mutation report.
        "PROC" => i(0),
        "HIHI" => d(l.hihi),
        "HIGH" => d(l.high),
        "LOW" => d(l.low),
        "LOLO" => d(l.lolo),
        "HHSV" => s(l.hhsv.epics_string()),
        "HSV" => s(l.hsv.epics_string()),
        "LSV" => s(l.lsv.epics_string()),
        "LLSV" => s(l.llsv.epics_string()),
        "HYST" => d(l.hyst),
        "MDEL" => d(l.mdel),
        "ADEL" => d(l.adel),
        _ => dbcommon_default_value(field),
    }
}

/// The scalar kind `field` serves as on a record of `kind`, without touching
/// a record. Kept in lock-step with [`record_field_value`] by
/// `the_kind_table_agrees_with_the_value_table_for_every_kind_and_field`.
pub fn record_field_kind(kind: Kind, field: &str) -> Option<FieldKind> {
    match field {
        "VAL" => Some(match kind {
            Kind::Ai | Kind::Ao => FieldKind::Double,
            Kind::LongIn | Kind::LongOut | Kind::Bi | Kind::Bo => FieldKind::Int,
        }),
        "NAME" | "RTYP" | "DTYP" | "DESC" | "EGU" | "SCAN" | "PINI" | "DISS" | "SDIS" | "FLNK"
        | "INP" | "OUT" | "DOL" | "OMSL" | "SEVR" | "STAT" | "HHSV" | "HSV" | "LSV" | "LLSV" => {
            Some(FieldKind::Str)
        }
        "PHAS" | "PACT" | "DISA" | "DISV" | "TSE" | "TPRO" | "UDF" | "PROC" => Some(FieldKind::Int),
        "HIHI" | "HIGH" | "LOW" | "LOLO" | "HYST" | "MDEL" | "ADEL" => Some(FieldKind::Double),
        _ => dbcommon_default(field).map(|(k, _)| k),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Record};
    use spvirit_server::field_provider::field_kind_of;
    use spvirit_server::record_fields::FieldKind;
    use spvirit_types::ScalarValue;

    /// These records come straight from `build_records`, before
    /// `RecordDb::build` resolves any link target to a slot id, so every
    /// `Target` is still a `Target::Name` and the resolver is never reached.
    fn value(r: &Record, field: &str) -> Option<ScalarValue> {
        record_field_value(r, field, &|_| None)
    }

    fn build(db: &str) -> Vec<Record> {
        let raw = spvirit_server::db::parse_db_records(
            db,
            "t.db",
            &std::collections::HashMap::new(),
        )
        .expect("parse");
        crate::build::build_records(&raw).expect("build")
    }

    fn sample() -> Record {
        build(
            "record(ai, \"PV:A\") {\n\
             field(DESC, \"a sample\")\n\
             field(EGU, \"C\")\n\
             field(SCAN, \"1 second\")\n\
             field(PINI, \"YES\")\n\
             field(PHAS, \"3\")\n\
             field(HIHI, \"90\")\n\
             field(HHSV, \"MAJOR\")\n\
             field(MDEL, \"0.5\")\n\
             field(INP, \"PV:B PP\")\n\
             field(FLNK, \"PV:B\")\n\
             }\n\
             record(ai, \"PV:B\") {\n\
             }\n",
        )
        .remove(0)
    }

    #[test]
    fn serves_the_typed_model_as_fields() {
        let r = sample();
        assert_eq!(value(&r, "NAME"), Some(ScalarValue::Str("PV:A".into())));
        assert_eq!(value(&r, "RTYP"), Some(ScalarValue::Str("ai".into())));
        assert_eq!(value(&r, "DESC"), Some(ScalarValue::Str("a sample".into())));
        assert_eq!(value(&r, "EGU"), Some(ScalarValue::Str("C".into())));
        assert_eq!(value(&r, "SCAN"), Some(ScalarValue::Str("1 second".into())));
        assert_eq!(value(&r, "PINI"), Some(ScalarValue::Str("YES".into())));
        assert_eq!(value(&r, "PHAS"), Some(ScalarValue::I32(3)));
        assert_eq!(value(&r, "HIHI"), Some(ScalarValue::F64(90.0)));
        assert_eq!(value(&r, "HHSV"), Some(ScalarValue::Str("MAJOR".into())));
        assert_eq!(value(&r, "MDEL"), Some(ScalarValue::F64(0.5)));
        assert_eq!(value(&r, "SEVR"), Some(ScalarValue::Str("NO_ALARM".into())));
        assert_eq!(value(&r, "STAT"), Some(ScalarValue::Str("NO_ALARM".into())));
        assert_eq!(value(&r, "UDF"), Some(ScalarValue::I32(1)));
        assert_eq!(value(&r, "VAL"), Some(ScalarValue::F64(0.0)));
    }

    /// `dbpr` prints links fully expanded — target, field, and both
    /// modifiers — regardless of how terse the `.db` was.
    #[test]
    fn renders_links_the_way_dbpr_would() {
        let r = sample();
        assert_eq!(value(&r, "INP"), Some(ScalarValue::Str("PV:B.VAL PP NMS".into())));
        // FLNK's implied field is whatever `build.rs` gives a link with no
        // explicit `.FIELD`. `build.rs::link()` defaults every link field —
        // FLNK included — to `Field::Val`; it does not special-case FLNK to
        // PROC the way real EPICS does. That is a pre-existing engine
        // behaviour from sub-project A, not something A2 changes here, so
        // the expectation below matches what the engine actually produces
        // and the divergence from `dbpr` (which would print PROC) is
        // reported as a concern rather than "fixed" in this task.
        assert_eq!(value(&r, "FLNK"), Some(ScalarValue::Str("PV:B.VAL NPP NMS".into())));
        // An absent link is the constant zero EPICS leaves it at, not an error.
        assert_eq!(value(&r, "OUT"), Some(ScalarValue::Str("0".into())));
    }

    #[test]
    fn falls_back_to_dbcommon_for_fields_the_ioc_does_not_model() {
        let r = sample();
        assert_eq!(value(&r, "PRIO"), Some(ScalarValue::Str("LOW".into())));
        assert_eq!(value(&r, "ASG"), Some(ScalarValue::Str("".into())));
        assert_eq!(value(&r, "NOTAFIELD"), None);
    }

    /// Fields whose dbCommon fallback default happens to equal the typed
    /// model's own default (0, "0", "NO_ALARM", ...) need a record where the
    /// `.db` sets them away from that default — otherwise a match arm that
    /// reads the typed model and one that falls through to
    /// [`dbcommon_default_value`] would produce the same answer and the test
    /// suite could not tell them apart.
    #[test]
    fn explicit_fields_reflect_the_db_not_the_dbcommon_default() {
        let r = build(
            "record(ao, \"PV:X\") {\n\
             field(DISA, \"5\")\n\
             field(DISV, \"7\")\n\
             field(DISS, \"MAJOR\")\n\
             field(SDIS, \"9\")\n\
             field(TSE, \"3\")\n\
             field(TPRO, \"1\")\n\
             field(ADEL, \"2.5\")\n\
             field(DOL, \"1\")\n\
             }\n",
        )
        .remove(0);
        assert_eq!(value(&r, "DISA"), Some(ScalarValue::I32(5)));
        assert_eq!(value(&r, "DISV"), Some(ScalarValue::I32(7)));
        assert_eq!(value(&r, "DISS"), Some(ScalarValue::Str("MAJOR".into())));
        assert_eq!(value(&r, "SDIS"), Some(ScalarValue::Str("9".into())));
        assert_eq!(value(&r, "TSE"), Some(ScalarValue::I32(3)));
        assert_eq!(value(&r, "TPRO"), Some(ScalarValue::I32(1)));
        assert_eq!(value(&r, "ADEL"), Some(ScalarValue::F64(2.5)));
        // A specified constant DOL clears UDF at init — recGblInitConstantLink.
        assert_eq!(value(&r, "UDF"), Some(ScalarValue::I32(0)));
    }

    /// The descriptor path must never disagree with the value path — that is
    /// the one way a cheap `claim` can lie to a client.
    #[test]
    fn the_kind_table_agrees_with_the_value_table_for_every_kind_and_field() {
        for kind in [Kind::Ai, Kind::Ao, Kind::Bi, Kind::Bo, Kind::LongIn, Kind::LongOut] {
            let r = build(&format!("record({}, \"PV:X\") {{\n}}\n", kind.db_name())).remove(0);
            for field in IOC_FIELDS {
                let observed = record_field_value(&r, field, &|_| None)
                    .unwrap_or_else(|| panic!("{kind:?}.{field} must have a value"));
                let declared = record_field_kind(kind, field)
                    .unwrap_or_else(|| panic!("{kind:?}.{field} must have a kind"));
                assert_eq!(
                    field_kind_of(&observed),
                    declared,
                    "{kind:?}.{field}: the kind table and the value table disagree"
                );
            }
        }
    }

    #[test]
    fn the_kind_table_also_covers_the_dbcommon_fallback() {
        assert_eq!(record_field_kind(Kind::Ai, "PRIO"), Some(FieldKind::Str));
        assert_eq!(record_field_kind(Kind::Ai, "DISP"), Some(FieldKind::Int));
        assert_eq!(record_field_kind(Kind::Ai, "NOTAFIELD"), None);
    }
}
