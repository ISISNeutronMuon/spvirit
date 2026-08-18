//! What a client can observe must not depend on which tier serves a PV.
//!
//! Per the A2 spec, the three tiers are numbered: tier 1 is the direct
//! store, `SimplePvStore`; tier 2 is the IOC engine,
//! `spvirit_ioc::IocSource`; tier 3 is a Python source via the `fields()`
//! protocol. `spvirit-ioc` depends on this crate, so tier 2's half of the
//! `.FIELD` parity check lives in `spvirit-ioc/tests/field_access.rs`, not
//! here. Tier 3 is not exercised anywhere in this file either — its
//! `.FIELD` parity is pinned separately in
//! `spvirit-py/tests/test_source_fields.py` (Task 8); the split is
//! deliberate, not an oversight.
//!
//! What this file compares is `SimplePvStore` (tier 1) against
//! `EchoSource`, a hand-written `Source` standing in for an arbitrary
//! custom source rather than any one of the three numbered tiers — the
//! same "what a client can observe" contract every tier must satisfy
//! identically.

use spvirit_codec::spvd_decode::{DecodedValue, StructureDesc};
use spvirit_server::field_provider::{
    RecordFieldDesc, RecordFieldProvider, field_kind_of, resolve_field_payload,
};
use spvirit_server::pva_server::PvaServer;
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_server::record_fields::{RecordFieldSource, dbcommon_default_value};
use spvirit_server::simple_store::SimplePvStore;
use spvirit_types::{NtPayload, NtScalar, ScalarValue};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// The observable result of driving a source through one script. Timestamps
/// are excluded: the engine stamps at process time and the builtin store
/// does not stamp at all, which is a real and intended difference in *when
/// a value was produced*, not in *what the value is*.
#[derive(Debug, PartialEq)]
struct Observed {
    claimed: bool,
    writable: bool,
    initial: Option<ScalarValue>,
    after_put: Option<ScalarValue>,
    monitor_names: Vec<String>,
}

async fn observe(src: &dyn Source, name: &str) -> Observed {
    let info = src.claim(name).await;
    let initial = src.get(name).await.map(|p| value_of(&p));
    let monitors = src.put(name, &DecodedValue::Float64(42.0)).await;
    let mut monitor_names: Vec<String> = monitors
        .as_ref()
        .map(|m| m.iter().map(|(n, _)| n.clone()).collect())
        .unwrap_or_default();
    // Sorted explicitly: a `put` in general may report more than one
    // affected PV (a forward link, say), and nothing about that list's
    // origin order is contractually meaningful — only its membership is.
    monitor_names.sort();
    Observed {
        claimed: info.is_some(),
        writable: info.map(|i| i.writable).unwrap_or(false),
        initial,
        after_put: src.get(name).await.map(|p| value_of(&p)),
        monitor_names,
    }
}

fn value_of(payload: &NtPayload) -> ScalarValue {
    match payload {
        NtPayload::Scalar(s) => s.value.clone(),
        other => panic!("expected a scalar, got {other:?}"),
    }
}

/// A hand-written `Source` over a single scalar PV: claim/get/put/subscribe
/// implemented directly, with no store machinery behind it. Stands in for
/// "an arbitrary custom source" the way `FakeStore` in `store_coexistence.rs`
/// stands in for an arbitrary engine — copied in shape, not shared, because
/// integration tests are separate binaries.
struct EchoSource {
    name: String,
    value: Mutex<ScalarValue>,
}

impl EchoSource {
    fn new(name: &str, initial: f64) -> Self {
        Self {
            name: name.to_string(),
            value: Mutex::new(ScalarValue::F64(initial)),
        }
    }
}

impl Source for EchoSource {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let owned = name == self.name;
        Box::pin(async move {
            owned.then(|| PvInfo {
                descriptor: StructureDesc::default(),
                writable: true,
            })
        })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let owned = name == self.name;
        Box::pin(async move {
            if !owned {
                return None;
            }
            let v = self.value.lock().await.clone();
            Some(NtPayload::Scalar(NtScalar::from_value(v)))
        })
    }

    fn put(
        &self,
        name: &str,
        value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        let owned = name == self.name;
        let name_owned = name.to_string();
        let new_value = match value {
            DecodedValue::Float64(f) => ScalarValue::F64(*f),
            other => panic!("EchoSource only exercises Float64 puts in this suite, got {other:?}"),
        };
        Box::pin(async move {
            if !owned {
                return Err(format!("PV '{name_owned}' not found"));
            }
            *self.value.lock().await = new_value.clone();
            Ok(vec![(
                name_owned,
                NtPayload::Scalar(NtScalar::from_value(new_value)),
            )])
        })
    }

    fn subscribe(
        &self,
        _name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        Box::pin(async { None })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        let name = self.name.clone();
        Box::pin(async move { vec![name] })
    }
}

/// A `SimplePvStore` holding one writable ("ao") record with the given
/// initial value — writable so it can present the same "puttable" contract
/// `EchoSource` does, which is the point of the comparison.
fn builtin_store_with(name: &str, initial: f64) -> Arc<SimplePvStore> {
    let server = PvaServer::builder().ao(name, initial).build();
    server.store().clone()
}

/// A `SimplePvStore` holding one record with a `DESC`, for the `.FIELD`
/// resolution test below. Record type does not matter there — only field
/// resolution is exercised.
fn simple_store_with_desc(name: &str, initial: f64, desc: &str) -> Arc<SimplePvStore> {
    let server = PvaServer::builder()
        .db_string(&format!(
            "record(ai, \"{name}\") {{\n    field(VAL, \"{initial}\")\n    field(DESC, \"{desc}\")\n}}\n"
        ))
        .build();
    server.store().clone()
}

#[tokio::test]
async fn a_custom_source_and_the_builtin_store_are_indistinguishable() {
    let custom_source: Arc<dyn Source> = Arc::new(EchoSource::new("PV:X", 1.0));
    let direct_store: Arc<dyn Source> = builtin_store_with("PV:X", 1.0);

    let a = observe(&*custom_source, "PV:X").await;
    let b = observe(&*direct_store, "PV:X").await;
    assert_eq!(
        a, b,
        "EchoSource and SimplePvStore (tier 1) must present the same contract"
    );
    assert_eq!(a.after_put, Some(ScalarValue::F64(42.0)));
    assert_eq!(a.monitor_names, vec!["PV:X".to_string()]);
}

#[tokio::test]
async fn an_unknown_pv_looks_the_same_on_both_tiers() {
    let custom_source: Arc<dyn Source> = Arc::new(EchoSource::new("PV:X", 1.0));
    let direct_store: Arc<dyn Source> = builtin_store_with("PV:X", 1.0);
    let a = observe(&*custom_source, "PV:NOPE").await;
    let b = observe(&*direct_store, "PV:NOPE").await;
    assert_eq!(
        a, b,
        "EchoSource and SimplePvStore (tier 1) must agree on an unclaimed PV"
    );
    assert!(!a.claimed);
}

/// A provider standing in for an arbitrary third `RecordFieldProvider`
/// implementation (mirroring `FakeProvider` in `field_provider.rs`'s own
/// unit tests, repeated here in full since integration tests cannot import
/// another test binary's helpers). Explicit fields take precedence; anything
/// else falls back to the dbCommon default table, matching what
/// `SimplePvStore` (through `crate::record_fields::field_value`) does for a
/// field it has no raw text for.
struct FakeProvider {
    base: String,
    fields: HashMap<&'static str, ScalarValue>,
}

impl FakeProvider {
    fn new(base: &str, fields: &[(&'static str, ScalarValue)]) -> Self {
        Self {
            base: base.to_string(),
            fields: fields.iter().cloned().collect(),
        }
    }

    fn resolve(&self, base: &str, field: &str) -> Option<ScalarValue> {
        if base != self.base {
            return None;
        }
        if let Some(v) = self.fields.get(field) {
            return Some(v.clone());
        }
        dbcommon_default_value(field)
    }
}

impl RecordFieldProvider for FakeProvider {
    fn field_value(
        &self,
        base: &str,
        field: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ScalarValue>> + Send + '_>> {
        let value = self.resolve(base, field);
        Box::pin(async move { value })
    }

    fn field_descriptor(
        &self,
        base: &str,
        field: &str,
    ) -> Pin<Box<dyn Future<Output = Option<RecordFieldDesc>> + Send + '_>> {
        let value = self.resolve(base, field);
        Box::pin(async move {
            value.map(|v| RecordFieldDesc {
                kind: field_kind_of(&v),
            })
        })
    }
}

/// A dotted name resolves the same way whichever provider is behind it: the
/// same fields, the same types, the same dbCommon fallback.
#[tokio::test]
async fn field_resolution_matches_across_providers() {
    let store = simple_store_with_desc("PV:X", 1.0, "shared text");
    let fake = FakeProvider::new("PV:X", &[("DESC", ScalarValue::Str("shared text".into()))]);

    for field in ["PV:X.DESC", "PV:X.PRIO", "PV:X.NOTAFIELD"] {
        let from_store = resolve_field_payload(&*store, field)
            .await
            .map(|p| value_of(&p));
        let from_fake = resolve_field_payload(&fake, field)
            .await
            .map(|p| value_of(&p));
        assert_eq!(
            from_store, from_fake,
            "{field}: SimplePvStore and FakeProvider must resolve the same value"
        );
    }
}

/// The documented divergence, pinned so it cannot widen silently.
///
/// `RecordFieldSource` reports field PVs as `writable: false`; the tier-model
/// spec's text says tier 2 (the IOC engine, `spvirit_ioc::IocSource`) reports
/// them writable. Tier 3 is correct and tier 2 is the outlier — the ruling
/// recorded in the A2 spec — so this asserts the correct, uniform behaviour
/// (`false`, on every tier, via this one seam) and exists to catch a
/// regression *back* to the outlier: a future change that makes tier 2
/// special-case field writability to match its record-level `writable()`
/// instead of going through `resolve_field_info` like everyone else. Field
/// PVs are read-only until sub-project B lands field puts everywhere at
/// once.
#[tokio::test]
async fn field_pvs_report_read_only_on_every_tier() {
    let store = simple_store_with_desc("PV:X", 1.0, "shared text");
    let src = RecordFieldSource::new(store);
    let info = src.claim("PV:X.DESC").await.expect("claimed");
    assert!(!info.writable, "field PVs are read-only until sub-project B");
    let err = src
        .put("PV:X.DESC", &DecodedValue::Float64(1.0))
        .await
        .expect_err("a put to a dotted name must be rejected on every tier");
    assert!(err.contains("read-only"), "unexpected error: {err}");
}
