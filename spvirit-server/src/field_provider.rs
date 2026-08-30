//! The field-access seam shared by the two stores.
//!
//! EPICS Base separates resolution from access: `dbNameToAddr` looks a field
//! up in the record type's field-description table and yields a `DBADDR`
//! carrying type and size, and `dbGetField` then reads through it.
//! [`RecordFieldProvider`] keeps that split — `field_descriptor` answers
//! "does this field exist and what type is it" without reading the value, so
//! `Source::claim` no longer has to perform a full read to answer a channel
//! search.
//!
//! The seam is deliberately **value-level**, not record-level. The two record
//! models are not variations on a theme: `SimplePvStore`'s `RecordInstance`
//! resolves through a raw string map of whatever the `.db` literally said,
//! while `spvirit-ioc`'s `Record` is fully typed and has no raw-field map at
//! all. A record-level seam would force `spvirit-ioc` to fabricate a
//! `RecordInstance` on every field read, and is lossy both ways.

use std::future::Future;
use std::pin::Pin;

use spvirit_codec::spvd_decode::StructureDesc;
use spvirit_types::{NtPayload, NtScalar, NtScalarArray, ScalarArrayValue, ScalarValue};

use crate::pvstore::PvInfo;
use crate::record_fields::{FieldKind, parse_field_ref, payload_for_value};
use crate::simple_store::{SimplePvStore, descriptor_for_payload};

/// What a field is, without reading it: the analogue of Base's `DBADDR`.
///
/// Only the scalar type is carried, because that is all a PVA structure
/// descriptor depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordFieldDesc {
    pub kind: FieldKind,
}

/// The field PVs that are writable when a tier's put path routes them into the
/// Scanner: `SCAN`, `EVNT`, `PHAS`. Every other field PV is read-only. Shared
/// so the IOC engine and any provider that honours field writes agree on the
/// set rather than each hard-coding it.
pub fn field_is_writable(field: &str) -> bool {
    matches!(field, "SCAN" | "EVNT" | "PHAS")
}

/// A store that can resolve `<record>.<FIELD>` references.
///
/// The implementors are the two stores: `SimplePvStore` (below) and
/// `spvirit_ioc::IocSource`. Both go through the free functions in this
/// module, so `.FIELD` behaviour cannot drift between the tiers.
pub trait RecordFieldProvider: Send + Sync {
    /// Resolve a field's value. The analogue of `dbGetField`.
    ///
    /// Returns `None` when the record does not exist or does not carry the
    /// field — an IOC would not serve either.
    fn field_value(
        &self,
        base: &str,
        field: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ScalarValue>> + Send + '_>>;

    /// Resolve a field's existence and type without reading it. The analogue
    /// of `dbNameToAddr`.
    ///
    /// Must agree with [`field_value`](Self::field_value): if this returns
    /// `Some(d)`, a subsequent read must produce a value whose
    /// [`field_kind_of`] is `d.kind`.
    fn field_descriptor(
        &self,
        base: &str,
        field: &str,
    ) -> Pin<Box<dyn Future<Output = Option<RecordFieldDesc>> + Send + '_>>;

    /// Whether a put to `<record>.<field>` is accepted on this tier.
    ///
    /// The default is read-only: a tier that has no field-write path (the
    /// builtin store, the Python tier) must not advertise a field as writable
    /// and then refuse the put. The IOC engine overrides this to accept
    /// `SCAN`/`EVNT`/`PHAS` (see [`field_is_writable`]), which its `put`
    /// routes into the Scanner — so the flag stays honest per tier.
    fn field_writable(&self, field: &str) -> bool {
        let _ = field;
        false
    }
}

/// The [`FieldKind`] a scalar value serves as.
pub fn field_kind_of(value: &ScalarValue) -> FieldKind {
    match value {
        ScalarValue::Str(_) => FieldKind::Str,
        ScalarValue::F32(_) | ScalarValue::F64(_) => FieldKind::Double,
        _ => FieldKind::Int,
    }
}

/// The PVA structure descriptor for a field of `kind`, without a value.
///
/// Built by describing a zero-valued probe payload rather than by
/// hand-rolling a second descriptor table — the descriptor a claim
/// advertises is then, by construction, the one `descriptor_for_payload`
/// derives from the payload a get actually serves. `None` for the `$` form
/// on a non-string field, which QSRV does not serve either.
pub fn descriptor_for_kind(kind: FieldKind, long_string: bool) -> Option<StructureDesc> {
    if long_string {
        if kind != FieldKind::Str {
            return None;
        }
        return Some(descriptor_for_payload(&NtPayload::ScalarArray(
            NtScalarArray::from_value(ScalarArrayValue::I8(Vec::new())),
        )));
    }
    let probe = match kind {
        FieldKind::Str => ScalarValue::Str(String::new()),
        FieldKind::Int => ScalarValue::I32(0),
        FieldKind::Double => ScalarValue::F64(0.0),
    };
    Some(descriptor_for_payload(&NtPayload::Scalar(
        NtScalar::from_value(probe),
    )))
}

/// Resolve `<base>.<FIELD>[$]` to a wire payload through `provider`.
///
/// The record's `DESC` is fetched as a second field read and carried as the
/// payload's `display_description`, matching what the record-level
/// `payload_for` has always done. A record with no `DESC` yields an empty
/// description, not a failure.
pub async fn resolve_field_payload(
    provider: &dyn RecordFieldProvider,
    name: &str,
) -> Option<NtPayload> {
    let field_ref = parse_field_ref(name)?;
    let value = provider
        .field_value(&field_ref.base, &field_ref.field)
        .await?;
    let desc = match provider.field_value(&field_ref.base, "DESC").await {
        Some(ScalarValue::Str(s)) => s,
        _ => String::new(),
    };
    payload_for_value(value, &desc, field_ref.long_string)
}

/// Resolve `<base>.<FIELD>[$]` to channel metadata through `provider`,
/// without reading the value.
///
/// Writability is the provider's to decide: `field_descriptor` reports it per
/// field, and this carries the flag through unchanged. Sub-project B makes
/// `SCAN`/`EVNT`/`PHAS` writable on the IOC tier (they route into the
/// Scanner); every other field PV stays read-only, matching Base's separate
/// `dbPutField` verb.
pub async fn resolve_field_info(provider: &dyn RecordFieldProvider, name: &str) -> Option<PvInfo> {
    let field_ref = parse_field_ref(name)?;
    let desc = provider
        .field_descriptor(&field_ref.base, &field_ref.field)
        .await?;
    Some(PvInfo {
        descriptor: descriptor_for_kind(desc.kind, field_ref.long_string)?,
        writable: provider.field_writable(&field_ref.field),
    })
}

impl RecordFieldProvider for SimplePvStore {
    fn field_value(
        &self,
        base: &str,
        field: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ScalarValue>> + Send + '_>> {
        let (base, field) = (base.to_string(), field.to_string());
        Box::pin(async move {
            let record = self.get_record(&base).await?;
            crate::record_fields::field_value(&record, &field)
        })
    }

    /// Tier 2's (`SimplePvStore`'s) cheap path is the same read: `field_value` resolves through
    /// an in-memory string map, so there is nothing cheaper to do. The split
    /// pays off on the IOC path, where a value read takes a lock-set mutex
    /// and a descriptor read does not.
    ///
    /// Deriving the kind from the value (rather than from `dbcommon_default`
    /// alone) is deliberate: `typed_value` falls back to `Str` when a raw
    /// `.db` string does not parse as its declared kind, so
    /// `field(MDEL, "abc")` really does serve a string and the descriptor
    /// must say so.
    fn field_descriptor(
        &self,
        base: &str,
        field: &str,
    ) -> Pin<Box<dyn Future<Output = Option<RecordFieldDesc>> + Send + '_>> {
        let (base, field) = (base.to_string(), field.to_string());
        Box::pin(async move {
            let record = self.get_record(&base).await?;
            let value = crate::record_fields::field_value(&record, &field)?;
            Some(RecordFieldDesc {
                kind: field_kind_of(&value),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record_fields::FieldKind;
    use spvirit_types::{NtPayload, ScalarValue};
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;

    /// A provider with one record and three fields, and a counter proving
    /// which method the caller actually used.
    struct FakeProvider {
        fields: HashMap<&'static str, ScalarValue>,
        value_calls: std::sync::atomic::AtomicUsize,
    }

    impl FakeProvider {
        fn new() -> Self {
            let mut fields = HashMap::new();
            fields.insert("RTYP", ScalarValue::Str("ao".into()));
            fields.insert("DESC", ScalarValue::Str("A test output".into()));
            fields.insert("VAL", ScalarValue::F64(2.34));
            fields.insert("SCAN", ScalarValue::Str("1 second".into()));
            fields.insert("EVNT", ScalarValue::Str("0".into()));
            fields.insert("PHAS", ScalarValue::I32(0));
            Self {
                fields,
                value_calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn value_calls(&self) -> usize {
            self.value_calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl RecordFieldProvider for FakeProvider {
        fn field_value(
            &self,
            base: &str,
            field: &str,
        ) -> Pin<Box<dyn Future<Output = Option<ScalarValue>> + Send + '_>> {
            let (base, field) = (base.to_string(), field.to_string());
            Box::pin(async move {
                self.value_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if base != "SIM:AO" {
                    return None;
                }
                self.fields.get(field.as_str()).cloned()
            })
        }

        fn field_descriptor(
            &self,
            base: &str,
            field: &str,
        ) -> Pin<Box<dyn Future<Output = Option<RecordFieldDesc>> + Send + '_>> {
            let (base, field) = (base.to_string(), field.to_string());
            Box::pin(async move {
                if base != "SIM:AO" {
                    return None;
                }
                self.fields.get(field.as_str()).map(|v| RecordFieldDesc {
                    kind: field_kind_of(v),
                })
            })
        }

        // Stands in for the IOC tier: SCAN/EVNT/PHAS are writable, and the
        // put path (not modelled here) would route them into the Scanner.
        fn field_writable(&self, field: &str) -> bool {
            field_is_writable(field)
        }
    }

    #[tokio::test]
    async fn resolves_a_field_payload_with_the_records_description() {
        let p = FakeProvider::new();
        match resolve_field_payload(&p, "SIM:AO.RTYP")
            .await
            .expect("resolved")
        {
            NtPayload::Scalar(nt) => {
                assert_eq!(nt.value, ScalarValue::Str("ao".into()));
                assert_eq!(nt.display_description, "A test output");
            }
            other => panic!("expected scalar, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn does_not_resolve_unknown_bases_fields_or_bare_names() {
        let p = FakeProvider::new();
        assert!(resolve_field_payload(&p, "SIM:AO").await.is_none());
        assert!(resolve_field_payload(&p, "SIM:AO.NOTAFIELD").await.is_none());
        assert!(resolve_field_payload(&p, "SIM:MISSING.RTYP").await.is_none());
    }

    #[tokio::test]
    async fn resolve_field_info_never_reads_the_value() {
        let p = FakeProvider::new();
        let info = resolve_field_info(&p, "SIM:AO.VAL").await.expect("claimed");
        assert!(!info.writable, "a genuinely read-only field PV stays read-only");
        assert_eq!(
            p.value_calls(),
            0,
            "claim must answer from field_descriptor alone — this is the \
             dbNameToAddr/dbGetField split the seam exists for"
        );
    }

    /// SCAN/EVNT/PHAS are the writable field PVs (they route into the
    /// Scanner); every other field, VAL included, stays read-only. The claim
    /// carries the provider's per-field flag through unchanged, and still
    /// without reading any value.
    #[tokio::test]
    async fn scan_evnt_phas_claim_writable_while_other_fields_do_not() {
        let p = FakeProvider::new();
        for field in ["SCAN", "EVNT", "PHAS"] {
            let info = resolve_field_info(&p, &format!("SIM:AO.{field}"))
                .await
                .expect("claimed");
            assert!(info.writable, "{field} must claim writable");
        }
        for field in ["VAL", "DESC", "RTYP"] {
            let info = resolve_field_info(&p, &format!("SIM:AO.{field}"))
                .await
                .expect("claimed");
            assert!(!info.writable, "{field} must claim read-only");
        }
        assert_eq!(
            p.value_calls(),
            0,
            "writability is answered from the descriptor, not by reading"
        );
    }

    #[tokio::test]
    async fn the_descriptor_matches_the_payload_the_value_would_produce() {
        let p = FakeProvider::new();
        for name in ["SIM:AO.VAL", "SIM:AO.RTYP"] {
            let info = resolve_field_info(&p, name).await.expect("claimed");
            let payload = resolve_field_payload(&p, name).await.expect("resolved");
            assert_eq!(
                info.descriptor,
                crate::simple_store::descriptor_for_payload(&payload),
                "{name}: claim's descriptor must match what get actually serves"
            );
        }
    }

    #[tokio::test]
    async fn long_string_claims_only_string_fields() {
        let p = FakeProvider::new();
        assert!(resolve_field_info(&p, "SIM:AO.DESC$").await.is_some());
        assert!(resolve_field_info(&p, "SIM:AO.VAL$").await.is_none());
    }

    #[test]
    fn field_kind_maps_scalar_variants() {
        assert_eq!(field_kind_of(&ScalarValue::Str("x".into())), FieldKind::Str);
        assert_eq!(field_kind_of(&ScalarValue::F64(1.0)), FieldKind::Double);
        assert_eq!(field_kind_of(&ScalarValue::F32(1.0)), FieldKind::Double);
        assert_eq!(field_kind_of(&ScalarValue::I32(1)), FieldKind::Int);
        assert_eq!(field_kind_of(&ScalarValue::Bool(true)), FieldKind::Int);
    }
}
