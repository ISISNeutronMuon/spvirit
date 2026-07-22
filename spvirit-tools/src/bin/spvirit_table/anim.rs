//! Animation generators: pure sampling of a value as a function of time.

use std::f64::consts::PI;

/// Tiny xorshift64 PRNG — deterministic, seedable, no external dependency.
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Avoid the all-zero state, which xorshift cannot leave.
        Rng { state: seed | 1 }
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Generator {
    Sine,
    Ramp,
    Triangle,
    Square,
    Noise,
    Walk,
    Count,
    Cycle,
}

#[derive(Copy, Clone, Debug)]
pub struct Params {
    pub amp: f64,
    pub offset: f64,
    pub period: f64,
    pub phase: f64,
    pub min: f64,
    pub max: f64,
    pub lo: f64,
    pub hi: f64,
    pub duty: f64,
    pub start: f64,
    pub step: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            amp: 1.0, offset: 0.0, period: 10.0, phase: 0.0,
            min: 0.0, max: 1.0, lo: 0.0, hi: 1.0, duty: 0.5,
            start: 0.0, step: 1.0,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct AnimSpec {
    pub generator: Generator,
    pub p: Params,
}

pub struct AnimState {
    rng: Rng,
    count: f64,
    walk: f64,
    walk_init: bool,
}

impl AnimState {
    pub fn new(seed: u64) -> Self {
        AnimState { rng: Rng::new(seed), count: 0.0, walk: 0.0, walk_init: false }
    }
}

pub fn is_enum_only(g: &Generator) -> bool {
    matches!(g, Generator::Cycle)
}

impl Params {
    /// Look up a resolved param value by its key (the inverse of the assignment
    /// in `build_anim`). Panics on an unknown key — callers only ever pass keys
    /// from `Generator::param_keys`, so a bad key is a programming error.
    fn get(&self, key: &str) -> f64 {
        match key {
            "amp" => self.amp,
            "offset" => self.offset,
            "period" => self.period,
            "phase" => self.phase,
            "min" => self.min,
            "max" => self.max,
            "lo" => self.lo,
            "hi" => self.hi,
            "duty" => self.duty,
            "start" => self.start,
            "step" => self.step,
            other => panic!("Params::get: unknown key {other:?}"),
        }
    }
}

impl Generator {
    /// Every generator, in a stable display order (used by help + dumps).
    pub const ALL: [Generator; 8] = [
        Generator::Sine,
        Generator::Ramp,
        Generator::Triangle,
        Generator::Square,
        Generator::Noise,
        Generator::Walk,
        Generator::Count,
        Generator::Cycle,
    ];

    /// The canonical command-line name (inverse of `build_anim`'s match).
    pub fn name(self) -> &'static str {
        match self {
            Generator::Sine => "sine",
            Generator::Ramp => "ramp",
            Generator::Triangle => "triangle",
            Generator::Square => "square",
            Generator::Noise => "noise",
            Generator::Walk => "walk",
            Generator::Count => "count",
            Generator::Cycle => "cycle",
        }
    }

    /// The parameter keys this generator actually reads in `sample`. A key
    /// outside this set is rejected by `build_anim` (rather than silently
    /// ignored) so that e.g. `sine min=-2` — which does nothing, since sine's
    /// range is `amp`/`offset` — fails loudly instead of misleading the user.
    fn param_keys(self) -> &'static [&'static str] {
        match self {
            Generator::Sine => &["amp", "offset", "period", "phase"],
            Generator::Ramp => &["min", "max", "period"],
            Generator::Triangle => &["min", "max", "period"],
            Generator::Square => &["lo", "hi", "period", "duty"],
            Generator::Noise => &["min", "max"],
            Generator::Walk => &["start", "step", "min", "max"],
            Generator::Count => &["start", "step"],
            Generator::Cycle => &["period"],
        }
    }

    /// The default `Params` for this generator — `Params::default` with the
    /// per-generator overrides `build_anim` applies (currently only cycle's
    /// `period=1`). Single source of truth for help text and `build_anim`.
    fn default_params(self) -> Params {
        let mut p = Params::default();
        if self == Generator::Cycle {
            p.period = 1.0;
        }
        p
    }

    /// One-line `key=default` reference for this generator's params (for help).
    pub fn param_help(self) -> String {
        let p = self.default_params();
        self.param_keys()
            .iter()
            .map(|k| format!("{k}={}", p.get(k)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl AnimSpec {
    /// Render this animation's resolved params as `key=value ...` for exactly
    /// the keys its generator uses — the inverse of `build_anim`, so a dumped
    /// `anim <name> <gen> <dump_params>` round-trips back to the same spec.
    pub fn dump_params(&self) -> String {
        self.generator
            .param_keys()
            .iter()
            .map(|k| format!("{k}={}", self.p.get(k)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Build an `AnimSpec` from a generator name and raw `key=value` params.
/// Unknown generators, unparsable values, and params the chosen generator does
/// not use are all errors (the last so that inapplicable params fail loudly
/// rather than being silently ignored).
pub fn build_anim(generator: &str, params: &[(String, String)]) -> Result<AnimSpec, String> {
    let g = match generator {
        "sine" => Generator::Sine,
        "ramp" => Generator::Ramp,
        "triangle" => Generator::Triangle,
        "square" => Generator::Square,
        "noise" => Generator::Noise,
        "walk" => Generator::Walk,
        "count" => Generator::Count,
        "cycle" => Generator::Cycle,
        other => return Err(format!("unknown generator {other:?}")),
    };
    let keys = g.param_keys();
    let mut p = g.default_params();
    for (k, v) in params {
        if !keys.contains(&k.as_str()) {
            return Err(format!(
                "{generator}: param {k:?} is not used by {generator} (uses: {})",
                keys.join(", ")
            ));
        }
        let f: f64 = v.parse().map_err(|_| format!("{generator}: param {k}={v:?} is not a number"))?;
        match k.as_str() {
            "amp" => p.amp = f,
            "offset" => p.offset = f,
            "period" => p.period = f,
            "phase" => p.phase = f,
            "min" => p.min = f,
            "max" => p.max = f,
            "lo" => p.lo = f,
            "hi" => p.hi = f,
            "duty" => p.duty = f,
            "start" => p.start = f,
            "step" => p.step = f,
            _ => {} // ignore unknown keys
        }
    }
    if p.period <= 0.0 {
        return Err(format!("{generator}: period must be positive"));
    }
    Ok(AnimSpec { generator: g, p })
}

/// Sample the generator at time `t` (seconds since animation start). Mutates
/// `st` for stateful generators (`walk`, `count`). Returns a raw number; the
/// caller coerces to the PV's wire type (or, for `cycle`, to an enum index).
pub fn sample(spec: &AnimSpec, st: &mut AnimState, t: f64) -> f64 {
    let p = &spec.p;
    match spec.generator {
        Generator::Sine => p.offset + p.amp * (2.0 * PI * t / p.period + p.phase).sin(),
        Generator::Ramp => {
            let frac = (t / p.period).rem_euclid(1.0);
            p.min + (p.max - p.min) * frac
        }
        Generator::Triangle => {
            let frac = (t / p.period).rem_euclid(1.0);
            let tri = if frac < 0.5 { frac * 2.0 } else { 2.0 - frac * 2.0 };
            p.min + (p.max - p.min) * tri
        }
        Generator::Square => {
            let frac = (t / p.period).rem_euclid(1.0);
            if frac < p.duty { p.hi } else { p.lo }
        }
        Generator::Noise => p.min + (p.max - p.min) * st.rng.next_f64(),
        Generator::Walk => {
            if !st.walk_init {
                st.walk = p.start;
                st.walk_init = true;
            }
            let delta = (st.rng.next_f64() - 0.5) * 2.0 * p.step;
            st.walk = (st.walk + delta).clamp(p.min, p.max);
            st.walk
        }
        Generator::Count => {
            let v = p.start + st.count * p.step;
            st.count += 1.0;
            v
        }
        Generator::Cycle => (t / p.period).floor(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(generator: &str, params: &[(&str, &str)]) -> AnimSpec {
        let p: Vec<(String, String)> =
            params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        build_anim(generator, &p).unwrap()
    }

    #[test]
    fn sine_at_quarter_periods() {
        let s = spec("sine", &[("amp", "1"), ("offset", "0"), ("period", "4"), ("phase", "0")]);
        let mut st = AnimState::new(1);
        assert!((sample(&s, &mut st, 0.0) - 0.0).abs() < 1e-9);
        assert!((sample(&s, &mut st, 1.0) - 1.0).abs() < 1e-9); // quarter period -> peak
        assert!((sample(&s, &mut st, 2.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn ramp_wraps() {
        let s = spec("ramp", &[("min", "0"), ("max", "10"), ("period", "10")]);
        let mut st = AnimState::new(1);
        assert!((sample(&s, &mut st, 0.0) - 0.0).abs() < 1e-9);
        assert!((sample(&s, &mut st, 5.0) - 5.0).abs() < 1e-9);
        assert!((sample(&s, &mut st, 10.0) - 0.0).abs() < 1e-9); // wrap
    }

    #[test]
    fn square_duty() {
        let s = spec("square", &[("lo", "0"), ("hi", "1"), ("period", "10"), ("duty", "0.5")]);
        let mut st = AnimState::new(1);
        assert_eq!(sample(&s, &mut st, 1.0), 1.0);
        assert_eq!(sample(&s, &mut st, 6.0), 0.0);
    }

    #[test]
    fn noise_in_range() {
        let s = spec("noise", &[("min", "-2"), ("max", "2")]);
        let mut st = AnimState::new(42);
        for k in 0..100 {
            let v = sample(&s, &mut st, k as f64);
            assert!((-2.0..=2.0).contains(&v), "noise {v} out of range");
        }
    }

    #[test]
    fn count_advances_and_cycle_is_enum_only() {
        let s = spec("count", &[("start", "0"), ("step", "1")]);
        let mut st = AnimState::new(1);
        assert_eq!(sample(&s, &mut st, 0.0), 0.0);
        assert_eq!(sample(&s, &mut st, 0.0), 1.0);
        assert_eq!(sample(&s, &mut st, 0.0), 2.0);

        let c = spec("cycle", &[("period", "2")]);
        assert!(is_enum_only(&c.generator));
        let mut cs = AnimState::new(1);
        // index grows as floor(t/period)
        assert_eq!(sample(&c, &mut cs, 0.0), 0.0);
        assert_eq!(sample(&c, &mut cs, 2.0), 1.0);
        assert_eq!(sample(&c, &mut cs, 5.0), 2.0);
    }

    #[test]
    fn unknown_generator_and_bad_param() {
        assert!(build_anim("bogus", &[]).is_err());
        assert!(build_anim("sine", &[("amp".into(), "x".into())]).is_err());
    }

    #[test]
    fn param_help_and_dump_params_round_trip() {
        // Every generator's help lists exactly its param keys with real defaults.
        for g in Generator::ALL {
            let help = g.param_help();
            for k in g.param_keys() {
                assert!(help.contains(&format!("{k}=")), "{} help missing {k}: {help}", g.name());
            }
        }
        // cycle's period default is 1 (the build_anim override), not 10.
        assert_eq!(Generator::Cycle.param_help(), "period=1");
        assert_eq!(Generator::Sine.param_help(), "amp=1 offset=0 period=10 phase=0");

        // dump_params of a built spec round-trips back through build_anim.
        let spec = build_anim("sine", &[("amp".into(), "1.5".into()), ("offset".into(), "-0.5".into())]).unwrap();
        let dumped = spec.dump_params(); // e.g. "amp=1.5 offset=-0.5 period=10 phase=0"
        assert!(dumped.contains("amp=1.5") && dumped.contains("offset=-0.5"));
        let kvs: Vec<(String, String)> = dumped
            .split_whitespace()
            .map(|kv| { let (k, v) = kv.split_once('=').unwrap(); (k.to_string(), v.to_string()) })
            .collect();
        let rebuilt = build_anim("sine", &kvs).unwrap();
        assert_eq!(rebuilt.p.amp, 1.5);
        assert_eq!(rebuilt.p.offset, -0.5);
    }

    #[test]
    fn rejects_params_a_generator_does_not_use() {
        // sine's range is amp/offset, not min/max — min must be rejected, not
        // silently ignored (the bug: sine min=-2 stayed at the default ±1).
        let err = build_anim("sine", &[("min".into(), "-2".into())]).unwrap_err();
        assert!(err.contains("min"), "error should name the offending key: {err}");
        assert!(err.contains("amp"), "error should list the params sine uses: {err}");

        // square uses lo/hi, not min/max.
        assert!(build_anim("square", &[("max".into(), "1".into())]).is_err());
        // count uses start/step, not min/max.
        assert!(build_anim("count", &[("min".into(), "0".into())]).is_err());

        // applicable params still build fine.
        assert!(build_anim("sine", &[("amp".into(), "1.5".into()), ("offset".into(), "-0.5".into())]).is_ok());
        assert!(build_anim("noise", &[("min".into(), "-2".into()), ("max".into(), "1".into())]).is_ok());
        assert!(build_anim("ramp", &[("min".into(), "-2".into()), ("max".into(), "1".into()), ("period".into(), "5".into())]).is_ok());
    }
}
