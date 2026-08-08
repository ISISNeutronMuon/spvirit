//! Build the typed [`Record`] model from the untyped `.db` parse.

use crate::alarm::Severity;
use crate::model::{Common, Field, Kind, Limits, Link, Omsl, Record, Value};
use spvirit_server::db::DbRecord;
use std::collections::HashMap;

/// A record the engine cannot represent, naming the record and field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildError {
    pub record: String,
    pub field: String,
    pub message: String,
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.field.is_empty() {
            write!(f, "record '{}': {}", self.record, self.message)
        } else {
            write!(
                f,
                "record '{}' field {}: {}",
                self.record, self.field, self.message
            )
        }
    }
}

impl std::error::Error for BuildError {}

fn err(record: &str, field: &str, message: impl Into<String>) -> BuildError {
    BuildError {
        record: record.to_string(),
        field: field.to_string(),
        message: message.into(),
    }
}

fn num(
    fields: &HashMap<String, String>,
    name: &str,
    record: &str,
    default: f64,
) -> Result<f64, BuildError> {
    match fields.get(name) {
        None => Ok(default),
        Some(raw) => raw
            .trim()
            .parse::<f64>()
            .map_err(|_| err(record, name, format!("'{raw}' is not a number"))),
    }
}

fn int(
    fields: &HashMap<String, String>,
    name: &str,
    record: &str,
    default: i32,
) -> Result<i32, BuildError> {
    match fields.get(name) {
        None => Ok(default),
        Some(raw) => raw
            .trim()
            .parse::<i32>()
            .map_err(|_| err(record, name, format!("'{raw}' is not an integer"))),
    }
}

/// Parse a severity field (`HHSV`, `DISS`, …). EPICS accepts the menu names.
fn sev(
    fields: &HashMap<String, String>,
    name: &str,
    record: &str,
    default: Severity,
) -> Result<Severity, BuildError> {
    let Some(raw) = fields.get(name) else {
        return Ok(default);
    };
    match raw.trim().to_ascii_uppercase().as_str() {
        "NO_ALARM" | "NOALARM" | "" => Ok(Severity::NoAlarm),
        "MINOR" => Ok(Severity::Minor),
        "MAJOR" => Ok(Severity::Major),
        "INVALID" => Ok(Severity::Invalid),
        other => Err(err(record, name, format!("'{other}' is not a severity"))),
    }
}

/// Parse a link field into [`Link`].
///
/// A purely numeric value is a constant; anything else is a db link whose
/// `PP`/`NPP` and `MS`/`NMS` modifiers follow the target. Sub-project C adds
/// `CA`, `CP`, `CPP` and the hardware address forms.
fn link(
    fields: &HashMap<String, String>,
    name: &str,
    record: &str,
    kind: Kind,
) -> Result<Link, BuildError> {
    let Some(raw) = fields.get(name) else {
        return Ok(Link::Constant(Value::default_for(kind)));
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Link::Constant(Value::default_for(kind)));
    }
    if let Ok(v) = raw.parse::<f64>() {
        return Ok(Link::Constant(Value::Double(v).coerce_to(kind)));
    }

    let mut parts = raw.split_whitespace();
    let target_spec = parts.next().expect("non-empty after the trim");
    let mut process_passive = false;
    let mut maximize_severity = false;
    for modifier in parts {
        match modifier.to_ascii_uppercase().as_str() {
            "PP" => process_passive = true,
            "NPP" => process_passive = false,
            "MS" => maximize_severity = true,
            "NMS" => maximize_severity = false,
            // MSS/MSI are severity refinements C handles; treat as MS for now.
            "MSS" | "MSI" => maximize_severity = true,
            other => {
                return Err(err(
                    record,
                    name,
                    format!("unsupported link modifier '{other}'"),
                ));
            }
        }
    }

    let (target, field) = match target_spec.split_once('.') {
        Some((t, f)) => {
            let parsed = Field::parse(f)
                .ok_or_else(|| err(record, name, format!("unsupported link field '.{f}'")))?;
            (t.to_string(), parsed)
        }
        None => (target_spec.to_string(), Field::Val),
    };

    Ok(Link::Db {
        target,
        field,
        process_passive,
        maximize_severity,
    })
}

/// Build every record, or fail naming the first record that cannot be built.
pub fn build_records(raw: &[DbRecord]) -> Result<Vec<Record>, BuildError> {
    raw.iter().map(build_one).collect()
}

fn build_one(raw: &DbRecord) -> Result<Record, BuildError> {
    let name = raw.name.as_str();
    let kind = Kind::from_db_name(&raw.record_type).ok_or_else(|| {
        err(
            name,
            "",
            format!(
                "record type '{}' is not supported by the processing core \
                 (sub-project A covers ai, ao, bi, bo, longin, longout)",
                raw.record_type
            ),
        )
    })?;
    let f = &raw.fields;

    let common = Common {
        desc: f.get("DESC").cloned().unwrap_or_default(),
        scan_raw: f
            .get("SCAN")
            .cloned()
            .unwrap_or_else(|| "Passive".to_string()),
        pini: matches!(
            f.get("PINI")
                .map(|s| s.trim().to_ascii_uppercase())
                .as_deref(),
            Some("YES") | Some("1")
        ),
        phas: int(f, "PHAS", name, 0)?,
        pact: false,
        disa: int(f, "DISA", name, 0)?,
        disv: int(f, "DISV", name, 1)?,
        diss: sev(f, "DISS", name, Severity::NoAlarm)?,
        sdis: link(f, "SDIS", name, kind)?,
        flnk: link(f, "FLNK", name, kind)?,
        tse: int(f, "TSE", name, 0)?,
        tpro: int(f, "TPRO", name, 0)? != 0,
        udf: true,
        sevr: Severity::NoAlarm,
        stat: crate::alarm::Condition::NoAlarm,
        nsev: Severity::NoAlarm,
        nsta: crate::alarm::Condition::NoAlarm,
    };

    let configured = ["HIHI", "HIGH", "LOW", "LOLO"]
        .iter()
        .any(|k| f.contains_key(*k));
    let limits = Limits {
        hihi: num(f, "HIHI", name, 0.0)?,
        high: num(f, "HIGH", name, 0.0)?,
        low: num(f, "LOW", name, 0.0)?,
        lolo: num(f, "LOLO", name, 0.0)?,
        hhsv: sev(f, "HHSV", name, Severity::NoAlarm)?,
        hsv: sev(f, "HSV", name, Severity::NoAlarm)?,
        lsv: sev(f, "LSV", name, Severity::NoAlarm)?,
        llsv: sev(f, "LLSV", name, Severity::NoAlarm)?,
        hyst: num(f, "HYST", name, 0.0)?,
        mdel: num(f, "MDEL", name, 0.0)?,
        adel: num(f, "ADEL", name, 0.0)?,
        configured,
    };

    let omsl = match f
        .get("OMSL")
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("supervisory") => Omsl::Supervisory,
        Some("closed_loop") => Omsl::ClosedLoop,
        Some(other) => return Err(err(name, "OMSL", format!("'{other}' is not an OMSL mode"))),
    };

    let val = match f.get("VAL") {
        None => Value::default_for(kind),
        Some(raw_val) => {
            let parsed = raw_val
                .trim()
                .parse::<f64>()
                .map_err(|_| err(name, "VAL", format!("'{raw_val}' is not a number")))?;
            Value::Double(parsed).coerce_to(kind)
        }
    };

    Ok(Record {
        name: raw.name.clone(),
        kind,
        common,
        limits,
        val,
        prev_val: val,
        prev_archive_val: val,
        prev_sevr: Severity::NoAlarm,
        prev_stat: crate::alarm::Condition::NoAlarm,
        inp: link(f, "INP", name, kind)?,
        out: link(f, "OUT", name, kind)?,
        dol: link(f, "DOL", name, kind)?,
        omsl,
        time_ns: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_server::db::parse_db_records;
    use std::collections::HashMap;

    fn build(db: &str) -> Vec<Record> {
        let raw = parse_db_records(db, "t.db", &HashMap::new()).expect("parse");
        build_records(&raw).expect("build")
    }

    #[test]
    fn defaults_match_the_record_reference() {
        let recs = build("record(ai, \"PV:A\") {\n}\n");
        let r = &recs[0];
        assert_eq!(r.kind, Kind::Ai);
        assert!(r.common.udf, "a record that has never processed is UDF");
        assert_eq!(r.common.phas, 0);
        assert_eq!(r.common.disv, 1, "DISV defaults to 1, not 0");
        assert_eq!(r.common.diss, Severity::NoAlarm);
        assert_eq!(r.common.disa, 0);
        assert_eq!(r.limits.mdel, 0.0);
        assert_eq!(r.limits.adel, 0.0);
        assert!(matches!(r.inp, Link::Constant(_)));
    }

    #[test]
    fn all_six_kinds_are_recognised() {
        for name in ["ai", "ao", "bi", "bo", "longin", "longout"] {
            let kind = Kind::from_db_name(name)
                .unwrap_or_else(|| panic!("{name} must be a supported kind"));
            assert_eq!(kind.db_name(), name, "db_name must round-trip");
        }
    }

    #[test]
    fn an_unsupported_kind_is_a_build_error_naming_the_type() {
        let raw = parse_db_records("record(calc, \"PV:C\") {\n}\n", "t.db", &HashMap::new())
            .expect("parse");
        let err = build_records(&raw).expect_err("calc belongs to sub-project D");
        assert!(err.message.contains("calc"), "got {}", err.message);
        assert_eq!(err.record, "PV:C");
    }

    #[test]
    fn a_db_link_parses_its_process_and_severity_flags() {
        let recs = build("record(ai, \"PV:A\") {\n    field(INP, \"PV:B.VAL PP MS\")\n}\n");
        match &recs[0].inp {
            Link::Db {
                target,
                field,
                process_passive,
                maximize_severity,
            } => {
                assert_eq!(target, "PV:B");
                assert_eq!(*field, Field::Val);
                assert!(*process_passive, "PP must be honoured");
                assert!(*maximize_severity, "MS must be honoured");
            }
            other => panic!("expected a db link, got {other:?}"),
        }
    }

    #[test]
    fn a_bare_target_defaults_to_nppncams() {
        let recs = build("record(ai, \"PV:A\") {\n    field(INP, \"PV:B\")\n}\n");
        match &recs[0].inp {
            Link::Db {
                field,
                process_passive,
                maximize_severity,
                ..
            } => {
                assert_eq!(*field, Field::Val, "a bare target means .VAL");
                assert!(!*process_passive, "the default is NPP");
                assert!(!*maximize_severity, "the default is NMS");
            }
            other => panic!("expected a db link, got {other:?}"),
        }
    }

    #[test]
    fn a_numeric_link_is_a_constant_of_the_records_value_type() {
        let recs = build("record(longin, \"PV:L\") {\n    field(INP, \"42\")\n}\n");
        assert_eq!(recs[0].inp, Link::Constant(Value::Long(42)));
    }

    #[test]
    fn a_malformed_number_in_a_numeric_field_names_the_field() {
        let raw = parse_db_records(
            "record(ai, \"PV:A\") {\n    field(HIHI, \"not-a-number\")\n}\n",
            "t.db",
            &HashMap::new(),
        )
        .expect("parse");
        let err = build_records(&raw).expect_err("HIHI must be numeric");
        assert_eq!(err.field, "HIHI");
        assert_eq!(err.record, "PV:A");
    }

    #[test]
    fn value_coercion_follows_the_records_kind() {
        assert_eq!(Value::Double(2.6).coerce_to(Kind::LongIn), Value::Long(3));
        assert_eq!(Value::Long(1).coerce_to(Kind::Ai), Value::Double(1.0));
        assert_eq!(Value::Double(0.0).coerce_to(Kind::Bi), Value::Enum(0));
        assert_eq!(
            Value::Double(7.0).coerce_to(Kind::Bo),
            Value::Enum(1),
            "any non-zero input sets a binary record"
        );
    }
}
