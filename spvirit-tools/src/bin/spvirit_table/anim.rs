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

/// Build an `AnimSpec` from a generator name and raw `key=value` params.
/// Unknown generators or unparsable params are errors; unknown param keys for
/// a generator are ignored (forward-compatible).
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
    let mut p = Params::default();
    // `period` for cycle defaults to 1.0
    if g == Generator::Cycle {
        p.period = 1.0;
    }
    for (k, v) in params {
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
}
