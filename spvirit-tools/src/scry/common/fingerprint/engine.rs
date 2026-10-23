//! Windowed accumulation of source fingerprints (spec §4.2).
//!
//! The engine never reads the clock: `observe` and `harvest` both take time
//! from the caller. In production that time is the pcap capture timestamp,
//! which is the honest measure of when the observer saw the packet; in tests
//! it makes the whole pipeline deterministic.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::cadence::cadence_stats;
use super::circular::CircularPhase;
use super::lattice::{DEFAULT_EPS_ABS, DEFAULT_REL_TOL, estimate_lattice};
use super::running::Welford;
use super::types::{
    FingerprintCoverage, FingerprintKey, FingerprintStatus, Observation, SamplingRegime,
    SourceFingerprint,
};

const NANOS_PER_SEC_I128: i128 = 1_000_000_000;

/// Tunables for the fingerprint engine. Defaults are the spec's
/// recommendations (§4.2, §4.3).
#[derive(Debug, Clone, PartialEq)]
pub struct FingerprintConfig {
    /// Rolling window length in seconds. Spec §4.2 recommends 15 minutes.
    pub window_secs: u64,
    /// Minimum samples before a window may emit a fingerprint (spec §4.2).
    pub min_samples: u64,
    /// Reservoir capacity per channel per window (spec §4.3).
    pub reservoir_cap: usize,
    /// Maximum channels accumulated concurrently; the excess is deferred and
    /// counted in the coverage report, never dropped silently.
    pub max_channels: usize,
    /// Number of round-robin slots for channel admission. `1` disables
    /// rotation. With `N > 1` only about `1/N` of the already-fingerprinted
    /// channels are admitted per window, extending time-to-detection by `N`
    /// (spec §4.3).
    pub round_robin_slots: usize,
    pub eps_abs: f64,
    pub rel_tol: f64,
}

impl Default for FingerprintConfig {
    fn default() -> Self {
        Self {
            window_secs: 900,
            min_samples: 30,
            reservoir_cap: 200,
            max_channels: 5000,
            round_robin_slots: 1,
            eps_abs: DEFAULT_EPS_ABS,
            rel_tol: DEFAULT_REL_TOL,
        }
    }
}

/// Per-channel accumulator. Phase and arrival are O(1); values and intervals
/// are bounded reservoirs.
#[derive(Debug, Clone)]
struct ChannelAccumulator {
    phase: CircularPhase,
    arrival: Welford,
    values: Vec<f64>,
    intervals: Vec<f64>,
    /// Reservoir counters, so sampling can be reported honestly.
    values_seen: u64,
    intervals_seen: u64,
    samples: u64,
    has_nan_limits: bool,
    struct_id: Option<String>,
    /// Sticky marker for a mid-window identity change (D1).
    struct_id_changed: bool,
    last_stamp_unix_nanos: Option<i128>,
    window_start_unix_nanos: i128,
    /// Deterministic xorshift state, seeded per channel so that reservoir
    /// decisions are reproducible without an RNG dependency.
    rng: u64,
}

impl ChannelAccumulator {
    fn new(window_start_unix_nanos: i128, seed: u64) -> Self {
        Self {
            phase: CircularPhase::new(),
            arrival: Welford::new(),
            values: Vec::new(),
            intervals: Vec::new(),
            values_seen: 0,
            intervals_seen: 0,
            samples: 0,
            has_nan_limits: false,
            struct_id: None,
            struct_id_changed: false,
            last_stamp_unix_nanos: None,
            window_start_unix_nanos,
            // xorshift64* degenerates to all-zero from a zero state, so force
            // the low bit; the hash is 64-bit so this costs no useful entropy.
            rng: seed | 1,
        }
    }

    fn next_rand(&mut self) -> u64 {
        // xorshift64*, deterministic and dependency-free.
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Reservoir insertion (Algorithm R), so a high-rate channel cannot grow
    /// unbounded while still yielding an unbiased sample of the window.
    ///
    /// The `*_seen` counter is incremented on every call, sampled or not, so
    /// that the drop is visible in the emitted [`SamplingRegime`] rather than
    /// silent.
    fn reservoir_push(&mut self, buf_is_values: bool, x: f64, cap: usize) {
        let seen = if buf_is_values { &mut self.values_seen } else { &mut self.intervals_seen };
        *seen += 1;
        let n = *seen;
        let r = self.next_rand();
        let buf = if buf_is_values { &mut self.values } else { &mut self.intervals };
        if buf.len() < cap {
            buf.push(x);
        } else if cap > 0 {
            let j = (r % n) as usize;
            if j < cap {
                buf[j] = x;
            }
        }
    }
}

/// Accumulates fingerprints for every observed (channel, server) pair.
#[derive(Debug, Clone)]
pub struct FingerprintEngine {
    config: FingerprintConfig,
    channels: HashMap<FingerprintKey, ChannelAccumulator>,
    /// Distinct keys turned away at capacity since the last harvest. Held as
    /// a set rather than a counter so `deferred_channels` reports channels
    /// and not updates.
    ///
    /// The set is itself capped at `max_channels`: an unbounded record of
    /// everything that was rejected for being unbounded would defeat the cap
    /// it exists to report on. `deferred_total` below is the uncapped,
    /// never-cleared figure that survives both that cap and `harvest`.
    deferred: HashSet<FingerprintKey>,
    /// Monotonic count of updates turned away at capacity, incremented
    /// before any cap is consulted and never reset.
    ///
    /// `deferred` is per-window and lossy by construction; deferred channels
    /// emit no `FingerprintStatus` at all, so without a figure that outlives
    /// both the window boundary and the set's own cap, a caller that
    /// harvested a single channel would read `deferred_channels: 0` while
    /// traffic was still being dropped. That is exactly the silent cap this
    /// counter exists to prevent.
    deferred_total: u64,
    /// Set once `deferred` has refused an insert, so the shortfall in
    /// `deferred_channels` is reported rather than inferred.
    deferred_saturated: bool,
    /// Which rotation slot is currently admitting.
    slot: usize,
    /// Channels that have produced at least one fingerprint. Anything absent
    /// here is prioritised past the rotation (spec §4.3 step 3).
    emitted: HashSet<FingerprintKey>,
    /// Distinct channels turned away by the rotation since the last harvest,
    /// mirroring `deferred` and bounded the same way. Held separately so an
    /// operator can tell "not this slot's turn" (recoverable in at most
    /// `slots` windows) from "engine at capacity" (spec §6.3).
    rotated_out: HashSet<FingerprintKey>,
    /// Monotonic count of updates skipped by the rotation. Incremented before
    /// any cap is consulted and never reset, so rotation loss stays visible
    /// even after the per-window set is cleared or saturates.
    rotated_out_total: u64,
    /// Set once `rotated_out` has refused an insert.
    rotated_out_saturated: bool,
    /// Monotonic count of updates discarded by the caller before `observe`
    /// was reached — see `FingerprintCoverage::unprobeable_updates`. Held
    /// here so it travels with the rest of the coverage accounting and is
    /// never reset by a harvest.
    unprobeable_updates: u64,
}

impl FingerprintEngine {
    pub fn new(config: FingerprintConfig) -> Self {
        Self {
            config,
            channels: HashMap::new(),
            deferred: HashSet::new(),
            deferred_total: 0,
            deferred_saturated: false,
            slot: 0,
            emitted: HashSet::new(),
            rotated_out: HashSet::new(),
            rotated_out_total: 0,
            rotated_out_saturated: false,
            unprobeable_updates: 0,
        }
    }

    /// Record one monitor-data update that never became an `Observation`
    /// because the caller could not read a timestamped scalar out of it, or
    /// could not parse its server address.
    ///
    /// Spec §6.3: coverage is measured, not assumed. These updates belong to
    /// no channel the engine knows about, so counting them here is the only
    /// way the operator learns that part of the wire is structurally
    /// invisible to the fingerprinter.
    pub fn note_unprobeable_update(&mut self) {
        self.unprobeable_updates = self.unprobeable_updates.saturating_add(1);
    }

    /// Whether this channel is admitted in the current rotation slot.
    ///
    /// Channels that have never produced a fingerprint are always admitted,
    /// so rotation delays re-fingerprinting without ever starving a first
    /// fingerprint (spec §4.3 step 3).
    pub fn admits(&self, key: &FingerprintKey) -> bool {
        let slots = self.config.round_robin_slots.max(1);
        if slots == 1 || !self.emitted.contains(key) {
            return true;
        }
        (seed_for(key) % slots as u64) as usize == self.slot
    }

    /// Advance to the next rotation slot. Called once per harvest.
    pub fn advance_slot(&mut self) {
        let slots = self.config.round_robin_slots.max(1);
        self.slot = (self.slot + 1) % slots;
    }

    pub fn current_slot(&self) -> usize {
        self.slot
    }

    pub fn has_ever_emitted(&self, key: &FingerprintKey) -> bool {
        self.emitted.contains(key)
    }

    /// Test hook: pretend this channel has already been fingerprinted, so
    /// rotation behaviour can be exercised without running a full window.
    #[cfg(test)]
    pub(crate) fn mark_emitted_for_test(&mut self, key: &FingerprintKey) {
        self.emitted.insert(key.clone());
    }

    pub fn config(&self) -> &FingerprintConfig {
        &self.config
    }

    pub fn tracked_channels(&self) -> usize {
        self.channels.len()
    }

    /// Record one decoded monitor update.
    ///
    /// Channels beyond `max_channels` are deferred rather than admitted; the
    /// count surfaces in [`FingerprintEngine::coverage`] so the gap in
    /// coverage is visible instead of implicit (spec §4.3, §6.3).
    pub fn observe(&mut self, key: FingerprintKey, obs: Observation) {
        // Rotation is consulted before capacity, but never silently: a
        // channel this slot is not admitting must not consume the deferral
        // budget that reports genuine capacity loss, and must still be
        // counted, or coverage would report it as absent from the wire
        // (spec §6.3, no silent caps).
        if !self.admits(&key) {
            // Counted before any cap is consulted, so the true figure
            // survives the set's capacity limit and `harvest`'s reset.
            self.rotated_out_total += 1;
            if self.rotated_out.len() < self.config.max_channels {
                self.rotated_out.insert(key);
            } else {
                self.rotated_out_saturated = true;
            }
            return;
        }
        let cap = self.config.reservoir_cap;
        if !self.channels.contains_key(&key) {
            if self.channels.len() >= self.config.max_channels {
                // Counted before either cap is consulted, so the true figure
                // survives the set's capacity limit and `harvest`'s reset.
                self.deferred_total += 1;
                if self.deferred.len() < self.config.max_channels {
                    self.deferred.insert(key);
                } else {
                    self.deferred_saturated = true;
                }
                return;
            }
            let seed = seed_for(&key);
            self.channels
                .insert(key.clone(), ChannelAccumulator::new(obs.observed_unix_nanos, seed));
        }
        let acc = self.channels.get_mut(&key).expect("inserted above");

        acc.samples += 1;
        acc.phase.push(obs.stamp_nanos);
        acc.has_nan_limits |= obs.has_nan_limits;
        match (&acc.struct_id, &obs.struct_id) {
            // First-wins, and sticky against a later `None`: a decode that
            // failed to recover the id is not evidence that it changed.
            (None, Some(_)) => acc.struct_id = obs.struct_id.clone(),
            (Some(seen), Some(now)) if seen != now => acc.struct_id_changed = true,
            _ => {}
        }

        let stamp_unix = obs.stamp_seconds as i128 * NANOS_PER_SEC_I128 + obs.stamp_nanos as i128;
        let delta_ms = (obs.observed_unix_nanos - stamp_unix) as f64 / 1e6;
        acc.arrival.push(delta_ms);

        if let Some(prev) = acc.last_stamp_unix_nanos {
            let interval_s = (stamp_unix - prev) as f64 / 1e9;
            if interval_s > 0.0 {
                acc.reservoir_push(false, interval_s, cap);
            }
        }
        acc.last_stamp_unix_nanos = Some(stamp_unix);

        if let Some(v) = obs.value {
            acc.reservoir_push(true, v, cap);
        }
    }

    /// Close every window that has elapsed as of `now_unix_nanos` and emit one
    /// status per channel.
    ///
    /// Channels whose window has not elapsed are left accumulating. Channels
    /// that closed with too few samples emit `Insufficient` (spec §6.1).
    pub fn harvest(&mut self, now_unix_nanos: i128) -> Vec<FingerprintStatus> {
        let window_nanos = self.config.window_secs as i128 * NANOS_PER_SEC_I128;
        let due: Vec<FingerprintKey> = self
            .channels
            .iter()
            .filter(|(_, acc)| now_unix_nanos - acc.window_start_unix_nanos >= window_nanos)
            .map(|(k, _)| k.clone())
            .collect();

        let mut out = Vec::with_capacity(due.len());
        for key in due {
            let acc = self.channels.remove(&key).expect("key came from the map");
            let status = self.finish(&key, &acc, now_unix_nanos);
            if status.ready().is_some() {
                self.emitted.insert(key.clone());
            }
            out.push(status);
        }
        if !out.is_empty() {
            self.advance_slot();
        }
        // Deferral is a per-window property: closing windows frees slots, so
        // the previous window's turned-away channels must not keep inflating
        // the next window's coverage report. `deferred_total` is deliberately
        // left alone, so the record of what was missed outlives the reset.
        if !out.is_empty() {
            self.deferred.clear();
            self.deferred_saturated = false;
            self.rotated_out.clear();
            self.rotated_out_saturated = false;
        }
        // `HashMap` iteration order varies per process, and this vector feeds
        // export lines and TUI rows; sort so the output is a function of the
        // traffic alone.
        out.sort_by_key(|a| a.key());
        out
    }

    /// Snapshot every in-flight channel without closing its window. Used by
    /// the TUI, which must show progress before the first harvest.
    pub fn snapshot(&self, now_unix_nanos: i128) -> Vec<FingerprintStatus> {
        let mut out: Vec<FingerprintStatus> = self
            .channels
            .iter()
            .map(|(key, acc)| self.finish(key, acc, now_unix_nanos))
            .collect();
        out.sort_by_key(|a| a.key());
        out
    }

    /// Coverage report for this window (spec §6.3): measured, not assumed.
    pub fn coverage(&self, interface: &str) -> FingerprintCoverage {
        let sampled = self.channels.values().filter(|a| self.is_sampled(a)).count();
        FingerprintCoverage {
            interface: interface.to_string(),
            // Every channel that reached the engine this window, whichever of
            // the three fates it met. Rotated-out channels are counted here so
            // that enabling rotation does not make the population appear to
            // shrink. Updates that never reached the engine at all have no
            // channel to count and are reported as `unprobeable_updates`.
            tracked_channels: self.channels.len() + self.deferred.len() + self.rotated_out.len(),
            admitted_channels: self.channels.len(),
            deferred_channels: self.deferred.len(),
            deferred_total: self.deferred_total,
            deferred_saturated: self.deferred_saturated,
            sampled_channels: sampled,
            rotated_out_channels: self.rotated_out.len(),
            rotated_out_total: self.rotated_out_total,
            rotated_out_saturated: self.rotated_out_saturated,
            unprobeable_updates: self.unprobeable_updates,
            min_samples: self.config.min_samples,
        }
    }

    /// A digest of exactly which samples the reservoirs retained, and in
    /// which slots, for one in-flight channel.
    ///
    /// Test-only and `pub(crate)`, deliberately kept off the API surface
    /// downstream tasks consume. It exists because every *derived* field is
    /// insensitive to the retention decision on well-behaved input — a
    /// clean lattice yields the same `value_lsb` from any subset of its
    /// values — so comparing derived fields cannot detect a seed that has
    /// silently become time-dependent. This compares the decision itself.
    #[cfg(test)]
    pub(crate) fn retained_digest(&self, key: &FingerprintKey) -> Option<u64> {
        let acc = self.channels.get(key)?;
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut fold = |x: u64| {
            for b in x.to_be_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        };
        // Lengths first, so two buffers cannot alias by concatenation.
        fold(acc.values.len() as u64);
        fold(acc.intervals.len() as u64);
        for v in &acc.values {
            fold(v.to_bits());
        }
        for v in &acc.intervals {
            fold(v.to_bits());
        }
        Some(h)
    }

    /// True when either reservoir turned samples away this window.
    fn is_sampled(&self, acc: &ChannelAccumulator) -> bool {
        acc.values_seen as usize > self.config.reservoir_cap
            || acc.intervals_seen as usize > self.config.reservoir_cap
    }

    fn finish(
        &self,
        key: &FingerprintKey,
        acc: &ChannelAccumulator,
        now_unix_nanos: i128,
    ) -> FingerprintStatus {
        let window_start = unix_nanos_to_system_time(acc.window_start_unix_nanos);
        if acc.samples < self.config.min_samples {
            return FingerprintStatus::Insufficient {
                key: key.clone(),
                samples: acc.samples,
                min_samples: self.config.min_samples,
                window_start,
            };
        }

        let lattice = estimate_lattice(&acc.values, self.config.eps_abs, self.config.rel_tol);
        let cadence = cadence_stats(&acc.intervals);
        let sampled = self.is_sampled(acc);

        FingerprintStatus::Ready(Box::new(SourceFingerprint {
            channel: key.channel.clone(),
            server_addr: key.server_addr,
            // O1 is only meaningful once the circular accumulator holds a
            // sample; `min_samples` above already guarantees that, but the
            // estimators return 0.0 for an empty accumulator either way.
            phase_mean_ms: acc.phase.mean_ms(),
            phase_spread_us: acc.phase.spread_us(),
            phase_circular_var: acc.phase.circular_var(),
            // Absent lattice stays absent: `None` means "no evidence of
            // quantisation", not "unquantised" (spec §6.1).
            value_lsb: lattice.map(|l| l.lsb),
            lsb_confidence: lattice.map(|l| l.confidence).unwrap_or(0.0),
            cadence_modal_s: cadence.map(|c| c.modal_s).unwrap_or(0.0),
            cadence_p50_s: cadence.map(|c| c.p50_s).unwrap_or(0.0),
            cadence_p95_s: cadence.map(|c| c.p95_s).unwrap_or(0.0),
            // The mean is defined for any non-empty window, and an
            // accumulator only exists because an update created it, so no
            // gate is needed here.
            arrival_delta_mean_ms: acc.arrival.mean(),
            // The spread is not defined for a single sample. `Welford`
            // returns 0.0 there, which is indistinguishable from a genuinely
            // steady transport, so absence is carried as `None` instead of
            // letting the estimator's floor pass for a measurement.
            arrival_delta_var_ms: (acc.arrival.count() >= 2).then(|| acc.arrival.variance()),
            // What actually backed O2 and O3 after sampling — not the raw
            // update count, which `observed_count` carries.
            sample_count: acc.values.len().max(acc.intervals.len()) as u64,
            window_start,
            window_end: unix_nanos_to_system_time(now_unix_nanos),
            struct_id: acc.struct_id.clone().unwrap_or_default(),
            struct_id_changed: acc.struct_id_changed,
            has_nan_limits: acc.has_nan_limits,
            observed_count: acc.samples,
            sampling: match (sampled, self.config.round_robin_slots.max(1)) {
                (_, slots) if slots > 1 => {
                    SamplingRegime::RoundRobin { cap: self.config.reservoir_cap, slots }
                }
                (true, _) => SamplingRegime::Reservoir { cap: self.config.reservoir_cap },
                (false, _) => SamplingRegime::Full,
            },
        }))
    }
}

/// Deterministic per-channel seed: FNV-1a over the channel name and the
/// serving socket. Keyed on identity alone rather than on any timestamp, so
/// the same traffic always makes the same sampling decisions no matter when
/// it is replayed.
fn seed_for(key: &FingerprintKey) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    };
    mix(key.channel.as_bytes());
    mix(key.server_addr.to_string().as_bytes());
    h
}

/// Convert a Unix-epoch nanosecond count to a `SystemTime`, saturating at the
/// epoch for negative input rather than panicking.
pub fn unix_nanos_to_system_time(nanos: i128) -> SystemTime {
    if nanos <= 0 {
        return UNIX_EPOCH;
    }
    let secs = (nanos / NANOS_PER_SEC_I128) as u64;
    let sub = (nanos % NANOS_PER_SEC_I128) as u32;
    UNIX_EPOCH + Duration::new(secs, sub)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    const SEC: i128 = 1_000_000_000;

    fn addr(last: u8) -> SocketAddr {
        format!("10.0.0.{}:5075", last).parse().unwrap()
    }

    fn key(channel: &str, last: u8) -> FingerprintKey {
        FingerprintKey::new(channel, addr(last))
    }

    /// Feed `n` updates one second apart, phase-locked at `phase_nanos` with
    /// +/- `jitter_nanos`, values on a `lsb` lattice, arriving `delay_ms`
    /// after the stamp.
    ///
    /// The alternating jitter cancels exactly only for an even `n`, so tests
    /// that assert the phase mean to 1e-6 ms must feed an even count.
    fn feed(
        engine: &mut FingerprintEngine,
        key: &FingerprintKey,
        n: u64,
        phase_nanos: i64,
        jitter_nanos: i64,
        lsb: f64,
        delay_ms: f64,
    ) {
        feed_at(engine, key, n, phase_nanos, jitter_nanos, lsb, delay_ms, 0);
    }

    /// As `feed`, but shifting the whole stream `epoch_offset_s` seconds
    /// along the wall clock. Only the absolute times move; phase, cadence,
    /// values and arrival delta are all identical.
    #[allow(clippy::too_many_arguments)]
    fn feed_at(
        engine: &mut FingerprintEngine,
        key: &FingerprintKey,
        n: u64,
        phase_nanos: i64,
        jitter_nanos: i64,
        lsb: f64,
        delay_ms: f64,
        epoch_offset_s: i64,
    ) {
        for i in 0..n {
            let jitter = if i % 2 == 0 { jitter_nanos } else { -jitter_nanos };
            let stamp_seconds = 1_755_756_000 + epoch_offset_s + i as i64;
            let stamp_nanos = (phase_nanos + jitter) as u32;
            let stamp_unix = stamp_seconds as i128 * SEC + stamp_nanos as i128;
            engine.observe(
                key.clone(),
                Observation {
                    stamp_seconds,
                    stamp_nanos,
                    observed_unix_nanos: stamp_unix + (delay_ms * 1e6) as i128,
                    value: Some(13.1625 + i as f64 * lsb),
                    has_nan_limits: false,
                    struct_id: Some("epics:nt/NTScalar:1.0".to_string()),
                },
            );
        }
    }

    /// One bare observation, for tests that only care about admission.
    fn obs_at(stamp_seconds: i64, stamp_nanos: u32) -> Observation {
        Observation {
            stamp_seconds,
            stamp_nanos,
            observed_unix_nanos: stamp_seconds as i128 * SEC + stamp_nanos as i128,
            value: Some(1.0),
            has_nan_limits: false,
            struct_id: None,
        }
    }

    fn window_end(config: &FingerprintConfig, first_stamp_unix: i128) -> i128 {
        first_stamp_unix + config.window_secs as i128 * SEC + SEC
    }

    /// Spec §10: the engine must reproduce the R55IWATER reference numbers
    /// end to end, not just in the estimators.
    #[test]
    fn reproduces_the_r55iwater_reference_fingerprint() {
        let config = FingerprintConfig::default();
        let mut engine = FingerprintEngine::new(config.clone());
        let k = key("R55IWATER::AMBIENT:TEMPERATURE", 11);
        let first = 1_755_756_000i128 * SEC;
        feed(&mut engine, &k, 40, 262_016_000, 85_600, 0.0175, 4.0);

        let out = engine.harvest(window_end(&config, first));
        let fp = out
            .iter()
            .find_map(|s| s.ready())
            .expect("40 samples is above the 30-sample floor");

        assert_eq!(fp.channel, "R55IWATER::AMBIENT:TEMPERATURE");
        assert_eq!(fp.server_addr, addr(11));
        assert!((fp.phase_mean_ms - 262.016).abs() < 1e-6, "{}", fp.phase_mean_ms);
        assert!((fp.phase_spread_us - 85.6).abs() < 0.01, "{}", fp.phase_spread_us);
        assert!((fp.value_lsb.unwrap() - 0.0175).abs() < 1e-9);
        assert!((fp.cadence_modal_s - 1.0).abs() < 0.01);
        assert!((fp.arrival_delta_mean_ms - 4.0).abs() < 0.01);
        assert_eq!(fp.sample_count, 40);
        assert_eq!(fp.struct_id, "epics:nt/NTScalar:1.0");
        assert_eq!(fp.sampling, SamplingRegime::Full);
        assert!(!fp.has_nan_limits);
    }

    /// Spec §4.2: fewer than `min_samples` must not produce a fingerprint,
    /// and spec §6.1: it must produce an explicit "insufficient" status.
    #[test]
    fn reports_insufficient_rather_than_a_zeroed_fingerprint() {
        let config = FingerprintConfig::default();
        let mut engine = FingerprintEngine::new(config.clone());
        let k = key("SPARSE:CHANNEL", 12);
        let first = 1_755_756_000i128 * SEC;
        feed(&mut engine, &k, 5, 100_000_000, 1000, 0.1, 2.0);

        let out = engine.harvest(window_end(&config, first));
        assert_eq!(out.len(), 1);
        match &out[0] {
            FingerprintStatus::Insufficient { key: got, samples, .. } => {
                assert_eq!(*got, k);
                assert_eq!(*samples, 5);
            }
            other => panic!("expected Insufficient, got {:?}", other),
        }
    }

    /// Spec §3: the same channel name on two servers is two fingerprints.
    ///
    /// 36 samples rather than the brief's 35: `feed`'s alternating jitter
    /// only cancels for an even count, and an odd count leaves a residual of
    /// `jitter_nanos / n` (2.4 us at n=35, 5.6 us for the second source) in
    /// the circular mean — three orders of magnitude outside this test's own
    /// 1e-6 ms tolerance. Raising the count keeps the strict assertion
    /// intact instead of loosening it.
    #[test]
    fn keys_on_channel_and_server_together() {
        let config = FingerprintConfig::default();
        let mut engine = FingerprintEngine::new(config.clone());
        let first = 1_755_756_000i128 * SEC;
        let a = key("SHARED:PV", 21);
        let b = key("SHARED:PV", 22);
        feed(&mut engine, &a, 36, 262_016_000, 85_600, 0.0175, 4.0);
        feed(&mut engine, &b, 36, 695_973_000, 196_600, 0.01, 4.0);

        let out = engine.harvest(window_end(&config, first));
        let ready: Vec<_> = out.iter().filter_map(|s| s.ready()).collect();
        assert_eq!(ready.len(), 2);
        let mut phases: Vec<f64> = ready.iter().map(|f| f.phase_mean_ms).collect();
        phases.sort_by(|x, y| x.partial_cmp(y).unwrap());
        assert!((phases[0] - 262.016).abs() < 1e-6, "{}", phases[0]);
        assert!((phases[1] - 695.973).abs() < 1e-6, "{}", phases[1]);
    }

    /// Harvest resets the window: a second harvest with no new traffic must
    /// emit nothing rather than re-emitting the same fingerprint.
    #[test]
    fn harvest_closes_and_resets_the_window() {
        let config = FingerprintConfig::default();
        let mut engine = FingerprintEngine::new(config.clone());
        let k = key("ROLL:PV", 31);
        let first = 1_755_756_000i128 * SEC;
        feed(&mut engine, &k, 35, 262_016_000, 85_600, 0.0175, 4.0);

        let t = window_end(&config, first);
        assert_eq!(engine.harvest(t).len(), 1);
        assert!(engine.harvest(t + SEC).is_empty());
    }

    /// A window that has not elapsed yet must not be harvested early, even
    /// when it already holds enough samples.
    #[test]
    fn does_not_harvest_before_the_window_elapses() {
        let config = FingerprintConfig::default();
        let mut engine = FingerprintEngine::new(config.clone());
        let k = key("EARLY:PV", 32);
        let first = 1_755_756_000i128 * SEC;
        feed(&mut engine, &k, 35, 262_016_000, 85_600, 0.0175, 4.0);
        assert!(engine.harvest(first + 60 * SEC).is_empty());
    }

    /// Spec §2.4: the arrival delta is what separates a bridge hop from a
    /// device. A steady 250 ms transport delay must show up as the mean.
    #[test]
    fn measures_the_arrival_delta() {
        let config = FingerprintConfig::default();
        let mut engine = FingerprintEngine::new(config.clone());
        let k = key("BRIDGED:PV", 41);
        let first = 1_755_756_000i128 * SEC;
        feed(&mut engine, &k, 35, 262_016_000, 85_600, 0.0175, 250.0);
        let out = engine.harvest(window_end(&config, first));
        let fp = out.iter().find_map(|s| s.ready()).unwrap();
        assert!((fp.arrival_delta_mean_ms - 250.0).abs() < 0.01);
        assert!(fp.arrival_delta_var_ms.expect("35 samples define a spread") < 1.0);
    }

    /// Spec §3.1: the marker is bitset-sparse, so it must be sticky across
    /// the window rather than reflecting only the last update.
    #[test]
    fn nan_limit_marker_is_sticky_across_the_window() {
        let config = FingerprintConfig::default();
        let mut engine = FingerprintEngine::new(config.clone());
        let k = key("SYNTH:PV", 42);
        let first = 1_755_756_000i128 * SEC;
        for i in 0..35u64 {
            engine.observe(
                k.clone(),
                Observation {
                    stamp_seconds: 1_755_756_000 + i as i64,
                    stamp_nanos: 262_016_000,
                    observed_unix_nanos: (1_755_756_000 + i as i128) * SEC
                        + 262_016_000
                        + 4_000_000,
                    value: Some(1.0 + i as f64 * 0.0175),
                    has_nan_limits: i == 0,
                    struct_id: Some("epics:nt/NTScalar:1.0".to_string()),
                },
            );
        }
        let out = engine.harvest(window_end(&config, first));
        assert!(out.iter().find_map(|s| s.ready()).unwrap().has_nan_limits);
    }

    /// Spec §4.3 / no-silent-caps: once the reservoir starts turning samples
    /// away, the fingerprint must say so rather than presenting a truncated
    /// window as a complete one.
    #[test]
    fn reports_the_reservoir_regime_once_samples_are_dropped() {
        let config = FingerprintConfig { reservoir_cap: 50, ..FingerprintConfig::default() };
        let mut engine = FingerprintEngine::new(config.clone());
        let k = key("FAST:PV", 51);
        let first = 1_755_756_000i128 * SEC;
        feed(&mut engine, &k, 400, 262_016_000, 85_600, 0.0175, 4.0);

        let fp = engine
            .harvest(window_end(&config, first))
            .into_iter()
            .find_map(|s| s.ready().cloned())
            .expect("400 samples is well above the floor");
        assert_eq!(fp.sampling, SamplingRegime::Reservoir { cap: 50 });
        assert_eq!(fp.sampling.label(), "reservoir");
        // The true rate is never understated by the sampling...
        assert_eq!(fp.observed_count, 400);
        // ...and the evidence behind O2/O3 is never overstated by it either.
        assert_eq!(fp.sample_count, 50);
        // Coverage says the same thing from the engine's side.
        let cov = engine.coverage("eth0");
        assert_eq!(cov.deferred_total, 0);
        assert!(!cov.deferred_saturated);
    }

    /// Reservoir decisions are seeded from the key alone, so replaying the
    /// same traffic — at a different wall time — yields byte-identical
    /// fingerprints. A time- or rand-seeded PRNG would fail this.
    #[test]
    fn reservoir_sampling_is_reproducible_across_replays() {
        let config = FingerprintConfig { reservoir_cap: 50, ..FingerprintConfig::default() };
        let k = key("FAST:PV", 51);
        let first = 1_755_756_000i128 * SEC;

        // The second replay is shifted a full day later, so a `seed_for`
        // that mixed in a capture timestamp would draw a different reservoir
        // and this test would catch it. Feeding both runs identical stamps
        // would not.
        let day: i64 = 86_400;
        let run = |offset_s: i64| {
            let mut engine = FingerprintEngine::new(config.clone());
            feed_at(&mut engine, &k, 400, 262_016_000, 85_600, 0.0175, 4.0, offset_s);
            // Taken before the harvest, which consumes the accumulator.
            let digest = engine.retained_digest(&k).expect("channel is tracked");
            let fp = engine
                .harvest(window_end(&config, first + offset_s as i128 * SEC))
                .into_iter()
                .find_map(|s| s.ready().cloned())
                .expect("ready");
            (digest, fp)
        };
        let (digest_a, a) = run(0);
        let (digest_b, b) = run(day);

        // The load-bearing assertion. Every field below is derived, and on a
        // clean lattice with uniform intervals every derived field is
        // identical across *any* retained subset — so only the digest can
        // tell a key-seeded reservoir from a timestamp-seeded one.
        assert_eq!(
            digest_a, digest_b,
            "reservoir retained a different sample set for the same traffic replayed              a day later: the seed is time-dependent"
        );

        // Windows differ, so compare the time-independent fields.
        assert_ne!(a.window_start, b.window_start);
        assert_eq!(a.value_lsb, b.value_lsb, "reservoir contents diverged");
        assert_eq!(a.lsb_confidence, b.lsb_confidence);
        assert_eq!(a.cadence_modal_s, b.cadence_modal_s);
        assert_eq!(a.cadence_p50_s, b.cadence_p50_s);
        assert_eq!(a.cadence_p95_s, b.cadence_p95_s);
        assert_eq!(a.phase_mean_ms, b.phase_mean_ms);
        assert_eq!(a.arrival_delta_mean_ms, b.arrival_delta_mean_ms);
        assert_eq!(a.sample_count, b.sample_count);
        assert_eq!(a.sampling, b.sampling);
    }

    /// Spec §6.3: channels turned away at capacity are counted, not silently
    /// forgotten, and the count is of channels rather than of updates.
    #[test]
    fn coverage_counts_deferred_channels_at_capacity() {
        let config = FingerprintConfig { max_channels: 3, ..FingerprintConfig::default() };
        let mut engine = FingerprintEngine::new(config.clone());
        for i in 0..5u8 {
            // Two updates each, so a per-update counter would report 4
            // deferred rather than 2.
            feed(&mut engine, &key("CROWD:PV", 60 + i), 2, 262_016_000, 0, 0.0175, 4.0);
        }
        let cov = engine.coverage("eth0");
        assert_eq!(cov.interface, "eth0");
        assert_eq!(cov.admitted_channels, 3);
        assert_eq!(cov.deferred_channels, 2);
        assert_eq!(cov.tracked_channels, 5);
        assert_eq!(cov.sampled_channels, 0);
        assert_eq!(engine.tracked_channels(), 3);
        // Two channels x two updates each were turned away.
        assert_eq!(cov.deferred_total, 4);
        assert!(!cov.deferred_saturated);
    }

    /// The deferred set is itself bounded by `max_channels` — otherwise a
    /// flood of one-off channel names would defeat the very cap it reports
    /// on. The saturation is documented behaviour, so pin it with a test
    /// rather than leaving it to be discovered as a bug.
    #[test]
    fn the_deferred_count_saturates_at_the_channel_cap() {
        let config = FingerprintConfig { max_channels: 2, ..FingerprintConfig::default() };
        let mut engine = FingerprintEngine::new(config.clone());
        for i in 0..9u8 {
            feed(&mut engine, &key("FLOOD:PV", 60 + i), 1, 262_016_000, 0, 0.0175, 4.0);
        }
        let cov = engine.coverage("eth0");
        assert_eq!(cov.admitted_channels, 2);
        // 7 channels were turned away; the set saturates at max_channels...
        assert_eq!(cov.deferred_channels, 2);
        assert_eq!(cov.tracked_channels, 4);
        // ...and says so, rather than letting 2 pass for the whole truth.
        assert!(cov.deferred_saturated);
        // The monotonic total is uncapped: one update each from 7 channels.
        assert_eq!(cov.deferred_total, 7);
    }

    /// A channel with no scalar values still yields O1/O3/O4; only the
    /// lattice is absent, and it must be absent rather than fabricated.
    #[test]
    fn omits_the_lattice_when_no_scalar_values_were_seen() {
        let config = FingerprintConfig::default();
        let mut engine = FingerprintEngine::new(config.clone());
        let k = key("WAVEFORM:PV", 43);
        let first = 1_755_756_000i128 * SEC;
        for i in 0..35u64 {
            engine.observe(
                k.clone(),
                Observation {
                    stamp_seconds: 1_755_756_000 + i as i64,
                    stamp_nanos: 262_016_000,
                    observed_unix_nanos: (1_755_756_000 + i as i128) * SEC + 262_016_000,
                    value: None,
                    has_nan_limits: false,
                    struct_id: None,
                },
            );
        }
        let out = engine.harvest(window_end(&config, first));
        let fp = out.iter().find_map(|s| s.ready()).unwrap();
        assert!(fp.value_lsb.is_none());
        assert_eq!(fp.lsb_confidence, 0.0);
        assert_eq!(fp.struct_id, "");
    }

    /// Export lines, TUI rows and any `out[0]` consumer depend on the order
    /// of this vector, and `HashMap` iteration order varies per process.
    #[test]
    fn harvest_and_snapshot_emit_a_deterministic_order() {
        let config = FingerprintConfig::default();
        let mut engine = FingerprintEngine::new(config.clone());
        let first = 1_755_756_000i128 * SEC;
        // Inserted out of order, on purpose.
        for last in [44u8, 11, 33, 22] {
            feed(&mut engine, &key(&format!("ORDER:PV{}", last), last), 32, 262_016_000, 0, 0.0175, 4.0);
        }

        let snap = engine.snapshot(first);
        let snap_keys: Vec<FingerprintKey> = snap.iter().map(|s| s.key()).collect();
        let mut sorted = snap_keys.clone();
        sorted.sort();
        assert_eq!(snap_keys, sorted, "snapshot is unsorted");

        let out = engine.harvest(window_end(&config, first));
        let out_keys: Vec<FingerprintKey> = out.iter().map(|s| s.key()).collect();
        let mut sorted = out_keys.clone();
        sorted.sort();
        assert_eq!(out_keys, sorted, "harvest is unsorted");
        assert_eq!(out_keys.len(), 4);
    }

    /// `snapshot` is the TUI's read path: it must report in-flight progress
    /// without closing anything, and must still distinguish "still filling"
    /// from "computed and unremarkable" (spec §6.1).
    #[test]
    fn snapshot_reports_progress_without_closing_the_window() {
        let config = FingerprintConfig::default();
        let mut engine = FingerprintEngine::new(config.clone());
        let first = 1_755_756_000i128 * SEC;
        let full = key("SNAP:READY", 71);
        let thin = key("SNAP:PENDING", 72);
        feed(&mut engine, &full, 32, 262_016_000, 85_600, 0.0175, 4.0);
        feed(&mut engine, &thin, 4, 262_016_000, 85_600, 0.0175, 4.0);

        let snap = engine.snapshot(first + 60 * SEC);
        assert_eq!(snap.len(), 2);
        let ready = snap.iter().find_map(|s| s.ready()).expect("the 32-sample channel");
        assert_eq!(ready.key(), full);
        assert!((ready.phase_mean_ms - 262.016).abs() < 0.01);
        match snap.iter().find(|s| s.ready().is_none()).expect("the 4-sample channel") {
            FingerprintStatus::Insufficient { key: k, samples, min_samples, .. } => {
                assert_eq!(*k, thin);
                assert_eq!(*samples, 4);
                // Renderable as "pending 4/30" without the engine's config.
                assert_eq!(*min_samples, 30);
            }
            other => panic!("expected Insufficient, got {:?}", other),
        }

        // Nothing was closed: both channels are still accumulating.
        assert_eq!(engine.tracked_channels(), 2);
        assert_eq!(engine.harvest(window_end(&config, first)).len(), 2);
    }

    /// Spec D1: a mid-window change of `struct_id` is a source-change signal.
    /// The first id is retained, but the change itself must be reported.
    #[test]
    fn a_changed_struct_id_is_recorded_rather_than_discarded() {
        let config = FingerprintConfig::default();
        let mut engine = FingerprintEngine::new(config.clone());
        let first = 1_755_756_000i128 * SEC;
        let steady = key("STEADY:PV", 81);
        let swapped = key("SWAPPED:PV", 82);
        for i in 0..35u64 {
            let obs = |id: Option<&str>| Observation {
                stamp_seconds: 1_755_756_000 + i as i64,
                stamp_nanos: 262_016_000,
                observed_unix_nanos: (1_755_756_000 + i as i128) * SEC + 262_016_000,
                value: Some(1.0 + i as f64 * 0.0175),
                has_nan_limits: false,
                struct_id: id.map(str::to_string),
            };
            // A later `None` is a failed decode, not a change.
            engine.observe(
                steady.clone(),
                obs(if i % 3 == 0 { None } else { Some("epics:nt/NTScalar:1.0") }),
            );
            engine.observe(
                swapped.clone(),
                obs(Some(if i < 20 { "epics:nt/NTScalar:1.0" } else { "epics:nt/NTEnum:1.0" })),
            );
        }
        let out = engine.harvest(window_end(&config, first));
        let by_key = |k: &FingerprintKey| {
            out.iter().find(|s| s.key() == *k).and_then(|s| s.ready()).expect("ready").clone()
        };
        let a = by_key(&steady);
        assert_eq!(a.struct_id, "epics:nt/NTScalar:1.0");
        assert!(!a.struct_id_changed, "a missing id is not a changed id");

        let b = by_key(&swapped);
        assert_eq!(b.struct_id, "epics:nt/NTScalar:1.0", "first id is retained");
        assert!(b.struct_id_changed);
    }

    /// Spec §6.1 / no-silent-caps: deferred channels emit no status at all,
    /// so `harvest` closing one window must not erase the evidence that
    /// other channels are still being turned away.
    #[test]
    fn a_partial_harvest_does_not_erase_the_deferral_record() {
        let config = FingerprintConfig { max_channels: 1, ..FingerprintConfig::default() };
        let mut engine = FingerprintEngine::new(config.clone());
        let first = 1_755_756_000i128 * SEC;
        feed(&mut engine, &key("ADMITTED:PV", 91), 35, 262_016_000, 85_600, 0.0175, 4.0);
        feed(&mut engine, &key("TURNED:AWAY", 92), 12, 262_016_000, 85_600, 0.0175, 4.0);
        assert_eq!(engine.coverage("eth0").deferred_total, 12);

        assert_eq!(engine.harvest(window_end(&config, first)).len(), 1);
        let cov = engine.coverage("eth0");
        // The per-window set is reset with the window...
        assert_eq!(cov.deferred_channels, 0);
        // ...but the record of what was never measured survives it.
        assert_eq!(cov.deferred_total, 12);
    }


    /// Spec §4.3 step 2: with rotation off (the default) every channel is
    /// admitted, so the ordinary path is unaffected.
    #[test]
    fn admits_everything_when_rotation_is_disabled() {
        let mut engine = FingerprintEngine::new(FingerprintConfig::default());
        assert_eq!(engine.config().round_robin_slots, 1);
        let keys: Vec<FingerprintKey> = (0..50u8).map(|i| key(&format!("PV:{}", i), 51)).collect();
        // Mark them emitted first: a fresh key is admitted by the
        // never-fingerprinted branch alone, which would leave the
        // `slots == 1` disjunct -- the thing under test -- unexercised.
        for k in &keys {
            engine.mark_emitted_for_test(k);
        }
        for k in &keys {
            assert!(engine.has_ever_emitted(k));
            assert!(engine.admits(k));
        }
        // ...and nothing was rotated out, so coverage is untouched.
        for k in &keys {
            engine.observe(k.clone(), obs_at(1_755_756_000, 0));
        }
        let cov = engine.coverage("eth0");
        assert_eq!(cov.rotated_out_total, 0);
        assert_eq!(cov.rotated_out_channels, 0);
        assert_eq!(cov.admitted_channels, 50);
    }

    /// With four slots, a steady-state population splits roughly evenly
    /// across slots rather than all landing in one.
    #[test]
    fn rotation_splits_the_population_across_slots() {
        let config = FingerprintConfig { round_robin_slots: 4, ..Default::default() };
        let mut engine = FingerprintEngine::new(config.clone());
        let keys: Vec<FingerprintKey> =
            (0..400u32).map(|i| key(&format!("PV:{}", i), 52)).collect();

        // Mark every channel as already fingerprinted, so prioritisation of
        // never-seen channels does not mask the rotation.
        for k in &keys {
            engine.mark_emitted_for_test(k);
        }

        let mut per_slot = Vec::new();
        for _ in 0..4 {
            per_slot.push(keys.iter().filter(|k| engine.admits(k)).count());
            engine.advance_slot();
        }
        // Every channel is admitted in exactly one slot: nothing is starved
        // and nothing is double-counted.
        assert_eq!(per_slot.iter().sum::<usize>(), 400);
        for n in &per_slot {
            assert!(*n > 40, "slot admitted only {} of 400 channels", n);
        }
    }

    /// Spec §4.3 step 3: a channel with no fingerprint yet is admitted in
    /// every slot, so rotation delays re-fingerprinting but never starves a
    /// first fingerprint.
    #[test]
    fn never_fingerprinted_channels_bypass_the_rotation() {
        let config = FingerprintConfig { round_robin_slots: 8, ..Default::default() };
        let mut engine = FingerprintEngine::new(config);
        let k = key("BRAND:NEW:PV", 53);
        for _ in 0..8 {
            assert!(engine.admits(&k), "a never-seen channel must always be admitted");
            engine.advance_slot();
        }
        assert!(!engine.has_ever_emitted(&k));
    }

    /// Spec §4.3: the regime must reach the output, so that a consumer can
    /// compute detection latency instead of guessing.
    #[test]
    fn records_the_round_robin_regime_on_the_fingerprint() {
        let config = FingerprintConfig { round_robin_slots: 4, ..Default::default() };
        let mut engine = FingerprintEngine::new(config.clone());
        let k = key("ROTATED:PV", 54);
        let first = 1_755_756_000i128 * SEC;
        feed(&mut engine, &k, 35, 262_016_000, 85_600, 0.0175, 4.0);
        let out = engine.harvest(window_end(&config, first));
        let fp = out.iter().find_map(|s| s.ready()).unwrap();
        assert_eq!(fp.sampling, SamplingRegime::RoundRobin { cap: 200, slots: 4 });
        // Emitting is what puts the channel into the rotation from here on.
        assert!(engine.has_ever_emitted(&k));
        assert_eq!(engine.current_slot(), 1);
    }

    /// A channel that emitted is thereafter admitted only in its own slot,
    /// and its updates in the other slots are not accumulated at all.
    #[test]
    fn an_emitted_channel_is_only_admitted_in_its_own_slot() {
        let config = FingerprintConfig { round_robin_slots: 4, ..Default::default() };
        let mut engine = FingerprintEngine::new(config.clone());
        let k = key("ROTATED:PV", 54);
        let first = 1_755_756_000i128 * SEC;
        feed(&mut engine, &k, 35, 262_016_000, 85_600, 0.0175, 4.0);
        engine.harvest(window_end(&config, first));
        assert_eq!(engine.tracked_channels(), 0);

        let mut admitted_slots = 0;
        for _ in 0..4 {
            if engine.admits(&k) {
                admitted_slots += 1;
            } else {
                // Not admitted means genuinely not accumulated, not charged
                // to the capacity-deferral budget -- and still *visible*: the
                // rotation counters must move, or an operator could not tell
                // a channel sitting out a slot from one gone off the wire.
                let before = engine.coverage("eth0");
                feed(&mut engine, &k, 5, 262_016_000, 85_600, 0.0175, 4.0);
                let after = engine.coverage("eth0");
                assert_eq!(engine.tracked_channels(), 0);
                assert_eq!(after.deferred_total, before.deferred_total);
                assert_eq!(after.deferred_channels, 0);
                assert_eq!(after.rotated_out_total, before.rotated_out_total + 5);
                assert_eq!(after.rotated_out_channels, 1);
                assert_eq!(after.tracked_channels, 1);
                assert!(!after.rotated_out_saturated);
            }
            engine.advance_slot();
        }
        assert_eq!(admitted_slots, 1, "exactly one slot in four must admit it");
    }

    /// Spec 6.3 / no silent caps: a channel that sits out the current slot
    /// must still be counted in coverage, and counted *separately* from a
    /// channel dropped at capacity. The two are different failure modes with
    /// different remedies -- rotation loss self-heals within `slots` windows,
    /// capacity loss does not -- so one number cannot serve for both.
    /// Spec §6.3: an update the fingerprinter could not use is still an
    /// update it did not see. Monitor data whose payload carries no
    /// timestamped scalar (NTNDArray, NTTable, plain structures) never
    /// reaches `observe`, so without this counter it would appear in no
    /// coverage field at all and a partial view would read as a complete one.
    #[test]
    fn unprobeable_updates_are_counted_in_coverage() {
        let mut engine = FingerprintEngine::new(FingerprintConfig::default());
        assert_eq!(engine.coverage("eth0").unprobeable_updates, 0);
        for _ in 0..3 {
            engine.note_unprobeable_update();
        }
        assert_eq!(engine.coverage("eth0").unprobeable_updates, 3);
        // Monotonic like the other loss totals: a harvest must not erase it.
        engine.harvest(0);
        assert_eq!(engine.coverage("eth0").unprobeable_updates, 3);
    }

    #[test]
    fn rotated_out_channels_are_visible_in_coverage() {
        let config = FingerprintConfig { round_robin_slots: 4, ..Default::default() };
        let mut engine = FingerprintEngine::new(config.clone());
        let first = 1_755_756_000i128 * SEC;
        let keys: Vec<FingerprintKey> =
            (0..40u32).map(|i| key(&format!("ROT:{}", i), 57)).collect();

        // One full window: every channel is new, so all 40 are admitted and
        // emit regardless of the slot (spec 4.3 step 3).
        for k in &keys {
            feed(&mut engine, k, 35, 262_016_000, 85_600, 0.0175, 4.0);
        }
        let out = engine.harvest(window_end(&config, first));
        assert_eq!(out.iter().filter_map(|s| s.ready()).count(), 40);
        let cov = engine.coverage("eth0");
        assert_eq!(cov.rotated_out_total, 0, "nothing rotates out before anything has emitted");
        assert_eq!(cov.tracked_channels, 0);

        // Second window: all 40 have emitted, so only the current slot's
        // share is admitted and the rest must show up as rotated out.
        for k in &keys {
            feed_at(&mut engine, k, 35, 262_016_000, 85_600, 0.0175, 4.0, 1000);
        }
        let cov = engine.coverage("eth0");
        let admitted = cov.admitted_channels;
        assert!(admitted > 0 && admitted < 40, "expected a strict subset, got {}", admitted);
        assert_eq!(cov.rotated_out_channels, 40 - admitted);
        assert_eq!(cov.rotated_out_total as usize, (40 - admitted) * 35);
        assert!(!cov.rotated_out_saturated);
        // The population has not silently shrunk: everything on the wire is
        // still accounted for.
        assert_eq!(cov.tracked_channels, 40);
        // Rotation is not capacity loss and must not be reported as such.
        assert_eq!(cov.deferred_channels, 0);
        assert_eq!(cov.deferred_total, 0);
    }

    /// The rotated-out set is bounded like the deferred set, and says so
    /// rather than quietly understating the loss. `rotated_out_total` is the
    /// uncapped truth that survives both the cap and `harvest`.
    #[test]
    fn the_rotated_out_count_saturates_and_declares_it() {
        let config =
            FingerprintConfig { round_robin_slots: 4, max_channels: 2, ..Default::default() };
        let mut engine = FingerprintEngine::new(config);
        let keys: Vec<FingerprintKey> =
            (0..12u32).map(|i| key(&format!("SAT:{}", i), 58)).collect();
        for k in &keys {
            engine.mark_emitted_for_test(k);
        }
        let rotated = keys.iter().filter(|k| !engine.admits(k)).count();
        assert!(rotated > 2, "need more rotated-out channels than the cap to saturate");
        for k in &keys {
            engine.observe(k.clone(), obs_at(1_755_756_000, 0));
        }
        let cov = engine.coverage("eth0");
        assert_eq!(cov.rotated_out_channels, 2, "the set is capped at max_channels");
        assert!(cov.rotated_out_saturated);
        assert_eq!(cov.rotated_out_total as usize, rotated, "the total is uncapped");
    }

    /// Reservoir sampling still bounds memory when a channel is far above the
    /// per-window cap, and the regime says so.
    #[test]
    fn reservoir_bounds_memory_and_is_reported() {
        let config = FingerprintConfig { reservoir_cap: 50, ..Default::default() };
        let mut engine = FingerprintEngine::new(config.clone());
        let k = key("FAST:PV", 55);
        let first = 1_755_756_000i128 * SEC;
        feed(&mut engine, &k, 500, 262_016_000, 85_600, 0.0175, 4.0);
        let out = engine.harvest(window_end(&config, first));
        let fp = out.iter().find_map(|s| s.ready()).unwrap();
        // `sample_count` is the retained count and `observed_count` the raw
        // total, so "bounded memory" and "500 updates really happened" are
        // both legible rather than one masking the other.
        assert_eq!(fp.sample_count, 50);
        assert_eq!(fp.observed_count, 500);
        assert_eq!(fp.sampling, SamplingRegime::Reservoir { cap: 50 });
        // Phase is O(1) and unaffected by sampling.
        assert!((fp.phase_mean_ms - 262.016).abs() < 1e-6);
    }

    /// Channels beyond the capacity cap are counted, not silently dropped
    /// (spec §4.3 / §6.3).
    #[test]
    fn capacity_overflow_is_counted_in_coverage() {
        let config = FingerprintConfig { max_channels: 3, ..Default::default() };
        let mut engine = FingerprintEngine::new(config);
        for i in 0..10u32 {
            let k = key(&format!("MANY:{}", i), 56);
            feed(&mut engine, &k, 1, 100_000_000, 0, 0.1, 1.0);
        }
        let cov = engine.coverage("eth0");
        assert_eq!(cov.interface, "eth0");
        assert_eq!(cov.admitted_channels, 3);
        // The per-window deferred *set* is itself bounded by `max_channels`,
        // so it saturates at 3 here and says so; `deferred_total` carries the
        // full seven turned-away updates. Nothing is dropped silently.
        assert_eq!(cov.deferred_channels, 3);
        assert!(cov.deferred_saturated);
        assert_eq!(cov.deferred_total, 7);
        assert_eq!(cov.tracked_channels, 6);
    }
}
