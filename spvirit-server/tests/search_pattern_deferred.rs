//! V4 MEDIUM-3: coverage for the *deferred* pattern-query reply path.
//!
//! `spawn_pattern_query_reply` and the two reply closures it feeds were
//! effectively untested: of 13 mutations of their load-bearing lines, 9
//! survived the whole `spvirit-server` suite. The permit that bounds the
//! enumeration could be deleted outright, the `wildcard_match` filter could be
//! deleted (claiming every pattern cid), the `pvlist_max` truncation could be
//! dropped, and the entire UDP deferred reply — its `seq`, its `found` flag,
//! its interaction with `response_required` — was unobserved.
//!
//! Each test below names the mutation(s) it kills.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::{encode_client_connection_validation, encode_search_request};
use spvirit_codec::spvd_decode::StructureDesc;
use spvirit_server::handler::{
    PATTERN_ENUM_CONCURRENCY, PvListMode, ServerState, rand_guid, run_tcp_server, run_udp_search,
};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::{PvInfo, Source, SourceRegistry, TryClaim};
use spvirit_types::NtPayload;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

const VERSION: u8 = 2;
const IS_BE: bool = false;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// Claims `served` decisively and enumerates `listed`. The two lists are kept
/// separate so a test can list a name nothing claims: the *only* route from a
/// listed-but-unclaimed name to a `found` response is the pattern path.
struct ListingSource {
    served: Vec<String>,
    listed: Vec<String>,
}

impl ListingSource {
    fn new(served: &[&str], listed: &[&str]) -> Self {
        Self {
            served: served.iter().map(|s| s.to_string()).collect(),
            listed: listed.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl Source for ListingSource {
    fn try_claim(&self, name: &str) -> TryClaim {
        if self.served.iter().any(|s| s == name) {
            TryClaim::Yes
        } else {
            TryClaim::No
        }
    }

    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let owned = self.served.iter().any(|s| s == name);
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
        let listed = self.listed.clone();
        Box::pin(async move { listed })
    }
}

/// Hangs forever in `names()`, counting entries. Registering it lets a test
/// observe how many enumerations the server allows to run at once.
struct HangingNames {
    hang: Duration,
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
        let hang = self.hang;
        Box::pin(async move {
            tokio::time::sleep(hang).await;
            Vec::new()
        })
    }
}

// ---------------------------------------------------------------------------
// TCP harness
// ---------------------------------------------------------------------------

async fn spawn_tcp(mode: PvListMode, max: usize, sources: Vec<Arc<dyn Source>>) -> SocketAddr {
    let registry = Arc::new(SourceRegistry::new());
    for (i, s) in sources.into_iter().enumerate() {
        registry.add(Box::leak(format!("s{i}").into_boxed_str()), i as i32, s).await;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let state = Arc::new(ServerState::new(
        registry,
        Arc::new(MonitorRegistry::new()),
        false,
        mode,
        max,
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

async fn read_until<F>(stream: &mut TcpStream, mut accept: F) -> PvaPacketCommand
where
    F: FnMut(&PvaPacketCommand) -> bool,
{
    for _ in 0..32 {
        let raw = read_frame(stream, IO_TIMEOUT).await.expect("frame");
        let mut pkt = PvaPacket::new(&raw);
        if let Some(cmd) = pkt.decode_payload()
            && accept(&cmd)
        {
            return cmd;
        }
    }
    panic!("expected command never arrived");
}

async fn handshake(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    read_until(&mut stream, |c| {
        matches!(c, PvaPacketCommand::ConnectionValidation(_))
    })
    .await;
    let validation =
        encode_client_connection_validation(16_384, 512, 0, "ca", "tester", "host", VERSION, IS_BE);
    stream.write_all(&validation).await.expect("write");
    read_until(&mut stream, |c| {
        matches!(c, PvaPacketCommand::ConnectionValidated(_))
    })
    .await;
    stream
}

async fn tcp_search(
    stream: &mut TcpStream,
    seq: u32,
    cid: u32,
    name: &str,
) -> spvirit_codec::epics_decode::PvaSearchResponsePayload {
    let req = encode_search_request(seq, 0x81, 0, [0u8; 16], &[(cid, name)], VERSION, IS_BE);
    stream.write_all(&req).await.expect("write search");
    match read_until(stream, |c| {
        matches!(c, PvaPacketCommand::SearchResponse(p) if p.seq == seq)
    })
    .await
    {
        PvaPacketCommand::SearchResponse(p) => p,
        other => panic!("expected SearchResponse, got {other:?}"),
    }
}

/// Drain every `SearchResponse` frame that arrives within `window`.
async fn tcp_collect(
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

// ---------------------------------------------------------------------------
// UDP harness
// ---------------------------------------------------------------------------

struct UdpHarness {
    server: SocketAddr,
    client: UdpSocket,
}

impl UdpHarness {
    async fn start(sources: Vec<Arc<dyn Source>>) -> Self {
        Self::start_with(PvListMode::List, 1024, sources).await
    }

    async fn start_with(mode: PvListMode, max: usize, sources: Vec<Arc<dyn Source>>) -> Self {
        let registry = Arc::new(SourceRegistry::new());
        for (i, s) in sources.into_iter().enumerate() {
            registry
                .add(Box::leak(format!("s{i}").into_boxed_str()), i as i32, s)
                .await;
        }
        let state = Arc::new(ServerState::new(
            registry,
            Arc::new(MonitorRegistry::new()),
            false,
            mode,
            max,
            None,
            rand_guid(),
            5075,
            None,
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ));
        // Bind-and-release to pick a free port; the readiness loop below
        // absorbs the rebind race rather than sleeping blindly.
        let port = UdpSocket::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let server: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        tokio::spawn(async move {
            if let Err(e) = run_udp_search(state, server, 5075, rand_guid(), None, None).await {
                eprintln!("test responder on {server} exited: {e}");
            }
        });
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let h = Self { server, client };
        h.wait_ready().await;
        h
    }

    /// Retry an exact, unserved name until the responder answers, so every
    /// later test can send its real datagram exactly *once* and count the
    /// replies it produces.
    async fn wait_ready(&self) {
        let req = encode_search_request(1, 0x01, 0, [0u8; 16], &[(1, "READY:PROBE")], VERSION, IS_BE);
        for _ in 0..100 {
            self.client.send_to(&req, self.server).await.unwrap();
            if !self.collect(Duration::from_millis(30)).await.is_empty() {
                return;
            }
        }
        panic!("the UDP search responder never came up");
    }

    /// Send `req` once and return every `SearchResponse` seen within `window`.
    async fn send_and_collect(
        &self,
        req: &[u8],
        window: Duration,
    ) -> Vec<spvirit_codec::epics_decode::PvaSearchResponsePayload> {
        self.client.send_to(req, self.server).await.unwrap();
        self.collect(window).await
    }

    async fn collect(
        &self,
        window: Duration,
    ) -> Vec<spvirit_codec::epics_decode::PvaSearchResponsePayload> {
        let deadline = Instant::now() + window;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(Ok((n, _))) = tokio::time::timeout(remaining, self.client.recv_from(&mut buf))
                .await
            else {
                break;
            };
            let Some(mut pkt) = PvaPacket::try_new(&buf[..n]) else {
                continue;
            };
            if let Some(PvaPacketCommand::SearchResponse(p)) = pkt.decode_payload() {
                out.push(p);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Kills M2 — **delete the permit acquisition entirely**.
///
/// The `PATTERN_ENUM_CONCURRENCY` semaphore is the thing that distinguishes
/// this design from "spawn a task per wildcard datagram", i.e. from an
/// unbounded-task DoS driven by one unauthenticated peer. It could be removed
/// wholesale with a green suite.
///
/// Eight wildcard searches are fired back to back at a server whose `names()`
/// never returns. Exactly `PATTERN_ENUM_CONCURRENCY` enumerations may enter.
#[tokio::test]
async fn concurrent_pattern_enumerations_are_capped_by_the_permit() {
    let name_calls = Arc::new(AtomicUsize::new(0));
    let hanging = Arc::new(HangingNames {
        hang: Duration::from_secs(30),
        name_calls: name_calls.clone(),
    });
    let addr = spawn_tcp(PvListMode::List, 1024, vec![hanging]).await;
    let mut stream = handshake(addr).await;

    let over = PATTERN_ENUM_CONCURRENCY * 2;
    for i in 0..over {
        let req = encode_search_request(
            100 + i as u32,
            0x81,
            0,
            [0u8; 16],
            &[(100 + i as u32, "CAP:*")],
            VERSION,
            IS_BE,
        );
        stream.write_all(&req).await.expect("write wildcard");
    }
    // Nothing can complete (every enumeration hangs), so once the queue has
    // drained the count is stable.
    tokio::time::sleep(Duration::from_millis(750)).await;

    let entered = name_calls.load(Ordering::SeqCst);
    assert!(
        entered > 0,
        "no enumeration ran at all; the test proves nothing"
    );
    assert_eq!(
        entered, PATTERN_ENUM_CONCURRENCY,
        "{over} wildcard searches started {entered} concurrent enumerations; \
         the PATTERN_ENUM_CONCURRENCY={PATTERN_ENUM_CONCURRENCY} bound is not \
         being applied, so one peer can spawn an unbounded number of tasks"
    );
}

/// Kills M9 — **delete the `wildcard_match` filter in the spawned task**.
///
/// Without the filter every pattern cid in the datagram is claimed, so a
/// wildcard matching *nothing* is answered `found=true` and the client opens a
/// channel the server cannot serve. The shipped negative test uses a plain
/// name, so it never reaches the pattern branch at all.
///
/// (Test shape due to adversarial verifier V4.)
#[tokio::test]
async fn a_wildcard_matching_nothing_is_not_claimed() {
    let src = Arc::new(ListingSource::new(&["DEF:SERVED"], &["DEF:SERVED"]));
    let addr = spawn_tcp(PvListMode::List, 1024, vec![src]).await;
    let mut stream = handshake(addr).await;

    // Anti-vacuity: a wildcard that *does* match must be claimed.
    let hit = tcp_search(&mut stream, 10, 60, "DEF:*").await;
    assert!(hit.found && hit.cids == vec![60], "a matching wildcard was not claimed");

    let miss = tcp_search(&mut stream, 11, 61, "NOSUCHPREFIX:*").await;
    assert!(
        !miss.found && miss.cids.is_empty(),
        "a wildcard matching no enumerated name was claimed (found={}, cids={:?}); \
         the wildcard_match filter is not being applied",
        miss.found,
        miss.cids
    );
}

/// Kills M4 — **drop the `pvlist_max` truncation on the spawned path**.
///
/// `pvlist_max` is an operator-set bound on what a wildcard may disclose. The
/// server lists three names with `pvlist_max = 2`; after the sort-and-truncate
/// only the first two are visible, so a wildcard for the third must not match.
#[tokio::test]
async fn the_spawned_enumeration_respects_pvlist_max() {
    let src = Arc::new(ListingSource::new(
        &[],
        &["MAXA:ONE", "MAXB:TWO", "MAXZ:THREE"],
    ));
    let addr = spawn_tcp(PvListMode::List, 2, vec![src]).await;
    let mut stream = handshake(addr).await;

    // Anti-vacuity: a name inside the cap is visible, so the enumeration ran.
    let inside = tcp_search(&mut stream, 20, 70, "MAXA:*").await;
    assert!(
        inside.found && inside.cids == vec![70],
        "the enumeration produced nothing at all; the test proves nothing"
    );

    let outside = tcp_search(&mut stream, 21, 71, "MAXZ:*").await;
    assert!(
        !outside.found && outside.cids.is_empty(),
        "a name past `pvlist_max` (=2 of 3 listed) was matched and disclosed \
         (found={}, cids={:?}); the truncation is not being applied on the \
         spawned path",
        outside.found,
        outside.cids
    );
}

/// Kills M11 — **drop the `pvlist_mode` gate on the TCP pattern path**.
///
/// With pvlist disabled the server must not enumerate or answer wildcards at
/// all. `collect_visible_pv_names` does *not* re-check the mode for anything
/// but the `__pvlist` entry, so the gate at the search arm is the only thing
/// enforcing this.
#[tokio::test]
async fn a_pattern_query_is_not_answered_when_pvlist_is_off() {
    let src = Arc::new(ListingSource::new(&["OFF:SERVED"], &["OFF:SERVED"]));
    let addr = spawn_tcp(PvListMode::Off, 1024, vec![src]).await;
    let mut stream = handshake(addr).await;

    // Anti-vacuity: the exact name is served on this very server.
    let exact = tcp_search(&mut stream, 30, 80, "OFF:SERVED").await;
    assert!(exact.found, "the served name was not found; the test proves nothing");

    let wild = tcp_search(&mut stream, 31, 81, "OFF:*").await;
    assert!(
        !wild.found && wild.cids.is_empty(),
        "a wildcard was answered with pvlist_mode = Off (found={}, cids={:?}); \
         the mode gate is not being applied",
        wild.found,
        wild.cids
    );
}

/// Kills M8 — **drop the TCP `answer_negative` short-circuit**.
///
/// A datagram carrying a served exact name *and* a pattern that matches
/// nothing is fully answered inline. The spawned task must then stay silent:
/// without the short-circuit it sends a second, contradictory `found=false`
/// frame for the same `seq`.
#[tokio::test]
async fn a_fully_answered_datagram_gets_no_second_negative_frame() {
    let src = Arc::new(ListingSource::new(&["MIX:SERVED"], &["MIX:SERVED"]));
    let addr = spawn_tcp(PvListMode::List, 1024, vec![src]).await;
    let mut stream = handshake(addr).await;

    let req = encode_search_request(
        40,
        0x81,
        0,
        [0u8; 16],
        &[(90, "MIX:SERVED"), (91, "NOMATCH:*")],
        VERSION,
        IS_BE,
    );
    stream.write_all(&req).await.expect("write search");

    let seen = tcp_collect(&mut stream, Duration::from_millis(800)).await;
    let mine: Vec<_> = seen.iter().filter(|p| p.seq == 40).collect();
    assert_eq!(
        mine.len(),
        1,
        "expected exactly one response for a datagram whose exact name was \
         answered inline and whose pattern matched nothing, got {:?}; the \
         spawned task is sending a spurious negative",
        mine.iter().map(|p| (p.found, p.cids.clone())).collect::<Vec<_>>()
    );
    assert!(mine[0].found && mine[0].cids == vec![90]);
}

/// Kills M6 (**wrong `seq` on the UDP deferred reply**) and M13 (**drop the
/// `deferred` term from the UDP `continue` condition**).
///
/// A wildcard-only datagram with `response_required` must produce exactly one
/// reply, and it must carry the searcher's own `seq` — a client keys its
/// pending searches by it, so a corrupted `seq` is an unanswerable search.
/// Dropping the `deferred` term makes the loop *also* send an immediate
/// `found=false`, giving two contradictory frames.
#[tokio::test]
async fn the_udp_deferred_reply_carries_the_requests_seq_exactly_once() {
    let src = Arc::new(ListingSource::new(&[], &["UDPSEQ:PV"]));
    let h = UdpHarness::start(vec![src]).await;

    let req = encode_search_request(
        101,
        0x01,
        0,
        [0u8; 16],
        &[(201, "UDPSEQ:*")],
        VERSION,
        IS_BE,
    );
    let seen = h.send_and_collect(&req, Duration::from_millis(800)).await;

    assert_eq!(
        seen.len(),
        1,
        "expected exactly one UDP reply to a wildcard-only datagram, got {:?}",
        seen.iter().map(|p| (p.seq, p.found, p.cids.clone())).collect::<Vec<_>>()
    );
    assert_eq!(
        seen[0].seq, 101,
        "the deferred UDP reply carried seq {} for a search sent with seq 101; \
         the client keys its pending searches by seq, so this reply is invisible",
        seen[0].seq
    );
    assert!(seen[0].found && seen[0].cids == vec![201]);
}

/// Kills M7 (**UDP `found` forced to `true`**) and M14 (**`answer_negative`
/// forced to `false`**).
///
/// A `response_required` wildcard that matches nothing must be answered once,
/// negatively. Forcing `found = true` turns it into a lie the client acts on;
/// forcing `answer_negative = false` drops the reply entirely, because the
/// search loop has already `continue`d on the strength of the deferral.
#[tokio::test]
async fn a_udp_wildcard_matching_nothing_is_answered_negative_once() {
    let src = Arc::new(ListingSource::new(&[], &["UDPNEG:PV"]));
    let h = UdpHarness::start(vec![src]).await;

    let req = encode_search_request(
        102,
        0x01,
        0,
        [0u8; 16],
        &[(202, "NOTHINGMATCHES:*")],
        VERSION,
        IS_BE,
    );
    let seen = h.send_and_collect(&req, Duration::from_millis(800)).await;

    assert_eq!(
        seen.len(),
        1,
        "a `response_required` wildcard that matched nothing produced {} \
         replies, expected exactly 1: {:?}",
        seen.len(),
        seen.iter().map(|p| (p.seq, p.found, p.cids.clone())).collect::<Vec<_>>()
    );
    assert_eq!(seen[0].seq, 102);
    assert!(
        !seen[0].found && seen[0].cids.is_empty(),
        "a wildcard that matched no enumerated name was answered found={} \
         cids={:?}",
        seen[0].found,
        seen[0].cids
    );
}

/// Kills `7u` — **drop the `pvlist_mode != Off` gate on the *UDP* pattern
/// path** (`handler.rs`, the UDP search arm).
///
/// The gate's TCP twin is covered by
/// `a_pattern_query_is_not_answered_when_pvlist_is_off`; deleting the
/// identical UDP one survived the entire suite. That matters more on UDP than
/// on TCP: an operator who sets `pvlist_mode = Off` specifically to stop
/// wildcard enumeration would still have their whole name list enumerated and
/// wildcards answered from a *single unauthenticated datagram*, with no
/// connection and no ConnectionValidation in the way.
///
/// `collect_visible_pv_names` does not re-check the mode for anything but the
/// `__pvlist` entry, so this gate is the only thing enforcing it.
#[tokio::test]
async fn a_udp_pattern_query_is_not_answered_when_pvlist_is_off() {
    let src = Arc::new(ListingSource::new(&["UDPOFF:SERVED"], &["UDPOFF:SERVED"]));
    let h = UdpHarness::start_with(PvListMode::Off, 1024, vec![src]).await;

    // Anti-vacuity: the exact name is served by this very server, over this
    // very socket, with pvlist off.
    let exact = encode_search_request(
        110,
        0x01,
        0,
        [0u8; 16],
        &[(210, "UDPOFF:SERVED")],
        VERSION,
        IS_BE,
    );
    let seen = h.send_and_collect(&exact, Duration::from_millis(800)).await;
    assert_eq!(seen.len(), 1, "the served name produced {seen:?}");
    assert!(
        seen[0].found && seen[0].cids == vec![210],
        "the served name was not found with pvlist_mode = Off; the test proves nothing"
    );

    // The wildcard matches that same served-and-listed name, so the *only*
    // thing that can keep it unanswered is the mode gate.
    let wild = encode_search_request(
        111,
        0x01,
        0,
        [0u8; 16],
        &[(211, "UDPOFF:*")],
        VERSION,
        IS_BE,
    );
    let seen = h.send_and_collect(&wild, Duration::from_millis(800)).await;
    assert_eq!(
        seen.len(),
        1,
        "a `response_required` wildcard must still get exactly one (negative) \
         reply with pvlist off, got {:?}",
        seen.iter().map(|p| (p.seq, p.found, p.cids.clone())).collect::<Vec<_>>()
    );
    assert!(
        !seen[0].found && seen[0].cids.is_empty(),
        "a wildcard was enumerated and answered over one unauthenticated \
         datagram with pvlist_mode = Off (found={}, cids={:?}); the UDP mode \
         gate is not being applied",
        seen[0].found,
        seen[0].cids
    );
}

/// Kills `p10` — **discard a UDP datagram's pattern queries whenever the same
/// datagram also matched an exact name**.
///
/// Both existing mixed-datagram tests (`a_fully_answered_datagram_gets_no_
/// second_negative_frame` and, in `search_pattern_shed.rs`,
/// `a_shed_pattern_does_not_suppress_an_exact_name_on_the_same_datagram`) are
/// TCP. On UDP the interaction was unobserved, and dropping the pattern half
/// of a mixed datagram survived the suite: the client gets its exact name,
/// sees a perfectly normal reply, and never learns the wildcard was never
/// looked at.
///
/// A pvget for one PV and one wildcard is a single datagram, so this is the
/// ordinary case, not a contrived one. Both halves must be answered — the
/// exact name inline, the pattern from the spawned enumeration — which is two
/// frames carrying the same `seq` and disjoint cids.
#[tokio::test]
async fn a_udp_datagram_mixing_an_exact_name_and_a_pattern_gets_both_answers() {
    // `UDPMIX:LISTED` is enumerated but claimed by nothing, so the only route
    // from it to a `found` response is the pattern path.
    let src = Arc::new(ListingSource::new(
        &["UDPMIX:SERVED"],
        &["UDPMIX:SERVED", "UDPMIX:LISTED"],
    ));
    let h = UdpHarness::start(vec![src]).await;

    let req = encode_search_request(
        120,
        0x01,
        0,
        [0u8; 16],
        &[(220, "UDPMIX:SERVED"), (221, "UDPMIX:*")],
        VERSION,
        IS_BE,
    );
    let seen = h.send_and_collect(&req, Duration::from_millis(800)).await;
    let mine: Vec<_> = seen.iter().filter(|p| p.seq == 120).collect();
    let summary: Vec<_> = mine.iter().map(|p| (p.found, p.cids.clone())).collect();

    assert_eq!(
        mine.len(),
        2,
        "a datagram carrying one exact name and one pattern must be answered \
         twice — inline for the exact name, deferred for the pattern — got \
         {summary:?}"
    );
    assert!(
        mine.iter().any(|p| p.found && p.cids == vec![220]),
        "the exact name's inline reply is missing: {summary:?}"
    );
    assert!(
        mine.iter().any(|p| p.found && p.cids == vec![221]),
        "the pattern half of a mixed datagram was never answered: {summary:?} \
         — the client is told about its exact name and silently never learns \
         the wildcard was dropped"
    );
}
