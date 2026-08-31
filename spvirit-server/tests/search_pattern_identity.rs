//! V4 HIGH-1: the off-task pattern enumeration must run under the requesting
//! peer's identity.
//!
//! `SourceRegistry::names()` is an **access-aware** call. The gateway's status
//! source filters its own listing with
//! `decide_local(Op::Get, name, &current_identity())`, and `current_identity()`
//! reads `spvirit_server::request_ctx::request_identity()` — a task-local. A
//! `tokio::spawn`ed task does not inherit task-locals, so moving the
//! enumeration off the search task silently dropped the identity and every
//! host-qualified pvlist rule stopped matching:
//!
//! * `<pat> DENY FROM <host>` no longer fires -> names policy hides from that
//!   peer are enumerated and answered `found=true` (existence disclosure).
//! * `<pat> ALLOW FROM <host>` no longer fires -> a legitimate operator's
//!   wildcard query returns nothing.
//!
//! These are **authorization** tests, not plumbing tests: the source below
//! returns a *different name list* depending on the identity it observes, and
//! both directions are asserted through the wire-level `SearchResponse`.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::{encode_client_connection_validation, encode_search_request};
use spvirit_server::handler::{PvListMode, ServerState, rand_guid, run_tcp_server};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::{PvInfo, Source, SourceRegistry, TryClaim};
use spvirit_server::request_ctx::request_identity;
use spvirit_types::NtPayload;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const VERSION: u8 = 2;
const IS_BE: bool = false;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// The `ca` user the test handshake asserts; the source below treats it as the
/// operator a host-qualified `ALLOW` rule would name.
const OPERATOR: &str = "tester";
/// Only visible to `OPERATOR` — the `ALLOW … FROM host` direction.
const ALLOWED_PV: &str = "IDENT:ALLOWED";
/// Visible only to *everyone else* — the `DENY … FROM host` direction. If the
/// enumeration runs with no identity, this name leaks.
const DENIED_PV: &str = "IDENT:DENIED";

/// A source whose `names()` output depends on `request_identity()`, exactly as
/// `spvirit-gateway`'s `GatewayStatusSource::names` does.
struct IdentityFilteredNames {
    /// Records the user each `names()` call observed, so a passing assertion
    /// cannot be vacuous.
    calls: Arc<AtomicUsize>,
    saw_identity: Arc<AtomicUsize>,
}

impl Source for IdentityFilteredNames {
    fn try_claim(&self, _name: &str) -> TryClaim {
        // Decisively "no": the *only* way either name's cid can come back is
        // through the pattern path's enumeration.
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
        let calls = self.calls.clone();
        let saw_identity = self.saw_identity.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let (peer_ip, user) = request_identity();
            if peer_ip.is_some() {
                saw_identity.fetch_add(1, Ordering::SeqCst);
            }
            if user.as_deref() == Some(OPERATOR) {
                vec![ALLOWED_PV.to_string()]
            } else {
                vec![DENIED_PV.to_string()]
            }
        })
    }
}

async fn spawn_server(extra: Arc<dyn Source>) -> SocketAddr {
    let sources = Arc::new(SourceRegistry::new());
    sources.add("identity", 0, extra).await;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");

    let state = Arc::new(ServerState::new(
        sources,
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

async fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 8];
    tokio::time::timeout(IO_TIMEOUT, stream.read_exact(&mut header))
        .await
        .expect("timeout reading header")
        .expect("read header");

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
            .expect("timeout reading payload")
            .expect("read payload");
    }

    let mut full = header.to_vec();
    full.extend_from_slice(&payload);
    full
}

async fn read_until<F>(stream: &mut TcpStream, mut accept: F) -> PvaPacketCommand
where
    F: FnMut(&PvaPacketCommand) -> bool,
{
    for _ in 0..32 {
        let raw = read_frame(stream).await;
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
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidation(_))
    })
    .await;
    let validation = encode_client_connection_validation(
        16_384, 512, 0, "ca", OPERATOR, "host", VERSION, IS_BE,
    );
    stream
        .write_all(&validation)
        .await
        .expect("write validation");
    read_until(&mut stream, |cmd| {
        matches!(cmd, PvaPacketCommand::ConnectionValidated(_))
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
    match read_until(stream, |cmd| {
        matches!(cmd, PvaPacketCommand::SearchResponse(p) if p.seq == seq)
    })
    .await
    {
        PvaPacketCommand::SearchResponse(p) => p,
        other => panic!("expected SearchResponse, got {other:?}"),
    }
}

/// Both halves of V4 HIGH-1 over the `EPICS_PVA_NAME_SERVERS` TCP route — the
/// route the fix's own comment names as the gateway's deployment target.
///
/// With the request context reinstated in the spawned task, the enumeration
/// sees `user = "tester"` and returns `IDENT:ALLOWED`. Without it,
/// `request_identity()` is `(None, None)`, the source falls into its
/// "everyone else" branch, and the two assertions invert: `IDENT:ALLOWED:*`
/// comes back not-found (an operator locked out) while `IDENT:DENIED:*` comes
/// back found (a name disclosed to a peer policy hides it from).
#[tokio::test]
async fn the_spawned_enumeration_filters_names_by_the_requesting_identity() {
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_identity = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(IdentityFilteredNames {
        calls: calls.clone(),
        saw_identity: saw_identity.clone(),
    });
    let addr = spawn_server(source).await;
    let mut stream = handshake(addr).await;

    // ALLOW direction: the operator must still see the name their identity
    // grants.
    let allowed = tcp_search(&mut stream, 1, 71, "IDENT:ALLOWED*").await;
    assert!(
        allowed.found && allowed.cids == vec![71],
        "the enumeration did not see the requesting user, so a name an \
         `ALLOW … FROM <host>` rule grants was hidden from a legitimate \
         operator (found={}, cids={:?})",
        allowed.found,
        allowed.cids
    );

    // DENY direction: a name only visible to *other* identities must not leak.
    let denied = tcp_search(&mut stream, 2, 72, "IDENT:DENIED*").await;
    assert!(
        !denied.found && denied.cids.is_empty(),
        "the enumeration ran with no request identity, so a `DENY … FROM \
         <host>` rule stopped matching and a hidden name was disclosed \
         (found={}, cids={:?})",
        denied.found,
        denied.cids
    );

    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "the pattern path never reached names(); the test proves nothing"
    );
    assert_eq!(
        saw_identity.load(Ordering::SeqCst),
        calls.load(Ordering::SeqCst),
        "at least one names() call ran with no peer identity at all"
    );
}
