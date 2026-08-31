//! Shared harness for the two shed-counter tests.
//!
//! `pattern_enum_shed` is a **process-global** counter, so a test that asserts
//! on a delta of it can be satisfied by a sibling test in the same binary
//! rather than by its own work. That is not hypothetical: the first attempt at
//! the panicking-`names()` test below passed against a deliberately broken
//! server, because the paused-clock timeout test in the same binary shed one
//! query concurrently.
//!
//! The fix is process isolation. Cargo gives each `tests/*.rs` file its own
//! binary and therefore its own process, so a file containing exactly one
//! counter test has no sibling that can move the counter. This module holds
//! the harness those single-test files share.

#![allow(dead_code)]

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use spvirit_codec::epics_decode::{PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::encode_search_request;
use spvirit_server::handler::{PvListMode, ServerState, rand_guid, run_udp_search};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::{PvInfo, Source, SourceRegistry, TryClaim};
use spvirit_types::NtPayload;
use tokio::net::UdpSocket;

pub const VERSION: u8 = 2;
pub const IS_BE: bool = false;

/// Everything a [`Source`] must provide that these tests do not care about.
macro_rules! inert_source_body {
    () => {
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
        ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>>
        {
            Box::pin(async { Err("read-only".to_string()) })
        }

        fn subscribe(
            &self,
            _name: &str,
        ) -> Pin<
            Box<dyn Future<Output = Option<tokio::sync::mpsc::Receiver<NtPayload>>> + Send + '_>,
        > {
            Box::pin(async { None })
        }
    };
}

/// Serves and lists exactly one name, instantly.
pub struct OneName(pub &'static str);

impl Source for OneName {
    fn try_claim(&self, name: &str) -> TryClaim {
        if name == self.0 {
            TryClaim::Yes
        } else {
            TryClaim::No
        }
    }

    inert_source_body!();

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        let n = self.0.to_string();
        Box::pin(async move { vec![n] })
    }
}

/// Hangs in `names()` so a handful of wildcards can pin every permit.
pub struct HangingNames {
    pub name_calls: Arc<AtomicUsize>,
}

impl Source for HangingNames {
    fn try_claim(&self, _name: &str) -> TryClaim {
        TryClaim::No
    }

    inert_source_body!();

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        self.name_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(120)).await;
            Vec::new()
        })
    }
}

/// Panics inside `names()`. Third-party code — a Python-backed source, a
/// proxying source indexing a malformed upstream reply — is exactly what runs
/// inside a pattern enumeration.
pub struct PanickingNames {
    pub name_calls: Arc<AtomicUsize>,
}

impl Source for PanickingNames {
    fn try_claim(&self, _name: &str) -> TryClaim {
        TryClaim::No
    }

    inert_source_body!();

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        self.name_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { panic!("this source's names() panics on purpose") })
    }
}

/// A running UDP search responder plus a client socket pointed at it.
pub struct UdpHarness {
    pub server: SocketAddr,
    pub client: UdpSocket,
}

impl UdpHarness {
    pub async fn start(sources: Vec<(&'static str, Arc<dyn Source>)>) -> Self {
        let registry = Arc::new(SourceRegistry::new());
        for (i, (name, s)) in sources.into_iter().enumerate() {
            registry.add(name, i as i32, s).await;
        }
        let state = Arc::new(ServerState::new(
            registry,
            Arc::new(MonitorRegistry::new()),
            false,
            PvListMode::List,
            1024,
            None,
            rand_guid(),
            5075,
            None,
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ));
        // Bind-and-release to pick a free port; `wait_ready` below absorbs the
        // rebind race rather than sleeping blindly.
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

    /// Retry an exact, unserved name until the responder answers, so a later
    /// datagram can be sent exactly once and its replies counted.
    async fn wait_ready(&self) {
        for _ in 0..200 {
            self.search(1, 1, "READY:PROBE").await;
            if !self.collect(Duration::from_millis(30)).await.is_empty() {
                return;
            }
        }
        panic!("the UDP search responder never came up");
    }

    /// Send one `response_required` search for `name`.
    pub async fn search(&self, seq: u32, cid: u32, name: &str) {
        let req = encode_search_request(seq, 0x01, 0, [0u8; 16], &[(cid, name)], VERSION, IS_BE);
        self.client.send_to(&req, self.server).await.unwrap();
    }

    /// Every `SearchResponse` seen within `window`.
    pub async fn collect(
        &self,
        window: Duration,
    ) -> Vec<spvirit_codec::epics_decode::PvaSearchResponsePayload> {
        let deadline = Instant::now() + window;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(Ok((n, _))) =
                tokio::time::timeout(remaining, self.client.recv_from(&mut buf)).await
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

/// Poll until `pattern_enum_shed` reaches `target`, or give up. Returns the
/// value actually observed, so the caller can put it in its failure message.
pub async fn await_shed_count(target: u64) -> u64 {
    for _ in 0..300 {
        let now = spvirit_server::search_resolve::global_stats().pattern_enum_shed;
        if now >= target {
            return now;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    spvirit_server::search_resolve::global_stats().pattern_enum_shed
}
