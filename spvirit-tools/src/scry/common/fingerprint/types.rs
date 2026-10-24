//! The fingerprint record and its supporting types (spec §3).

use std::net::SocketAddr;
use std::time::SystemTime;

/// A fingerprint is accumulated per (channel, serving socket) pair, never per
/// channel alone: the same channel name served from two addresses is two
/// sources, and that distinction is the point of the exercise (spec §3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FingerprintKey {
    pub channel: String,
    pub server_addr: SocketAddr,
}

impl FingerprintKey {
    pub fn new(channel: impl Into<String>, server_addr: SocketAddr) -> Self {
        Self { channel: channel.into(), server_addr }
    }
}

/// One decoded monitor update, as the engine consumes it.
///
/// `observed_unix_nanos` is the capture timestamp, supplied by the caller
/// rather than read from the clock, so that the engine is deterministic under
/// test and honest about capture time in production.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub stamp_seconds: i64,
    pub stamp_nanos: u32,
    pub observed_unix_nanos: i128,
    pub value: Option<f64>,
    pub has_nan_limits: bool,
    pub struct_id: Option<String>,
}

/// How the window's samples were selected (spec §4.3).
///
/// This travels with the fingerprint so that consumers can reason about
/// detection latency instead of guessing at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingRegime {
    /// Every update in the window was used.
    Full,
    /// Updates were reservoir-sampled to at most `cap` per window.
    Reservoir { cap: usize },
    /// Only a subset of channels was admitted this window; time-to-detection
    /// is extended by `slots`.
    RoundRobin { cap: usize, slots: usize },
}

impl SamplingRegime {
    /// Stable tag value for InfluxDB export.
    pub fn label(&self) -> &'static str {
        match self {
            SamplingRegime::Full => "full",
            SamplingRegime::Reservoir { .. } => "reservoir",
            SamplingRegime::RoundRobin { .. } => "round_robin",
        }
    }
}

/// The accumulated source fingerprint for one (channel, server) pair over one
/// window (spec §3).
#[derive(Debug, Clone, PartialEq)]
pub struct SourceFingerprint {
    pub channel: String,
    pub server_addr: SocketAddr,

    // O1 — sub-second timestamp phase
    pub phase_mean_ms: f64,
    pub phase_spread_us: f64,
    pub phase_circular_var: f64,

    // O2 — value quantisation lattice
    pub value_lsb: Option<f64>,
    pub lsb_confidence: f64,

    // O3 — update cadence structure
    pub cadence_modal_s: f64,
    pub cadence_p50_s: f64,
    pub cadence_p95_s: f64,

    // O4 — arrival minus stamp
    pub arrival_delta_mean_ms: f64,
    /// Sample variance of the arrival delta, or `None` when the window held
    /// fewer than two updates and the spread is genuinely undefined. An
    /// `Option` rather than a sentinel zero, because a rock-steady transport
    /// really does have a variance of 0.0 and must not read as "unknown"
    /// (spec §6.1).
    pub arrival_delta_var_ms: Option<f64>,

    // provenance of the fingerprint itself
    /// Samples actually *retained* to compute the sampled observables, O2
    /// (lattice) and O3 (cadence): the larger of the two reservoirs. Under
    /// `SamplingRegime::Full` this tracks `observed_count`; under
    /// `Reservoir` it is the honest, smaller figure that backed the
    /// estimates rather than the raw update count.
    pub sample_count: u64,
    pub window_start: SystemTime,
    pub window_end: SystemTime,
    pub struct_id: String,
    /// True when the window saw more than one distinct `struct_id`. The
    /// first is retained in `struct_id`; the fact that it changed is a D1
    /// source-change signal in its own right and must not be swallowed.
    pub struct_id_changed: bool,
    pub has_nan_limits: bool,

    /// Every update seen in the window, before any reservoir sampling, so a
    /// sampled fingerprint does not understate the channel's true rate. This
    /// is also the exact sample count behind O1 (phase) and O4 (arrival),
    /// which are streaming and never sampled.
    pub observed_count: u64,
    /// How this window's samples were selected.
    pub sampling: SamplingRegime,
}

impl SourceFingerprint {
    /// True when the phase is tight enough to be treated as a signature at
    /// all. Spec §2.1: a wide phase is absence of evidence, not evidence of
    /// absence, so comparisons must be skipped rather than made loosely.
    pub fn phase_is_locked(&self) -> bool {
        self.phase_circular_var < 1e-3
    }

    pub fn key(&self) -> FingerprintKey {
        FingerprintKey::new(self.channel.clone(), self.server_addr)
    }
}

/// What a window produced for one channel.
///
/// Spec §6.1 requires "no fingerprint" to be reported distinctly from "a
/// fingerprint that happens to be unremarkable", so silence is a value here
/// rather than an omission.
#[derive(Debug, Clone, PartialEq)]
pub enum FingerprintStatus {
    Insufficient {
        key: FingerprintKey,
        samples: u64,
        /// The floor `samples` fell short of, carried on the status so a
        /// consumer can render "pending n/m" without also holding the
        /// engine's config — these get cloned into TUI vectors that are
        /// detached from the engine.
        min_samples: u64,
        window_start: SystemTime,
    },
    Ready(Box<SourceFingerprint>),
}

impl FingerprintStatus {
    pub fn key(&self) -> FingerprintKey {
        match self {
            FingerprintStatus::Insufficient { key, .. } => key.clone(),
            FingerprintStatus::Ready(fp) => fp.key(),
        }
    }

    pub fn ready(&self) -> Option<&SourceFingerprint> {
        match self {
            FingerprintStatus::Ready(fp) => Some(fp),
            _ => None,
        }
    }
}

/// What fraction of the observable traffic the fingerprinter actually looked
/// at (spec §4.3, §6.3). Coverage is measured and reported, never assumed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FingerprintCoverage {
    pub interface: String,
    /// Channels that reached the engine this window, whichever of the three
    /// fates they met (admitted, deferred at capacity, rotated out). It is
    /// not the whole of the wire: an update the probe could not read never
    /// reaches the engine and so has no channel here — those are counted in
    /// `unprobeable_updates` instead.
    pub tracked_channels: usize,
    /// Channels the engine actually accumulated.
    pub admitted_channels: usize,
    /// Distinct channels dropped because the engine was at capacity. Capped
    /// (see `deferred_saturated`); `deferred_total` is the uncapped truth.
    pub deferred_channels: usize,
    /// Monotonic count of *updates* turned away at capacity over the whole
    /// life of the engine. Never reset by `harvest` and never saturates, so
    /// it remains a true lower bound on what was missed even when the
    /// per-window channel set has been cleared or has hit its cap.
    pub deferred_total: u64,
    /// True when `deferred_channels` hit its own cap and therefore
    /// understates the number of distinct channels turned away.
    pub deferred_saturated: bool,
    /// Channels whose window was reservoir-sampled rather than complete.
    pub sampled_channels: usize,
    /// Distinct channels that were on the wire but sat out the current
    /// round-robin slot. Bounded like `deferred_channels` (see
    /// `rotated_out_saturated`). Deliberately separate from the deferral
    /// fields: rotation is a scheduled delay of `slots` windows, capacity
    /// deferral is unscheduled loss, and merging them into one number would
    /// make neither actionable.
    pub rotated_out_channels: usize,
    /// Monotonic count of *updates* skipped because their channel was not in
    /// the admitting slot. Never reset, never saturates.
    pub rotated_out_total: u64,
    /// True when `rotated_out_channels` hit its own cap and understates the
    /// number of distinct channels rotated out.
    pub rotated_out_saturated: bool,
    /// Monotonic count of monitor-data updates discarded before the engine
    /// ever saw them: the payload carried no timestamped scalar the probe
    /// could read (NTNDArray, NTTable, plain structures), its server
    /// address did not parse, or it had no decoded payload at all because
    /// the observer joined mid-stream and missed the MONITOR INIT carrying
    /// the introspection. Never reset; saturates at `u64::MAX` rather than
    /// wrapping, so a channel population the fingerprinter structurally
    /// cannot cover shows up as a number rather than as apparently complete
    /// coverage (spec §6.3).
    pub unprobeable_updates: u64,
    /// The engine's configured `min_samples` threshold at the time this
    /// coverage was measured, so a reader can tell "pending n/min_samples"
    /// apart from a hard-coded guess (spec §6.1).
    pub min_samples: u64,
}
