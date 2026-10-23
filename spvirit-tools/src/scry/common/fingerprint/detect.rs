//! Detection rules D1–D4 of spec §5.
//!
//! The detector is stateful by necessity: D1 compares consecutive windows,
//! and D2's "previously identical" clause means an address that has always
//! served diverse fingerprints must stay quiet while a pair that has just
//! diverged must not.
//!
//! # What a detection may and may not claim
//!
//! Spec §6.2 states that a planned failover is indistinguishable from silent
//! re-sourcing on the wire, and calls it the main obstacle to D1 being useful
//! in production. Nothing here resolves that. Two consequences are load
//! bearing and are enforced by tests:
//!
//! * D1 is **off by default**, behind `d1_enabled` (`--fp-d1`), and
//!   `suppress_until` (`--fp-suppress-secs`) implements the maintenance-window
//!   option the spec lists. The other two options it lists — correlation with
//!   container restart events, and operator acknowledgement with fingerprint
//!   pinning — need inputs this tool does not have and remain open.
//! * No `detail` string states *why* something changed, and none names a
//!   producing process (spec §6.4: a fingerprint is a discriminator, not an
//!   identity). Every detail says what was observed and stops there.
//!
//! # What D3 does not implement
//!
//! Spec §5.3 names several inputs. Only one is implemented, and a reader
//! meeting a D3 event should know which:
//!
//! * `has_nan_limits` — implemented. It is the whole of D3 today.
//! * A declared `provenance` field — not observable on this wire, so nothing
//!   is compared against a declaration. Every D3 detail says so.
//! * "An inferred claim from naming convention" — deliberately **not**
//!   implemented. Inferring a source from a site's PV naming scheme is
//!   guesswork, and a wrong inference is precisely the identity claim §6.4
//!   forbids.
//! * `arrival_delta_var` "consistent with a message-bus hop" — not
//!   implemented. There is no calibrated threshold for what a bus hop looks
//!   like in this figure, and §2.4 puts the observer's own capture jitter
//!   inside it; asserting one without deployment evidence would be a
//!   fabricated boundary.
//!
//! # Why D2 compares pairs rather than clusters
//!
//! `PHASE_SCALE_MS` is 500 ms and `DEFAULT_MATCH_THRESHOLD` is 0.05, so for
//! two fingerprints that agree on everything else the phase component alone
//! admits `0.05 * 500 / 0.5 = 50 ms` of phase difference as "same source" —
//! about 250x the 0.197 ms within-channel spread measured in spec §10. Feed
//! that into the single-linkage clustering of [`super::cluster`] and a run of
//! channels 40 ms apart chains into one cluster whose diameter is unbounded:
//! twelve such channels span 440 ms, which is the §10 reference separation
//! between two unambiguously different sources.
//!
//! Reading D2 off cluster membership would therefore *mask* exactly the case
//! D2 exists to catch, because the chain has already merged the distinct
//! fingerprints into one cluster and there is no disagreement left to see.
//! D2 consults pairwise distances instead. Every pair on an address is tested
//! directly, so the effective cluster D2 reasons about has a hard diameter of
//! one threshold and cannot be widened by intermediates. That is also the
//! literal reading of spec §5.2 — "two or more clearly distinct
//! fingerprints", a statement about fingerprints, not about clusters.
//!
//! The cost is `O(k^2)` in the channels one address serves, so the channel
//! count per address is bounded by `d2_max_channels_per_server` and whatever
//! the bound skips is counted in [`DetectorCounts`] rather than dropped
//! quietly.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::time::SystemTime;

use super::circular::circ_diff_ms;
use super::similarity::{DEFAULT_MATCH_THRESHOLD, SimilarityWeights, fingerprint_distance_opt};
use super::types::{FingerprintKey, SourceFingerprint};

/// The Loki stream name detection events are published under (spec §7).
pub const DETECTION_STREAM: &str = "pv_fingerprint_event";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DetectionRule {
    D1SourceChange,
    D2ServerDisagreement,
    D3DeclaredContradiction,
    D4ClockOffsetOutlier,
}

impl DetectionRule {
    pub fn code(&self) -> &'static str {
        match self {
            Self::D1SourceChange => "D1",
            Self::D2ServerDisagreement => "D2",
            Self::D3DeclaredContradiction => "D3",
            Self::D4ClockOffsetOutlier => "D4",
        }
    }
}

impl fmt::Display for DetectionRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// One rule firing. Carries both fingerprints so the exported event is
/// self-contained (spec §7).
#[derive(Debug, Clone, PartialEq)]
pub struct DetectionEvent {
    pub rule: DetectionRule,
    pub channel: String,
    pub server_addr: SocketAddr,
    pub detail: String,
    pub prior: Option<SourceFingerprint>,
    pub current: SourceFingerprint,
    pub at: SystemTime,
}

/// Everything the detector declined to emit.
///
/// Monotonic over the life of the detector and never reset. A detector that
/// silently deduplicated, bounded or suppressed firings would be
/// indistinguishable from a quiet network, which is the failure mode spec
/// §6.1 exists to forbid; these counters are the difference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetectorCounts {
    /// D1 firings evaluated and then silenced by the maintenance window.
    pub d1_suppressed: u64,
    /// D2 pairs still divergent that had already been reported.
    pub d2_repeat_suppressed: u64,
    /// D2 pairs sharing no comparable observable, so neither banked as a
    /// match nor reported as a divergence.
    pub d2_incomparable_pairs: u64,
    /// Channel-windows that `d2_max_channels_per_server` left uncompared.
    /// A monotonic running total over the detector's life, not a level: a
    /// channel truncated in ten windows contributes ten.
    pub d2_channels_skipped: u64,
    /// Absent channels whose retained prior fingerprint D2 compared against
    /// anyway, because a harvest tick carries only the windows that closed
    /// on it (spec §6.1: not comparing must not look like agreeing).
    pub d2_channels_carried_over: u64,
    /// Banked pair records — matched or reported — discarded because a
    /// channel stopped appearing on its address.
    pub d2_pairs_evicted: u64,
    /// D4 batches below three fingerprints, where no statistic can call a
    /// member an outlier. See `d4_events` for the arithmetic.
    pub d4_population_too_small: u64,
    /// D4 batches whose median absolute deviation and mean absolute
    /// deviation were both zero, so the population has no spread at all.
    pub d4_no_spread: u64,
    /// D3 firings for a channel already reported.
    pub d3_repeat_suppressed: u64,
    /// Events discarded by `max_events_per_ingest`.
    pub events_dropped_by_cap: u64,
}

impl DetectorCounts {
    /// True when nothing at all has been withheld.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DetectorConfig {
    /// D1 is opt-in. See spec §6.2.
    pub d1_enabled: bool,
    /// Maintenance window: D1 stays silent up to this instant, while priors
    /// continue to advance.
    pub suppress_until: Option<SystemTime>,
    /// Relative tolerance for calling two lattices the same.
    pub lsb_rel_tol: f64,
    /// Sigma multiple for D1's arrival clause and, as a modified z-score,
    /// D4's population test.
    pub d4_sigma: f64,
    /// Floor on D1's phase threshold, per spec §5 D1's `max(..., 1 ms)`.
    pub min_phase_shift_ms: f64,
    /// Multiple of the observed phase spread that counts as a shift.
    pub phase_sigma: f64,
    /// Distance at or below which D2 calls two fingerprints one source.
    pub match_threshold: f64,
    /// Component weights D2 compares with.
    pub weights: SimilarityWeights,
    /// Upper bound on the channels compared pairwise per address per window.
    /// D2 is `O(k^2)`; the overflow is counted, not hidden.
    pub d2_max_channels_per_server: usize,
    /// Upper bound on events returned from one `ingest`. The overflow is
    /// counted in `DetectorCounts::events_dropped_by_cap`.
    pub max_events_per_ingest: usize,
    /// Windows a channel may be absent before its banked D2 pair state is
    /// evicted and its prior stops being carried into comparisons. Bounds
    /// the only otherwise unbounded structure in the detector.
    pub d2_state_ttl_windows: u64,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            d1_enabled: false,
            suppress_until: None,
            lsb_rel_tol: 1e-6,
            d4_sigma: 3.0,
            min_phase_shift_ms: 1.0,
            phase_sigma: 5.0,
            match_threshold: DEFAULT_MATCH_THRESHOLD,
            weights: SimilarityWeights::default(),
            d2_max_channels_per_server: 512,
            max_events_per_ingest: 256,
            d2_state_ttl_windows: 8,
        }
    }
}

#[derive(Debug, Default)]
struct ServerPairs {
    /// Pairs of channels on this address that have looked identical at least
    /// once. D2 fires only for pairs in this set (spec §5.2).
    matched: HashSet<(String, String)>,
    /// Pairs already reported, so a persistent divergence is logged once.
    reported: HashSet<(String, String)>,
    /// Window index in which each channel was last seen in a batch on this
    /// address. Drives both the carry-over of absent channels and the
    /// eviction of pair state for channels that have left the wire.
    last_seen: HashMap<String, u64>,
}

/// A deduplication record that must only be written once the event it
/// belongs to has actually survived the per-ingest cap.
///
/// Marking while building and truncating afterwards retires a detection that
/// was never logged: the channel is recorded as reported and no later window
/// will say otherwise.
#[derive(Debug, Clone)]
enum DedupMark {
    D3(FingerprintKey),
    D2(SocketAddr, (String, String)),
}

pub struct Detector {
    config: DetectorConfig,
    prior: HashMap<FingerprintKey, SourceFingerprint>,
    servers: HashMap<SocketAddr, ServerPairs>,
    d3_reported: HashSet<FingerprintKey>,
    counts: DetectorCounts,
    /// Monotonic ingest counter, used as the unit of "windows absent".
    window: u64,
}

impl Detector {
    pub fn new(config: DetectorConfig) -> Self {
        Self {
            config,
            prior: HashMap::new(),
            servers: HashMap::new(),
            d3_reported: HashSet::new(),
            counts: DetectorCounts::default(),
            window: 0,
        }
    }

    pub fn config(&self) -> &DetectorConfig {
        &self.config
    }

    /// Everything withheld so far. Never reset.
    pub fn counts(&self) -> &DetectorCounts {
        &self.counts
    }

    pub fn prior_for(&self, key: &FingerprintKey) -> Option<&SourceFingerprint> {
        self.prior.get(key)
    }

    pub fn suppress_until(&self) -> Option<SystemTime> {
        self.config.suppress_until
    }

    pub fn set_suppress_until(&mut self, until: Option<SystemTime>) {
        self.config.suppress_until = until;
    }

    /// Evaluate a batch of freshly harvested fingerprints.
    ///
    /// `now` is passed in rather than read from the clock so that replayed
    /// captures and tests are deterministic.
    pub fn ingest(&mut self, batch: &[SourceFingerprint], now: SystemTime) -> Vec<DetectionEvent> {
        self.window = self.window.saturating_add(1);
        // Events are carried with their pending dedup mark so that the mark
        // is written only for events that survive `max_events_per_ingest`.
        let mut pending: Vec<(DetectionEvent, Option<DedupMark>)> = Vec::new();
        let mut pending_d3: HashSet<FingerprintKey> = HashSet::new();
        let suppressed = matches!(self.config.suppress_until, Some(t) if now < t);

        for fp in batch {
            let key = fp.key();

            // D1. Evaluated only when the operator has enabled it — that is
            // an explicit choice, not a silent drop. Suppression *is* a
            // silent drop, so a firing lost to the maintenance window is
            // evaluated first and counted.
            if self.config.d1_enabled
                && let Some(prior) = self.prior.get(&key)
                && let Some(detail) = self.d1_detail(prior, fp)
            {
                if suppressed {
                    self.counts.d1_suppressed += 1;
                } else {
                    pending.push((
                        DetectionEvent {
                            rule: DetectionRule::D1SourceChange,
                            channel: fp.channel.clone(),
                            server_addr: fp.server_addr,
                            detail,
                            prior: Some(prior.clone()),
                            current: fp.clone(),
                            at: now,
                        },
                        None,
                    ));
                }
            }

            // D3: the only observable form of declared/observed contradiction
            // available on this wire today (spec §3.1). Reported once per
            // channel; the repeats are counted rather than forgotten.
            if fp.has_nan_limits {
                if self.d3_reported.contains(&key) || pending_d3.contains(&key) {
                    self.counts.d3_repeat_suppressed += 1;
                } else {
                    pending_d3.insert(key.clone());
                    pending.push((
                        DetectionEvent {
                            rule: DetectionRule::D3DeclaredContradiction,
                            channel: fp.channel.clone(),
                            server_addr: fp.server_addr,
                            detail: "synthesised metadata: valueAlarm/display limits are NaN \
                                     or all-zero, a shape a direct device reading does not \
                                     produce; no declared-provenance field is observable on \
                                     this wire, so nothing was compared against a declaration"
                                .to_string(),
                            prior: self.prior.get(&key).cloned(),
                            current: fp.clone(),
                            at: now,
                        },
                        Some(DedupMark::D3(key.clone())),
                    ));
                }
            }
        }

        pending.extend(self.d2_events(batch, now));
        pending.extend(self.d4_events(batch, now).into_iter().map(|e| (e, None)));

        // Ordered by rule then channel so the exported log is a function of
        // the batch alone and not of any internal hash order.
        pending
            .sort_by(|a, b| a.0.rule.cmp(&b.0.rule).then_with(|| a.0.channel.cmp(&b.0.channel)));
        if pending.len() > self.config.max_events_per_ingest {
            let dropped = pending.len() - self.config.max_events_per_ingest;
            self.counts.events_dropped_by_cap += dropped as u64;
            // Truncated *before* any dedup mark is written. An event the cap
            // discards was never logged, so it must stay eligible to fire in
            // a later window rather than being retired unseen.
            pending.truncate(self.config.max_events_per_ingest);
        }

        let mut events = Vec::with_capacity(pending.len());
        for (event, mark) in pending {
            match mark {
                Some(DedupMark::D3(key)) => {
                    self.d3_reported.insert(key);
                }
                Some(DedupMark::D2(addr, pair)) => {
                    self.servers.entry(addr).or_default().reported.insert(pair);
                }
                None => {}
            }
            events.push(event);
        }

        // Priors advance even while suppressed, so the first window after a
        // maintenance period compares against reality rather than a stale
        // fingerprint.
        for fp in batch {
            self.prior.insert(fp.key(), fp.clone());
        }
        self.prune_server_state();

        events
    }

    /// Which D1 clauses fired, if any (spec §5.1).
    ///
    /// Every clause that fired is reported, not just the first: an operator
    /// looking at a phase shift wants to know whether the lattice moved too.
    fn d1_detail(&self, prior: &SourceFingerprint, now: &SourceFingerprint) -> Option<String> {
        let mut clauses: Vec<String> = Vec::new();

        // Phase, circularly, and only when both windows were locked
        // (spec §4.1, §2.1). An unlocked window has no phase to compare, and
        // absence of evidence is not evidence of a change.
        if prior.phase_is_locked() && now.phase_is_locked() {
            let shift = circ_diff_ms(prior.phase_mean_ms, now.phase_mean_ms);
            let threshold = (self.config.phase_sigma * prior.phase_spread_us / 1000.0)
                .max(self.config.min_phase_shift_ms);
            if shift > threshold {
                clauses.push(format!(
                    "phase shifted {:.3} ms (threshold {:.3} ms): {:.3} -> {:.3}",
                    shift, threshold, prior.phase_mean_ms, now.phase_mean_ms
                ));
            }
        }

        if let (Some(a), Some(b)) = (prior.value_lsb, now.value_lsb) {
            let denom = a.abs().max(b.abs()).max(f64::MIN_POSITIVE);
            if (a - b).abs() / denom > self.config.lsb_rel_tol {
                clauses.push(format!("lsb changed: {} -> {}", a, b));
            }
        }

        // Spec §6.1: `arrival_delta_var_ms` is `None` when the prior window
        // held fewer than two updates. There is no sigma then, and reading
        // the absence as zero would make every following window an
        // infinite-sigma jump.
        if let Some(var) = prior.arrival_delta_var_ms {
            let sigma = var.max(0.0).sqrt();
            let jump = (now.arrival_delta_mean_ms - prior.arrival_delta_mean_ms).abs();
            if sigma > 0.0 && jump > self.config.d4_sigma * sigma {
                clauses.push(format!(
                    "arrival delta shifted {:.3} ms ({:.1} sigma): {:.3} -> {:.3}",
                    jump,
                    jump / sigma,
                    prior.arrival_delta_mean_ms,
                    now.arrival_delta_mean_ms
                ));
            }
        }

        // The record documents `struct_id_changed` as a D1 signal in its own
        // right, so it is carried here rather than swallowed.
        // Spec §6.1: `struct_id` is `""` when no MONITOR INIT was observed —
        // "not computed yet", not a measured value. An observer that starts
        // mid-stream, or misses a late INIT, would otherwise report the
        // moment it *learned* the id as a source change. Both sides must be
        // known before the comparison means anything.
        let both_ids_known = !prior.struct_id.is_empty() && !now.struct_id.is_empty();
        if both_ids_known && prior.struct_id != now.struct_id {
            clauses.push(format!(
                "struct_id changed between windows: {} -> {}",
                prior.struct_id, now.struct_id,
            ));
        } else if now.struct_id_changed && !prior.struct_id_changed {
            clauses.push("struct_id changed within the window".to_string());
        }

        if clauses.is_empty() {
            return None;
        }
        clauses.push(
            "observed change only: on the wire a planned failover and a silent re-source are \
             indistinguishable (spec 6.2)"
                .to_string(),
        );
        Some(clauses.join("; "))
    }

    /// Spec §5.2: one address, channels that were previously identical and
    /// are now clearly distinct.
    ///
    /// Pairwise, never by cluster membership — see the module documentation
    /// for the arithmetic.
    fn d2_events(
        &mut self,
        batch: &[SourceFingerprint],
        now: SystemTime,
    ) -> Vec<(DetectionEvent, Option<DedupMark>)> {
        let window = self.window;
        for fp in batch {
            self.servers
                .entry(fp.server_addr)
                .or_default()
                .last_seen
                .insert(fp.channel.clone(), window);
        }

        // BTreeMap, not HashMap: the per-server cap keeps a prefix of the
        // channels, and which prefix that is must not depend on hash order.
        let mut by_server: BTreeMap<SocketAddr, Vec<SourceFingerprint>> = BTreeMap::new();
        for fp in batch {
            by_server.entry(fp.server_addr).or_default().push(fp.clone());
        }

        // Harvest runs on a one-second tick and a batch holds only the
        // windows that closed on that tick, so two channels on one address
        // routinely never appear together. Comparing within the batch alone
        // would make "never compared" indistinguishable from "compared and
        // agreed", which is the §6.1 line. Each address is therefore topped
        // up with the retained prior fingerprint of every channel it served
        // recently and that is absent this window; the top-up is counted.
        let ttl = self.config.d2_state_ttl_windows;
        // Channel -> how many windows old the carried fingerprint is, per
        // address. Kept so the event can say which side is stale and by how
        // much, and so the counter can be taken after the cap.
        let mut carried_age: BTreeMap<SocketAddr, HashMap<String, u64>> = BTreeMap::new();
        for (addr, fps) in by_server.iter_mut() {
            let Some(state) = self.servers.get(addr) else {
                continue;
            };
            let ages = carried_age.entry(*addr).or_default();
            let mut extra: Vec<SourceFingerprint> = Vec::new();
            for (channel, seen) in &state.last_seen {
                if *seen == window || window.saturating_sub(*seen) > ttl {
                    continue;
                }
                if let Some(prior) = self.prior.get(&FingerprintKey::new(channel.clone(), *addr)) {
                    ages.insert(channel.clone(), window.saturating_sub(*seen));
                    extra.push(prior.clone());
                }
            }
            fps.append(&mut extra);
        }

        let weights = self.config.weights;
        let threshold = self.config.match_threshold;
        let cap = self.config.d2_max_channels_per_server;
        let mut events: Vec<(DetectionEvent, Option<DedupMark>)> = Vec::new();
        let mut incomparable = 0u64;
        let mut skipped = 0u64;
        let mut carried = 0u64;

        for (addr, mut fps) in by_server {
            // Sorted so the pair ordering, the reported channel and the
            // truncation are all deterministic.
            fps.sort_by(|a, b| a.channel.cmp(&b.channel));
            if fps.len() > cap {
                skipped += (fps.len() - cap) as u64;
                fps.truncate(cap);
            }
            // Counted after the truncation, not before: `carried_over` means
            // "compared against anyway", and a top-up the cap threw away was
            // never compared. `d2_channels_skipped` already records it, so
            // counting it here as well would put the same discard under two
            // different meanings.
            let ages = carried_age.remove(&addr).unwrap_or_default();
            carried += fps.iter().filter(|f| ages.contains_key(&f.channel)).count() as u64;
            let state = self.servers.entry(addr).or_default();
            for i in 0..fps.len() {
                for j in (i + 1)..fps.len() {
                    let pair = (fps[i].channel.clone(), fps[j].channel.clone());
                    // The `Option` form, deliberately: `None` means the two
                    // share no comparable observable, which is neither a
                    // match nor a difference and must be banked as neither.
                    let Some(distance) = fingerprint_distance_opt(&fps[i], &fps[j], &weights)
                    else {
                        incomparable += 1;
                        continue;
                    };
                    if distance <= threshold {
                        state.matched.insert(pair.clone());
                        state.reported.remove(&pair);
                        continue;
                    }
                    if !state.matched.contains(&pair) {
                        continue;
                    }
                    if state.reported.contains(&pair) {
                        self.counts.d2_repeat_suppressed += 1;
                        continue;
                    }
                    let age_i = ages.get(&fps[i].channel).copied();
                    let age_j = ages.get(&fps[j].channel).copied();
                    // A carried fingerprint is up to `d2_state_ttl_windows`
                    // old. It must never occupy the `current` position of an
                    // event stamped `at: now`, so when exactly one side was
                    // carried the fresh side becomes `current` and names the
                    // event. The `pair` used for banking is unaffected.
                    let (cur, was) =
                        if age_i.is_none() && age_j.is_some() { (i, j) } else { (j, i) };
                    let mut detail = format!(
                        "{} and {} matched within {:.3} in an earlier window and now differ by \
                         {:.3}; this address is presenting fingerprints that no longer agree",
                        pair.0, pair.1, threshold, distance
                    );
                    // Spec §6.1: "these differ" and "these differ, one side
                    // measured up to two hours ago" are not the same claim.
                    for (idx, age) in [(i, age_i), (j, age_j)] {
                        if let Some(age) = age {
                            detail.push_str(&format!(
                                "; {} was not harvested this window and was compared from its \
                                 retained fingerprint, last observed {} window(s) ago, so 'now' \
                                 does not apply to it",
                                fps[idx].channel, age
                            ));
                        }
                    }
                    events.push((
                        DetectionEvent {
                            rule: DetectionRule::D2ServerDisagreement,
                            channel: fps[cur].channel.clone(),
                            server_addr: addr,
                            detail,
                            prior: Some(fps[was].clone()),
                            current: fps[cur].clone(),
                            at: now,
                        },
                        Some(DedupMark::D2(addr, pair)),
                    ));
                }
            }
        }

        self.counts.d2_incomparable_pairs += incomparable;
        self.counts.d2_channels_skipped += skipped;
        self.counts.d2_channels_carried_over += carried;
        events.sort_by(|a, b| a.0.channel.cmp(&b.0.channel));
        events
    }

    /// Drop banked pair state for channels that have stopped appearing.
    ///
    /// `ServerPairs::matched` is `O(k^2)` in the channels an address serves
    /// and would otherwise grow for the process lifetime. What it drops is
    /// counted, like everything else the detector discards.
    fn prune_server_state(&mut self) {
        let ttl = self.config.d2_state_ttl_windows;
        let window = self.window;
        let mut evicted = 0u64;
        self.servers.retain(|_, state| {
            let stale: HashSet<String> = state
                .last_seen
                .iter()
                .filter(|(_, seen)| window.saturating_sub(**seen) > ttl)
                .map(|(channel, _)| channel.clone())
                .collect();
            if !stale.is_empty() {
                let before = state.matched.len() + state.reported.len();
                state.last_seen.retain(|channel, _| !stale.contains(channel));
                state.matched.retain(|(a, b)| !stale.contains(a) && !stale.contains(b));
                state.reported.retain(|(a, b)| !stale.contains(a) && !stale.contains(b));
                evicted += (before - (state.matched.len() + state.reported.len())) as u64;
            }
            !state.last_seen.is_empty()
        });
        self.counts.d2_pairs_evicted += evicted;
    }

    /// Spec §5.4: arrival delta far from the population.
    ///
    /// The statistic is a modified z-score — median and median absolute
    /// deviation — not a standard score, because a standard score cannot
    /// fire at all on the batch sizes this detector actually sees.
    ///
    /// With the *population* standard deviation the largest attainable |z|
    /// for `n` points is `(n-1)/sqrt(n)`: 2.846 at n = 10, 3.015 at n = 11.
    /// With `d4_sigma = 3.0` that means no batch smaller than eleven could
    /// ever fire. The *sample* standard deviation is no better — its ceiling
    /// is `sqrt(n-1)`, exactly 3.0 at n = 10, so it too first clears the
    /// threshold at n = 11. Harvest runs on a one-second tick and a batch
    /// holds only the windows that closed on it, so batches of two or three
    /// are the common case: a channel 5000 ms adrift was being reported
    /// nowhere at all.
    ///
    /// A modified z-score has no such ceiling, because one outlier does not
    /// inflate the median absolute deviation: `0.6745 * |x - median| / MAD`
    /// is unbounded and fires at n = 3. When more than half the batch shares
    /// one value the MAD collapses to zero, and the documented fallback
    /// (Iglewicz & Hoaglin) rescales the *mean* absolute deviation by
    /// 1.253314 instead; that form's ceiling is `n / 1.253314`, which clears
    /// 3.0 at n = 4. So the real floor is three points, or four in the
    /// degenerate all-but-one-identical case, against eleven before.
    ///
    /// Both early returns are counted: "D4 evaluated and found nothing" and
    /// "D4 could not evaluate" must not be the same output (spec §6.1).
    fn d4_events(&mut self, batch: &[SourceFingerprint], now: SystemTime) -> Vec<DetectionEvent> {
        if batch.len() < 3 {
            self.counts.d4_population_too_small += 1;
            return Vec::new();
        }
        let mut values: Vec<f64> = batch.iter().map(|f| f.arrival_delta_mean_ms).collect();
        let median = median_of(&mut values);
        let mut deviations: Vec<f64> =
            batch.iter().map(|f| (f.arrival_delta_mean_ms - median).abs()).collect();
        let mad = median_of(&mut deviations);
        let (scale, spread) = if mad > 0.0 {
            (0.6745, mad)
        } else {
            (1.0 / 1.253314, deviations.iter().sum::<f64>() / batch.len() as f64)
        };
        // A population with no spread at all has no outliers, and dividing
        // by it would make every member infinitely deviant.
        if spread <= 0.0 || !spread.is_finite() {
            self.counts.d4_no_spread += 1;
            return Vec::new();
        }

        let mut events: Vec<DetectionEvent> = Vec::new();
        for fp in batch {
            let z = scale * (fp.arrival_delta_mean_ms - median).abs() / spread;
            if z <= self.config.d4_sigma {
                continue;
            }
            events.push(DetectionEvent {
                rule: DetectionRule::D4ClockOffsetOutlier,
                channel: fp.channel.clone(),
                server_addr: fp.server_addr,
                detail: format!(
                    "arrival delta {:.3} ms is {:.1} on a modified z-score against the batch \
                     median {:.3} ms (spread {:.3} ms over {} fingerprints); the observer's own \
                     clock and capture jitter are in this figure, so read it as a difference \
                     between channels (spec 2.4)",
                    fp.arrival_delta_mean_ms,
                    z,
                    median,
                    spread,
                    batch.len()
                ),
                prior: self.prior.get(&fp.key()).cloned(),
                current: fp.clone(),
                at: now,
            });
        }
        events.sort_by(|a, b| a.channel.cmp(&b.channel));
        events
    }
}

/// Seconds since the Unix epoch, for a window bound on an exported event.
///
/// A time before the epoch — which a replayed capture can produce — reads as
/// 0 rather than panicking.
fn unix_secs(t: SystemTime) -> f64 {
    t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Median of a slice, sorting it in place.
///
/// NaN compares as equal rather than panicking: a NaN arrival delta is a
/// broken measurement, not a reason to take the capture loop down.
fn median_of(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    if n % 2 == 1 { values[n / 2] } else { (values[n / 2 - 1] + values[n / 2]) / 2.0 }
}

/// Strip characters that would break the `key=value` / `detail="..."` shape
/// of a log line. PV names really do contain quotes and newlines.
fn sanitise(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '"' => '\'',
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect()
}

/// Render a detection event as a Loki entry: `(stream, line, labels)`.
///
/// `stream` is the stream name; `IfStats::to_loki_stream` maps it onto the
/// `request_type` metadata key, which is what `LokiLogger` promotes to a
/// Loki stream label, so detection events land in their own stream instead of
/// mixing into `pv_events`.
///
/// Both fingerprints are inlined so the event can be read on its own, per
/// spec §7, and both carry their `window_start`/`window_end`. D2 may compare
/// a freshly harvested window against a retained prior up to
/// `d2_state_ttl_windows` old, so without the bounds an operator could not
/// tell how old either side of the comparison is.
pub fn detection_loki_entry(ev: &DetectionEvent) -> (String, String, HashMap<String, String>) {
    let mut labels = HashMap::new();
    labels.insert("rule".to_string(), ev.rule.code().to_string());
    labels.insert("channel".to_string(), ev.channel.clone());
    labels.insert("server".to_string(), ev.server_addr.to_string());

    let struct_id =
        if ev.current.struct_id.is_empty() { "unknown" } else { ev.current.struct_id.as_str() };

    let mut line = format!(
        "rule={} channel={} server={} detail=\"{}\" \
         phase_mean_ms={} phase_spread_us={} phase_circular_var={} \
         value_lsb={} lsb_confidence={} cadence_modal_s={} cadence_p50_s={} cadence_p95_s={} \
         arrival_delta_mean_ms={} arrival_delta_var_ms={} \
         sample_count={} observed_count={} struct_id={} struct_id_changed={} \
         has_nan_limits={} sampling={} window_start={:.3} window_end={:.3}",
        ev.rule.code(),
        sanitise(&ev.channel),
        ev.server_addr,
        sanitise(&ev.detail),
        ev.current.phase_mean_ms,
        ev.current.phase_spread_us,
        ev.current.phase_circular_var,
        ev.current.value_lsb.map(|v| v.to_string()).unwrap_or_else(|| "none".to_string()),
        ev.current.lsb_confidence,
        ev.current.cadence_modal_s,
        ev.current.cadence_p50_s,
        ev.current.cadence_p95_s,
        ev.current.arrival_delta_mean_ms,
        ev.current
            .arrival_delta_var_ms
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        ev.current.sample_count,
        ev.current.observed_count,
        sanitise(struct_id),
        ev.current.struct_id_changed,
        ev.current.has_nan_limits,
        ev.current.sampling.label(),
        unix_secs(ev.current.window_start),
        unix_secs(ev.current.window_end),
    );

    if let Some(p) = &ev.prior {
        line.push_str(&format!(
            " prior_phase_mean_ms={} prior_phase_spread_us={} prior_phase_circular_var={} \
             prior_value_lsb={} prior_cadence_modal_s={} prior_arrival_delta_mean_ms={} \
             prior_arrival_delta_var_ms={} prior_sample_count={} prior_struct_id={} \
             prior_window_start={:.3} prior_window_end={:.3}",
            p.phase_mean_ms,
            p.phase_spread_us,
            p.phase_circular_var,
            p.value_lsb.map(|v| v.to_string()).unwrap_or_else(|| "none".to_string()),
            p.cadence_modal_s,
            p.arrival_delta_mean_ms,
            p.arrival_delta_var_ms.map(|v| v.to_string()).unwrap_or_else(|| "none".to_string()),
            p.sample_count,
            sanitise(if p.struct_id.is_empty() { "unknown" } else { &p.struct_id }),
            unix_secs(p.window_start),
            unix_secs(p.window_end),
        ));
    } else {
        // Spec §6.1: no prior is a different statement from a prior that
        // happened to match, and the line must not let the two read alike.
        line.push_str(" prior=none");
    }

    (DETECTION_STREAM.to_string(), line, labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scry::common::fingerprint::types::{SamplingRegime, SourceFingerprint};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn fp(channel: &str, server: &str, phase_ms: f64, arrival: f64) -> SourceFingerprint {
        SourceFingerprint {
            channel: channel.to_string(),
            server_addr: server.parse().unwrap(),
            phase_mean_ms: phase_ms,
            phase_spread_us: 85.6,
            phase_circular_var: 3e-7,
            value_lsb: Some(0.0175),
            lsb_confidence: 1.0,
            cadence_modal_s: 1.0,
            cadence_p50_s: 1.0,
            cadence_p95_s: 25.0,
            arrival_delta_mean_ms: arrival,
            arrival_delta_var_ms: Some(0.25),
            sample_count: 40,
            window_start: UNIX_EPOCH,
            window_end: UNIX_EPOCH + Duration::from_secs(900),
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            struct_id_changed: false,
            has_nan_limits: false,
            observed_count: 40,
            sampling: SamplingRegime::Full,
        }
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn d1_on() -> DetectorConfig {
        DetectorConfig { d1_enabled: true, ..Default::default() }
    }

    /// Spec §5 D1: a phase shift far beyond the observed spread is a
    /// discontinuity in the source's timing.
    #[test]
    fn d1_fires_on_a_phase_discontinuity() {
        let mut det = Detector::new(d1_on());
        assert!(det.ingest(&[fp("PV:A", "10.0.0.11:5075", 262.016, 4.0)], at(1000)).is_empty());
        let events = det.ingest(&[fp("PV:A", "10.0.0.11:5075", 695.973, 4.0)], at(1900));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].rule, DetectionRule::D1SourceChange);
        assert!(events[0].prior.is_some());
        assert!(events[0].detail.contains("phase"));
    }

    /// Spec §4.1: 0.999 s and 0.001 s differ by 2 ms. A linear comparison
    /// would fire D1 on a source that did not move.
    #[test]
    fn d1_compares_phase_circularly() {
        let mut det = Detector::new(d1_on());
        det.ingest(&[fp("PV:WRAP", "10.0.0.11:5075", 999.9, 4.0)], at(1000));
        let events = det.ingest(&[fp("PV:WRAP", "10.0.0.11:5075", 0.1, 4.0)], at(1900));
        assert!(events.is_empty(), "circular wrap fired: {:?}", events);
    }

    /// Spec §2.1: a window with no phase lock has no phase to compare.
    #[test]
    fn d1_ignores_phase_when_either_window_is_unlocked() {
        let mut det = Detector::new(d1_on());
        let mut a = fp("PV:EVT", "10.0.0.11:5075", 100.0, 4.0);
        a.phase_circular_var = 0.9;
        let mut b = fp("PV:EVT", "10.0.0.11:5075", 800.0, 4.0);
        b.phase_circular_var = 0.9;
        det.ingest(&[a], at(1000));
        assert!(det.ingest(&[b], at(1900)).is_empty());
    }

    /// Spec §5 D1: the lattice is a hardware property, so a change of LSB is
    /// a change of digitiser.
    #[test]
    fn d1_fires_on_an_lsb_change() {
        let mut det = Detector::new(d1_on());
        det.ingest(&[fp("PV:L", "10.0.0.11:5075", 262.016, 4.0)], at(1000));
        let mut next = fp("PV:L", "10.0.0.11:5075", 262.016, 4.0);
        next.value_lsb = Some(0.01);
        let events = det.ingest(&[next], at(1900));
        assert_eq!(events.len(), 1);
        assert!(events[0].detail.contains("lsb"));
    }

    /// Spec §5 D1: a jump in arrival delta beyond 3 sigma of the prior
    /// variance is a topology change.
    #[test]
    fn d1_fires_on_an_arrival_delta_jump() {
        let mut det = Detector::new(d1_on());
        det.ingest(&[fp("PV:B", "10.0.0.11:5075", 262.016, 4.0)], at(1000));
        let events = det.ingest(&[fp("PV:B", "10.0.0.11:5075", 262.016, 900.0)], at(1900));
        assert_eq!(events.len(), 1);
        assert!(events[0].detail.contains("arrival"));
    }

    /// `arrival_delta_var_ms` is `None` when the prior window held fewer than
    /// two updates. There is then no sigma to test against, and spec §6.1
    /// forbids reading that absence as a measured zero — which would turn
    /// every subsequent window into an infinite-sigma jump.
    #[test]
    fn d1_skips_the_arrival_clause_when_the_prior_variance_is_unknown() {
        let mut det = Detector::new(d1_on());
        let mut first = fp("PV:V", "10.0.0.11:5075", 262.016, 4.0);
        first.arrival_delta_var_ms = None;
        det.ingest(&[first], at(1000));
        let events = det.ingest(&[fp("PV:V", "10.0.0.11:5075", 262.016, 900.0)], at(1900));
        assert!(events.is_empty(), "fired without a prior variance: {:?}", events);
    }

    /// `struct_id_changed` is documented on the record as a D1 source-change
    /// signal in its own right, so the detector must not swallow it.
    #[test]
    fn d1_fires_when_the_struct_id_changes() {
        let mut det = Detector::new(d1_on());
        det.ingest(&[fp("PV:S", "10.0.0.11:5075", 262.016, 4.0)], at(1000));
        let mut next = fp("PV:S", "10.0.0.11:5075", 262.016, 4.0);
        next.struct_id = "epics:nt/NTEnum:1.0".to_string();
        let events = det.ingest(&[next], at(1900));
        assert_eq!(events.len(), 1);
        assert!(events[0].detail.contains("struct_id"), "{}", events[0].detail);
    }

    /// Spec §6.1: an empty `struct_id` is "no MONITOR INIT observed", not a
    /// measured value. An observer that starts mid-stream and only learns the
    /// id in a later window must not report its own learning as a source
    /// change — in either direction.
    #[test]
    fn d1_does_not_fire_when_a_struct_id_was_merely_unknown() {
        for (first_id, second_id) in [("", "epics:nt/NTScalar:1.0"), ("epics:nt/NTScalar:1.0", "")] {
            let mut det = Detector::new(d1_on());
            let mut first = fp("PV:U", "10.0.0.11:5075", 262.016, 4.0);
            first.struct_id = first_id.to_string();
            det.ingest(&[first], at(1000));
            let mut next = fp("PV:U", "10.0.0.11:5075", 262.016, 4.0);
            next.struct_id = second_id.to_string();
            let events = det.ingest(&[next], at(1900));
            assert!(
                events.iter().all(|e| !e.detail.contains("struct_id")),
                "fired on an absent struct_id ({:?} -> {:?}): {:?}",
                first_id,
                second_id,
                events
            );
        }
    }

    /// Spec §6.2: planned failover is indistinguishable from silent
    /// re-sourcing, so D1 must be off unless the operator turns it on.
    #[test]
    fn d1_is_disabled_by_default() {
        assert!(!DetectorConfig::default().d1_enabled);
        let mut det = Detector::new(DetectorConfig::default());
        det.ingest(&[fp("PV:A", "10.0.0.11:5075", 262.016, 4.0)], at(1000));
        let events = det.ingest(&[fp("PV:A", "10.0.0.11:5075", 695.973, 4.0)], at(1900));
        assert!(events.iter().all(|e| e.rule != DetectionRule::D1SourceChange));
    }

    /// Spec §6.2 again: the event may report what changed, but must not claim
    /// to know why. Planned failover and silent re-sourcing produce the same
    /// wire signature, and the detail has to say so.
    #[test]
    fn a_d1_detail_does_not_claim_to_know_the_cause() {
        let mut det = Detector::new(d1_on());
        det.ingest(&[fp("PV:A", "10.0.0.11:5075", 262.016, 4.0)], at(1000));
        let events = det.ingest(&[fp("PV:A", "10.0.0.11:5075", 695.973, 4.0)], at(1900));
        assert_eq!(events.len(), 1);
        assert!(
            events[0].detail.contains("indistinguishable"),
            "D1 detail must carry the spec 6.2 caveat: {}",
            events[0].detail
        );
    }

    /// Spec §6.4: a fingerprint is a discriminator, not an identity. No
    /// detail may name or imply a specific producing process.
    #[test]
    fn no_detail_asserts_a_specific_process() {
        let mut det = Detector::new(d1_on());
        let mut nan = fp("PV:BRIDGED", "10.0.0.30:5075", 262.016, 4.0);
        nan.has_nan_limits = true;
        // PV:A and PV:C look identical here, so D2 has a "previously
        // identical" pair to fire on when PV:A moves below. Without the
        // seeded pair this guard covers only D1 and D3, and the D2 detail —
        // the one string rewritten to satisfy §6.4 — goes untested.
        det.ingest(
            &[
                fp("PV:A", "10.0.0.11:5075", 262.016, 4.0),
                fp("PV:C", "10.0.0.11:5075", 262.016, 4.0),
            ],
            at(1000),
        );
        let mut batch = vec![
            fp("PV:A", "10.0.0.11:5075", 695.973, 4.0),
            nan,
            fp("PV:C", "10.0.0.11:5075", 262.016, 4.0),
            fp("PV:D", "10.0.0.11:5075", 262.016, 4.1),
            fp("PV:E", "10.0.0.11:5075", 262.016, 9000.0),
        ];
        batch.sort_by(|a, b| a.channel.cmp(&b.channel));
        let events = det.ingest(&batch, at(1900));
        let mut fired: Vec<&str> = events.iter().map(|e| e.rule.code()).collect();
        fired.sort_unstable();
        fired.dedup();
        assert_eq!(
            fired,
            vec!["D1", "D2", "D3", "D4"],
            "the 6.4 guard must exercise every rule's detail string, fired: {:?}",
            events.iter().map(|e| (e.rule.code(), e.channel.as_str())).collect::<Vec<_>>()
        );
        for e in &events {
            let d = e.detail.to_lowercase();
            for banned in ["process", "ioc ", "gateway", "produced by", "is served by"] {
                assert!(!d.contains(banned), "{} asserts identity: {}", e.rule, e.detail);
            }
        }
    }

    /// Spec §6.2: the maintenance-window option. Suppression must silence D1
    /// while still updating the prior, so the window after maintenance
    /// compares against reality rather than against a stale fingerprint.
    #[test]
    fn suppression_silences_d1_but_still_advances_the_prior() {
        let mut det = Detector::new(DetectorConfig { suppress_until: Some(at(2000)), ..d1_on() });
        det.ingest(&[fp("PV:A", "10.0.0.11:5075", 262.016, 4.0)], at(1000));
        assert!(det.ingest(&[fp("PV:A", "10.0.0.11:5075", 695.973, 4.0)], at(1500)).is_empty());
        // Same fingerprint as the suppressed window: nothing changed since,
        // so nothing fires once suppression lapses.
        let after = det.ingest(&[fp("PV:A", "10.0.0.11:5075", 695.973, 4.0)], at(2500));
        assert!(after.iter().all(|e| e.rule != DetectionRule::D1SourceChange));
    }

    /// A suppressed firing is a firing that happened and was thrown away. It
    /// must be counted, or the maintenance window becomes a silent hole in
    /// the record.
    #[test]
    fn suppressed_d1_firings_are_counted() {
        let mut det = Detector::new(DetectorConfig { suppress_until: Some(at(2000)), ..d1_on() });
        det.ingest(&[fp("PV:A", "10.0.0.11:5075", 262.016, 4.0)], at(1000));
        assert_eq!(det.counts().d1_suppressed, 0);
        det.ingest(&[fp("PV:A", "10.0.0.11:5075", 695.973, 4.0)], at(1500));
        assert_eq!(det.counts().d1_suppressed, 1);
    }

    /// Spec §5 D2: one server, two channels that used to look identical and
    /// now do not.
    #[test]
    fn d2_fires_when_previously_identical_channels_diverge() {
        let mut det = Detector::new(DetectorConfig::default());
        det.ingest(
            &[
                fp("PV:A", "10.0.0.11:5075", 262.016, 4.0),
                fp("PV:B", "10.0.0.11:5075", 262.018, 4.0),
            ],
            at(1000),
        );
        let events = det.ingest(
            &[
                fp("PV:A", "10.0.0.11:5075", 262.016, 4.0),
                fp("PV:B", "10.0.0.11:5075", 695.973, 4.0),
            ],
            at(1900),
        );
        assert!(events.iter().any(|e| e.rule == DetectionRule::D2ServerDisagreement));
    }

    /// Spec §5 D2: an address that has always served diverse fingerprints is
    /// the normal case for a multiplexing endpoint. Channels that were never
    /// identical must not fire, or the signal drowns.
    #[test]
    fn d2_stays_quiet_for_a_server_that_was_always_diverse() {
        let mut det = Detector::new(DetectorConfig::default());
        let batch = || {
            vec![
                fp("PV:A", "10.0.0.20:5075", 262.016, 4.0),
                fp("PV:B", "10.0.0.20:5075", 695.973, 4.0),
            ]
        };
        det.ingest(&batch(), at(1000));
        let events = det.ingest(&batch(), at(1900));
        assert!(events.iter().all(|e| e.rule != DetectionRule::D2ServerDisagreement));
    }

    /// The mechanism that would mask D2 if it read cluster membership.
    ///
    /// `PHASE_SCALE_MS` is 500 ms against a 0.05 threshold, so two otherwise
    /// identical fingerprints still "match" with 50 ms of phase between them.
    /// Under single linkage a run of channels 40 ms apart chains into one
    /// cluster of unbounded diameter — here PV:00 at 262 ms and PV:11 at
    /// 702 ms, 440 ms apart, which is the spec §10 reference separation and
    /// unambiguously two sources. D2 must still see that disagreement, so it
    /// tests pairs directly and never asks which cluster a channel is in.
    #[test]
    fn d2_sees_through_a_single_linkage_chain() {
        use crate::scry::common::fingerprint::cluster::cluster_fingerprints;

        let chain: Vec<SourceFingerprint> = (0..12)
            .map(|i| fp(&format!("PV:{:02}", i), "10.0.0.11:5075", 262.0 + 40.0 * i as f64, 4.0))
            .collect();

        // The premise: single linkage really does merge the whole run into
        // one cluster, so cluster membership carries no disagreement at all.
        let (clusters, _) = cluster_fingerprints(&chain, DEFAULT_MATCH_THRESHOLD, 1000);
        assert_eq!(clusters.len(), 1, "premise broken: the chain did not merge");
        assert_eq!(clusters[0].members.len(), 12);

        // ...and the ends of that one cluster are the spec §10 separation
        // apart, which the pairwise test calls distinct.
        let weights = SimilarityWeights::default();
        let ends = fingerprint_distance_opt(&chain[0], &chain[11], &weights).unwrap();
        assert!(ends > DEFAULT_MATCH_THRESHOLD, "endpoints must be distinct: {}", ends);

        // First window: every channel starts identical, so every pair is a
        // "previously identical" pair.
        let mut det = Detector::new(DetectorConfig::default());
        let flat: Vec<SourceFingerprint> = (0..12)
            .map(|i| fp(&format!("PV:{:02}", i), "10.0.0.11:5075", 262.0, 4.0))
            .collect();
        det.ingest(&flat, at(1000));

        let events = det.ingest(&chain, at(1900));
        assert!(
            events.iter().any(|e| {
                e.rule == DetectionRule::D2ServerDisagreement && e.channel == "PV:11"
            }),
            "D2 missed a divergence that single linkage had merged into one cluster"
        );
    }

    /// Spec §6.1: a pair with nothing comparable is neither a match nor a
    /// difference, and must be recorded as neither.
    #[test]
    fn d2_treats_an_incomparable_pair_as_neither_match_nor_divergence() {
        let mut det = Detector::new(DetectorConfig {
            weights: SimilarityWeights { phase: 0.0, lsb: 0.0, cadence: 0.0, arrival: 0.0 },
            ..Default::default()
        });
        let batch = vec![
            fp("PV:A", "10.0.0.11:5075", 262.016, 4.0),
            fp("PV:B", "10.0.0.11:5075", 695.973, 900.0),
        ];
        let events = det.ingest(&batch, at(1000));
        assert!(events.iter().all(|e| e.rule != DetectionRule::D2ServerDisagreement));
        assert_eq!(det.counts().d2_incomparable_pairs, 1);
        // And it was not banked as a match either, so the pair still has no
        // "previously identical" record to fire on.
        let events = det.ingest(&batch, at(1900));
        assert!(events.iter().all(|e| e.rule != DetectionRule::D2ServerDisagreement));
        assert_eq!(det.counts().d2_incomparable_pairs, 2);
    }

    /// D2 is O(k^2) in the channels one address serves, so the comparison is
    /// bounded — and both the bound itself and what it skipped are
    /// observable (spec §4.3, §6.3). Every weight is zero here so that no
    /// pair is comparable, which makes `d2_incomparable_pairs` an exact
    /// count of the comparisons D2 actually performed.
    #[test]
    fn d2_compares_only_the_bounded_prefix_and_counts_the_rest() {
        let mut det = Detector::new(DetectorConfig {
            d2_max_channels_per_server: 3,
            weights: SimilarityWeights { phase: 0.0, lsb: 0.0, cadence: 0.0, arrival: 0.0 },
            ..Default::default()
        });
        let batch: Vec<SourceFingerprint> =
            (0..5).map(|i| fp(&format!("PV:{}", i), "10.0.0.11:5075", 262.0, 4.0)).collect();
        det.ingest(&batch, at(1000));
        // Three channels of five: C(3,2) = 3 pairs, not C(5,2) = 10.
        assert_eq!(det.counts().d2_incomparable_pairs, 3);
        assert_eq!(det.counts().d2_channels_skipped, 2);
    }

    /// Spec §6.1: harvest runs on a one-second tick, so two channels on one
    /// address routinely close their windows on different ticks and never
    /// share a batch. Comparing only within a batch would make D2
    /// permanently blind to their divergence, and that blindness would look
    /// exactly like agreement. The retained prior stands in for the absent
    /// channel, and the substitution is counted.
    #[test]
    fn d2_compares_a_channel_against_a_prior_it_did_not_see_this_window() {
        let mut det = Detector::new(DetectorConfig::default());
        det.ingest(
            &[
                fp("PV:A", "10.0.0.11:5075", 262.016, 4.0),
                fp("PV:B", "10.0.0.11:5075", 262.016, 4.0),
            ],
            at(1000),
        );
        // Only PV:B is harvested now, and it has moved. Within-batch
        // comparison alone sees a batch of one and says nothing, forever.
        let events = det.ingest(&[fp("PV:B", "10.0.0.11:5075", 695.973, 4.0)], at(1900));
        assert!(
            events.iter().any(|e| e.rule == DetectionRule::D2ServerDisagreement),
            "D2 was blind to a divergence spanning two harvest ticks: {:?}",
            events
        );
        assert_eq!(det.counts().d2_channels_carried_over, 1);
        let d2 = events.iter().find(|e| e.rule == DetectionRule::D2ServerDisagreement).unwrap();
        // The freshly harvested side is the one stamped `current`.
        assert_eq!(d2.channel, "PV:B");
        assert_eq!(d2.current.phase_mean_ms, 695.973);
    }

    /// Spec §6.1: "these differ" and "these differ, one side was measured up
    /// to two hours ago" are not the same claim. When D2 compares against a
    /// carried prior the event must say which side is stale and by how much,
    /// and the stale side must not be presented as `current` — the top-up is
    /// appended and then sorted by channel, so it lands on either side of
    /// the pair depending only on the channel names.
    #[test]
    fn a_d2_event_says_which_side_was_carried_and_how_old_it_is() {
        let mut det = Detector::new(DetectorConfig::default());
        det.ingest(
            &[
                fp("PV:A", "10.0.0.11:5075", 262.016, 4.0),
                fp("PV:B", "10.0.0.11:5075", 262.016, 4.0),
            ],
            at(1000),
        );
        // Only PV:A is harvested, so PV:B is the carried side and sorts
        // second — the position that used to receive `current`.
        let events = det.ingest(&[fp("PV:A", "10.0.0.11:5075", 695.973, 4.0)], at(1900));
        let d2 = events
            .iter()
            .find(|e| e.rule == DetectionRule::D2ServerDisagreement)
            .expect("D2 did not fire");
        assert_eq!(d2.channel, "PV:A", "the stale side named the event");
        assert_eq!(
            d2.current.phase_mean_ms, 695.973,
            "a fingerprint from an earlier window was exported as current"
        );
        assert_eq!(d2.prior.as_ref().unwrap().channel, "PV:B");
        assert!(
            d2.detail.contains("PV:B was not harvested this window"),
            "the event does not say a side was carried: {}",
            d2.detail
        );
        assert!(
            d2.detail.contains("1 window(s) ago"),
            "the event does not say how stale the carried side is: {}",
            d2.detail
        );
    }

    /// `d2_channels_carried_over` is documented as "compared against
    /// anyway", so it must be taken after the per-address cap. A top-up the
    /// cap threw away was never compared, and `d2_channels_skipped` already
    /// records it; counting it twice would put one discard under two
    /// different meanings.
    #[test]
    fn a_carried_channel_the_cap_dropped_is_not_counted_as_compared() {
        let mut det =
            Detector::new(DetectorConfig { d2_max_channels_per_server: 2, ..Default::default() });
        let all: Vec<SourceFingerprint> =
            (0..4).map(|i| fp(&format!("PV:{}", i), "10.0.0.11:5075", 262.016, 4.0)).collect();
        det.ingest(&all, at(1000));
        // PV:2 and PV:3 are absent now, so they are topped up — and then the
        // cap of two drops them again before any comparison happens.
        det.ingest(&all[..2], at(1900));
        assert_eq!(det.counts().d2_channels_carried_over, 0);
        assert!(det.counts().d2_channels_skipped >= 2);
    }

    /// `ServerPairs::matched` is O(k^2) per address and would otherwise grow
    /// for the process lifetime. Channels that leave the wire take their
    /// banked pairs with them, and the eviction is counted like every other
    /// discard.
    #[test]
    fn d2_evicts_pair_state_for_channels_that_left_the_wire() {
        let mut det =
            Detector::new(DetectorConfig { d2_state_ttl_windows: 1, ..Default::default() });
        det.ingest(
            &[
                fp("PV:A", "10.0.0.11:5075", 262.016, 4.0),
                fp("PV:B", "10.0.0.11:5075", 262.016, 4.0),
            ],
            at(1000),
        );
        assert_eq!(det.counts().d2_pairs_evicted, 0);
        for w in 1..=4 {
            det.ingest(&[fp("PV:A", "10.0.0.11:5075", 262.016, 4.0)], at(1000 + w * 900));
        }
        assert_eq!(
            det.counts().d2_pairs_evicted,
            1,
            "the banked (PV:A, PV:B) pair outlived the channel that left"
        );
    }

    /// Spec §3.1 / §5 D3: NaN or all-zero limits are a direct marker of
    /// synthesised metadata.
    #[test]
    fn d3_fires_on_synthesised_metadata() {
        let mut det = Detector::new(DetectorConfig::default());
        let mut f = fp("PV:BRIDGED", "10.0.0.30:5075", 262.016, 4.0);
        f.has_nan_limits = true;
        let events = det.ingest(&[f], at(1000));
        let d3: Vec<_> =
            events.iter().filter(|e| e.rule == DetectionRule::D3DeclaredContradiction).collect();
        assert_eq!(d3.len(), 1);
        // The detail must not imply a comparison against a declared field
        // that this repo cannot yet observe.
        assert!(d3[0].detail.contains("synthesised"));
        assert!(
            d3[0].detail.contains("no declared-provenance field"),
            "D3 must say the declaration side was never observed: {}",
            d3[0].detail
        );
    }

    /// D3 must fire once per channel, not once per window forever — and the
    /// repeats it swallows must be counted.
    #[test]
    fn d3_does_not_repeat_for_an_unchanged_channel() {
        let mut det = Detector::new(DetectorConfig::default());
        let mut f = fp("PV:BRIDGED", "10.0.0.30:5075", 262.016, 4.0);
        f.has_nan_limits = true;
        det.ingest(&[f.clone()], at(1000));
        let events = det.ingest(&[f], at(1900));
        assert!(events.iter().all(|e| e.rule != DetectionRule::D3DeclaredContradiction));
        assert_eq!(det.counts().d3_repeat_suppressed, 1);
    }

    /// Spec §5 D4: a source whose clock has drifted stands out from the
    /// population.
    #[test]
    fn d4_fires_on_a_clock_offset_outlier() {
        let mut det = Detector::new(DetectorConfig::default());
        let mut batch: Vec<SourceFingerprint> = (0..10)
            .map(|i| fp(&format!("PV:{}", i), "10.0.0.11:5075", 262.016, 4.0 + i as f64 * 0.1))
            .collect();
        batch.push(fp("PV:DRIFTED", "10.0.0.44:5075", 262.016, 5000.0));
        let events = det.ingest(&batch, at(1000));
        let d4: Vec<_> =
            events.iter().filter(|e| e.rule == DetectionRule::D4ClockOffsetOutlier).collect();
        assert_eq!(d4.len(), 1);
        assert_eq!(d4[0].channel, "PV:DRIFTED");
    }

    /// With the population standard deviation the largest attainable |z| is
    /// `(n-1)/sqrt(n)` — 2.846 at n = 10 — so D4 could not fire below eleven
    /// fingerprints, and a one-second harvest tick routinely produces three.
    /// The modified z-score has no such ceiling.
    #[test]
    fn d4_fires_on_a_three_point_batch() {
        let mut det = Detector::new(DetectorConfig::default());
        let batch = vec![
            fp("PV:0", "10.0.0.11:5075", 262.016, 4.0),
            fp("PV:1", "10.0.0.11:5075", 262.016, 4.1),
            fp("PV:DRIFTED", "10.0.0.44:5075", 262.016, 5000.0),
        ];
        let events = det.ingest(&batch, at(1000));
        let d4: Vec<_> =
            events.iter().filter(|e| e.rule == DetectionRule::D4ClockOffsetOutlier).collect();
        assert_eq!(d4.len(), 1, "{:?}", events);
        assert_eq!(d4[0].channel, "PV:DRIFTED");
    }

    /// Spec §6.1: "D4 could not evaluate" and "D4 evaluated and found
    /// nothing" must not be the same output. Both refusals are counted.
    #[test]
    fn d4_counts_the_batches_it_could_not_evaluate() {
        let mut det = Detector::new(DetectorConfig::default());
        det.ingest(&[fp("PV:LONE", "10.0.0.11:5075", 262.016, 9999.0)], at(1000));
        assert_eq!(det.counts().d4_population_too_small, 1);
        assert_eq!(det.counts().d4_no_spread, 0);
        let flat: Vec<SourceFingerprint> =
            (0..5).map(|i| fp(&format!("PV:{}", i), "10.0.0.11:5075", 262.016, 4.0)).collect();
        det.ingest(&flat, at(1900));
        assert_eq!(det.counts().d4_no_spread, 1);
        assert_eq!(det.counts().d4_population_too_small, 1);
    }

    /// D4 is a population test; a population of one has no population.
    #[test]
    fn d4_needs_a_population() {
        let mut det = Detector::new(DetectorConfig::default());
        let events = det.ingest(&[fp("PV:LONE", "10.0.0.11:5075", 262.016, 9999.0)], at(1000));
        assert!(events.iter().all(|e| e.rule != DetectionRule::D4ClockOffsetOutlier));
    }

    /// A population with no spread has no outliers. Dividing by a zero
    /// standard deviation would make every member infinitely deviant.
    #[test]
    fn d4_is_quiet_when_the_population_has_no_spread() {
        let mut det = Detector::new(DetectorConfig::default());
        let batch: Vec<SourceFingerprint> =
            (0..5).map(|i| fp(&format!("PV:{}", i), "10.0.0.11:5075", 262.016, 4.0)).collect();
        let events = det.ingest(&batch, at(1000));
        assert!(events.iter().all(|e| e.rule != DetectionRule::D4ClockOffsetOutlier));
    }

    /// Events are ordered by rule then channel, so two runs over the same
    /// batch produce the same log and the detector's internal `HashMap`
    /// iteration order cannot leak into the output.
    #[test]
    fn output_is_deterministic_and_ordered() {
        let build = || {
            let mut det = Detector::new(d1_on());
            let mut nan = fp("PV:NAN", "10.0.0.30:5075", 262.016, 4.0);
            nan.has_nan_limits = true;
            det.ingest(
                &[
                    fp("PV:A", "10.0.0.11:5075", 262.016, 4.0),
                    fp("PV:B", "10.0.0.11:5075", 262.017, 4.0),
                    fp("PV:C", "10.0.0.11:5075", 262.018, 4.0),
                ],
                at(1000),
            );
            det.ingest(
                &[
                    fp("PV:C", "10.0.0.11:5075", 262.018, 4.0),
                    nan,
                    fp("PV:A", "10.0.0.11:5075", 695.973, 4.0),
                    fp("PV:B", "10.0.0.11:5075", 262.017, 5000.0),
                ],
                at(1900),
            )
        };
        let first = build();
        let second = build();
        assert_eq!(first, second);
        assert!(first.len() > 2, "expected several rules to fire: {:?}", first);
        let keys: Vec<(&str, &str)> =
            first.iter().map(|e| (e.rule.code(), e.channel.as_str())).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "events are not in (rule, channel) order: {:?}", keys);
    }

    /// Nothing is dropped silently: a batch that fires more events than the
    /// per-ingest cap allows must report the overflow.
    #[test]
    fn the_per_batch_event_cap_is_counted_not_silent() {
        let mut det =
            Detector::new(DetectorConfig { max_events_per_ingest: 2, ..Default::default() });
        let mut batch: Vec<SourceFingerprint> = (0..6)
            .map(|i| {
                let mut f = fp(&format!("PV:{}", i), "10.0.0.30:5075", 262.016, 4.0);
                f.has_nan_limits = true;
                f
            })
            .collect();
        batch.push(fp("PV:PLAIN", "10.0.0.30:5075", 262.016, 4.0));
        let events = det.ingest(&batch, at(1000));
        assert_eq!(events.len(), 2);
        assert_eq!(det.counts().events_dropped_by_cap, 4);
    }

    /// An event the cap discarded was never logged, so it must not have been
    /// deduplicated either. Marking a firing "reported" while building it and
    /// then truncating the vector retires the detection permanently: the
    /// channel's synthesised metadata would never appear in any window.
    #[test]
    fn an_event_the_cap_dropped_is_not_retired() {
        let mut det =
            Detector::new(DetectorConfig { max_events_per_ingest: 2, ..Default::default() });
        let batch: Vec<SourceFingerprint> = (0..6)
            .map(|i| {
                let mut f = fp(&format!("PV:{}", i), "10.0.0.30:5075", 262.016, 4.0);
                f.has_nan_limits = true;
                f
            })
            .collect();
        let first = det.ingest(&batch, at(1000));
        assert_eq!(first.len(), 2);
        assert_eq!(det.counts().events_dropped_by_cap, 4);
        // Only the two that were emitted are deduplicated; the four the cap
        // threw away come back.
        let second = det.ingest(&batch, at(1900));
        assert_eq!(second.len(), 2);
        assert_eq!(det.counts().d3_repeat_suppressed, 2);
        let seen: HashSet<&str> =
            first.iter().chain(second.iter()).map(|e| e.channel.as_str()).collect();
        assert_eq!(seen.len(), 4, "the dropped firings never came back: {:?}", seen);
    }

    /// Spec §7 and §6.1: D2 may compare a fresh window against a prior up to
    /// `d2_state_ttl_windows` old, so the exported line has to carry the
    /// window bounds of both sides or the reader cannot tell how old the
    /// comparison was.
    #[test]
    fn a_loki_line_carries_the_window_bounds_of_both_sides() {
        let mut det = Detector::new(d1_on());
        det.ingest(&[fp("PV:A", "10.0.0.11:5075", 262.016, 4.0)], at(1000));
        let events = det.ingest(&[fp("PV:A", "10.0.0.11:5075", 695.973, 4.0)], at(1900));
        let (_, line, _) = detection_loki_entry(&events[0]);
        // Leading spaces on purpose: without them "window_start=" is a
        // substring of "prior_window_start=" and the current side could
        // vanish from the line with this test still passing.
        for field in
            [" window_start=", " window_end=", " prior_window_start=", " prior_window_end="]
        {
            assert!(line.contains(field), "missing {}: {}", field, line);
        }
    }

    /// Spec §7: each event must be self-contained, carrying prior and current
    /// so the log line can be read without the database.
    #[test]
    fn a_loki_entry_carries_both_fingerprints() {
        let mut det = Detector::new(d1_on());
        det.ingest(&[fp("PV:A", "10.0.0.11:5075", 262.016, 4.0)], at(1000));
        let events = det.ingest(&[fp("PV:A", "10.0.0.11:5075", 695.973, 4.0)], at(1900));
        let (stream, line, labels) = detection_loki_entry(&events[0]);
        assert_eq!(stream, "pv_fingerprint_event");
        assert_eq!(labels.get("rule").map(String::as_str), Some("D1"));
        assert_eq!(labels.get("channel").map(String::as_str), Some("PV:A"));
        assert!(line.contains("prior_phase_mean_ms=262.016"));
        assert!(line.contains("phase_mean_ms=695.973"));
    }

    /// A quote anywhere in the event must not break the `detail="..."` field
    /// and swallow the rest of the line.
    #[test]
    fn a_loki_line_survives_a_quote_in_the_detail() {
        let mut f = fp("PV:Q", "10.0.0.11:5075", 262.016, 4.0);
        f.channel = "PV:\"Q\"".to_string();
        let ev = DetectionEvent {
            rule: DetectionRule::D4ClockOffsetOutlier,
            channel: f.channel.clone(),
            server_addr: f.server_addr,
            detail: "a \"quoted\" detail".to_string(),
            prior: None,
            current: f,
            at: at(1000),
        };
        let (_, line, _) = detection_loki_entry(&ev);
        assert_eq!(line.matches('"').count(), 2, "unbalanced quoting: {}", line);
        assert!(line.contains("sample_count=40"), "line truncated: {}", line);
    }
}
