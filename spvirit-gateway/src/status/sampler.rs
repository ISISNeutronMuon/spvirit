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
fn rate_rows(cur: &[(String, u64)], prev: &HashMap<String, u64>, dt: f64) -> Vec<(String, f64)> {
    cur.iter()
        .map(|(k, v)| {
            let p = prev.get(k).copied().unwrap_or(0);
            (k.clone(), compute_rate(*v, p, dt))
        })
        .collect()
}

/// Converts the `ClientRegistry::byhost` cumulative rows
/// (`Vec<(account, client_ip, u64)>`) into per-`(account, client)` rate rows
/// against `prev`, using `dt` seconds elapsed.
fn rate_rows_byhost(
    cur: &[(String, String, u64)],
    prev: &HashMap<(String, String), u64>,
    dt: f64,
) -> Vec<(String, String, f64)> {
    cur.iter()
        .map(|(account, client, v)| {
            let p = prev
                .get(&(account.clone(), client.clone()))
                .copied()
                .unwrap_or(0);
            (account.clone(), client.clone(), compute_rate(*v, p, dt))
        })
        .collect()
}

fn to_map(rows: &[(String, u64)]) -> HashMap<String, u64> {
    rows.iter().cloned().collect()
}

fn to_map_byhost(rows: &[(String, String, u64)]) -> HashMap<(String, String), u64> {
    rows.iter()
        .map(|(a, c, v)| ((a.clone(), c.clone()), *v))
        .collect()
}

/// Previous-tick cumulative state, kept per counter so [`BandwidthSampler`]
/// can compute a delta against the last observed value.
#[derive(Default)]
struct PrevState {
    ds_bypv_tx: HashMap<String, u64>,
    ds_bypv_rx: HashMap<String, u64>,
    us_bypv_tx: HashMap<String, u64>,
    us_bypv_rx: HashMap<String, u64>,
    us_byhost_tx: HashMap<String, u64>,
    us_byhost_rx: HashMap<String, u64>,
    ds_byhost_tx: HashMap<(String, String), u64>,
    ds_byhost_rx: HashMap<(String, String), u64>,
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

        self.prev.ds_bypv_tx = to_map(&ds_bypv_tx);
        self.prev.ds_bypv_rx = to_map(&ds_bypv_rx);
        self.prev.us_bypv_tx = to_map(&us_bypv_tx);
        self.prev.us_bypv_rx = to_map(&us_bypv_rx);
        self.prev.us_byhost_tx = to_map(&us_byhost_tx);
        self.prev.us_byhost_rx = to_map(&us_byhost_rx);
        self.prev.ds_byhost_tx = to_map_byhost(&ds_byhost_tx);
        self.prev.ds_byhost_rx = to_map_byhost(&ds_byhost_rx);
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
}
