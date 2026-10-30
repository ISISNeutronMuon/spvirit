//! Shared, batching UDP name search for one [`PvaClient`](crate::PvaClient).
//!
//! [`search_pv`] is one-shot: every call binds its own socket(s), sends one
//! name per datagram, and retransmits on a fixed schedule. A client resolving
//! many names at once — a gateway answering a burst of downstream searches —
//! therefore pays one socket and up to five datagrams per target *per name*,
//! with every caller's retries landing in lockstep.
//!
//! [`SearchEngine`] runs one background task per client instead. It owns one
//! socket per bind address for its whole life, keeps every unresolved name in
//! one table, and on each send packs every name that is due into as few
//! datagrams as fit (greedy next-fit, see [`pack_next_fit`]). Retries are
//! jittered so independent clients drift apart rather than retransmit in
//! step. The wire format is the ordinary multi-entry SEARCH message the
//! protocol has always allowed; servers already answer every found cid of a
//! request in one SEARCH_RESPONSE.
//!
//! The task ends — and its sockets close — once the owning client is dropped
//! and nothing is left pending.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::net::SocketAddr;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::debug;

use spvirit_codec::epics_decode::{PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::encode_search_request;

use crate::search::{
    SearchGroup, SearchTarget, bind_search_groups, decode_search_response_addr, search_pv,
    send_to_group, spawn_search_receivers,
};
use crate::types::PvGetError;

type Found = (SocketAddr, [u8; 12]);
type Reply = oneshot::Sender<Result<Found, PvGetError>>;

/// Largest SEARCH datagram built for an IPv4 socket: a 1500-byte Ethernet
/// MTU less the IPv4 (20) and UDP (8) headers, so a batch never fragments.
const MAX_DATAGRAM_V4: usize = 1472;
/// The same for IPv6 (40-byte header).
const MAX_DATAGRAM_V6: usize = 1452;

/// Bytes of a SEARCH message before its first entry: the 8-byte header, then
/// seq(4) flags(1) reserved(3) reply-addr(16) reply-port(2), the protocol list
/// (size 1, `"tcp"` 4) and the entry count(2). Pinned against the encoder by
/// a test.
const SEARCH_FIXED_BYTES: usize = 41;

/// Gaps between the sends of one name: 0, 100, 500, 1000, 2000 ms from its
/// first send — the schedule [`search_pv`] uses — so at most five sends.
const RETRY_GAPS_MS: [u64; 4] = [100, 400, 500, 1000];

/// Each gap is scaled by a factor drawn from `1 ± RETRY_JITTER`.
const RETRY_JITTER: f64 = 0.25;

/// A name whose retry falls due within this window of a send rides along in
/// that send rather than going out alone a moment later. Well inside the
/// jitter, so it does not change the schedule's shape.
const RETRY_COALESCE: Duration = Duration::from_millis(25);

/// How many recent datagram sequence numbers each name remembers. A response
/// must echo one of them (as well as the name's cid) to be accepted.
const SEQS_KEPT: usize = 16;

/// Encoded size of one `(cid, name)` search entry: cid(4) plus the PVA
/// size-prefixed name (1-byte size below 254, else 0xFE + u32).
fn search_entry_bytes(name: &str) -> usize {
    let n = name.len();
    let prefix = if n < 254 { 1 } else { 5 };
    4 + prefix + n
}

/// Split `sizes`, in order, into consecutive runs whose totals are at most
/// `capacity`, by greedy next-fit: keep adding to the current run until the
/// next item does not fit, then start a new one. An item larger than
/// `capacity` gets a run of its own (it is sent oversized rather than
/// dropped, as a one-name `search_pv` datagram would be).
///
/// For packing that keeps order and never splits an item, next-fit is
/// optimal: by induction, the greedy k-th run ends at or after the k-th run of
/// any feasible packing (the greedy run starts no later, and takes every item
/// that fits), so no packing uses fewer runs. `next_fit_is_optimal` checks this
/// against an exhaustive minimum.
pub(crate) fn pack_next_fit(sizes: &[usize], capacity: usize) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (i, &size) in sizes.iter().enumerate() {
        if i > start && used + size > capacity {
            runs.push(start..i);
            start = i;
            used = 0;
        }
        used += size;
    }
    if start < sizes.len() {
        runs.push(start..sizes.len());
    }
    runs
}

/// A per-process random seed, for jitter and for the starting cid/seq.
fn random_seed() -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    h.write_u32(std::process::id());
    // xorshift has a fixed point at zero.
    h.finish() | 1
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

struct Request {
    name: String,
    deadline: Instant,
    reply: Reply,
}

struct Waiter {
    deadline: Instant,
    reply: Reply,
}

/// One unresolved name.
struct Pending {
    name: String,
    /// Arrival order: batches go out oldest name first.
    order: u64,
    waiters: Vec<Waiter>,
    /// Datagrams that have carried this name so far.
    sends: usize,
    /// When to send it next; `None` once the retry schedule is spent.
    next_send: Option<Instant>,
    /// Sequence numbers of the datagrams that carried it.
    seqs: Vec<u32>,
}

/// The running engine's handle, and what it was started for.
struct Handle {
    udp_port: u16,
    targets: Vec<SearchTarget>,
    debug: bool,
    tx: mpsc::UnboundedSender<Request>,
}

/// The shared search for one client. Cheap to clone; clones share the task.
#[derive(Clone, Default)]
pub(crate) struct SearchEngine {
    handle: Arc<Mutex<Option<Handle>>>,
}

impl SearchEngine {
    /// Resolve `pv_name` over UDP: same contract and errors as [`search_pv`],
    /// but batched with every other name this client is searching for.
    pub(crate) async fn search(
        &self,
        pv_name: &str,
        udp_port: u16,
        timeout_dur: Duration,
        targets: &[SearchTarget],
        debug_enabled: bool,
    ) -> Result<Found, PvGetError> {
        if targets.is_empty() {
            return Err(PvGetError::Search("no search targets"));
        }
        let deadline = Instant::now() + timeout_dur;
        let (reply, rx) = oneshot::channel();
        let req = Request {
            name: pv_name.to_string(),
            deadline,
            reply,
        };
        self.submit(req, udp_port, targets, debug_enabled)?;
        match rx.await {
            Ok(result) => result,
            // The task is gone without answering: the runtime it was spawned
            // on has shut down. Search the one-shot way for the time left;
            // the next call starts a fresh task.
            Err(_) => {
                let left = deadline.saturating_duration_since(Instant::now());
                search_pv(pv_name, udp_port, left, targets, debug_enabled).await
            }
        }
    }

    /// Hand `req` to the running task, starting one if there is none, the
    /// last one has exited, or it was started for different targets (the
    /// targets are rebuilt from the environment per resolve, so they can
    /// change; the superseded task finishes what it has and exits).
    fn submit(
        &self,
        req: Request,
        udp_port: u16,
        targets: &[SearchTarget],
        debug_enabled: bool,
    ) -> Result<(), PvGetError> {
        let mut guard = self
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reusable = guard.as_ref().is_some_and(|h| {
            h.udp_port == udp_port
                && h.targets == targets
                && h.debug == debug_enabled
                && !h.tx.is_closed()
        });
        if !reusable {
            *guard = Some(spawn_engine(udp_port, targets, debug_enabled)?);
        }
        let handle = guard.as_ref().expect("set just above");
        // Cannot fail: the receiver was open a moment ago under this lock, and
        // only the task (which is not dropping it while requests are open)
        // holds it. Were it to, the caller's `rx.await` sees the dropped
        // reply and falls back to a one-shot search.
        let _ = handle.tx.send(req);
        Ok(())
    }
}

fn spawn_engine(
    udp_port: u16,
    targets: &[SearchTarget],
    debug_enabled: bool,
) -> Result<Handle, PvGetError> {
    let (groups, last_io_error) = bind_search_groups(udp_port, targets, debug_enabled, "search");
    if groups.is_empty() {
        // Same errors `search_pv` gives when no socket comes up.
        return Err(match last_io_error {
            Some(err) => PvGetError::Io(err),
            None => PvGetError::Timeout("search response"),
        });
    }
    let (recv_tasks, packets) = spawn_search_receivers(groups.iter().map(|g| &g.socket));
    let (tx, requests) = mpsc::unbounded_channel();
    let mut rng = random_seed();
    let engine = Engine {
        udp_port,
        debug_enabled,
        capacity: if groups.iter().any(SearchGroup::is_ipv6) {
            MAX_DATAGRAM_V6
        } else {
            MAX_DATAGRAM_V4
        },
        groups,
        pending: HashMap::new(),
        by_name: HashMap::new(),
        next_cid: xorshift(&mut rng) as u32,
        next_seq: xorshift(&mut rng) as u32,
        next_order: 0,
        rng,
    };
    tokio::spawn(engine.run(requests, packets, recv_tasks));
    Ok(Handle {
        udp_port,
        targets: targets.to_vec(),
        debug: debug_enabled,
        tx,
    })
}

struct Engine {
    udp_port: u16,
    debug_enabled: bool,
    /// Largest datagram to build for these sockets.
    capacity: usize,
    groups: Vec<SearchGroup>,
    /// Unresolved names by cid.
    pending: HashMap<u32, Pending>,
    /// Name -> cid, so concurrent searches for one name share one entry.
    by_name: HashMap<String, u32>,
    next_cid: u32,
    next_seq: u32,
    next_order: u64,
    rng: u64,
}

impl Engine {
    async fn run(
        mut self,
        mut requests: mpsc::UnboundedReceiver<Request>,
        mut packets: mpsc::Receiver<(Vec<u8>, SocketAddr)>,
        // Held for its `Drop`: ending the task aborts the receivers and
        // closes the sockets.
        _recv_tasks: tokio::task::JoinSet<()>,
    ) {
        let mut requests_open = true;
        loop {
            if !requests_open && self.pending.is_empty() {
                break;
            }
            let wake = self
                .next_wake()
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
            tokio::select! {
                req = requests.recv(), if requests_open => match req {
                    Some(req) => {
                        self.add(req, Instant::now());
                        // Let every caller that is ready right now enqueue
                        // too, so a burst leaves in as few datagrams as fit.
                        tokio::task::yield_now().await;
                        while let Ok(req) = requests.try_recv() {
                            self.add(req, Instant::now());
                        }
                    }
                    None => requests_open = false,
                },
                pkt = packets.recv() => match pkt {
                    Some((buf, src)) => self.on_packet(&buf, src),
                    // Every receiver has exited, so no answer can arrive.
                    // Let the waiters time out on their own deadlines rather
                    // than fail early; the next search starts a new task.
                    None => {
                        self.wait_out_deadlines().await;
                        break;
                    }
                },
                _ = tokio::time::sleep_until(wake) => {}
            }
            let now = Instant::now();
            self.expire(now);
            self.send_due(now).await;
        }
    }

    fn add(&mut self, req: Request, now: Instant) {
        if req.reply.is_closed() {
            return;
        }
        let waiter = Waiter {
            deadline: req.deadline,
            reply: req.reply,
        };
        if let Some(cid) = self.by_name.get(&req.name)
            && let Some(p) = self.pending.get_mut(cid)
        {
            p.waiters.push(waiter);
            // A new caller is owed a full search: restart a spent schedule.
            if p.next_send.is_none() {
                p.sends = 0;
                p.next_send = Some(now);
            }
            return;
        }
        let mut cid = self.next_cid;
        while self.pending.contains_key(&cid) {
            cid = cid.wrapping_add(1);
        }
        self.next_cid = cid.wrapping_add(1);
        self.by_name.insert(req.name.clone(), cid);
        self.pending.insert(
            cid,
            Pending {
                name: req.name,
                order: self.next_order,
                waiters: vec![waiter],
                sends: 0,
                next_send: Some(now),
                seqs: Vec::new(),
            },
        );
        self.next_order += 1;
    }

    fn next_wake(&self) -> Option<Instant> {
        self.pending
            .values()
            .flat_map(|p| p.next_send.into_iter().chain(p.waiters.iter().map(|w| w.deadline)))
            .min()
    }

    /// Time out waiters whose deadline has passed, drop those whose caller
    /// went away, and forget names nobody is waiting for any more.
    fn expire(&mut self, now: Instant) {
        let by_name = &mut self.by_name;
        self.pending.retain(|_, p| {
            for w in std::mem::take(&mut p.waiters) {
                if w.reply.is_closed() {
                    continue;
                }
                if w.deadline <= now {
                    let _ = w.reply.send(Err(PvGetError::Timeout("search response")));
                    continue;
                }
                p.waiters.push(w);
            }
            if p.waiters.is_empty() {
                by_name.remove(&p.name);
                false
            } else {
                true
            }
        });
    }

    async fn wait_out_deadlines(&mut self) {
        while let Some(deadline) = self
            .pending
            .values()
            .flat_map(|p| p.waiters.iter().map(|w| w.deadline))
            .min()
        {
            tokio::time::sleep_until(deadline).await;
            self.expire(Instant::now());
        }
    }

    /// Send every name that is due, packed into as few datagrams as fit.
    async fn send_due(&mut self, now: Instant) {
        if !self
            .pending
            .values()
            .any(|p| p.next_send.is_some_and(|t| t <= now))
        {
            return;
        }
        let horizon = now + RETRY_COALESCE;
        let mut due: Vec<(u64, u32)> = self
            .pending
            .iter()
            .filter(|(_, p)| p.next_send.is_some_and(|t| t <= horizon))
            .map(|(cid, p)| (p.order, *cid))
            .collect();
        due.sort_unstable();

        let sizes: Vec<usize> = due
            .iter()
            .map(|(_, cid)| search_entry_bytes(&self.pending[cid].name))
            .collect();
        let capacity = self.capacity - SEARCH_FIXED_BYTES;
        for run in pack_next_fit(&sizes, capacity) {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            let batch: Vec<(u32, &str)> = due[run]
                .iter()
                .map(|(_, cid)| (*cid, self.pending[cid].name.as_str()))
                .collect();
            if self.debug_enabled {
                debug!("pva search seq={seq} sending {} name(s)", batch.len());
            }
            for group in &self.groups {
                let msg = encode_search_request(
                    seq,
                    0x81,
                    group.reply_port,
                    group.reply_addr,
                    &batch,
                    2,
                    false,
                );
                // A failed send is retried on the name's schedule, exactly as
                // `search_pv` ignores a failed retransmit.
                let _ = send_to_group(group, &msg, self.udp_port, self.debug_enabled, "search")
                    .await;
            }
            let cids: Vec<u32> = batch.iter().map(|(cid, _)| *cid).collect();
            for cid in cids {
                let p = self.pending.get_mut(&cid).expect("due names are pending");
                if p.seqs.len() == SEQS_KEPT {
                    p.seqs.remove(0);
                }
                p.seqs.push(seq);
            }
        }

        for (_, cid) in due {
            let gap = {
                let p = &self.pending[&cid];
                RETRY_GAPS_MS.get(p.sends).copied()
            };
            let next = gap.map(|ms| now + self.jitter(ms));
            let p = self.pending.get_mut(&cid).expect("due names are pending");
            p.sends += 1;
            p.next_send = next;
        }
    }

    /// `ms` scaled by a factor in `1 ± RETRY_JITTER`.
    fn jitter(&mut self, ms: u64) -> Duration {
        let unit = (xorshift(&mut self.rng) >> 11) as f64 / (1u64 << 53) as f64;
        let factor = 1.0 + RETRY_JITTER * (2.0 * unit - 1.0);
        Duration::from_secs_f64(ms as f64 * factor / 1000.0)
    }

    /// Resolve every waiting name a response answers.
    ///
    /// Untrusted UDP ingress: a datagram that does not decode is dropped
    /// rather than failing every name in flight. A response is accepted for a
    /// cid only if it also echoes the seq of a datagram that carried that
    /// cid — the same pairing `search_pv` checks.
    fn on_packet(&mut self, buf: &[u8], src: SocketAddr) {
        let Some(mut pkt) = PvaPacket::try_new(buf) else {
            debug!(
                "search: dropping short datagram ({} bytes) from {}",
                buf.len(),
                src
            );
            return;
        };
        let Some(PvaPacketCommand::SearchResponse(payload)) = pkt.decode_payload() else {
            return;
        };
        if self.debug_enabled {
            debug!(
                "pva search response found={} cids={:?} addr={:?} port={}",
                payload.found, payload.cids, payload.addr, payload.port
            );
        }
        if !payload.found {
            return;
        }
        if !payload.protocol.is_empty() && !payload.protocol.eq_ignore_ascii_case("tcp") {
            return;
        }
        let addr = decode_search_response_addr(payload.addr, payload.port, src);
        for cid in &payload.cids {
            let matches = self
                .pending
                .get(cid)
                .is_some_and(|p| p.seqs.contains(&payload.seq));
            if !matches {
                continue;
            }
            let p = self.pending.remove(cid).expect("checked just above");
            self.by_name.remove(&p.name);
            if self.debug_enabled {
                debug!("pva search response for {} from {}", p.name, addr);
            }
            for w in p.waiters {
                let _ = w.reply.send(Ok((addr, payload.guid)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    /// Fewest runs any order-preserving, no-split packing can use.
    fn min_runs(sizes: &[usize], capacity: usize) -> usize {
        // best[i] = fewest runs covering sizes[..i].
        let mut best = vec![usize::MAX; sizes.len() + 1];
        best[0] = 0;
        for end in 1..=sizes.len() {
            let mut total = 0;
            for start in (0..end).rev() {
                total += sizes[start];
                // A lone oversized item is allowed its own run.
                if total > capacity && start + 1 < end {
                    break;
                }
                if best[start] != usize::MAX {
                    best[end] = best[end].min(best[start] + 1);
                }
            }
        }
        best[sizes.len()]
    }

    #[test]
    fn next_fit_is_optimal() {
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..5_000 {
            let n = (xorshift(&mut rng) % 12) as usize;
            let capacity = 5 + (xorshift(&mut rng) % 40) as usize;
            let sizes: Vec<usize> = (0..n)
                .map(|_| 1 + (xorshift(&mut rng) % (capacity as u64 + 8)) as usize)
                .collect();
            let runs = pack_next_fit(&sizes, capacity);
            // Order preserved, nothing split, nothing lost.
            let mut next = 0;
            for r in &runs {
                assert_eq!(r.start, next);
                assert!(r.end > r.start);
                let total: usize = sizes[r.clone()].iter().sum();
                assert!(total <= capacity || r.len() == 1, "{sizes:?} {runs:?}");
                next = r.end;
            }
            assert_eq!(next, sizes.len());
            assert_eq!(runs.len(), min_runs(&sizes, capacity), "{sizes:?} cap={capacity}");
        }
    }

    /// The packing arithmetic must agree with the real encoder, byte for byte.
    #[test]
    fn entry_and_fixed_sizes_match_the_encoder() {
        let long = "L".repeat(300);
        let names = ["A", "THIRTY:CHARACTER:PV:NAME:XXXXX", long.as_str()];
        for k in 0..=names.len() {
            let batch: Vec<(u32, &str)> = names[..k].iter().map(|n| (7, *n)).collect();
            let msg = encode_search_request(1, 0x81, 5076, [0u8; 16], &batch, 2, false);
            let expected: usize = SEARCH_FIXED_BYTES
                + names[..k].iter().map(|n| search_entry_bytes(n)).sum::<usize>();
            assert_eq!(msg.len(), expected, "k={k}");
        }
    }

    /// The figure from the review: about forty 30-character names fit in one
    /// 1472-byte datagram.
    #[test]
    fn forty_thirty_character_names_fit_one_ipv4_datagram() {
        let sizes = vec![search_entry_bytes(&"N".repeat(30)); 200];
        let runs = pack_next_fit(&sizes, MAX_DATAGRAM_V4 - SEARCH_FIXED_BYTES);
        assert_eq!(runs[0].len(), 40);
        assert_eq!(runs.len(), 5);
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let mut engine = Engine {
            udp_port: 0,
            debug_enabled: false,
            capacity: MAX_DATAGRAM_V4,
            groups: Vec::new(),
            pending: HashMap::new(),
            by_name: HashMap::new(),
            next_cid: 0,
            next_seq: 0,
            next_order: 0,
            rng: random_seed(),
        };
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..1_000 {
            let d = engine.jitter(1000);
            assert!(d >= Duration::from_millis(750) && d <= Duration::from_millis(1250));
            distinct.insert(d.as_micros());
        }
        assert!(distinct.len() > 100, "jitter should actually vary");
    }

    /// A fake responder that records every SEARCH it gets and answers found
    /// for every cid whose name it hosts, echoing seq — like a real server.
    async fn fake_server(
        hosted: &'static [&'static str],
    ) -> (u16, Arc<Mutex<Vec<Vec<String>>>>, tokio::task::JoinHandle<()>) {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let seen2 = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok((n, src)) = sock.recv_from(&mut buf).await {
                let mut pkt = PvaPacket::new(&buf[..n]);
                let Some(PvaPacketCommand::Search(req)) = pkt.decode_payload() else {
                    continue;
                };
                seen2
                    .lock()
                    .unwrap()
                    .push(req.pv_requests.iter().map(|(_, n)| n.clone()).collect());
                let cids: Vec<u32> = req
                    .pv_requests
                    .iter()
                    .filter(|(_, n)| hosted.contains(&n.as_str()))
                    .map(|(c, _)| *c)
                    .collect();
                if cids.is_empty() {
                    continue;
                }
                let resp = spvirit_codec::spvirit_encode::encode_search_response(
                    [3u8; 12],
                    req.seq,
                    spvirit_codec::spvirit_encode::ip_to_bytes(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                    5075,
                    "tcp",
                    true,
                    &cids,
                    2,
                    false,
                );
                let _ = sock.send_to(&resp, src).await;
            }
        });
        (port, seen, task)
    }

    fn loopback() -> Vec<SearchTarget> {
        vec![SearchTarget {
            target: IpAddr::V4(Ipv4Addr::LOCALHOST),
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        }]
    }

    /// A burst of concurrent searches leaves in one datagram, and every caller
    /// gets its answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_searches_share_one_datagram() {
        const NAMES: [&str; 30] = [
            "B:00", "B:01", "B:02", "B:03", "B:04", "B:05", "B:06", "B:07", "B:08", "B:09",
            "B:10", "B:11", "B:12", "B:13", "B:14", "B:15", "B:16", "B:17", "B:18", "B:19",
            "B:20", "B:21", "B:22", "B:23", "B:24", "B:25", "B:26", "B:27", "B:28", "B:29",
        ];
        let (port, seen, _srv) = fake_server(&NAMES).await;
        let engine = SearchEngine::default();
        let targets = loopback();
        let results = futures_join_all(NAMES.iter().map(|name| {
            let engine = engine.clone();
            let targets = targets.clone();
            async move {
                engine
                    .search(name, port, Duration::from_secs(3), &targets, false)
                    .await
            }
        }))
        .await;
        for r in &results {
            let (addr, guid) = r.as_ref().expect("resolved");
            assert_eq!(addr.port(), 5075);
            assert_eq!(*guid, [3u8; 12]);
        }
        let seen = seen.lock().unwrap();
        let names_sent: usize = seen.iter().map(Vec::len).sum();
        assert_eq!(names_sent, NAMES.len(), "each name sent once: {seen:?}");
        assert!(seen.len() <= 2, "a burst should pack, got {} datagrams", seen.len());
    }

    /// Two callers searching for the same name share one cid and both get
    /// the answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_names_share_one_entry() {
        let (port, seen, _srv) = fake_server(&["DUP"]).await;
        let engine = SearchEngine::default();
        let targets = loopback();
        let (a, b) = tokio::join!(
            engine.search("DUP", port, Duration::from_secs(3), &targets, false),
            engine.search("DUP", port, Duration::from_secs(3), &targets, false),
        );
        assert!(a.is_ok() && b.is_ok());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().flatten().filter(|n| *n == "DUP").count(), 1);
    }

    /// An unanswered name times out with `search_pv`'s error, after at most
    /// five sends; it does not block a name that is answered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unanswered_name_times_out_after_bounded_retries() {
        let (port, seen, _srv) = fake_server(&["HERE"]).await;
        let engine = SearchEngine::default();
        let targets = loopback();
        let (missing, here) = tokio::join!(
            engine.search("NOWHERE", port, Duration::from_millis(3_000), &targets, false),
            engine.search("HERE", port, Duration::from_secs(3), &targets, false),
        );
        assert!(here.is_ok());
        assert!(matches!(missing, Err(PvGetError::Timeout("search response"))));
        let sends = seen
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .filter(|n| *n == "NOWHERE")
            .count();
        assert!((1..=5).contains(&sends), "sent {sends} times");
    }

    /// A response that names our cid under a seq we never sent it in is not
    /// accepted (the pairing `search_pv` also enforces).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn response_with_a_foreign_seq_is_ignored() {
        let mut engine = Engine {
            udp_port: 0,
            debug_enabled: false,
            capacity: MAX_DATAGRAM_V4,
            groups: Vec::new(),
            pending: HashMap::new(),
            by_name: HashMap::new(),
            next_cid: 40,
            next_seq: 0,
            next_order: 0,
            rng: 1,
        };
        let (reply, mut rx) = oneshot::channel();
        engine.add(
            Request {
                name: "X".to_string(),
                deadline: Instant::now() + Duration::from_secs(5),
                reply,
            },
            Instant::now(),
        );
        engine.pending.get_mut(&40).unwrap().seqs.push(11);
        let src: SocketAddr = "127.0.0.1:5076".parse().unwrap();
        let resp = |seq| {
            spvirit_codec::spvirit_encode::encode_search_response(
                [1u8; 12],
                seq,
                [0u8; 16],
                5075,
                "tcp",
                true,
                &[40],
                2,
                false,
            )
        };
        engine.on_packet(&resp(12), src);
        assert!(rx.try_recv().is_err(), "foreign seq must not resolve");
        engine.on_packet(&resp(11), src);
        assert!(rx.try_recv().unwrap().is_ok());
    }

    /// When the engine's runtime goes away, a later search on a new runtime
    /// starts a fresh task instead of hanging on the dead one.
    #[test]
    fn a_dead_engine_is_replaced() {
        let engine = SearchEngine::default();
        let targets = loopback();
        for _ in 0..2 {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let engine = engine.clone();
            let targets = targets.clone();
            rt.block_on(async move {
                let (port, _seen, _srv) = fake_server(&["RT"]).await;
                let r = engine
                    .search("RT", port, Duration::from_secs(3), &targets, false)
                    .await;
                assert!(r.is_ok(), "{r:?}");
            });
        }
    }

    /// Minimal join-all without a `futures` dependency.
    async fn futures_join_all<F, T>(futs: impl IntoIterator<Item = F>) -> Vec<T>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let handles: Vec<_> = futs.into_iter().map(tokio::spawn).collect();
        let mut out = Vec::with_capacity(handles.len());
        for h in handles {
            out.push(h.await.unwrap());
        }
        out
    }
}
