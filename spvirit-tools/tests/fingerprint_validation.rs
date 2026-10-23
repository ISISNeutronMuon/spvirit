//! Validation against spec §10.
//!
//! The reference channels are real ISIS channels observed 2026-08-21
//! 06:00-06:20Z. The fixture below is synthetic but constructed to have
//! exactly the published phase, spread, lattice and cadence, so the
//! assertions are on the implementation rather than on captured data.
//!
//! O4 (arrival delta) is not asserted against a value: spec §10 states it
//! has no archival reference and must be validated live.
//!
//! The fixture is deliberately *not* the easiest one that reproduces the
//! table. Three properties are load-bearing, and each exists because the
//! easy version of it makes a real defect invisible:
//!
//! * the value walk never steps a single LSB, so an estimator that reports
//!   the smallest observed difference, or that skips the subdivision
//!   refinement in `lattice.rs`, returns a multiple of the true lattice;
//! * the lattice offset is non-integral (`base_value` is not a whole number
//!   of LSBs), so a GCD taken over raw values really does report the
//!   spurious 0.0025 that spec §2.2 warns about -- demonstrated below rather
//!   than merely asserted;
//! * the cadence has a tail (spec §2.3's "modal 1 s, gaps to 207 s") and the
//!   modal interval is exactly 1.000000000 s, so the histogram is actually
//!   exercised and p50/p95 carry information.

#![cfg(feature = "scry")]

use spvirit_tools::scry::common::fingerprint::cluster::cluster_fingerprints;
use spvirit_tools::scry::common::fingerprint::detect::{DetectionRule, Detector, DetectorConfig};
use spvirit_tools::scry::common::fingerprint::engine::{FingerprintConfig, FingerprintEngine};
use spvirit_tools::scry::common::fingerprint::export::fingerprint_line_protocol;
use spvirit_tools::scry::common::fingerprint::similarity::{DEFAULT_MATCH_THRESHOLD, fingerprints_match};
use spvirit_tools::scry::common::fingerprint::types::{FingerprintKey, Observation, SourceFingerprint};

const SEC: i128 = 1_000_000_000;

/// Spec §10's rounding bound on `phase_mean_ms`: `262.016` is printed to
/// three decimal places, so half a unit in the last published place is the
/// widest value that still rounds to the printed figure.
///
/// This is the bound the spec *requires*, and it is asserted as such. It is
/// not a tight bound on this fixture: samples sit symmetrically about the
/// reference phase, so the circular mean is exact and the observed error is
/// below 1e-12 ms. `PHASE_EXACT_TOL_MS` is the bound that actually bites.
/// Both are kept so the spec's requirement and the fixture's much stronger
/// guarantee stay separately visible.
const PHASE_TOL_MS: f64 = 5e-4;

/// What the symmetric fixture actually guarantees, with ~1000x margin on the
/// observed error. A radian/nanosecond conversion dropped or applied twice
/// moves the mean by whole milliseconds and fails both bounds; an arithmetic
/// slip too small for §10's bound to notice still fails this one.
const PHASE_EXACT_TOL_MS: f64 = 1e-9;

/// Half a unit in the last place the spec prints `spread_us` to (`85.6`).
/// The observed error is 2.3e-6 us, the second-order term in
/// `sqrt(-2 ln cos t) ~ t`, so this bound is loose by construction: it is
/// §10's bound, not the fixture's.
const SPREAD_TOL_US: f64 = 0.05;

/// The fixture emits inter-stamp intervals of exactly 1.000000000 s -- whole
/// second steps, with sub-second phase held constant across each half of the
/// window -- so the modal interval, the median and the 95th percentile must
/// all come back exactly.
///
/// Far tighter than §10's printed `1` would license, deliberately. At one
/// histogram bin (0.05 s) every cadence defect worth catching survives: a
/// half-bin shift of the reported mode, and a bin index computed with
/// `round` instead of `floor`, which sweeps the single jittered interval
/// into the modal bin and drags its mean 5e-6 s off 1 s.
const CADENCE_TOL_S: f64 = 1e-9;

/// Spec §2.3 gives R55IWATER as "modal 1 s, gaps to 207 s". The fixture has
/// that shape, which puts p95 on a gap rather than on the mode.
const REF_P50_S: f64 = 1.0;
const REF_P95_S: f64 = 25.0;

/// Steps of the value walk, in whole LSBs.
///
/// Successive gaps are 2 or 3 LSBs and never 1, so the single-LSB step is
/// never directly observed. The true lattice is recoverable only by
/// subdividing the observed differences (`lattice.rs`'s `MAX_SUBDIVISION`
/// loop); an estimator that takes the minimum difference, or a GCD of the
/// differences without refinement, reports 2 or 3 LSBs instead. This is the
/// realistic deadbanded case rather than a contrivance: a channel that only
/// republishes past a deadband rarely shows its own quantum.
const LATTICE_WALK: [u64; 7] = [0, 2, 5, 7, 10, 12, 15];

/// Seconds added to the stamp before sample `i`.
///
/// Normal steps are the channel's modal interval; three samples are preceded
/// by a gap instead, giving the cadence spec §2.3's shape. The three gaps are
/// deliberately *distinct* (5 s, 25 s, 207 s) rather than repeated: with 39
/// intervals the 95th percentile lands on nearest rank `ceil(0.95 * 39) = 38`,
/// and only distinct tail values make that index load-bearing. Two gaps of
/// the same size would put ranks 37 and 38 on the same number, and a
/// `ceil`-to-`floor` slip in `cadence.rs` would be invisible.
fn step_secs(i: u64, modal_s: f64) -> i64 {
    match i {
        8 => 5,
        16 => 207,
        28 => 25,
        _ => modal_s as i64,
    }
}

/// One reference channel, as spec §10 tabulates it.
#[derive(Clone, Copy)]
struct Reference {
    channel: &'static str,
    server: &'static str,
    phase_ms: f64,
    spread_us: f64,
    lsb: f64,
    modal_s: f64,
    /// Deliberately not a whole number of LSBs. 21.0025 is 8401 quarter-
    /// hundredths and 0.0175 is seven of them, so the raw values share the
    /// spurious 0.0025 divisor spec §2.2 calls out while their successive
    /// differences do not. A round 21.0 would be exactly 1200 LSBs and the
    /// offset dependence would be unobservable.
    base_value: f64,
    /// Constant capture-minus-stamp delay injected by the fixture (O4).
    delay_ms: f64,
}

const R55IWATER: Reference = Reference {
    channel: "R55IWATER::AMBIENT:TEMPERATURE",
    server: "10.0.0.11:5075",
    phase_ms: 262.016,
    spread_us: 85.6,
    lsb: 0.0175,
    modal_s: 1.0,
    base_value: 21.0025,
    delay_ms: 4.0,
};

const R4WATER: Reference = Reference {
    channel: "R4WATER::DEMINWAT:PH",
    server: "10.0.0.12:5075",
    phase_ms: 695.973,
    spread_us: 196.6,
    lsb: 0.01,
    modal_s: 1.0,
    base_value: 7.2025,
    delay_ms: 4.0,
};

/// A second channel behaving like R55IWATER on a different address, as a
/// second process cut from the same template would.
///
/// It differs on three of the four observables, not one: phase by 30 us
/// (well inside R55IWATER's own 85.6 us spread), lattice 0.0175 -> 0.02, and
/// transport delay 4 -> 6 ms. Each difference is small enough that the pair
/// must still be called one source, but large enough that its component
/// carries weight, so the match is a real weighted decision rather than a
/// comparison of near-identical structs. The lattice term in particular is
/// sized so that transposing the phase and lsb weights in `similarity.rs`
/// pushes the pair over the threshold and this control fails.
const R55IWATER_TWIN: Reference = Reference {
    channel: "R55IWATER::AMBIENT:HUMIDITY",
    server: "10.0.0.13:5075",
    phase_ms: 262.046,
    lsb: 0.02,
    delay_ms: 6.0,
    ..R55IWATER
};

/// R55IWATER's behaviour at a quarter of the rate, and otherwise identical.
///
/// Exists so the cadence component of the distance is load-bearing somewhere.
/// Both §10 channels scan at 1 s, so cadence contributes exactly zero to the
/// separation §10 demands; without this channel a cadence term that was
/// mis-scaled, or dropped entirely, would change no assertion in this file.
/// Two octaves apart is 0.5 on the cadence axis, which alone exceeds the
/// match threshold.
const R55IWATER_SLOW: Reference = Reference {
    channel: "R55IWATER::AMBIENT:PRESSURE",
    server: "10.0.0.14:5075",
    modal_s: 4.0,
    ..R55IWATER
};

const STRUCT_ID: &str = "epics:nt/NTScalar:1.0";

/// Feed `n` updates for one reference channel, stamping from `base_secs`.
///
/// Phase is held one spread above the reference for the first half of the
/// window and one spread below for the second, so the circular mean lands
/// exactly on the reference and the circular SD lands on the spread (`n` must
/// be even for that to hold exactly), while every inter-stamp interval but
/// one is a whole number of seconds.
///
/// Capture time is the stamp plus a constant transport delay, so O4 is a
/// property of the fixture rather than of the loop index.
fn feed(engine: &mut FingerprintEngine, r: &Reference, n: u64, base_secs: i64) {
    assert_eq!(n % 2, 0, "the symmetric phase fixture needs an even sample count");
    let key = FingerprintKey::new(r.channel.to_string(), r.server.parse().unwrap());
    let phase_nanos = (r.phase_ms * 1e6).round() as i64;
    let spread_nanos = (r.spread_us * 1e3).round() as i64;
    let delay_nanos = (r.delay_ms * 1e6) as i128;

    let mut stamp_secs = base_secs;
    for i in 0..n {
        if i > 0 {
            stamp_secs += step_secs(i, r.modal_s);
        }
        let sign: i64 = if i < n / 2 { 1 } else { -1 };
        let nanos = phase_nanos + sign * spread_nanos;
        let stamp_unix = stamp_secs as i128 * SEC + nanos as i128;
        engine.observe(
            key.clone(),
            Observation {
                stamp_seconds: stamp_secs,
                stamp_nanos: nanos as u32,
                observed_unix_nanos: stamp_unix + delay_nanos,
                // Values walk the lattice in steps of 2 and 3 LSBs, so the
                // successive differences are multiples of the LSB without
                // the LSB itself ever appearing (spec §2.2).
                value: Some(r.base_value + LATTICE_WALK[(i % 7) as usize] as f64 * r.lsb),
                has_nan_limits: false,
                struct_id: Some(STRUCT_ID.to_string()),
            },
        );
    }
}

const BASE_SECS: i64 = 1_755_756_000;

fn harvest_all(refs: &[Reference], n: u64, base_secs: i64) -> Vec<SourceFingerprint> {
    let config = FingerprintConfig::default();
    let mut engine = FingerprintEngine::new(config.clone());
    for r in refs {
        feed(&mut engine, r, n, base_secs);
    }
    let after = base_secs as i128 * SEC + (config.window_secs as i128 + 1) * SEC;
    engine.harvest(after).into_iter().filter_map(|s| s.ready().cloned()).collect()
}

fn harvest_both(n: u64) -> Vec<SourceFingerprint> {
    harvest_all(&[R55IWATER, R4WATER], n, BASE_SECS)
}

fn find<'a>(fps: &'a [SourceFingerprint], channel: &str) -> &'a SourceFingerprint {
    fps.iter().find(|f| f.channel == channel).expect("fingerprint missing")
}

/// Spec §10: "A correct implementation MUST reproduce these to within
/// rounding."
#[test]
fn reproduces_the_reference_table() {
    let fps = harvest_both(40);
    assert_eq!(fps.len(), 2);

    for r in [&R55IWATER, &R4WATER] {
        let fp = find(&fps, r.channel);
        // §10's bound, then the bound the symmetric fixture justifies.
        assert!(
            (fp.phase_mean_ms - r.phase_ms).abs() < PHASE_TOL_MS,
            "{}: phase_mean_ms {} outside the §10 rounding bound on {}",
            r.channel,
            fp.phase_mean_ms,
            r.phase_ms
        );
        assert!(
            (fp.phase_mean_ms - r.phase_ms).abs() < PHASE_EXACT_TOL_MS,
            "{}: phase_mean_ms {} != {} (the fixture is symmetric; this must be exact)",
            r.channel,
            fp.phase_mean_ms,
            r.phase_ms
        );
        assert!(
            (fp.phase_spread_us - r.spread_us).abs() < SPREAD_TOL_US,
            "{}: phase_spread_us {} != {}",
            r.channel,
            fp.phase_spread_us,
            r.spread_us
        );
        assert!(
            (fp.value_lsb.expect("lattice missing") - r.lsb).abs() < 1e-9,
            "{}: value_lsb {:?} != {}",
            r.channel,
            fp.value_lsb,
            r.lsb
        );
        // Every observed difference is an exact multiple of the lattice, so
        // anything below full confidence means differences were scored
        // against a quantum that does not divide them.
        assert!(
            (fp.lsb_confidence - 1.0).abs() < 1e-12,
            "{}: lsb_confidence {}",
            r.channel,
            fp.lsb_confidence
        );
        // O3, all three figures: spec §2.3 makes the distribution tail, not
        // the mode alone, the discriminator.
        assert!(
            (fp.cadence_modal_s - r.modal_s).abs() < CADENCE_TOL_S,
            "{}: cadence_modal_s {} != {}",
            r.channel,
            fp.cadence_modal_s,
            r.modal_s
        );
        assert!(
            (fp.cadence_p50_s - REF_P50_S).abs() < CADENCE_TOL_S,
            "{}: cadence_p50_s {} != {}",
            r.channel,
            fp.cadence_p50_s,
            REF_P50_S
        );
        assert!(
            (fp.cadence_p95_s - REF_P95_S).abs() < CADENCE_TOL_S,
            "{}: cadence_p95_s {} != {}",
            r.channel,
            fp.cadence_p95_s,
            REF_P95_S
        );
        // Spec §4.1: a locked source must report a near-zero circular
        // variance, so a consumer can tell "locked" from "no signal".
        assert!(fp.phase_circular_var < 1e-6, "{}: not locked", r.channel);
        // O4. Spec §10 publishes no reference value for the arrival delta and
        // none is invented here: `r.delay_ms` is the constant this fixture
        // itself injected between stamp and capture, so recovering it is a
        // statement about the engine's arithmetic, not about the spec. The
        // delay is constant across the window, so the mean must equal it and
        // the variance must be zero; a sign slip, an operand swap or a unit
        // error all move the mean off `delay_ms`.
        assert!(
            (fp.arrival_delta_mean_ms - r.delay_ms).abs() < 1e-9,
            "{}: arrival_delta_mean_ms {} != the {} ms the fixture injected",
            r.channel,
            fp.arrival_delta_mean_ms,
            r.delay_ms
        );
        let var = fp.arrival_delta_var_ms.expect("40 samples must yield a variance");
        assert!(var.abs() < 1e-12, "{}: arrival_delta_var_ms {} != 0", r.channel, var);
        // Spec §3: `struct_id` is part of the record's own provenance. It is
        // asserted here because an id that never arrives is structurally
        // indistinguishable from a non-NT channel downstream, where the
        // exporter substitutes `unknown` -- a silent, permanent `unknown` is
        // exactly what no other test would notice.
        assert_eq!(fp.struct_id, STRUCT_ID, "{}: struct_id lost", r.channel);
        assert!(!fp.struct_id.is_empty());
        assert!(!fp.struct_id_changed);
    }
}

/// Spec §10: the implementation "MUST classify the two channels as distinct
/// sources".
#[test]
fn classifies_the_reference_channels_as_distinct_sources() {
    let fps = harvest_both(40);
    let a = find(&fps, R55IWATER.channel);
    let b = find(&fps, R4WATER.channel);
    assert!(!fingerprints_match(a, b, DEFAULT_MATCH_THRESHOLD));

    let (clusters, dropped) = cluster_fingerprints(&fps, DEFAULT_MATCH_THRESHOLD, 100);
    assert_eq!(dropped, 0);
    assert_eq!(clusters.len(), 2, "the two channels must not cluster together");
}

/// The positive control for the test above.
///
/// "Distinct" is only a finding if "same" is reachable: a similarity function
/// that never matches anything satisfies §10's separation requirement while
/// answering spec §1's first operational question -- "do these two channels
/// come from the same source?" -- wrongly in every case. Two channels built
/// to share a source must be recognised as one through the same
/// `fingerprints_match` / `cluster_fingerprints` path, in the same batch that
/// keeps R4WATER apart.
#[test]
fn groups_two_channels_that_share_a_source() {
    let fps = harvest_all(&[R55IWATER, R4WATER, R55IWATER_TWIN, R55IWATER_SLOW], 40, BASE_SECS);
    assert_eq!(fps.len(), 4);

    let a = find(&fps, R55IWATER.channel);
    let twin = find(&fps, R55IWATER_TWIN.channel);
    let other = find(&fps, R4WATER.channel);
    let slow = find(&fps, R55IWATER_SLOW.channel);
    // The twin differs from R55IWATER in phase, lattice and arrival delay at
    // once, so every weighted term but cadence contributes to this decision
    // and none of them may be mis-weighted for it to hold.
    assert!(
        fingerprints_match(a, twin, DEFAULT_MATCH_THRESHOLD),
        "channels sharing a source must match"
    );
    assert!(!fingerprints_match(a, other, DEFAULT_MATCH_THRESHOLD));
    // ... and cadence alone must be able to separate two otherwise identical
    // channels, which is the one term the §10 pair cannot exercise.
    assert!(
        !fingerprints_match(a, slow, DEFAULT_MATCH_THRESHOLD),
        "a four-fold difference in scan rate must not be called one source"
    );

    let (clusters, dropped) = cluster_fingerprints(&fps, DEFAULT_MATCH_THRESHOLD, 100);
    assert_eq!(dropped, 0);
    assert_eq!(clusters.len(), 3, "four channels, three sources");
    let shared = clusters
        .iter()
        .find(|c| c.members.iter().any(|m| m.channel == R55IWATER.channel))
        .expect("R55IWATER must be in a cluster");
    assert_eq!(shared.members.len(), 2, "R55IWATER and its twin must share a cluster");
    // Spec §8: a cluster spanning two addresses is the finding worth seeing.
    assert_eq!(shared.servers.len(), 2);
}

/// Spec §4.2: minimum 30 samples before emitting. Below that the record must
/// say "insufficient", not present a fingerprint computed from too little
/// (spec §6.1).
#[test]
fn withholds_a_fingerprint_below_the_sample_floor() {
    let config = FingerprintConfig::default();
    let mut engine = FingerprintEngine::new(config.clone());
    feed(&mut engine, &R55IWATER, 20, BASE_SECS);
    let after = BASE_SECS as i128 * SEC + (config.window_secs as i128 + 1) * SEC;
    let statuses = engine.harvest(after);
    assert_eq!(statuses.len(), 1);
    assert!(statuses[0].ready().is_none(), "20 samples must not produce a fingerprint");
}

/// A tolerance-based Euclidean GCD over *raw values* -- exactly the estimator
/// spec §2.2 forbids.
///
/// Reproduced here, deliberately wrong, so the offset dependence is
/// demonstrated on the same data the engine sees rather than asserted in a
/// comment. Not production code and not called by anything else.
fn naive_raw_value_gcd(values: &[f64]) -> f64 {
    fn pair(a: f64, b: f64) -> f64 {
        let (mut a, mut b) = (a.abs().max(b.abs()), a.abs().min(b.abs()));
        while b > 1e-9 {
            let r = a - (a / b).round() * b;
            a = b;
            b = r.abs();
        }
        a
    }
    let mut distinct: Vec<f64> = values.to_vec();
    distinct.sort_by(|x, y| x.partial_cmp(y).expect("finite"));
    distinct.dedup_by(|x, y| (*x - *y).abs() < 1e-9);
    distinct.iter().skip(1).fold(distinct[0], |g, v| pair(g, *v))
}

/// Spec §2.2: the lattice comes from successive differences, never from the
/// raw values.
///
/// The fixture has a non-integral lattice offset, so the two estimators
/// genuinely disagree on this data: the raw-value GCD divides past the true
/// quantum and returns the 0.0025 the spec names, while the engine returns
/// 0.0175. Both halves are checked, so the test fails whether the engine
/// regresses to raw values or the fixture stops being able to tell the
/// difference.
#[test]
fn does_not_report_the_offset_dependent_lattice() {
    let values: Vec<f64> =
        LATTICE_WALK.iter().map(|k| R55IWATER.base_value + *k as f64 * R55IWATER.lsb).collect();
    let naive = naive_raw_value_gcd(&values);
    assert!(
        (naive - 0.0025).abs() < 1e-9,
        "the fixture must exhibit the offset dependence it guards against; raw GCD gave {}",
        naive
    );

    let fps = harvest_both(40);
    let lsb = find(&fps, R55IWATER.channel).value_lsb.expect("lattice missing");
    assert!((lsb - naive).abs() > 1e-6, "reported the offset-dependent GCD");
    assert!((lsb - R55IWATER.lsb).abs() < 1e-9, "value_lsb {}", lsb);
}

/// Spec §7: the exported line protocol carries the reference values.
#[test]
fn exports_the_reference_values_in_line_protocol() {
    let fps = harvest_both(40);
    let fp = find(&fps, R55IWATER.channel);
    let line = fingerprint_line_protocol(fp, "testhost", 1_755_757_000_000_000_000);
    assert!(line.starts_with("pv_fingerprint,"));
    assert!(line.contains("channel=R55IWATER::AMBIENT:TEMPERATURE"), "{line}");
    assert!(line.contains("server=10.0.0.11:5075"), "{line}");
    assert!(line.contains("struct_id=epics:nt/NTScalar:1.0"), "{line}");
    assert!(line.contains("sample_count=40i"), "{line}");
    assert!(line.contains("has_nan_limits=false"), "{line}");

    // The exported numbers are checked by parsing them back out and holding
    // them to the same bounds as the record itself, rather than by
    // string-matching a decimal rendering: `{}` on an f64 prints the shortest
    // round-tripping form, so a literal comparison would be testing Rust's
    // float formatter as much as the exporter.
    assert!(
        (field(&line, "phase_mean_ms") - R55IWATER.phase_ms).abs() < PHASE_EXACT_TOL_MS,
        "{line}"
    );
    assert!(
        (field(&line, "phase_spread_us") - R55IWATER.spread_us).abs() < SPREAD_TOL_US,
        "{line}"
    );
    assert!((field(&line, "value_lsb") - R55IWATER.lsb).abs() < 1e-9, "{line}");
    assert!((field(&line, "cadence_modal_s") - R55IWATER.modal_s).abs() < CADENCE_TOL_S, "{line}");
    assert!((field(&line, "cadence_p95_s") - REF_P95_S).abs() < CADENCE_TOL_S, "{line}");
}

/// Pull one numeric field out of a line-protocol line.
fn field(line: &str, name: &str) -> f64 {
    let fields = line.split(' ').nth(1).expect("line protocol has a field set");
    fields
        .split(',')
        .find_map(|kv| kv.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')))
        .unwrap_or_else(|| panic!("field {name} missing from {line}"))
        .parse()
        .expect("numeric field")
}

/// Spec §5 D1, end to end: a channel that starts arriving with the other
/// reference channel's phase has been re-sourced.
#[test]
fn detects_a_re_source_between_the_two_reference_phases() {
    let mut det = Detector::new(DetectorConfig { d1_enabled: true, ..Default::default() });
    let first = harvest_both(40);
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_755_756_900);
    assert!(det.ingest(&first, now).iter().all(|e| e.rule != DetectionRule::D1SourceChange));

    // Same channel name and server, now stamping at the other phase.
    let moved = Reference { phase_ms: R4WATER.phase_ms, ..R55IWATER };
    let second = harvest_all(&[moved], 40, BASE_SECS + 1000);

    let later = now + std::time::Duration::from_secs(900);
    let events = det.ingest(&second, later);
    let d1: Vec<_> = events.iter().filter(|e| e.rule == DetectionRule::D1SourceChange).collect();
    assert_eq!(d1.len(), 1);
    assert_eq!(d1[0].channel, R55IWATER.channel);
    assert!(d1[0].prior.is_some(), "the event must carry the prior fingerprint (spec §7)");
}
