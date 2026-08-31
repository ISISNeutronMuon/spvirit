//! V4 MEDIUM-2: what a *shed* pattern query does, and whether anyone can see
//! it happen.
//!
//! Shedding used to fall through to the immediate response with `found=false`
//! — an authoritative "I do not serve that" for a name the server may well
//! serve, on a path where a retry cannot correct it (every retry gets the same
//! confident lie while the cap stays saturated). The ruling is that a shed
//! query is an *undecided* one, and this branch's whole architecture is that
//! undecided means stay silent so the client's retry hits a decisive answer —
//! exactly what `TryClaim::Unknown` does on the exact-name path.
//!
//! Exact names on the same datagram are unaffected: only the
//! negative-because-nothing-matched response is suppressed.
//!
//! Shedding is silent on the wire by construction, so it also gets a counter.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvd_decode::StructureDesc;
use spvirit_codec::spvirit_encode::{encode_client_connection_validation, encode_search_request};
use spvirit_server::handler::{
    PATTERN_ENUM_CONCURRENCY, PvListMode, ServerState, rand_guid, run_tcp_server,
};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::{PvInfo, Source, SourceRegistry, TryClaim};
use spvirit_server::search_resolve::global_stats;
use spvirit_types::NtPayload;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const VERSION: u8 = 2;
const IS_BE: bool = false;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const SERVED: &str = "SHED:SERVED";

/// Serves one name and lists it. Its `names()` returns instantly.
struct OneName;

impl Source for OneName {
    fn try_claim(&self, name: &str) -> TryClaim {
        if name == SERVED {
            TryClaim::Yes
        } else {
            TryClaim::No
        }
    }

    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let owned = name == SERVED;
        Box::pin(async move {
            owned.then(|| PvInfo {
                descriptor: StructureDesc::default(),
                writable: false,
            })
        })
    }

    fn get(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        Box::pin(async { None })
    }

    fn put(
        &self,
        _name: &str,
        _value: &spvirit_codec::spvd_decode::DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        Box::pin(async { Err("read-only".to_string()) })
    }

    fn subscribe(
        &self,
        _name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<tokio::sync::mpsc::Receiver<NtPayload>>> + Send + '_>>
    {
        Box::pin(async { None })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async { vec![SERVED.to_string()] })
    }
}

/// Hangs in `names()`, so a handful of wildcards can pin every permit.
struct HangingNames {
    name_calls: Arc<AtomicUsize>,
}

impl Source for HangingNames {
    fn try_claim(&self, _name: &str) -> TryClaim {
        TryClaim::No
    }

    fn claim(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        Box::pin(async { None })
    }

    fn get(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        Box::pin(async { None })
    }

    fn put(
        &self,
        _name: &str,
        _value: &spvirit_codec::spvd_decode::DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        Box::pin(async { Err("read-only".to_string()) })
    }

    fn subscribe(
        &self,
        _name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<tokio::sync::mpsc::Receiver<NtPayload>>> + Send + '_>>
    {
        Box::pin(async { None })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        self.name_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Vec::new()
        })
    }
}

async fn spawn_tcp(name_calls: Arc<AtomicUsize>) -> SocketAddr {
    let registry = Arc::new(SourceRegistry::new());
    registry.add("one", 0, Arc::new(OneName)).await;
    registry
        .add("hanging", 1, Arc::new(HangingNames { name_calls }))
        .await;
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let state = Arc::new(ServerState::new(
        registry,
        Arc::new(MonitorRegistry::new()),
        false,
        PvListMode::List,
        1024,
        None,
        rand_guid(),
        addr.port(),
        None,
        IpAddr::V4(Ipv4Addr::LOCALHOST),
    ));
    tokio::spawn(async move {
        let _ = run_tcp_server(state, listener, IO_TIMEOUT).await;
    });
    addr
}

async fn read_frame(stream: &mut TcpStream, budget: Duration) -> Option<Vec<u8>> {
    let mut header = [0u8; 8];
    tokio::time::timeout(budget, stream.read_exact(&mut header))
        .await
        .ok()?
        .ok()?;
    let parsed = PvaHeader::new(&header);
    let payload_len = if parsed.flags.is_control {
        0
    } else {
        parsed.payload_length as usize
    };
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        tokio::time::timeout(IO_TIMEOUT, stream.read_exact(&mut payload))
            .await
            .ok()?
            .ok()?;
    }
    let mut full = header.to_vec();
    full.extend_from_slice(&payload);
    Some(full)
}

async fn handshake(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    for _ in 0..32 {
        let raw = read_frame(&mut stream, IO_TIMEOUT).await.expect("frame");
        let mut pkt = PvaPacket::new(&raw);
        if matches!(
            pkt.decode_payload(),
            Some(PvaPacketCommand::ConnectionValidation(_))
        ) {
            break;
        }
    }
    let validation =
        encode_client_connection_validation(16_384, 512, 0, "ca", "tester", "host", VERSION, IS_BE);
    stream.write_all(&validation).await.expect("write");
    for _ in 0..32 {
        let raw = read_frame(&mut stream, IO_TIMEOUT).await.expect("frame");
        let mut pkt = PvaPacket::new(&raw);
        if matches!(
            pkt.decode_payload(),
            Some(PvaPacketCommand::ConnectionValidated(_))
        ) {
            break;
        }
    }
    stream
}

async fn collect(
    stream: &mut TcpStream,
    window: Duration,
) -> Vec<spvirit_codec::epics_decode::PvaSearchResponsePayload> {
    let deadline = Instant::now() + window;
    let mut out = Vec::new();
    while Instant::now() < deadline {
        let budget = deadline.saturating_duration_since(Instant::now());
        let Some(raw) = read_frame(stream, budget).await else {
            break;
        };
        let mut pkt = PvaPacket::new(&raw);
        if let Some(PvaPacketCommand::SearchResponse(p)) = pkt.decode_payload() {
            out.push(p);
        }
    }
    out
}

/// Pin every enumeration permit with wildcards whose `names()` never returns,
/// leaving the next pattern query with nothing to acquire.
async fn saturate(stream: &mut TcpStream, name_calls: &Arc<AtomicUsize>) {
    for i in 0..PATTERN_ENUM_CONCURRENCY {
        let req = encode_search_request(
            500 + i as u32,
            0x81,
            0,
            [0u8; 16],
            &[(500 + i as u32, "SHED:*")],
            VERSION,
            IS_BE,
        );
        stream.write_all(&req).await.expect("write wildcard");
    }
    for _ in 0..100 {
        if name_calls.load(Ordering::SeqCst) >= PATTERN_ENUM_CONCURRENCY {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "only {} enumerations entered names(); the permits were never \
         saturated and the test proves nothing",
        name_calls.load(Ordering::SeqCst)
    );
}

/// A shed pattern query must be answered with silence, not with an
/// authoritative `found=false` for a name the server does in fact serve.
#[tokio::test]
async fn a_shed_pattern_query_is_not_answered_at_all() {
    let name_calls = Arc::new(AtomicUsize::new(0));
    let addr = spawn_tcp(name_calls.clone()).await;
    let mut stream = handshake(addr).await;
    let before = global_stats().pattern_enum_shed;
    saturate(&mut stream, &name_calls).await;

    // `SHED:*` matches `SHED:SERVED`, which this very server serves. The
    // enumeration cap is saturated, so the query cannot be decided.
    let req = encode_search_request(
        600,
        0x81,
        0,
        [0u8; 16],
        &[(600, "SHED:*")],
        VERSION,
        IS_BE,
    );
    stream.write_all(&req).await.expect("write shed query");

    let seen = collect(&mut stream, Duration::from_millis(800)).await;
    let mine: Vec<_> = seen.iter().filter(|p| p.seq == 600).collect();
    assert!(
        mine.is_empty(),
        "a shed pattern query for a name the server DOES serve was answered \
         {:?} — the server states 'not found' authoritatively under load, and \
         a retry cannot correct it",
        mine.iter()
            .map(|p| (p.found, p.cids.clone()))
            .collect::<Vec<_>>()
    );
    assert!(
        global_stats().pattern_enum_shed > before,
        "nothing was shed, so the silence above proves nothing"
    );
}

/// The other half of the shed ruling: suppression is confined to the
/// negative-because-nothing-matched answer. Exact names carried on the same
/// datagram must still be answered normally.
#[tokio::test]
async fn a_shed_pattern_does_not_suppress_an_exact_name_on_the_same_datagram() {
    let name_calls = Arc::new(AtomicUsize::new(0));
    let addr = spawn_tcp(name_calls.clone()).await;
    let mut stream = handshake(addr).await;
    saturate(&mut stream, &name_calls).await;

    let req = encode_search_request(
        601,
        0x81,
        0,
        [0u8; 16],
        &[(610, SERVED), (611, "SHED:*")],
        VERSION,
        IS_BE,
    );
    stream.write_all(&req).await.expect("write mixed datagram");

    let seen = collect(&mut stream, Duration::from_millis(800)).await;
    let mine: Vec<_> = seen.iter().filter(|p| p.seq == 601).collect();
    assert_eq!(
        mine.len(),
        1,
        "expected exactly one response (the exact name, answered inline), got {:?}",
        mine.iter()
            .map(|p| (p.found, p.cids.clone()))
            .collect::<Vec<_>>()
    );
    assert!(
        mine[0].found && mine[0].cids == vec![610],
        "a shed pattern query on the same datagram swallowed the exact name's \
         answer (found={}, cids={:?})",
        mine[0].found,
        mine[0].cids
    );
}
