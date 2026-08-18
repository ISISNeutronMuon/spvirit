//! The coexistence contract between the two stores: they must be disjoint,
//! and the builtin store's callbacks must not name a record they cannot
//! reach.

use spvirit_codec::StructureDesc;
use spvirit_server::pva_server::PvaServer;
use spvirit_server::pvstore::{PvInfo, Source, StoreSource};
use spvirit_types::{NtPayload, NtScalar, ScalarValue};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;

/// A minimal store standing in for an engine, so this suite does not depend
/// on `spvirit-ioc` (which depends on this crate).
struct FakeStore {
    names: Vec<String>,
}

impl FakeStore {
    fn new(names: &[&str]) -> Arc<FakeStore> {
        let mut names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        names.sort();
        Arc::new(FakeStore { names })
    }
}

impl Source for FakeStore {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let owned = self.names.iter().any(|n| n == name);
        Box::pin(async move {
            owned.then(|| PvInfo {
                descriptor: StructureDesc::default(),
                writable: true,
            })
        })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let owned = self.names.iter().any(|n| n == name);
        Box::pin(async move {
            owned.then(|| NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(0.0))))
        })
    }

    fn put(
        &self,
        _name: &str,
        _value: &spvirit_codec::spvd_decode::DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn subscribe(
        &self,
        _name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        Box::pin(async { None })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        let names = self.names.clone();
        Box::pin(async move { names })
    }
}

impl StoreSource for FakeStore {
    fn record_names(&self) -> Vec<String> {
        self.names.clone()
    }
}

#[test]
#[should_panic(expected = "PV:SHARED")]
fn two_stores_owning_the_same_record_is_a_build_error() {
    PvaServer::builder()
        .ai("PV:SHARED", 1.0)
        .ioc(FakeStore::new(&["PV:SHARED"]))
        .build();
}

#[test]
#[should_panic(expected = "PV:A, PV:B")]
fn every_overlapping_name_is_reported_at_once_and_sorted() {
    PvaServer::builder()
        .ai("PV:B", 1.0)
        .ai("PV:A", 1.0)
        .ai("PV:OK", 1.0)
        .ioc(FakeStore::new(&["PV:A", "PV:B"]))
        .build();
}

#[test]
#[should_panic(expected = ".scan(\"PV:ENGINE\")")]
fn a_scan_naming_an_engine_record_is_a_build_error() {
    PvaServer::builder()
        .scan("PV:ENGINE", std::time::Duration::from_secs(1), |_| {
            ScalarValue::F64(0.0)
        })
        .ioc(FakeStore::new(&["PV:ENGINE"]))
        .build();
}

#[test]
#[should_panic(expected = ".link(\"PV:ENGINE\", …)")]
fn a_link_naming_an_engine_record_is_a_build_error() {
    PvaServer::builder()
        .ai("PV:IN", 1.0)
        .link("PV:ENGINE", &["PV:IN"], |values| values[0].clone())
        .ioc(FakeStore::new(&["PV:ENGINE"]))
        .build();
}

#[test]
#[should_panic(expected = ".on_put(\"PV:ENGINE\")")]
fn an_on_put_naming_an_engine_record_is_a_build_error() {
    PvaServer::builder()
        .on_put("PV:ENGINE", |_, _| {})
        .ioc(FakeStore::new(&["PV:ENGINE"]))
        .build();
}

#[test]
fn disjoint_stores_build_cleanly() {
    let server = PvaServer::builder()
        .ai("PV:DIRECT", 1.0)
        .ioc(FakeStore::new(&["PV:ENGINE"]))
        .build();
    drop(server);
}

use spvirit_server::pvstore::SourceRegistry;

/// Both stores serve their own records through one registry, and neither
/// can see the other's.
#[tokio::test]
async fn two_disjoint_stores_each_serve_their_own_records() {
    let reg = SourceRegistry::new();
    reg.add_store("builtin", 0, FakeStore::new(&["PV:DIRECT"])).await;
    reg.add_store("ioc", 5, FakeStore::new(&["PV:ENGINE"])).await;

    assert!(reg.claim("PV:DIRECT").await.is_some());
    assert!(reg.claim("PV:ENGINE").await.is_some());
    assert!(reg.claim("PV:NEITHER").await.is_none());

    let mut names = reg.names().await;
    names.sort();
    assert_eq!(names, vec!["PV:DIRECT".to_string(), "PV:ENGINE".to_string()]);
}

/// `names()` must be deterministic — it is what a `pvlist` client sees.
///
/// `SourceRegistry::names` aggregates through a `HashSet` (for dedup) before
/// sorting the result (see `SourceRegistry::names` in `pvstore.rs`), and
/// `StoreSource::record_names`'s doc contract states the returned list "must
/// be deterministic (sorted)". This test rebuilds the registry many times —
/// not just twice — and additionally pins the exact sorted order, so a
/// regression that dropped the final `sort()` (leaving output at the mercy
/// of `HashMap`/`HashSet` iteration order) would be caught even though a
/// single process run tends to reuse one hasher state across repeats.
#[tokio::test]
async fn the_combined_name_list_is_stable_across_runs() {
    async fn build() -> Vec<String> {
        let reg = SourceRegistry::new();
        reg.add_store("ioc", 5, FakeStore::new(&["PV:B", "PV:A"])).await;
        reg.add_store("builtin", 0, FakeStore::new(&["PV:C"])).await;
        reg.names().await
    }

    let expected = vec!["PV:A".to_string(), "PV:B".to_string(), "PV:C".to_string()];
    let first = build().await;
    assert_eq!(first, expected, "names() must be sorted");
    for _ in 0..20 {
        assert_eq!(build().await, first, "names() must be stable across rebuilds");
    }
}
