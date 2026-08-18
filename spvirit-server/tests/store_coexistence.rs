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
#[should_panic(expected = ".link")]
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
