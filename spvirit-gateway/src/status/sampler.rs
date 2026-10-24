//! `BandwidthSampler` — converts the cumulative [`BandwidthCounters`] (Task 6)
//! and [`ClientRegistry::byhost`] (Task 3) into 1 Hz per-second RATE
//! snapshots, stored in a shared `Arc<Mutex<RateSnapshot>>` for Task 13
//! (served bandwidth tables) and Task 14 (`/metrics`) to read.
//!
//! `tick()` (driven by `runtime.rs`'s `tokio::time::interval(1s)` loop) does
//! all counter reads and the snapshot-mutex write synchronously — no lock is
//! ever held across an `.await`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use spvirit_server::diag::{BandwidthCounters, ClientRegistry};
use spvirit_types::NtTimeStamp;

use super::now_ts;

/// A point-in-time set of rate rows for all 8 bandwidth tables + sample time.
#[derive(Clone, Default)]
pub struct RateSnapshot {
    pub ts: NtTimeStamp,
    pub ds_bypv_tx: Vec<(String, f64)>,
    pub ds_bypv_rx: Vec<(String, f64)>,
    pub us_bypv_tx: Vec<(String, f64)>,
    pub us_bypv_rx: Vec<(String, f64)>,
    pub us_byhost_tx: Vec<(String, f64)>,
    pub us_byhost_rx: Vec<(String, f64)>,
    // ds:byhost carries (account, client, rate):
    pub ds_byhost_tx: Vec<(String, String, f64)>,
    pub ds_byhost_rx: Vec<(String, String, f64)>,
}

/// `now.saturating_sub(prev) / dt`, guarded against a non-positive `dt`
/// (first tick has no meaningful elapsed time / a paused-clock test) and a
/// counter that went backwards (wrapped or was reset) — both map to a `0.0`
/// rate rather than a negative or infinite one.
pub fn compute_rate(now: u64, prev: u64, dt: f64) -> f64 {
    if dt <= 0.0 {
        return 0.0;
    }
    now.saturating_sub(prev) as f64 / dt
}

/// Converts a cumulative [`ByteMap`](spvirit_server::diag::ByteMap) snapshot
/// (`Vec<(String, u64)>`) into per-key rate rows against `prev`, using `dt`
/// seconds elapsed. A key present in `cur` but absent from `prev` is treated
/// as `prev = 0` — the intended first-sample behavior (the whole cumulative
/// count divided by `dt`).
///
/// Rows come out **sorted by key**. The source is a `HashMap`, whose
/// iteration order is arbitrary and jumps wholesale on a rehash; the status
/// ticker compares whole payloads to decide whether anything changed, so an
/// unstable row order manufactures "changes" out of nothing.
fn rate_rows(cur: &[(String, u64)], prev: &HashMap<String, u64>, dt: f64) -> Vec<(String, f64)> {
    let mut rows: Vec<(String, f64)> = cur
        .iter()
        .map(|(k, v)| {
            let p = prev.get(k).copied().unwrap_or(0);
            (k.clone(), compute_rate(*v, p, dt))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// Converts the `ClientRegistry::byhost` cumulative rows
/// (`Vec<(account, client_ip, u64)>`) into per-`(account, client)` rate rows
/// against `prev`, using `dt` seconds elapsed.
///
/// Rows come out **sorted by `(account, client)`**. This is the worst case of
/// the ordering problem: `ClientRegistry::byhost` allocates a fresh `HashMap`
/// on every call, so its iteration order is re-randomized every single tick
/// and the `ds:byhost:*` status PVs would otherwise emit a monitor frame per
/// second on a completely idle gateway.
/// Probes `prev` by borrow — the nested map shape exists for exactly this —
/// so the only allocation per row is the one the output row genuinely owns.
fn rate_rows_byhost(
    cur: &[(String, String, u64)],
    prev: &HashMap<String, HashMap<String, u64>>,
    dt: f64,
) -> Vec<(String, String, f64)> {
    let mut rows: Vec<(String, String, f64)> = cur
        .iter()
        .map(|(account, client, v)| {
            let p = prev
                .get(account.as_str())
                .and_then(|by_client| by_client.get(client.as_str()))
                .copied()
                .unwrap_or(0);
            (account.clone(), client.clone(), compute_rate(*v, p, dt))
        })
        .collect();
    rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    rows
}

/// Consumes a cumulative snapshot into the next tick's baseline.
///
/// By value, not by reference: the rows come straight from `ByteMap::snapshot`
/// and are dropped at the end of the tick, so moving their `String`s into the
/// map beats cloning every one of them a second time.
fn to_map(rows: Vec<(String, u64)>) -> HashMap<String, u64> {
    rows.into_iter().collect()
}

/// Consumes cumulative byhost rows into the next tick's nested baseline. By
/// value for the same reason as [`to_map`].
fn to_map_byhost(rows: Vec<(String, String, u64)>) -> HashMap<String, HashMap<String, u64>> {
    let mut out: HashMap<String, HashMap<String, u64>> = HashMap::new();
    for (account, client, bytes) in rows {
        out.entry(account).or_default().insert(client, bytes);
    }
    out
}

/// Cumulative counts from the previous tick, used as the delta baseline.
///
/// The byhost maps are nested (`account -> client -> bytes`) rather than keyed
/// by a `(String, String)` tuple purely so a lookup can borrow: a tuple key
/// cannot be probed with `(&str, &str)`, so every row used to allocate a whole
/// throwaway key just to index the map.
#[derive(Default)]
struct PrevState {
    ds_bypv_tx: HashMap<String, u64>,
    ds_bypv_rx: HashMap<String, u64>,
    us_bypv_tx: HashMap<String, u64>,
    us_bypv_rx: HashMap<String, u64>,
    us_byhost_tx: HashMap<String, u64>,
    us_byhost_rx: HashMap<String, u64>,
    ds_byhost_tx: HashMap<String, HashMap<String, u64>>,
    ds_byhost_rx: HashMap<String, HashMap<String, u64>>,
}

/// Drives the 1 Hz conversion of cumulative [`BandwidthCounters`] +
/// [`ClientRegistry::byhost`] byte counts into a [`RateSnapshot`] of B/s
/// rows, published into a shared `Arc<Mutex<RateSnapshot>>`.
pub struct BandwidthSampler {
    counters: Arc<BandwidthCounters>,
    registry: Arc<ClientRegistry>,
    snapshot: Arc<Mutex<RateSnapshot>>,
    prev: PrevState,
    last: Option<Instant>,
}

impl BandwidthSampler {
    /// Builds a sampler over the given counters/registry, publishing into
    /// `snapshot` (created by the caller in `runtime.rs` and shared with the
    /// status/metrics readers).
    pub fn new(
        counters: Arc<BandwidthCounters>,
        registry: Arc<ClientRegistry>,
        snapshot: Arc<Mutex<RateSnapshot>>,
    ) -> Self {
        Self {
            counters,
            registry,
            snapshot,
            prev: PrevState::default(),
            last: None,
        }
    }

    /// One 1 Hz sampling step: reads all 6 [`BandwidthCounters`] `ByteMap`s +
    /// both `ClientRegistry::byhost` directions, computes `dt` as the wall
    /// time since the previous tick (first tick: `dt <= 0.0`, so every rate
    /// is `0.0` via `compute_rate`'s guard — a safe, well-defined "no rate
    /// yet" first sample), computes per-key rates, publishes the resulting
    /// [`RateSnapshot`] into the shared mutex, and rolls `prev`/`last`
    /// forward.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let dt = match self.last {
            Some(last) => now.duration_since(last).as_secs_f64(),
            None => 0.0,
        };
        self.tick_with_dt(dt);
        self.last = Some(now);
    }

    /// The rate-computation core of [`Self::tick`], with `dt` supplied
    /// directly rather than measured from a wall clock — the testable seam
    /// that lets sampler math be exercised deterministically (no sleeping,
    /// no `tokio::time::advance` needed) while `tick()` remains the only
    /// entry point the runtime's 1 Hz loop calls.
    fn tick_with_dt(&mut self, dt: f64) {
        let ds_bypv_tx = self.counters.ds_bypv_tx.snapshot();
        let ds_bypv_rx = self.counters.ds_bypv_rx.snapshot();
        let us_bypv_tx = self.counters.us_bypv_tx.snapshot();
        let us_bypv_rx = self.counters.us_bypv_rx.snapshot();
        let us_byhost_tx = self.counters.us_byhost_tx.snapshot();
        let us_byhost_rx = self.counters.us_byhost_rx.snapshot();
        let ds_byhost_tx = self.registry.byhost(true);
        let ds_byhost_rx = self.registry.byhost(false);

        let snap = RateSnapshot {
            ts: now_ts(),
            ds_bypv_tx: rate_rows(&ds_bypv_tx, &self.prev.ds_bypv_tx, dt),
            ds_bypv_rx: rate_rows(&ds_bypv_rx, &self.prev.ds_bypv_rx, dt),
            us_bypv_tx: rate_rows(&us_bypv_tx, &self.prev.us_bypv_tx, dt),
            us_bypv_rx: rate_rows(&us_bypv_rx, &self.prev.us_bypv_rx, dt),
            us_byhost_tx: rate_rows(&us_byhost_tx, &self.prev.us_byhost_tx, dt),
            us_byhost_rx: rate_rows(&us_byhost_rx, &self.prev.us_byhost_rx, dt),
            ds_byhost_tx: rate_rows_byhost(&ds_byhost_tx, &self.prev.ds_byhost_tx, dt),
            ds_byhost_rx: rate_rows_byhost(&ds_byhost_rx, &self.prev.ds_byhost_rx, dt),
        };

        *self.snapshot.lock().unwrap() = snap;

        // Move the cumulative rows into the baseline — they are freshly
        // allocated by snapshot()/byhost() and dropped at the end of this
        // tick, so cloning them again here bought nothing.
        self.prev.ds_bypv_tx = to_map(ds_bypv_tx);
        self.prev.ds_bypv_rx = to_map(ds_bypv_rx);
        self.prev.us_bypv_tx = to_map(us_bypv_tx);
        self.prev.us_bypv_rx = to_map(us_bypv_rx);
        self.prev.us_byhost_tx = to_map(us_byhost_tx);
        self.prev.us_byhost_rx = to_map(us_byhost_rx);
        self.prev.ds_byhost_tx = to_map_byhost(ds_byhost_tx);
        self.prev.ds_byhost_rx = to_map_byhost(ds_byhost_rx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn rate_is_delta_over_dt() {
        // 2000 bytes over 2.0 s => 1000 B/s
        assert_eq!(compute_rate(5000, 3000, 2.0), 1000.0);
    }

    #[test]
    fn first_sample_and_wraparound_are_zero() {
        assert_eq!(compute_rate(100, 0, 1.0), 100.0); // prev 0 first sample
        assert_eq!(compute_rate(10, 999, 1.0), 0.0); // counter wrapped/reset
        assert_eq!(compute_rate(10, 5, 0.0), 0.0); // dt 0 guard
    }

    fn peer(port: u16) -> SocketAddr {
        format!("10.0.0.5:{port}").parse().unwrap()
    }

    /// `tick_with_dt` over a real `BandwidthCounters` + `ClientRegistry`:
    /// two ticks with a known `dt` produce the expected B/s rates for both a
    /// plain `ByteMap` counter (`ds_bypv_tx`) and the registry-derived
    /// `ds_byhost_tx`, and `ts` is populated (UNIX seconds, not zero).
    #[test]
    fn tick_with_dt_computes_expected_rates_and_stamps_ts() {
        let counters = Arc::new(BandwidthCounters::new());
        let registry = Arc::new(ClientRegistry::new());
        registry.connect(1, peer(1));
        registry.set_identity(1, Some("alice".to_string()), None);

        let snapshot = Arc::new(Mutex::new(RateSnapshot::default()));
        let mut sampler = BandwidthSampler::new(counters.clone(), registry.clone(), snapshot.clone());

        // First tick: 1000 cumulative bytes, dt = 1.0s -> prev absent (0),
        // so rate = 1000/1.0 = 1000.0 (first-sample behavior).
        counters.ds_bypv_tx.add("PV:A", 1000);
        registry.add_tx(1, 1000);
        sampler.tick_with_dt(1.0);
        {
            let s = snapshot.lock().unwrap();
            assert_eq!(s.ds_bypv_tx, vec![("PV:A".to_string(), 1000.0)]);
            assert_eq!(
                s.ds_byhost_tx,
                vec![("alice".to_string(), "10.0.0.5".to_string(), 1000.0)]
            );
            assert!(s.ts.seconds_past_epoch > 1_577_836_800, "ts must be stamped UNIX seconds");
        }

        // Second tick: +2000 more cumulative bytes over dt = 2.0s ->
        // (3000 - 1000) / 2.0 = 1000.0 B/s.
        counters.ds_bypv_tx.add("PV:A", 2000);
        registry.add_tx(1, 2000);
        sampler.tick_with_dt(2.0);
        {
            let s = snapshot.lock().unwrap();
            assert_eq!(s.ds_bypv_tx, vec![("PV:A".to_string(), 1000.0)]);
            assert_eq!(
                s.ds_byhost_tx,
                vec![("alice".to_string(), "10.0.0.5".to_string(), 1000.0)]
            );
        }
    }

    /// A `dt <= 0.0` tick (e.g. two ticks landing in the same instant) must
    /// zero every rate, not divide by zero or produce a stale value.
    #[test]
    fn tick_with_dt_zero_guards_all_rates_to_zero() {
        let counters = Arc::new(BandwidthCounters::new());
        let registry = Arc::new(ClientRegistry::new());
        counters.us_bypv_rx.add("PV:B", 500);

        let snapshot = Arc::new(Mutex::new(RateSnapshot::default()));
        let mut sampler = BandwidthSampler::new(counters, registry, snapshot.clone());
        sampler.tick_with_dt(0.0);

        let s = snapshot.lock().unwrap();
        assert_eq!(s.us_bypv_rx, vec![("PV:B".to_string(), 0.0)]);
    }

    /// Consecutive samples over *unchanged* counters must produce
    /// byte-identical rows.
    ///
    /// `ClientRegistry::byhost` builds a fresh `HashMap` per call, so it gets
    /// a fresh `RandomState` and a fresh iteration order every single tick.
    /// Unsorted, the status ticker's whole-payload change detector reads that
    /// reshuffle as new data and pushes a monitor frame for
    /// `ds:byhost:rx`/`ds:byhost:tx` every second on a completely idle
    /// gateway. The `ByteMap`-backed rows have the same defect one degree
    /// weaker (stable until the map rehashes, then a wholesale jump).
    #[test]
    fn repeated_ticks_over_unchanged_counters_produce_identical_rows() {
        let counters = Arc::new(BandwidthCounters::new());
        let registry = Arc::new(ClientRegistry::new());
        // Five distinct rows: with an unsorted map the chance that 20
        // consecutive samples all happen to iterate in the same order is
        // about (1/120)^20, so this cannot pass by luck.
        for (i, user) in ["alice", "bob", "carol", "dave", "erin"].iter().enumerate() {
            let id = i as u64 + 1;
            registry.connect(id, peer(40000 + i as u16));
            registry.set_identity(id, Some((*user).to_string()), None);
            registry.add_tx(id, 1000 * id);
            registry.add_rx(id, 500 * id);
            counters.ds_bypv_tx.add(&format!("PV:{user}"), 100 * id);
            counters.us_byhost_rx.add(&format!("ioc-{user}"), 7 * id);
        }

        let snapshot = Arc::new(Mutex::new(RateSnapshot::default()));
        let mut sampler = BandwidthSampler::new(counters, registry, snapshot.clone());

        // Two warm-up ticks: after the second, `prev` equals the current
        // cumulative counts, so every rate is a steady 0.0 and only the row
        // ORDER can still differ between samples.
        sampler.tick_with_dt(1.0);
        sampler.tick_with_dt(1.0);
        let first = snapshot.lock().unwrap().clone();

        for i in 0..20 {
            sampler.tick_with_dt(1.0);
            let s = snapshot.lock().unwrap();
            assert_eq!(s.ds_byhost_tx, first.ds_byhost_tx, "ds:byhost:tx row order changed on tick {i}");
            assert_eq!(s.ds_byhost_rx, first.ds_byhost_rx, "ds:byhost:rx row order changed on tick {i}");
            assert_eq!(s.ds_bypv_tx, first.ds_bypv_tx, "ds:bypv:tx row order changed on tick {i}");
            assert_eq!(s.us_byhost_rx, first.us_byhost_rx, "us:byhost:rx row order changed on tick {i}");
        }
    }

    /// Rate bookkeeping across ticks must survive the switch to a nested
    /// previous-state map: a second tick sees the first tick's cumulative
    /// counts as its baseline, per (account, client) and per key, including
    /// two clients under one account and one client under two accounts —
    /// the cases a nested map could plausibly get wrong.
    #[test]
    fn byhost_rates_are_per_account_and_client_across_ticks() {
        let counters = Arc::new(BandwidthCounters::new());
        let registry = Arc::new(ClientRegistry::new());
        // alice on two IPs, and .6 shared by alice and bob.
        registry.connect(1, peer(40001)); // 10.0.0.5
        registry.set_identity(1, Some("alice".to_string()), None);
        registry.connect(2, "10.0.0.6:40002".parse().unwrap());
        registry.set_identity(2, Some("alice".to_string()), None);
        registry.connect(3, "10.0.0.6:40003".parse().unwrap());
        registry.set_identity(3, Some("bob".to_string()), None);
        registry.add_tx(1, 100);
        registry.add_tx(2, 200);
        registry.add_tx(3, 300);

        let snapshot = Arc::new(Mutex::new(RateSnapshot::default()));
        let mut sampler = BandwidthSampler::new(counters, registry.clone(), snapshot.clone());

        sampler.tick_with_dt(1.0);
        assert_eq!(
            snapshot.lock().unwrap().ds_byhost_tx,
            vec![
                ("alice".to_string(), "10.0.0.5".to_string(), 100.0),
                ("alice".to_string(), "10.0.0.6".to_string(), 200.0),
                ("bob".to_string(), "10.0.0.6".to_string(), 300.0),
            ],
            "first tick: prev is empty, so the rate is the whole cumulative count"
        );

        // Only alice@10.0.0.6 moves.
        registry.add_tx(2, 50);
        sampler.tick_with_dt(2.0);
        assert_eq!(
            snapshot.lock().unwrap().ds_byhost_tx,
            vec![
                ("alice".to_string(), "10.0.0.5".to_string(), 0.0),
                ("alice".to_string(), "10.0.0.6".to_string(), 25.0),
                ("bob".to_string(), "10.0.0.6".to_string(), 0.0),
            ],
            "second tick: each row is deltaed against its own previous count"
        );
    }

    /// The ordering contract itself, stated directly: rows come out sorted by
    /// key, so two samplers fed the same data in any internal order agree.
    #[test]
    fn rate_rows_are_sorted_by_key() {
        let counters = Arc::new(BandwidthCounters::new());
        for name in ["PV:zulu", "PV:alpha", "PV:mike", "PV:bravo"] {
            counters.ds_bypv_tx.add(name, 10);
        }
        let registry = Arc::new(ClientRegistry::new());
        registry.connect(1, peer(40001));
        registry.set_identity(1, Some("zoe".to_string()), None);
        registry.add_tx(1, 1);
        registry.connect(2, peer(40002));
        registry.set_identity(2, Some("adam".to_string()), None);
        registry.add_tx(2, 1);

        let snapshot = Arc::new(Mutex::new(RateSnapshot::default()));
        let mut sampler = BandwidthSampler::new(counters, registry, snapshot.clone());
        sampler.tick_with_dt(1.0);

        let s = snapshot.lock().unwrap();
        let names: Vec<&str> = s.ds_bypv_tx.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["PV:alpha", "PV:bravo", "PV:mike", "PV:zulu"]);
        let accounts: Vec<&str> = s.ds_byhost_tx.iter().map(|(a, _, _)| a.as_str()).collect();
        assert_eq!(accounts, vec!["adam", "zoe"]);
    }
}
