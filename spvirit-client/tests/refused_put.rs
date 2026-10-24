//! A wire PUT the record cannot represent is answered with a PUT error.
//!
//! Since the coercion layer refuses out-of-range values instead of wrapping
//! them, `SimplePvStore::put` must surface that refusal: the client gets an
//! error status, the record is not restamped, and `on_put` does not fire.
//!
//! A conforming client range-checks against the advertised descriptor, so it
//! cannot encode 300 for a `ubyte` field. A source in front of the store that
//! advertises a wider type than the backing record (as a gateway or an
//! aggregating source may) is how such a value reaches the store; the test
//! puts exactly that in front of the builtin store.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spvirit_client::PvaClient;
use spvirit_codec::spvd_decode::{DecodedValue, StructureDesc};
use spvirit_codec::spvd_encode::{nt_scalar_array_desc, nt_scalar_desc};
use spvirit_server::pvstore::TryClaim;
use spvirit_server::{PvInfo, PvaServer, RunningServer, SimplePvStore, Source};
use spvirit_types::{NtPayload, NtTimeStamp, ScalarArrayValue, ScalarValue};
use tokio::sync::mpsc;

const BYTE_PV: &str = "REFUSE:BYTE";
const ARRAY_PV: &str = "REFUSE:ARRAY";

/// Serves the builtin store's records, but advertises `int`/`int[]` for
/// them, so a client may legitimately encode a value the record cannot hold.
struct WideningSource {
    store: Arc<SimplePvStore>,
}

impl WideningSource {
    fn widened(name: &str) -> Option<StructureDesc> {
        match name {
            BYTE_PV => Some(nt_scalar_desc(&ScalarValue::I32(0))),
            ARRAY_PV => Some(nt_scalar_array_desc(&ScalarArrayValue::I32(Vec::new()))),
            _ => None,
        }
    }
}

impl Source for WideningSource {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let descriptor = Self::widened(&name)?;
            let inner = self.store.claim(&name).await?;
            Some(PvInfo {
                descriptor,
                writable: inner.writable,
            })
        })
    }

    fn try_claim(&self, name: &str) -> TryClaim {
        if Self::widened(name).is_some() {
            TryClaim::Yes
        } else {
            TryClaim::No
        }
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        self.store.get(name)
    }

    fn put(
        &self,
        name: &str,
        value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        self.store.put(name, value)
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        self.store.subscribe(name)
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async { vec![BYTE_PV.to_string(), ARRAY_PV.to_string()] })
    }
}

struct Fixture {
    running: RunningServer,
    client: PvaClient,
    on_put_calls: Arc<AtomicUsize>,
}

fn free_ports() -> (u16, u16) {
    let tcp = TcpListener::bind("127.0.0.1:0")
        .expect("bind tcp")
        .local_addr()
        .expect("local_addr")
        .port();
    let udp = UdpSocket::bind("127.0.0.1:0")
        .expect("bind udp")
        .local_addr()
        .expect("local_addr")
        .port();
    (tcp, udp)
}

/// A byte (`ubyte`, value 7) output record and a `ubyte[]` aao record, both
/// behind a [`WideningSource`], with one shared `on_put` counter.
async fn start() -> Fixture {
    let (tcp_port, udp_port) = free_ports();
    let on_put_calls = Arc::new(AtomicUsize::new(0));
    let (c1, c2) = (on_put_calls.clone(), on_put_calls.clone());

    let mut server = PvaServer::builder()
        .aao(ARRAY_PV, ScalarArrayValue::U8(vec![1, 2, 3]))
        .on_put(BYTE_PV, move |_, _| {
            c1.fetch_add(1, Ordering::SeqCst);
        })
        .on_put(ARRAY_PV, move |_, _| {
            c2.fetch_add(1, Ordering::SeqCst);
        })
        .port(tcp_port)
        .udp_port(udp_port)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .build();
    let store = server.store().clone();
    server.add_source("widening", -1, Arc::new(WideningSource { store }));
    let running = server.start().await.expect("server start");
    running.add_scalar(BYTE_PV, ScalarValue::U8(7), true).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let ns: SocketAddr = format!("127.0.0.1:{tcp_port}").parse().unwrap();
    let client = PvaClient::builder()
        .port(tcp_port)
        .udp_port(udp_port)
        .timeout(Duration::from_secs(5))
        .name_server(ns)
        .no_broadcast()
        .build();
    Fixture {
        running,
        client,
        on_put_calls,
    }
}

async fn byte_state(store: &SimplePvStore) -> (ScalarValue, NtTimeStamp) {
    match store.get_nt(BYTE_PV).await.expect("byte record") {
        NtPayload::Scalar(nt) => (nt.value, nt.time_stamp.expect("stamped on insert")),
        other => panic!("expected a scalar payload, got {other:?}"),
    }
}

async fn array_state(store: &SimplePvStore) -> (ScalarArrayValue, NtTimeStamp) {
    match store.get_nt(ARRAY_PV).await.expect("array record") {
        NtPayload::ScalarArray(nt) => (nt.value, nt.time_stamp),
        other => panic!("expected a scalar-array payload, got {other:?}"),
    }
}

/// `on_put` runs on a spawned task; give it time to run if it was spawned.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wire_put_of_300_into_a_byte_record_is_answered_with_an_error() {
    let f = start().await;
    let store = f.running.store().clone();
    let (value_before, ts_before) = byte_state(&store).await;
    assert_eq!(value_before, ScalarValue::U8(7));

    let result = f.client.pvput(BYTE_PV, 300).await;
    settle().await;

    assert!(
        result.is_err(),
        "300 does not fit a ubyte record; the client must see a PUT error, got {result:?}"
    );
    let (value_after, ts_after) = byte_state(&store).await;
    assert_eq!(
        value_after,
        ScalarValue::U8(7),
        "a refused PUT must not mutate"
    );
    assert_eq!(
        ts_after, ts_before,
        "a refused PUT must not restamp the record"
    );
    assert_eq!(
        f.on_put_calls.load(Ordering::SeqCst),
        0,
        "on_put must not fire for a refused PUT"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_value_wire_put_still_succeeds_and_a_representable_one_applies() {
    let f = start().await;
    let store = f.running.store().clone();

    f.client
        .pvput(BYTE_PV, 7)
        .await
        .expect("re-sending the current value is not a refusal");
    settle().await;
    assert_eq!(byte_state(&store).await.0, ScalarValue::U8(7));
    assert_eq!(
        f.on_put_calls.load(Ordering::SeqCst),
        1,
        "an accepted PUT fires on_put"
    );

    f.client
        .pvput(BYTE_PV, 200)
        .await
        .expect("200 fits a ubyte record");
    settle().await;
    assert_eq!(byte_state(&store).await.0, ScalarValue::U8(200));
    assert_eq!(f.on_put_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wire_array_put_with_an_unrepresentable_element_is_answered_with_an_error() {
    let f = start().await;
    let store = f.running.store().clone();
    let (value_before, ts_before) = array_state(&store).await;
    assert_eq!(value_before, ScalarArrayValue::U8(vec![1, 2, 3]));

    let result = f
        .client
        .pvput(ARRAY_PV, serde_json::json!([4, 300, 6]))
        .await;
    settle().await;

    assert!(
        result.is_err(),
        "300 does not fit a ubyte[] element; the client must see a PUT error, got {result:?}"
    );
    let (value_after, ts_after) = array_state(&store).await;
    assert_eq!(value_after, ScalarArrayValue::U8(vec![1, 2, 3]));
    assert_eq!(
        ts_after, ts_before,
        "a refused PUT must not restamp the record"
    );
    assert_eq!(f.on_put_calls.load(Ordering::SeqCst), 0);

    // Control: a representable array still applies over the same path.
    f.client
        .pvput(ARRAY_PV, serde_json::json!([4, 5, 6]))
        .await
        .expect("every element fits");
    assert_eq!(
        array_state(&store).await.0,
        ScalarArrayValue::U8(vec![4, 5, 6])
    );
}
