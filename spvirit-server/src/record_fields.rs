//! IOC/QSRV-style record field access.
//!
//! Serves `<pvname>.<FIELD>` (and the obsolete `<pvname>.<FIELD>$` long-string
//! form) as independent read-only channels, mimicking IOC field-access
//! semantics so tools such as the EPICS Archiver Appliance can fetch record
//! metadata (`RTYP`, `NAME`, `DESC`, dbCommon fields, ...).
//!
//! Ported from the p4pillon `RecordProvider` / `DynamicRecordFields` design
//! (branch `50-access-to-ioc-fields`).

use crate::types::RecordType;

/// A parsed `<base>.<FIELD>[$]` channel-name reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRef {
    pub base: String,
    pub field: String,
    /// `true` when the obsolete `$` long-string suffix was present.
    pub long_string: bool,
}

/// Scalar type of a record field, per dbCommon.dbd semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Str,
    Int,
    Double,
}

/// Split a channel name into a [`FieldRef`] on its last `.`.
///
/// Returns `None` for names that cannot be an IOC field reference: no dot,
/// empty base or field, or a field part that is not all ASCII
/// uppercase/digits (record field names are uppercase by convention, so
/// `a.record.like.this` is never misclaimed).
pub fn parse_field_ref(name: &str) -> Option<FieldRef> {
    let (base, field_part) = name.rsplit_once('.')?;
    if base.is_empty() {
        return None;
    }
    let (field, long_string) = match field_part.strip_suffix('$') {
        Some(stripped) => (stripped, true),
        None => (field_part, false),
    };
    if field.is_empty() || !field.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
        return None;
    }
    Some(FieldRef {
        base: base.to_string(),
        field: field.to_string(),
        long_string,
    })
}

/// dbCommon field defaults: `(name, kind, default-as-string)`.
///
/// The default strings are parsed per `kind` when served. Table follows
/// dbCommon.dbd (the fields every EPICS record carries), as p4pillon's
/// `fields.py` encodes.
const DBCOMMON_DEFAULTS: &[(&str, FieldKind, &str)] = &[
    ("DESC", FieldKind::Str, ""),
    ("SCAN", FieldKind::Str, "Passive"),
    ("PINI", FieldKind::Str, "NO"),
    ("PHAS", FieldKind::Int, "0"),
    ("EVNT", FieldKind::Str, ""),
    ("PRIO", FieldKind::Str, "LOW"),
    ("DISV", FieldKind::Int, "1"),
    ("DISA", FieldKind::Int, "0"),
    ("SDIS", FieldKind::Str, ""),
    ("DISS", FieldKind::Str, "NO_ALARM"),
    ("PROC", FieldKind::Int, "0"),
    ("STAT", FieldKind::Str, "UDF"),
    ("SEVR", FieldKind::Str, "INVALID"),
    ("UDF", FieldKind::Int, "1"),
    ("TPRO", FieldKind::Int, "0"),
    ("FLNK", FieldKind::Str, ""),
    ("ADEL", FieldKind::Double, "0"),
    ("MDEL", FieldKind::Double, "0"),
    ("TSE", FieldKind::Int, "0"),
    ("DISP", FieldKind::Int, "0"),
    ("ACKS", FieldKind::Str, "NO_ALARM"),
    ("ACKT", FieldKind::Str, "YES"),
    ("ASG", FieldKind::Str, ""),
];

/// Look up the dbCommon default for `field`, if it is a common field.
pub fn dbcommon_default(field: &str) -> Option<(FieldKind, &'static str)> {
    DBCOMMON_DEFAULTS
        .iter()
        .find(|(name, _, _)| *name == field)
        .map(|(_, kind, default)| (*kind, *default))
}

/// The `.db` record-type name for a [`RecordType`] (what `RTYP` reports).
pub fn record_type_name(rt: &RecordType) -> &'static str {
    match rt {
        RecordType::Ai => "ai",
        RecordType::Ao => "ao",
        RecordType::Bi => "bi",
        RecordType::Bo => "bo",
        RecordType::StringIn => "stringin",
        RecordType::StringOut => "stringout",
        RecordType::Waveform => "waveform",
        RecordType::Aai => "aai",
        RecordType::Aao => "aao",
        RecordType::SubArray => "subArray",
        RecordType::NtTable => "ntTable",
        RecordType::NtNdArray => "ntNDArray",
        RecordType::Mbbi => "mbbi",
        RecordType::Mbbo => "mbbo",
        RecordType::Generic => "generic",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_field_ref() {
        let r = parse_field_ref("SIM:AO.RTYP").unwrap();
        assert_eq!(
            (r.base.as_str(), r.field.as_str(), r.long_string),
            ("SIM:AO", "RTYP", false)
        );
    }

    #[test]
    fn parses_long_string_suffix() {
        let r = parse_field_ref("SIM:AO.DESC$").unwrap();
        assert_eq!((r.field.as_str(), r.long_string), ("DESC", true));
    }

    #[test]
    fn rejects_non_field_names() {
        assert!(parse_field_ref("SIM:AO").is_none()); // no dot
        assert!(parse_field_ref("SIM:AO.").is_none()); // empty field
        assert!(parse_field_ref("SIM:AO.rtyp").is_none()); // lowercase = not a field
        assert!(parse_field_ref("SIM:AO.$").is_none()); // bare $
        assert!(parse_field_ref(".RTYP").is_none()); // empty base
    }

    #[test]
    fn dbcommon_defaults_cover_key_fields() {
        assert!(matches!(
            dbcommon_default("SCAN"),
            Some((FieldKind::Str, "Passive"))
        ));
        assert!(matches!(dbcommon_default("PINI"), Some((FieldKind::Str, "NO"))));
        assert!(matches!(dbcommon_default("PHAS"), Some((FieldKind::Int, "0"))));
        assert!(matches!(
            dbcommon_default("MDEL"),
            Some((FieldKind::Double, "0"))
        ));
        assert!(dbcommon_default("NOTAFIELD").is_none());
    }

    #[test]
    fn record_type_names_match_db_names() {
        assert_eq!(record_type_name(&RecordType::Ao), "ao");
        assert_eq!(record_type_name(&RecordType::StringIn), "stringin");
    }
}
