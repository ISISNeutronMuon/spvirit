//! IOC/QSRV-style record field access.
//!
//! Serves `<pvname>.<FIELD>` (and the obsolete `<pvname>.<FIELD>$` long-string
//! form) as independent read-only channels, mimicking IOC field-access
//! semantics so tools such as the EPICS Archiver Appliance can fetch record
//! metadata (`RTYP`, `NAME`, `DESC`, dbCommon fields, ...).
//!
//! Ported from the p4pillon `RecordProvider` / `DynamicRecordFields` design
//! (branch `50-access-to-ioc-fields`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_types::{NtPayload, NtScalar, NtScalarArray, ScalarArrayValue};

use crate::field_provider::{RecordFieldProvider, resolve_field_info, resolve_field_payload};
use crate::pvstore::{PvInfo, Source};
use crate::types::{RecordInstance, RecordType, ScalarValue};

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
    if field.is_empty()
        || !field
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
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

/// The dbCommon default for `field`, already parsed to its declared type.
///
/// The fallback both stores use for fields their own model does not carry —
/// the analogue of `dbCommon.dbd` being included in every record type.
pub fn dbcommon_default_value(field: &str) -> Option<ScalarValue> {
    dbcommon_default(field).map(|(kind, default)| typed_value(kind, default))
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
        RecordType::LongIn => "longin",
        RecordType::LongOut => "longout",
    }
}

/// The record's MDEL monitor deadband (0.0 when absent or unparsable).
pub fn mdel_of(record: &RecordInstance) -> f64 {
    record
        .raw_fields
        .get("MDEL")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Parse a raw field string as `kind`, falling back to `Str` on parse failure.
fn typed_value(kind: FieldKind, raw: &str) -> ScalarValue {
    match kind {
        FieldKind::Int => raw
            .trim()
            .parse::<i32>()
            .map(ScalarValue::I32)
            .unwrap_or_else(|_| ScalarValue::Str(raw.to_string())),
        FieldKind::Double => raw
            .trim()
            .parse::<f64>()
            .map(ScalarValue::F64)
            .unwrap_or_else(|_| ScalarValue::Str(raw.to_string())),
        FieldKind::Str => ScalarValue::Str(raw.to_string()),
    }
}

/// Resolve the value of `field` for `record`.
///
/// Lookup order: computed fields (`RTYP`, `NAME`, `DTYP`), then any field
/// literally present in the parsed `.db` (`raw_fields`), then dbCommon
/// defaults. Returns `None` for fields an IOC would not serve either.
pub fn field_value(record: &RecordInstance, field: &str) -> Option<ScalarValue> {
    match field {
        "RTYP" => {
            return Some(ScalarValue::Str(
                record_type_name(&record.record_type).to_string(),
            ));
        }
        "NAME" => return Some(ScalarValue::Str(record.name.clone())),
        "VAL" => return Some(record.current_value()),
        "DTYP" => {
            let dtyp = record
                .raw_fields
                .get("DTYP")
                .cloned()
                .unwrap_or_else(|| "Soft Channel".to_string());
            return Some(ScalarValue::Str(dtyp));
        }
        _ => {}
    }

    let kind = dbcommon_default(field).map(|(kind, _)| kind);
    if let Some(raw) = record.raw_fields.get(field) {
        return Some(typed_value(kind.unwrap_or(FieldKind::Str), raw));
    }
    if field == "DESC" {
        return Some(ScalarValue::Str(record.common.desc.clone()));
    }
    dbcommon_default(field).map(|(kind, default)| typed_value(kind, default))
}

/// Wrap a resolved field value as a wire payload.
///
/// Regular fields are served as an NTScalar; the `$` long-string form is
/// served as an NTScalarArray of Int8 holding the UTF-8 bytes (QSRV
/// long-string semantics). `$` on a non-string value resolves to `None`.
///
/// Both the record-level [`payload_for`] and the provider-level
/// `resolve_field_payload` go through here, so the two stores cannot drift
/// in how they wrap.
pub fn payload_for_value(value: ScalarValue, desc: &str, long_string: bool) -> Option<NtPayload> {
    if long_string {
        let ScalarValue::Str(s) = value else {
            return None;
        };
        let bytes: Vec<i8> = s.into_bytes().into_iter().map(|b| b as i8).collect();
        return Some(NtPayload::ScalarArray(NtScalarArray::from_value(
            ScalarArrayValue::I8(bytes),
        )));
    }
    let mut nt = NtScalar::from_value(value);
    nt.display_description = desc.to_string();
    Some(NtPayload::Scalar(nt))
}

/// Build the wire payload for a resolved field reference on a record.
pub fn payload_for(record: &RecordInstance, field_ref: &FieldRef) -> Option<NtPayload> {
    let value = field_value(record, &field_ref.field)?;
    payload_for_value(value, &record.common.desc, field_ref.long_string)
}

/// A read-only [`Source`] serving `<pvname>.<FIELD>` channels for any
/// [`RecordFieldProvider`].
///
/// Registered by `PvaServer::run` after the builtin store; the builtin only
/// claims exact record names, so the two never compete.
pub struct RecordFieldSource {
    provider: Arc<dyn RecordFieldProvider>,
    /// Senders for open field-PV subscriptions. Field values are static in
    /// A2 (field writes are B's), so each channel only ever carries the
    /// initial snapshot; the senders are retained here purely to keep the
    /// channels open.
    open_subs: Mutex<Vec<mpsc::Sender<NtPayload>>>,
}

impl RecordFieldSource {
    pub fn new(provider: Arc<dyn RecordFieldProvider>) -> Self {
        Self {
            provider,
            open_subs: Mutex::new(Vec::new()),
        }
    }
}

impl Source for RecordFieldSource {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move { resolve_field_info(self.provider.as_ref(), &name).await })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move { resolve_field_payload(self.provider.as_ref(), &name).await })
    }

    fn put(
        &self,
        name: &str,
        _value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move { Err(format!("field PV '{}' is read-only", name)) })
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let initial = resolve_field_payload(self.provider.as_ref(), &name).await?;
            let (tx, rx) = mpsc::channel(4);
            let _ = tx.try_send(initial);
            self.open_subs.lock().await.push(tx);
            Some(rx)
        })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        // Field PVs are derived on demand; enumerating every possible
        // <record>.<FIELD> combination would flood name listings.
        Box::pin(async move { Vec::new() })
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
        assert!(matches!(
            dbcommon_default("PINI"),
            Some((FieldKind::Str, "NO"))
        ));
        assert!(matches!(
            dbcommon_default("PHAS"),
            Some((FieldKind::Int, "0"))
        ));
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
        assert_eq!(record_type_name(&RecordType::LongIn), "longin");
    }

    fn test_record() -> RecordInstance {
        let recs = crate::db::parse_db(
            r#"
record(ao, "SIM:AO") {
    field(VAL, "2.34")
    field(DESC, "A test output")
    field(EGU, "V")
    field(MDEL, "0.5")
}"#,
        )
        .expect("parse");
        recs.get("SIM:AO").expect("record present").clone()
    }

    #[test]
    fn computed_fields_resolve() {
        let r = test_record();
        assert_eq!(field_value(&r, "RTYP"), Some(ScalarValue::Str("ao".into())));
        assert_eq!(
            field_value(&r, "NAME"),
            Some(ScalarValue::Str("SIM:AO".into()))
        );
        assert_eq!(
            field_value(&r, "DTYP"),
            Some(ScalarValue::Str("Soft Channel".into()))
        );
        assert_eq!(field_value(&r, "VAL"), Some(ScalarValue::F64(2.34)));
    }

    #[test]
    fn raw_db_fields_take_precedence_over_defaults() {
        let r = test_record();
        assert_eq!(
            field_value(&r, "DESC"),
            Some(ScalarValue::Str("A test output".into()))
        );
        assert_eq!(field_value(&r, "EGU"), Some(ScalarValue::Str("V".into())));
        assert_eq!(field_value(&r, "MDEL"), Some(ScalarValue::F64(0.5)));
    }

    #[test]
    fn dbcommon_defaults_fill_absent_fields() {
        let r = test_record();
        assert_eq!(
            field_value(&r, "SCAN"),
            Some(ScalarValue::Str("Passive".into()))
        );
        assert_eq!(field_value(&r, "PHAS"), Some(ScalarValue::I32(0)));
        assert_eq!(field_value(&r, "ADEL"), Some(ScalarValue::F64(0.0)));
        assert_eq!(field_value(&r, "NOTAFIELD"), None);
    }

    fn test_source() -> RecordFieldSource {
        let recs = crate::db::parse_db(
            r#"
record(ao, "SIM:AO") {
    field(VAL, "2.34")
    field(DESC, "A test output")
    field(MDEL, "0.5")
}"#,
        )
        .expect("parse");
        let store = crate::simple_store::SimplePvStore::new(
            recs,
            std::collections::HashMap::new(),
            Vec::new(),
            false,
        );
        let provider: Arc<dyn RecordFieldProvider> = Arc::new(store);
        RecordFieldSource::new(provider)
    }

    #[tokio::test]
    async fn claims_field_pvs_read_only() {
        let src = test_source();
        let info = src.claim("SIM:AO.RTYP").await.expect("claimed");
        assert!(!info.writable);
        match src.get("SIM:AO.RTYP").await.expect("payload") {
            NtPayload::Scalar(nt) => assert_eq!(nt.value, ScalarValue::Str("ao".into())),
            other => panic!("expected scalar, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn does_not_claim_non_field_names() {
        let src = test_source();
        assert!(src.claim("SIM:AO").await.is_none()); // base PV: builtin's job
        assert!(src.claim("SIM:AO.NOTAFIELD").await.is_none());
        assert!(src.claim("SIM:MISSING.RTYP").await.is_none());
        assert!(src.claim("SIM:AO.MDEL$").await.is_none()); // $ on non-string
    }

    #[tokio::test]
    async fn put_is_rejected() {
        let src = test_source();
        let err = src
            .put("SIM:AO.DESC", &DecodedValue::Int32(1))
            .await
            .expect_err("put must fail");
        assert!(err.contains("read-only"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn long_string_serves_utf8_bytes() {
        let src = test_source();
        match src.get("SIM:AO.DESC$").await.expect("payload") {
            NtPayload::ScalarArray(arr) => {
                let ScalarArrayValue::I8(bytes) = arr.value else {
                    panic!("expected Int8 array, got {:?}", arr.value);
                };
                let expected: Vec<i8> = "A test output".bytes().map(|b| b as i8).collect();
                assert_eq!(bytes, expected);
            }
            other => panic!("expected scalar array, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_delivers_initial_snapshot() {
        let src = test_source();
        let mut rx = src.subscribe("SIM:AO.RTYP").await.expect("subscribed");
        match rx.recv().await.expect("initial value") {
            NtPayload::Scalar(nt) => assert_eq!(nt.value, ScalarValue::Str("ao".into())),
            other => panic!("expected scalar, got {other:?}"),
        }
        // Channel stays open (sender retained) — no immediate close.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn payload_for_value_wraps_a_scalar_with_its_description() {
        let p = payload_for_value(ScalarValue::F64(2.34), "A test output", false)
            .expect("scalars always wrap");
        match p {
            NtPayload::Scalar(nt) => {
                assert_eq!(nt.value, ScalarValue::F64(2.34));
                assert_eq!(nt.display_description, "A test output");
            }
            other => panic!("expected scalar, got {other:?}"),
        }
    }

    #[test]
    fn payload_for_value_long_string_needs_a_string() {
        assert!(payload_for_value(ScalarValue::F64(1.0), "", true).is_none());
        let p = payload_for_value(ScalarValue::Str("hi".into()), "", true).expect("string wraps");
        match p {
            NtPayload::ScalarArray(arr) => {
                assert_eq!(arr.value, ScalarArrayValue::I8(vec![104, 105]));
            }
            other => panic!("expected scalar array, got {other:?}"),
        }
    }
}
