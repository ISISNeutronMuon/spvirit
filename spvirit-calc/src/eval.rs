use std::cell::Cell;

use crate::op::Op;
use crate::parse::Expression;

// Backing generator for `Op::Rndm` (`RNDM`, `refs/calcPerform.c:296-298`
// `case RANDOM`: `*++ptop = calcRandom();`).
//
// State lives in a `thread_local! Cell<u64>`, not a field on `Expression`:
// `eval` must stay `&self`, and a `Cell` field would make `Expression`
// `!Sync` and give every clone its own diverging stream. A `thread_local`
// keeps `Expression` plain data (`Vec<Op>`, all `Copy`/`Send`/`Sync`
// payloads) - see `expression_keeps_its_auto_traits_after_adding_rndm`
// below - and each thread gets its own independent state, so concurrent
// `eval()` calls never race. See `Expression::eval`'s doc for what this
// means for callers: `RNDM` draws from one shared per-thread stream, not
// per-`Expression` state; `eval_with_rng` exists for callers who need a
// private, reproducible stream instead.
//
// Generator: xorshift64* (Marsaglia/Vigna) - short, dependency-free, well
// distributed enough for this purpose. **Not cryptographically secure**,
// and makes **no attempt** to be bit-compatible with C's `rand()`
// (`calcPerform.c:509-521`): `rand()`'s algorithm and `RAND_MAX` are
// libc-specific, so there is no single sequence to match, and Base's own
// `RNDM` output is equally platform-dependent for the same reason. This
// crate reproduces Base's documented value *range* only, not its sequence.
//
// Seeded once per thread from `std::time` (part of `std`, so the crate
// stays zero-dependency), mirroring `calcRandom`'s lazy one-time `srand()`
// (`calcPerform.c:511-518`) without its exact seed source
// (`epicsTimeGetMonotonic`, not part of `std`).
//
// Range: `calcRandom` returns `rand() / RAND_MAX` (`calcPerform.c:520`),
// and `rand()` can return `RAND_MAX` itself, so Base's documented range is
// the CLOSED interval `[0, 1]`, not the half-open `[0, 1)` most Rust RNGs
// produce. Reproduced here by dividing by `2^53 - 1`, not `2^53`, so the
// maximum mantissa maps to exactly `1.0`.
thread_local! {
    static RNDM_STATE: Cell<u64> = Cell::new(seed_from_time());
}

/// One-time-per-thread seed, mirroring `calcRandom`'s lazy `srand()` but
/// sourced from `std::time` rather than `epicsTimeGetMonotonic` (not part of
/// `std`, and pulling in a dependency for it would violate the
/// zero-runtime-dependency constraint).
fn seed_from_time() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // xorshift64* requires a nonzero seed to ever produce nonzero output;
    // mix in a fixed odd constant and floor at 1 so a `SystemTime` read of
    // exactly the epoch (or a clock that fails outright, falling back to 0
    // above) can't zero-lock the generator.
    (nanos ^ 0x9E37_79B9_7F4A_7C15).max(1)
}

/// Crate-internal test hook: seed the calling thread's `RNDM` generator
/// deterministically. `RNDM` is otherwise the one genuinely nondeterministic
/// part of this crate's public API, which would make it untestable by value
/// without this - not exported outside the crate, so nothing in the public
/// API can force a caller's sequence.
#[cfg(test)]
pub(crate) fn seed_rndm(seed: u64) {
    RNDM_STATE.with(|cell| cell.set(seed.max(1)));
}

/// Draw the next `[0, 1]`-inclusive value and advance this thread's state.
fn next_rndm() -> f64 {
    RNDM_STATE.with(|cell| {
        let mut x = cell.get();
        // xorshift64* (Marsaglia/Vigna): three shift-xors advance the state,
        // then a final multiply by an odd constant mixes the output - the
        // "*" in the name. Not cryptographic; see the module-level doc.
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        cell.set(x);
        let mixed = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        // Top 53 bits become the mantissa, scaled by `2^53 - 1` (not `2^53`)
        // so the maximum possible mantissa maps to exactly `1.0` - matching
        // `calcRandom`'s documented CLOSED `[0, 1]` range (see module doc),
        // not the half-open `[0, 1)` a `/ 2^53` scaling would give.
        (mixed >> 11) as f64 / ((1u64 << 53) - 1) as f64
    })
}

/// Convert to the signed 32-bit integer EPICS bitwise operators work on.
///
/// Mirrors `calcPerform.c:325`'s macro exactly:
/// `#define d2i(x) ((x)<0?(epicsInt32)(x):(epicsInt32)(epicsUInt32)(x))`
///
/// The twelve-line comment at `calcPerform.c:314-324` explains why the cast
/// is asymmetric by SIGN rather than by magnitude: a negative double casts
/// straight to `epicsInt32`, but a non-negative one casts to `epicsUInt32`
/// FIRST and is then bit-reinterpreted as signed. This is what lets e.g.
/// `2_863_311_530.0` - too big for `i32` (max ~2.1e9) but within `u32`
/// (max ~4.3e9) - become the `u32` bit pattern `0xAAAAAAAA` and then the
/// *negative* `i32` `-1_431_655_766`, landing on the exact same value a
/// direct negative double with that bit pattern reaches via the `x < 0`
/// branch - rather than saturating to `i32::MAX` the way a naive `as i32`
/// on the positive double would. See
/// `d2i_bit31_asymmetry_positive_and_negative_agree_where_naive_cast_would_not`
/// (eval.rs tests) and `refs/epicsCalcTest.cpp:1052-1053`, which assert
/// exactly this identity.
///
/// The result of every bitwise op built on this is a SIGNED `epicsInt32`
/// widened back to `f64` - a result with bit 31 set comes out negative, by
/// Base's own design (same twelve-line comment): "so avoid problems when
/// writing the value to signed integer fields".
///
/// Rust-vs-C divergence, deliberately chosen and documented (per task
/// instructions, since there is no ground truth to match): C's
/// double->int/uint cast is undefined behavior outside the target type's
/// range (and for NaN); Rust's `as` saturates instead (`f64::NAN as i32` is
/// `0`; out-of-range magnitudes clamp to the type's MIN/MAX). x86-64's
/// `cvttsd2si` instruction - what a real EPICS IOC most likely runs it as -
/// produces a THIRD, again different, "integer indefinite" answer
/// (`i32::MIN`) for every out-of-range/NaN input, which Rust's `as` does not
/// reproduce either. Since Base itself has no defined behavior for these
/// inputs (the entire point of `d2i`/`d2ui`'s trick is to avoid landing on
/// an out-of-range *intermediate* magnitude for values that matter - see
/// `bitwise_ops_never_panic_on_nan_or_infinity`, which only requires no
/// panic, not agreement with any particular platform), this crate picks
/// Rust's own saturating `as` behavior: deterministic, panic-free, and the
/// least surprising choice for a Rust API to make.
fn d2i(x: f64) -> i32 {
    if x < 0.0 {
        x as i32
    } else {
        (x as u32) as i32
    }
}

/// Mirrors `calcPerform.c:326`'s macro exactly:
/// `#define d2ui(x) ((x)<0?(epicsUInt32)(epicsInt32)(x):(epicsUInt32)(x))`
///
/// The mirror image of `d2i`: a negative double casts to `epicsInt32` first
/// and is then bit-reinterpreted as unsigned, while a non-negative one casts
/// directly. Used only by `Op::ShrLogic` (`>>>`), the sole bitwise op that
/// operates on the unsigned reinterpretation rather than the signed one -
/// see `d2i`'s doc for the shared Rust-vs-C cast divergence this inherits.
fn d2ui(x: f64) -> u32 {
    if x < 0.0 {
        (x as i32) as u32
    } else {
        x as u32
    }
}

impl Expression {
    /// Evaluate against operands `A`-`U`.
    ///
    /// The array is 21 wide (`CALCPERFORM_NARGS`, `refs/postfix.h:29`), not
    /// the brief's 12 - see RULINGS.md Ruling 1.
    ///
    /// Never fails: numeric edge cases propagate as `inf` or `NaN`, matching
    /// EPICS `calcPerform` (`refs/calcPerform.c`). An empty expression
    /// evaluates to `NaN`. This holds for every input `compile` accepts —
    /// including `?:`, which genuinely short-circuits as of Task 5 (see the
    /// `Op::CondIf`/`Op::CondElse`/`Op::CondEnd` arms below): the untaken
    /// branch's instructions are never reached, matching
    /// `calcPerform.c:400-411`, not evaluated-then-discarded the way Task
    /// 3's placeholder `Op::Cond` used to.
    ///
    /// # Evaluation may WRITE to `args`
    ///
    /// Takes `&mut [f64; 21]`, not `&`. RULINGS.md Ruling 2 left the choice
    /// between write-back and returning a separate store set; task-8a-brief.md
    /// settled it on write-back, because Base's own
    /// `calcPerform(double *parg, ...)` takes a non-const pointer and the
    /// `STORE_A`..`STORE_U` opcodes assign straight through it
    /// (`refs/calcPerform.c:102-124`), so write-back is the Base-faithful
    /// shape and the standing "Base wins" rule selects it.
    ///
    /// Which slots can be written: **any** of the 21, determined entirely by
    /// the compiled expression. Every `:=` in the source compiles to an
    /// `Op::Store(i)` that assigns `args[i]`. An expression containing no
    /// `:=` never writes (verified by
    /// `eval_does_not_write_args_without_a_store`), but the signature cannot
    /// express that distinction, so callers who need their input preserved
    /// must copy it themselves. Task 8's `calcArgUsage`-equivalent stores
    /// mask will let callers learn which slots a given expression can write
    /// before evaluating it.
    ///
    /// `RNDM` makes this method's output nondeterministic in three ways a
    /// caller can't see from the signature: (1) draws come from a
    /// thread-local generator, invisible here; (2) that generator is shared
    /// per-thread, not per-`Expression` - evaluating one `Expression`
    /// advances the stream that every other `Expression` on the same thread
    /// also draws from; (3) each draw lies in the CLOSED interval `[0, 1]`
    /// (matching `calcRandom`'s documented range), unlike the half-open
    /// `[0, 1)` most Rust RNGs produce. Callers who need a reproducible or
    /// isolated sequence should use [`Expression::eval_with_rng`] instead.
    pub fn eval(&self, args: &mut [f64; 21]) -> f64 {
        self.run(args, &mut next_rndm)
    }

    /// Like [`Expression::eval`], but draws `RNDM` from the caller-supplied
    /// `rng` instead of the shared thread-local generator - the reproducible
    /// alternative to `eval`'s entropy-seeded default, for callers (e.g. a
    /// differential-testing oracle, or a determinism test) that need to pin
    /// or replay `RNDM`'s output.
    ///
    /// Deliberately does NOT make plain `eval` deterministic by seeding a
    /// generator from the compiled program instead of the clock (a design
    /// the brief for this feature originally proposed): `calcRandom`
    /// (`refs/calcPerform.c:509-521`) seeds once from
    /// `epicsTimeGetMonotonic` and every draw after that genuinely varies -
    /// an IOC calc record returning the same "random" value on every scan
    /// would be a behavioral bug, not a feature. RULINGS.md's standing rule
    /// (Base wins wherever the plan and Base disagree) governs here, so
    /// `eval`'s default stays entropy-seeded and `eval_with_rng` is purely
    /// an opt-in escape hatch, not the normal path.
    pub fn eval_with_rng(&self, args: &mut [f64; 21], rng: &mut dyn FnMut() -> f64) -> f64 {
        self.run(args, rng)
    }

    fn run(&self, args: &mut [f64; 21], rng: &mut dyn FnMut() -> f64) -> f64 {
        // Stack discipline: `check_arity` (parse.rs) statically proves every
        // well-formed `Expression` never pops below depth 0 or leaves more
        // than one value at the end, so the `.expect()` calls below are a
        // documented invariant, not a real runtime possibility - they exist
        // to fail loudly (rather than silently produce a wrong number, e.g.
        // defaulting a starved pop to 0.0) if that invariant is ever broken
        // by a future parser bug. A panic on a broken invariant is
        // preferable to `calcPerform`-style silent misbehavior, since this
        // is a compile-time-checked precondition, not a runtime edge case -
        // those (div-by-zero, etc.) are handled below without panicking.
        let mut stack: Vec<f64> = Vec::with_capacity(16);
        let ops = &self.ops;
        // An instruction-pointer loop rather than a `for op in ops`
        // iteration, so `Op::CondIf`/`Op::CondElse` can jump `ip` forward
        // and genuinely skip the untaken branch's instructions - a plain
        // iterator has no way to skip ahead. `check_arity` (parse.rs) is
        // what guarantees every jump target driven by `Op::CondIf`'s/
        // `Op::CondElse`'s stored `usize`s stays within `0..=ops.len()`,
        // so indexing and the loop bound below never go out of range.
        let mut ip: usize = 0;
        while ip < ops.len() {
            let op = &ops[ip];
            match op {
                Op::Arg(i) => stack.push(args[*i]),
                Op::Lit(v) => stack.push(*v),
                // calcPerform.c:138-140 `case UNARY_NEG`: `*ptop = -*ptop`.
                Op::Neg => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(-a);
                }
                // calcPerform.c:142-145 `case ADD`: `*ptop += top`.
                Op::Add => {
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a + b);
                }
                // calcPerform.c:147-150 `case SUB`: `*ptop -= top`.
                Op::Sub => {
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a - b);
                }
                // calcPerform.c:152-155 `case MULT`: `*ptop *= top`.
                Op::Mul => {
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a * b);
                }
                // calcPerform.c:157-160 `case DIV`: `*ptop /= top`. Plain
                // f64 division, so div-by-zero yields inf/NaN via IEEE 754
                // rather than a panic or Err - matching Base and the global
                // "Err is compile-time only" constraint.
                Op::Div => {
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a / b);
                }
                // calcPerform.c:162-168 `case MODULO`
                // (RULINGS.md Ruling 5): both operands truncate to
                // `epicsInt32` and C's `%` applies; a zero divisor yields
                // `epicsNAN`, not inf and not an f64 remainder/fmod. Rust's
                // `as i32` float-to-int cast saturates instead of the C
                // cast's undefined behavior on out-of-range values, which
                // is a deliberate, safer divergence for inputs outside
                // `epicsInt32`'s range (not reachable by any test here).
                Op::Modulo => {
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    let ib = b as i32;
                    stack.push(if ib == 0 {
                        f64::NAN
                    } else {
                        ((a as i32) % ib) as f64
                    });
                }
                // calcPerform.c:170-173 `case POWER`: `pow(*ptop, top)`.
                Op::Pow => {
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.powf(b));
                }
                // Task 5 replaces Task 3's eager "pop three, select one"
                // placeholder with Base's real short-circuit shape
                // (`calcPerform.c:400-411`). `CondIf`/`CondElse`/`CondEnd`
                // together implement it as a jump over the untaken
                // branch's instructions - they are never executed, not
                // evaluated-then-discarded.
                //
                // calcPerform.c:400-403 `case COND_IF`:
                // `if (*ptop-- == 0.0 && cond_search(...)) return -1;` -
                // pop the condition; a zero condition jumps into the
                // else-branch (`*else_target`), anything else (including
                // NaN, which is `!= 0.0`) falls through into the
                // then-branch that immediately follows this instruction.
                Op::CondIf { else_target } => {
                    let cond = stack.pop().expect("arity checked at compile time");
                    if cond == 0.0 {
                        ip = *else_target;
                        continue;
                    }
                }
                // calcPerform.c:405-407 `case COND_ELSE`:
                // `if (cond_search(...)) return -1;`. Only reached by
                // falling off the end of a then-branch (a `CondIf` jump
                // lands past this, at `else_target`, never on it) - and
                // when reached, unconditionally jumps to `end_target`,
                // skipping the else-branch that follows. No condition to
                // check here: by the time control reaches this opcode at
                // all, the then-branch already ran and its result is the
                // answer, so the else-branch must not run too.
                Op::CondElse { end_target } => {
                    ip = *end_target;
                    continue;
                }
                // calcPerform.c:409-410 `case COND_END`: `break;` - a pure
                // no-op landing pad, reached either by falling off the end
                // of an else-branch (normal execution) or by a `CondElse`
                // jump (see above).
                Op::CondEnd => {}

                // Unary algebraic/transcendental functions (Task 4). Each
                // cited line is the corresponding `case` in
                // `refs/calcPerform.c`.
                Op::Abs => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.abs()); // calcPerform.c:175-177 ABS_VAL: fabs
                }
                Op::Sqrt => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.sqrt()); // calcPerform.c:209-211 SQU_RT: sqrt
                }
                Op::Exp => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.exp()); // calcPerform.c:179-181 EXP: exp
                }
                Op::Log10 => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.log10()); // calcPerform.c:183-185 LOG_10: log10
                }
                Op::Ln => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.ln()); // calcPerform.c:187-189 LOG_E: log
                }
                Op::Ceil => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.ceil()); // calcPerform.c:254-256 CEIL: ceil
                }
                Op::Floor => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.floor()); // calcPerform.c:258-260 FLOOR: floor
                }
                // calcPerform.c:291-294 `case NINT`:
                // `(epicsInt32)(top >= 0 ? top+0.5 : top-0.5)` - round
                // half-away-from-zero via a truncating int cast (task
                // instructions, trap 2).
                //
                // Correction from an earlier version of this comment
                // (review Important 2): for every value in bounds,
                // `top >= 0 ? top+0.5 : top-0.5` truncated to an int is
                // bit-identical to `top.round()`, since Rust's `f64::round`
                // already rounds half-away-from-zero rather than banker's
                // rounding. `nint_rounds_half_away_from_zero_at_boundaries`
                // (eval.rs tests) only proves this ISN'T banker's rounding
                // - it does not, by itself, distinguish this cast-based
                // implementation from a bare `top.round()`. The genuine
                // discriminator is magnitude large enough to overflow
                // `epicsInt32`: Rust's `as i32` saturates instead of C's
                // undefined-behavior cast (the same deliberate, safer
                // divergence Task 3 documented for `%`, see `Op::Modulo`
                // above), whereas `top.round()` alone would return the
                // unsaturated double. See
                // `nint_saturates_on_i32_overflow_matching_rust_cast_semantics`.
                Op::Nint => {
                    let a = stack.pop().expect("arity checked at compile time");
                    let shifted = if a >= 0.0 { a + 0.5 } else { a - 0.5 };
                    stack.push((shifted as i32) as f64);
                }
                Op::Sin => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.sin()); // calcPerform.c:234-236 SIN: sin
                }
                Op::Cos => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.cos()); // calcPerform.c:230-232 COS: cos
                }
                Op::Tan => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.tan()); // calcPerform.c:238-240 TAN: tan
                }
                Op::Asin => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.asin()); // calcPerform.c:217-219 ASIN: asin
                }
                Op::Acos => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.acos()); // calcPerform.c:213-215 ACOS: acos
                }
                Op::Atan => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.atan()); // calcPerform.c:221-223 ATAN: atan
                }
                // calcPerform.c:225-228, Base's own comment: "Ouch!: Args
                // backwards!" - `top = *ptop--` pops the SECOND pushed
                // argument (B) into `top`, then `*ptop = atan2(top, *ptop)`
                // computes `atan2(B, A)`. So `ATAN2(A,B)` evaluates to
                // `atan2(B, A)`, not the naively-expected `atan2(A, B)`.
                // Do NOT "fix" this to `a.atan2(b)` - that would silently
                // reverse Base's (deliberately preserved) bug. Rust's
                // `x.atan2(y)` computes `atan2(x, y)` with `x` as `self`, so
                // reproducing `atan2(B, A)` is `b.atan2(a)`.
                //
                // Task-4-brief.md's own Step 5 snippet has this backwards
                // (`a.atan2(b)`) - a divergence from Base caught only by
                // testing with asymmetric arguments; see
                // `atan2_arguments_are_backwards_matching_base` in the test
                // module below, which the brief's own symmetric
                // A=B=1.0 example cannot detect.
                Op::Atan2 => {
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(b.atan2(a));
                }
                Op::Sinh => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.sinh()); // calcPerform.c:246-248 SINH: sinh
                }
                Op::Cosh => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.cosh()); // calcPerform.c:242-244 COSH: cosh
                }
                Op::Tanh => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(a.tanh()); // calcPerform.c:250-252 TANH: tanh
                }

                // calcPerform.c:191-207 `case MAX`/`case MIN` (task
                // instructions, trap 3): each pops `nargs` values, and the
                // comparison `if (*ptop < top || isnan(top))` (MAX; MIN uses
                // `>`) means a NaN challenger always dethrones the current
                // champion, and a NaN champion is never dethroned by a
                // finite challenger (comparisons against NaN are always
                // false). Tracing every possible position shows this always
                // collapses to: any NaN anywhere in the argument list makes
                // the whole result NaN. task-4-brief.md's own Step 5
                // snippet (a `fold` seeded with a NaN sentinel, using
                // `acc.min`/`acc.max`) does NOT reproduce this - Rust's
                // `f64::min`/`f64::max` ignore NaN, so that version silently
                // drops a NaN argument instead of propagating it. Written
                // directly against the any-NaN-wins conclusion instead.
                Op::Min(n) | Op::Max(n) => {
                    let at = stack.len() - n;
                    let tail = stack.split_off(at);
                    let result = if tail.iter().any(|v| v.is_nan()) {
                        f64::NAN
                    } else if matches!(op, Op::Min(_)) {
                        tail.into_iter().fold(f64::INFINITY, f64::min)
                    } else {
                        tail.into_iter().fold(f64::NEG_INFINITY, f64::max)
                    };
                    stack.push(result);
                }

                // calcPerform.c:296-298 `case RANDOM`: `*++ptop =
                // calcRandom();` - pushes without popping (see `Op::Rndm`'s
                // doc in op.rs and this module's `RNDM_STATE` doc for the
                // generator design).
                Op::Rndm => stack.push(rng()),

                // RULINGS.md Ruling 6 / calcPerform.c:277-279 `case ISINF`:
                // `*ptop = isinf(*ptop);` - strictly unary, no fold.
                Op::IsInf => {
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(if a.is_infinite() { 1.0 } else { 0.0 });
                }
                // calcPerform.c:281-289 `case ISNAN`: OR-fold over `n`
                // arguments - true if ANY is NaN.
                Op::IsNan(n) => {
                    let at = stack.len() - n;
                    let tail = stack.split_off(at);
                    let result = tail.iter().any(|v| v.is_nan());
                    stack.push(if result { 1.0 } else { 0.0 });
                }
                // calcPerform.c:267-275 `case FINITE`: AND-fold over `n`
                // arguments - true only if EVERY one is finite. The opposite
                // fold from `ISNAN` immediately above - this asymmetry is
                // the trap RULINGS.md Ruling 6 and the task instructions
                // both flag.
                Op::Finite(n) => {
                    let at = stack.len() - n;
                    let tail = stack.split_off(at);
                    let result = tail.iter().all(|v| v.is_finite());
                    stack.push(if result { 1.0 } else { 0.0 });
                }

                // Relational (Task 5): all yield exactly `1.0`/`0.0`. Rust's
                // `f64` comparison operators already follow IEEE 754 (same
                // as C's `double` comparisons in `calcPerform.c`), so every
                // comparison against NaN is false here without any special
                // casing - including `Eq` with two NaNs, which is `0.0`.
                Op::Gt => {
                    // calcPerform.c:395-398 `case GR_THAN`: `*ptop > top`.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(if a > b { 1.0 } else { 0.0 });
                }
                Op::Ge => {
                    // calcPerform.c:390-393 `case GR_OR_EQ`: `*ptop >= top`.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(if a >= b { 1.0 } else { 0.0 });
                }
                Op::Lt => {
                    // calcPerform.c:375-378 `case LESS_THAN`: `*ptop < top`.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(if a < b { 1.0 } else { 0.0 });
                }
                Op::Le => {
                    // calcPerform.c:380-383 `case LESS_OR_EQ`: `*ptop <= top`.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(if a <= b { 1.0 } else { 0.0 });
                }
                Op::Eq => {
                    // calcPerform.c:385-388 `case EQUAL`: `*ptop == top`.
                    // `NaN == NaN` is `0.0`, matching Base/IEEE 754 - `A=B`
                    // is equality, not assignment, and this is the sole
                    // reason `NaN = NaN` is false rather than true.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(if a == b { 1.0 } else { 0.0 });
                }
                Op::Ne => {
                    // calcPerform.c:370-373 `case NOT_EQ`: `*ptop != top`.
                    // The one comparison where NaN yields `1.0`: `NaN != x`
                    // is true for every `x`, NaN included.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(if a != b { 1.0 } else { 0.0 });
                }
                // Logical (Task 5). `refs/calcPerform.c:300-312`: `REL_OR`/
                // `REL_AND`/`REL_NOT` operate on values already popped off
                // the stack - by the time these opcodes run, BOTH operands
                // have already been evaluated. This is deliberately NOT a
                // short-circuit at the operand-evaluation level (unlike
                // `Op::CondIf`/`CondElse`/`CondEnd` above, which is): Base's
                // postfix form has no way to skip evaluating one operand of
                // `&&`/`||`, so matching that faithfully here means both
                // sides always run, even though the Rust `&&`/`||` used
                // below (applied only to the already-computed truthiness of
                // each) would themselves short-circuit if that mattered
                // (it doesn't - `!= 0.0` on an already-popped f64 has no
                // side effects to skip).
                Op::AndL => {
                    // calcPerform.c:305-308 `case REL_AND`:
                    // `*ptop = *ptop && top`. C's `&&` treats any non-zero
                    // double as true, NaN included (`NaN != 0.0`), so
                    // `NaN && 1.0` is `1.0`.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(if a != 0.0 && b != 0.0 { 1.0 } else { 0.0 });
                }
                Op::OrL => {
                    // calcPerform.c:300-303 `case REL_OR`:
                    // `*ptop = *ptop || top`. Same NaN-is-truthy rule as
                    // `AndL`.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(if a != 0.0 || b != 0.0 { 1.0 } else { 0.0 });
                }
                Op::NotL => {
                    // calcPerform.c:310-312 `case REL_NOT`: `*ptop = ! *ptop`.
                    // `!NaN` is `0.0`: NaN is truthy, so its negation is
                    // false, matching C.
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push(if a == 0.0 { 1.0 } else { 0.0 });
                }

                // Bitwise (Task 6). Every arm converts through `d2i`/`d2ui`
                // (see their docs above for the bit-31 asymmetry this
                // implements) rather than a bare `as i32`/`as u32`, and every
                // result is widened back to `f64` as a SIGNED `i32` -
                // `calcPerform.c:314-324`'s comment states this explicitly,
                // and it's why e.g. `AndB`/`Shl`, which don't "look" signed
                // at the call site, can still produce a negative `f64`.
                Op::AndB => {
                    // calcPerform.c:333-336 `case BIT_AND`.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push((d2i(a) & d2i(b)) as f64);
                }
                Op::OrB => {
                    // calcPerform.c:328-331 `case BIT_OR`.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push((d2i(a) | d2i(b)) as f64);
                }
                Op::XorB => {
                    // calcPerform.c:338-341 `case BIT_EXCL_OR`.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push((d2i(a) ^ d2i(b)) as f64);
                }
                Op::NotB => {
                    // calcPerform.c:343-345 `case BIT_NOT`: `~d2i(*ptop)`.
                    let a = stack.pop().expect("arity checked at compile time");
                    stack.push((!d2i(a)) as f64);
                }
                // calcPerform.c:347-353's comment: shift counts are masked
                // to 0..=31 (`d2i(top) & 31` / `d2ui(top) & 31u`) before
                // shifting a 32-bit value - both because that's what Base
                // does, and because an unmasked count would be out of range
                // for a 32-bit shift and panic under Rust's plain `<<`/`>>`
                // in debug builds (the no-panic invariant this crate
                // maintains everywhere - see
                // `shift_count_is_masked_to_five_bits_and_never_panics`).
                // `wrapping_shl`/`wrapping_shr` are additional defense: they
                // never panic regardless of shift count (masking the count
                // to the bit width internally too), so even if the explicit
                // `& 31` above were ever wrong, this couldn't panic - belt
                // and suspenders around the same invariant.
                Op::Shl => {
                    // calcPerform.c:360-363 `case LEFT_SHIFT_ARITH`:
                    // `d2i(*ptop) << (d2i(top) & 31)`.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    let shift = (d2i(b) & 31) as u32;
                    stack.push(d2i(a).wrapping_shl(shift) as f64);
                }
                Op::Shr => {
                    // calcPerform.c:355-358 `case RIGHT_SHIFT_ARITH`:
                    // `d2i(*ptop) >> (d2i(top) & 31)` - arithmetic
                    // (sign-extending) shift. Rust's `>>`/`wrapping_shr` on
                    // `i32` is arithmetic by definition, matching C's on
                    // every real platform (C's is technically
                    // implementation-defined for negative operands, but
                    // arithmetic everywhere that matters) - no divergence
                    // to paper over here, unlike the cast in `d2i` itself.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    let shift = (d2i(b) & 31) as u32;
                    stack.push(d2i(a).wrapping_shr(shift) as f64);
                }
                Op::ShrLogic => {
                    // calcPerform.c:365-368 `case RIGHT_SHIFT_LOGIC`:
                    // `*ptop = (double)(d2ui(*ptop) >> (d2ui(top) & 31u));`
                    // The C shift expression has type `epicsUInt32` - Base
                    // widens it to `double` DIRECTLY from `epicsUInt32`, not
                    // through a re-cast to `epicsInt32` first. This is the
                    // one bitwise op where the "result is always signed"
                    // rule from the `d2i`/`d2ui` twelve-line comment
                    // (`calcPerform.c:314-324`) does NOT apply - that rule
                    // is stated in the context of `d2i`, and
                    // `RIGHT_SHIFT_LOGIC` is Base's documented exception to
                    // it, not another instance of it. So the result stays
                    // in `[0, 4294967295]`, unlike every other bitwise op
                    // here (all of which route through `d2i` and do widen
                    // as signed). See
                    // `logical_shift_right_result_is_unsigned_unlike_every_other_bitwise_op`
                    // for the pinning test - a fixed value like
                    // `-1 >>> 0` must come out `4294967295.0`, not `-1.0`.
                    let b = stack.pop().expect("arity checked at compile time");
                    let a = stack.pop().expect("arity checked at compile time");
                    let shift = d2ui(b) & 31;
                    stack.push(d2ui(a).wrapping_shr(shift) as f64);
                }
            }
            ip += 1;
        }

        stack.pop().unwrap_or(f64::NAN)
    }
}

#[cfg(test)]
mod tests {
    use crate::compile;
    use super::seed_rndm;

    /// Operand array is 21 wide (A-U), per RULINGS.md Ruling 1 /
    /// `refs/postfix.h:29` (`CALCPERFORM_NARGS` = 21), not the brief's 12.
    fn ev(src: &str, args: &[f64]) -> f64 {
        let mut a = [0.0f64; 21];
        a[..args.len()].copy_from_slice(args);
        compile(src).expect("compile").eval(&mut a)
    }

    #[test]
    fn evaluates_arithmetic() {
        assert_eq!(ev("A+B", &[2.0, 3.0]), 5.0);
        assert_eq!(ev("A-B", &[2.0, 3.0]), -1.0);
        assert_eq!(ev("A*B", &[2.0, 3.0]), 6.0);
        assert_eq!(ev("A/B", &[6.0, 3.0]), 2.0);
        assert_eq!(ev("A^B", &[2.0, 10.0]), 1024.0);
    }

    #[test]
    fn respects_precedence_end_to_end() {
        assert_eq!(ev("A+B*C", &[1.0, 2.0, 3.0]), 7.0);
        assert_eq!(ev("(A+B)*C", &[1.0, 2.0, 3.0]), 9.0);
    }

    // RULINGS.md Ruling 5 / `refs/calcPerform.c:162-168` (`case MODULO`):
    // both operands are truncated to `epicsInt32` and C's `%` is applied,
    // NOT an f64 remainder/fmod. The brief's original test asserted this
    // was "fmod semantics" - that's wrong per Base, even though the two
    // integral test cases below happen to produce the same numbers either
    // way. The truncation and zero-divisor cases below only pass under
    // integer-modulo semantics.
    #[test]
    fn modulo_is_integer_not_float_remainder() {
        assert_eq!(ev("A%B", &[7.0, 3.0]), 1.0);
        assert_eq!(ev("A%B", &[-7.0, 3.0]), -1.0);
        // 7.9 truncates to 7 (epicsInt32 cast), not rounded: 7 % 3 == 1.
        assert_eq!(ev("A%B", &[7.9, 3.0]), 1.0);
    }

    #[test]
    fn modulo_by_zero_yields_nan_not_infinity() {
        // calcPerform.c:164-167: divisor casts to 0 -> epicsNAN, never inf.
        assert!(ev("A%B", &[7.0, 0.0]).is_nan());
    }

    #[test]
    fn modulo_with_negative_divisor() {
        // C truncated division: 7 % -3 == 1 (sign follows the dividend).
        assert_eq!(ev("A%B", &[7.0, -3.0]), 1.0);
    }

    #[test]
    fn division_by_zero_yields_infinity_not_error() {
        assert!(ev("A/B", &[1.0, 0.0]).is_infinite());
        assert!(ev("A/B", &[0.0, 0.0]).is_nan());
    }

    #[test]
    fn empty_expression_is_nan() {
        assert!(ev("", &[]).is_nan());
    }

    #[test]
    fn unary_minus_applies_to_operand() {
        assert_eq!(ev("-A", &[5.0]), -5.0);
        assert_eq!(ev("A*-B", &[2.0, 3.0]), -6.0);
    }

    // `Op::CondIf`/`CondElse`/`CondEnd`, evaluated with true short-circuit
    // as of Task 5 (calcPerform.c:400-411): nonzero condition selects the
    // `then` branch, zero selects `else`, and the untaken branch's
    // instructions are never reached (see the `Op::CondIf` match arm's
    // comment). These result assertions are carried over unchanged from
    // Task 3's eager "pop three, select one" placeholder - the values were
    // never expected to differ, only the mechanism producing them did.
    #[test]
    fn conditional_selects_then_branch_on_nonzero_condition() {
        assert_eq!(ev("A?B:C", &[1.0, 2.0, 3.0]), 2.0);
    }

    #[test]
    fn conditional_selects_else_branch_on_zero_condition() {
        assert_eq!(ev("A?B:C", &[0.0, 2.0, 3.0]), 3.0);
    }

    // Chained (right-associative) ternary in the else-branch, per
    // parse.rs's `chained_ternary_in_else_branch`: `A?B:C?D:E` compiles to
    // `A ? B : (C ? D : E)`.
    #[test]
    fn chained_ternary_in_else_branch_evaluates_correctly() {
        assert_eq!(ev("A?B:C?D:E", &[1.0, 10.0, 0.0, 20.0, 30.0]), 10.0);
        assert_eq!(ev("A?B:C?D:E", &[0.0, 10.0, 1.0, 20.0, 30.0]), 20.0);
        assert_eq!(ev("A?B:C?D:E", &[0.0, 10.0, 0.0, 20.0, 30.0]), 30.0);
    }

    // Nested ternary in the then-branch, per parse.rs's
    // `nested_ternary_in_then_branch`: `A?B?C:D:E` compiles to
    // `A ? (B?C:D) : E`.
    #[test]
    fn nested_ternary_in_then_branch_evaluates_correctly() {
        assert_eq!(ev("A?B?C:D:E", &[0.0, 1.0, 10.0, 20.0, 30.0]), 30.0);
        assert_eq!(ev("A?B?C:D:E", &[1.0, 1.0, 10.0, 20.0, 30.0]), 10.0);
        assert_eq!(ev("A?B?C:D:E", &[1.0, 0.0, 10.0, 20.0, 30.0]), 20.0);
    }

    // --- Task 4: algebraic and transcendental functions ---

    #[test]
    fn sqr_is_square_root_not_squaring() {
        // refs/postfix.c:137-138: SQR and SQRT are both aliases for the same
        // SQU_RT opcode (calcPerform.c:209-211) - SQR is square *root*, not
        // squaring.
        assert_eq!(ev("SQR(A)", &[9.0]), 3.0);
        assert_eq!(ev("SQRT(A)", &[9.0]), 3.0);
    }

    #[test]
    fn log_is_base_ten_and_loge_is_natural() {
        // refs/postfix.c:117-119 + refs/calcPerform.c:183-189:
        // LOG -> LOG_10 (log10), LOGE/LN -> LOG_E (natural log).
        assert_eq!(ev("LOG(A)", &[1000.0]), 3.0);
        assert_eq!(ev("LOGE(A)", &[1.0]), 0.0);
        assert_eq!(ev("LN(A)", &[1.0]), 0.0);
    }

    #[test]
    fn rounding_functions() {
        assert_eq!(ev("CEIL(A)", &[1.2]), 2.0);
        assert_eq!(ev("FLOOR(A)", &[1.8]), 1.0);
        assert_eq!(ev("NINT(A)", &[1.5]), 2.0);
        assert_eq!(ev("NINT(A)", &[-1.5]), -2.0);
    }

    // Trap 2 (task instructions): NINT is round-half-away-from-zero via
    // `(epicsInt32)(top >= 0 ? top+0.5 : top-0.5)` (calcPerform.c:291-294),
    // not Rust's f64::round in general and not banker's rounding. Exercise
    // both boundary directions and a couple of interior values so a
    // round()-only implementation and a floor/ceil-based one both fail.
    #[test]
    fn nint_rounds_half_away_from_zero_at_boundaries() {
        assert_eq!(ev("NINT(A)", &[0.5]), 1.0);
        assert_eq!(ev("NINT(A)", &[-0.5]), -1.0);
        assert_eq!(ev("NINT(A)", &[2.5]), 3.0);
        assert_eq!(ev("NINT(A)", &[-2.5]), -3.0);
        assert_eq!(ev("NINT(A)", &[2.4]), 2.0);
        assert_eq!(ev("NINT(A)", &[-2.4]), -2.0);
        assert_eq!(ev("NINT(A)", &[0.0]), 0.0);
    }

    // Review fix (Important 2): the boundary test above only rules out
    // banker's rounding - Rust's `f64::round` already rounds
    // half-away-from-zero, so for every in-range value the truncating-cast
    // implementation and a bare `top.round()` are bit-identical, and the
    // boundary test alone cannot tell them apart. The genuine discriminator
    // is a magnitude that overflows `epicsInt32`: Rust's `as i32` cast
    // saturates (a deliberate, documented divergence from C's
    // undefined-behavior cast at calcPerform.c:291-294, mirroring Task 3's
    // `%` divergence), whereas `top.round()` alone would return the
    // unsaturated double unchanged.
    #[test]
    fn nint_saturates_on_i32_overflow_matching_rust_cast_semantics() {
        assert_eq!(ev("NINT(A)", &[5e9]), i32::MAX as f64);
        assert_eq!(ev("NINT(A)", &[-5e9]), i32::MIN as f64);
    }

    #[test]
    fn min_and_max_are_variadic() {
        assert_eq!(ev("MIN(A,B,C)", &[3.0, 1.0, 2.0]), 1.0);
        assert_eq!(ev("MAX(A,B,C)", &[3.0, 1.0, 2.0]), 3.0);
        assert_eq!(ev("MIN(A)", &[7.0]), 7.0);
    }

    // Trap 3 (task instructions / calcPerform.c:191-207): any NaN argument
    // makes the WHOLE result NaN, regardless of position - the opposite of
    // Rust's f64::min/f64::max, which ignore NaN. Cover NaN at the front,
    // middle, and end, plus varying argument counts (2 and 4).
    #[test]
    fn min_and_max_propagate_nan_from_any_position() {
        let nan = f64::NAN;
        assert!(ev("MAX(A,B,C)", &[1.0, nan, 3.0]).is_nan());
        assert!(ev("MAX(A,B,C)", &[nan, 1.0, 3.0]).is_nan());
        assert!(ev("MAX(A,B,C)", &[1.0, 3.0, nan]).is_nan());
        assert!(ev("MIN(A,B,C)", &[1.0, nan, 3.0]).is_nan());
        assert!(ev("MIN(A,B)", &[nan, 1.0]).is_nan());
        assert!(ev("MAX(A,B,C,D)", &[1.0, 2.0, 3.0, nan]).is_nan());
        // Sanity: no NaN present still behaves normally with 4 args.
        assert_eq!(ev("MAX(A,B,C,D)", &[1.0, 4.0, 2.0, 3.0]), 4.0);
        assert_eq!(ev("MIN(A,B,C,D)", &[1.0, 4.0, 2.0, 3.0]), 1.0);
    }

    #[test]
    fn trig_functions() {
        assert!((ev("SIN(A)", &[0.0]) - 0.0).abs() < 1e-12);
        assert!((ev("COS(A)", &[0.0]) - 1.0).abs() < 1e-12);
        assert!((ev("ATAN2(A,B)", &[1.0, 1.0]) - std::f64::consts::FRAC_PI_4).abs() < 1e-12);
    }

    // Trap 1 (task instructions / calcPerform.c:225-228, commented by Base
    // itself as "Ouch!: Args backwards!"): `ATAN2(A,B)` computes
    // `atan2(B, A)`, not the mathematically expected `atan2(A, B)`. The
    // brief's own trig_functions test above uses A=B=1.0, which is
    // symmetric and can't detect this - use asymmetric arguments so a
    // naive `a.atan2(b)` implementation fails.
    #[test]
    fn atan2_arguments_are_backwards_matching_base() {
        // atan2(B=0, A=1) == 0, but atan2(A=1, B=0) == FRAC_PI_2 - very
        // different, so this pins the argument order precisely.
        assert!((ev("ATAN2(A,B)", &[1.0, 0.0]) - 0.0).abs() < 1e-12);
        // atan2(B=1, A=0) == FRAC_PI_2.
        assert!(
            (ev("ATAN2(A,B)", &[0.0, 1.0]) - std::f64::consts::FRAC_PI_2).abs() < 1e-12
        );
    }

    #[test]
    fn log_of_negative_is_nan_not_error() {
        assert!(ev("LOG(A)", &[-1.0]).is_nan());
    }

    #[test]
    fn abs_exp_hyperbolic_and_inverse_trig() {
        assert_eq!(ev("ABS(A)", &[-3.0]), 3.0);
        assert_eq!(ev("EXP(A)", &[0.0]), 1.0);
        assert!((ev("ASIN(A)", &[1.0]) - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        // acos(2) is out of domain: propagates NaN, never Err (global
        // "Err is compile-time only" constraint).
        assert!(ev("ACOS(A)", &[2.0]).is_nan());
        assert_eq!(ev("SINH(A)", &[0.0]), 0.0);
        assert_eq!(ev("COSH(A)", &[0.0]), 1.0);
        assert_eq!(ev("TANH(A)", &[0.0]), 0.0);
    }

    // --- Task 5: relational, logical, and conditional operators ---

    #[test]
    fn single_equals_is_equality_not_assignment() {
        assert_eq!(ev("A=B", &[1.0, 1.0]), 1.0);
        assert_eq!(ev("A==B", &[1.0, 1.0]), 1.0);
        assert_eq!(ev("A=B", &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn hash_is_not_equal() {
        assert_eq!(ev("A#B", &[1.0, 2.0]), 1.0);
        assert_eq!(ev("A!=B", &[1.0, 2.0]), 1.0);
        assert_eq!(ev("A#B", &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn relational_operators_yield_one_or_zero() {
        assert_eq!(ev("A>B", &[2.0, 1.0]), 1.0);
        assert_eq!(ev("A>=B", &[1.0, 1.0]), 1.0);
        assert_eq!(ev("A<B", &[2.0, 1.0]), 0.0);
        assert_eq!(ev("A<=B", &[1.0, 1.0]), 1.0);
    }

    // Review fix (Important 3): the brief's own equal-operand coverage only
    // ever exercised `>=`/`<=` at the boundary (`[1.0, 1.0] -> 1.0`), never
    // `>`/`<` there. `Op::Gt => a >= b` and `Op::Lt => a <= b` — both wrong,
    // swallowing the strict/non-strict distinction entirely — would still
    // pass the whole suite including `relational_operators_yield_one_or_zero`
    // above (equal operands never appear there for `>`/`<`),
    // `nan_comparisons_are_always_false_except_not_equal` (NaN operands make
    // `>` and `>=` agree, so that test can't tell them apart either), and
    // `conditional_binds_loosest` (no equal-operand case). Equal operands
    // are the one input that pulls `>` and `>=` (or `<` and `<=`) apart.
    #[test]
    fn strict_relational_operators_differ_from_non_strict_at_equal_operands() {
        assert_eq!(ev("A>B", &[1.0, 1.0]), 0.0);
        assert_eq!(ev("A<B", &[1.0, 1.0]), 0.0);
        assert_eq!(ev("A>=B", &[1.0, 1.0]), 1.0);
        assert_eq!(ev("A<=B", &[1.0, 1.0]), 1.0);
    }

    #[test]
    fn logical_operators_treat_nonzero_as_true() {
        assert_eq!(ev("A&&B", &[5.0, 3.0]), 1.0);
        assert_eq!(ev("A&&B", &[5.0, 0.0]), 0.0);
        assert_eq!(ev("A||B", &[0.0, 3.0]), 1.0);
        assert_eq!(ev("!A", &[0.0]), 1.0);
        assert_eq!(ev("!A", &[5.0]), 0.0);
    }

    // calcPerform.c:300-312 (REL_OR/REL_AND/REL_NOT operate directly on C
    // `double`s: `||`, `&&`, and `!` all treat non-zero as true, and C's
    // rule is that ANY non-zero pattern is true, NaN included (NaN != 0.0).
    // Not an "improvement" to short-circuit or to special-case NaN as
    // false - that would diverge from Base.
    #[test]
    fn logical_operators_treat_nan_as_true() {
        let nan = f64::NAN;
        assert_eq!(ev("A&&B", &[nan, 1.0]), 1.0);
        assert_eq!(ev("A||B", &[nan, 0.0]), 1.0);
        assert_eq!(ev("!A", &[nan]), 0.0);
    }

    // Every comparison against NaN is false in IEEE 754 (and thus in C),
    // including equality of NaN with itself - `A=B` with both NaN is 0.0,
    // not 1.0. Rust's f64 `==`/`<`/`>`/etc. already follow IEEE 754, so this
    // is a "does the translation preserve it" check, not new behavior.
    #[test]
    fn nan_comparisons_are_always_false_except_not_equal() {
        let nan = f64::NAN;
        assert_eq!(ev("A=B", &[nan, nan]), 0.0);
        assert_eq!(ev("A>B", &[nan, 1.0]), 0.0);
        assert_eq!(ev("A<B", &[nan, 1.0]), 0.0);
        assert_eq!(ev("A>=B", &[nan, 1.0]), 0.0);
        assert_eq!(ev("A<=B", &[nan, 1.0]), 0.0);
        // `!=`/`#` is the sole exception: NaN is unequal to everything.
        assert_eq!(ev("A#B", &[nan, nan]), 1.0);
    }

    #[test]
    fn conditional_selects_branch() {
        assert_eq!(ev("A?B:C", &[1.0, 10.0, 20.0]), 10.0);
        assert_eq!(ev("A?B:C", &[0.0, 10.0, 20.0]), 20.0);
    }

    #[test]
    fn conditional_binds_loosest() {
        // Parses as (A>B) ? (C+1) : (C-1), not A > (B?...)
        assert_eq!(ev("A>B?C+1:C-1", &[2.0, 1.0, 10.0]), 11.0);
        assert_eq!(ev("A>B?C+1:C-1", &[1.0, 2.0, 10.0]), 9.0);
    }

    // Proof that the ternary genuinely short-circuits, not just that it
    // "looks like" eager pop-three-select happens to agree with it.
    //
    // NaN-propagation is *not* a valid demonstration here (as task-5-brief.md
    // warns): every op in this crate's current feature set is numerically
    // total (division/log/sqrt/etc. of anything yield inf/NaN, never a
    // panic), so an eager evaluator that runs both branches and discards one
    // is numerically indistinguishable from a short-circuiting one - there
    // is nothing observable through `eval`'s f64 return value alone.
    //
    // What *is* observable: `check_arity` (parse.rs) guarantees a
    // `compile()`-produced `Expression` never underflows the stack, so
    // `eval`'s `.expect("arity checked at compile time")` pops are provably
    // sound for anything the public API can produce. But `Expression.ops` is
    // only `pub(crate)`, not `pub` - nothing outside this crate can violate
    // that invariant, but code *inside* the crate (i.e. this test) can
    // construct a hand-built, deliberately-underflowing `Expression`
    // directly. Doing so here gives a decisive white-box test: the "else"
    // branch below is a bare `Op::Add` with nothing pushed for it to pop -
    // executing it *would* panic. If `eval` ever regresses to evaluating
    // both branches (eagerly, discarding the untaken one, as Task 3's
    // `Op::Cond` used to), this test panics. Because the real short-circuit
    // implementation jumps clean over that branch, it doesn't.
    #[test]
    fn conditional_never_executes_the_untaken_branch() {
        use crate::op::Op;
        use crate::parse::Expression;

        // A ? 1.0 : <panics if reached>
        //   0: Arg(0)
        //   1: CondIf { else_target: 4 }   -- pop A; if zero, jump to 4
        //   2: Lit(1.0)                    -- then-branch
        //   3: CondElse { end_target: 6 }  -- unconditionally skip to 6
        //   4: Add                          -- else-branch: pop 2 from an
        //                                      empty stack -> would panic
        //   5: CondEnd
        let taken_then = Expression {
            ops: vec![
                Op::Arg(0),
                Op::CondIf { else_target: 4 },
                Op::Lit(1.0),
                Op::CondElse { end_target: 6 },
                Op::Add,
                Op::CondEnd,
            ],
        };
        assert_eq!(taken_then.eval(&mut [1.0; 21]), 1.0);

        // A ? <panics if reached> : 2.0, with A == 0 so the else-branch (the
        // one that doesn't panic) is the one actually taken - mirrors the
        // above in the other direction.
        //   0: Arg(0)
        //   1: CondIf { else_target: 4 }
        //   2: Add                          -- then-branch: would panic
        //   3: CondElse { end_target: 6 }
        //   4: Lit(2.0)                     -- else-branch
        //   5: CondEnd
        let taken_else = Expression {
            ops: vec![
                Op::Arg(0),
                Op::CondIf { else_target: 4 },
                Op::Add,
                Op::CondElse { end_target: 6 },
                Op::Lit(2.0),
                Op::CondEnd,
            ],
        };
        assert_eq!(taken_else.eval(&mut [0.0; 21]), 2.0);
    }

    // --- Task 6: bitwise operators ---

    #[test]
    fn caret_is_power_and_xor_is_the_word() {
        assert_eq!(ev("A^B", &[2.0, 3.0]), 8.0);
        assert_eq!(ev("A XOR B", &[6.0, 3.0]), 5.0);
    }

    #[test]
    fn bitwise_and_or_not() {
        assert_eq!(ev("A&B", &[6.0, 3.0]), 2.0);
        assert_eq!(ev("A AND B", &[6.0, 3.0]), 2.0);
        assert_eq!(ev("A|B", &[6.0, 3.0]), 7.0);
        assert_eq!(ev("A OR B", &[6.0, 3.0]), 7.0);
        assert_eq!(ev("~A", &[0.0]), -1.0);
        assert_eq!(ev("NOT A", &[0.0]), -1.0);
    }

    #[test]
    fn shifts_operate_on_integers() {
        assert_eq!(ev("A<<B", &[1.0, 4.0]), 16.0);
        assert_eq!(ev("A>>B", &[16.0, 4.0]), 1.0);
    }

    #[test]
    fn bitwise_truncates_toward_zero() {
        assert_eq!(ev("A&B", &[6.9, 3.9]), 2.0);
    }

    // `d2i`/`d2ui` (`calcPerform.c:325-326`) treat non-finite operands the
    // same as any other out-of-range magnitude: no defined C behavior, and
    // this crate picks Rust's own saturating `as` cast (see `d2i`'s doc).
    // `NaN as u32` is `0` in Rust, so `d2i(NaN) == 0`.
    #[test]
    fn bitwise_on_non_finite_yields_zero_operand() {
        assert_eq!(ev("A&B", &[f64::NAN, 3.0]), 0.0);
    }

    // The flagship bit-31 trap (task instructions / `calcPerform.c:314-324`'s
    // twelve-line comment): `d2i` is asymmetric by SIGN, not by magnitude.
    // `2863311530.1` doesn't fit in `i32` (max ~2.1e9) but does fit in `u32`
    // (max ~4.3e9), so per `d2i`'s positive branch it goes
    // double -> u32 (2863311530, bit pattern 0xAAAAAAAA) -> reinterpret as
    // i32 (-1431655766), landing on the SAME i32 a direct negative double
    // with that bit pattern would. `-1431655766.1` needs no such detour (its
    // magnitude already fits `i32`) and reaches the identical value via the
    // negative branch. `refs/epicsCalcTest.cpp:1052-1053` asserts both
    // (`OR 0`) equal `0xaaaaaaaa` reinterpreted - i.e. this exact identity.
    //
    // The naive `as i32` a careless implementation might reach for instead
    // SATURATES on the positive value (`2147483647`, `i32::MAX`) rather than
    // wrapping to the bit-reinterpreted `-1431655766` - a different answer,
    // which is exactly why the task requires implementing `d2i`/`d2ui`
    // explicitly rather than reaching for `as i32` directly.
    #[test]
    fn d2i_bit31_asymmetry_positive_and_negative_agree_where_naive_cast_would_not() {
        assert_eq!(ev("A|B", &[-1431655766.1, 0.0]), -1431655766.0);
        assert_eq!(ev("A|B", &[2863311530.1, 0.0]), -1431655766.0);
        // The naive cast a wrong implementation might use instead:
        assert_ne!(2863311530.1_f64 as i32, -1431655766);
        assert_eq!(2863311530.1_f64 as i32, i32::MAX);
    }

    // `refs/calcPerform.c:355-368`: the result of every bitwise op is a
    // SIGNED `epicsInt32` widened back to `double` - a result with bit 31
    // set comes out negative, even for `AndB`/`OrB`/`XorB`/`Shl`/`Shr`/
    // `ShrLogic`, none of which "look" signed at the call site.
    #[test]
    fn bitwise_result_with_bit_31_set_is_negative() {
        // ~0 == 0xFFFFFFFF, all bits set -> -1, not 4294967295.
        assert_eq!(ev("~A", &[0.0]), -1.0);
        // 0xFFFF0000 | 0 == 0xFFFF0000, bit 31 set -> negative.
        assert_eq!(ev("A|B", &[4294901760.0, 0.0]), -65536.0);
    }

    // `>>>` (`RIGHT_SHIFT_LOGIC`, `calcPerform.c:365-368`) is the UNSIGNED
    // shift Ruling 2 requires even though the task-6-brief.md text never
    // mentions it. Unlike `>>` (arithmetic, sign-extending), a negative
    // operand's sign bit is treated as data, not extended - so a negative
    // dividend right-shifted with `>>>` comes out small and
    // positive-looking in bit pattern. This op's RESULT also stays
    // unsigned/non-negative (see the fix-round-1 note on `Op::ShrLogic`'s
    // doc and `logical_shift_right_result_is_unsigned_unlike_every_other_bitwise_op`
    // below) - `15.0` here happens not to distinguish that, since it has no
    // sign ambiguity either way; that distinction is pinned separately.
    #[test]
    fn logical_shift_right_does_not_sign_extend_unlike_arithmetic_shift_right() {
        // -1 (0xFFFFFFFF) >> 28 (arithmetic, sign-extends) == -1 (still all
        // bits set: 0xFFFFFFFF).
        assert_eq!(ev("A>>B", &[-1.0, 28.0]), -1.0);
        // -1 (0xFFFFFFFF) >>> 28 (logical) == 0xF == 15: the top 28 bits
        // become zero instead of staying set.
        assert_eq!(ev("A>>>B", &[-1.0, 28.0]), 15.0);
    }

    // `refs/epicsCalcTest.cpp:1030-1033`: unsigned shift-right examples,
    // transcribed directly. `>>` (`Op::Shr`) widens its result as signed
    // (like every op routed through `d2i`), so `0xFFAAAAAA` comes out as the
    // negative `i32` reinterpretation `-5592406.0`. `>>>` (`Op::ShrLogic`)
    // is Base's documented exception (`calcPerform.c:365-368`: the shift
    // expression is `epicsUInt32`, widened to `double` directly, never
    // re-cast to signed) - `0x00AAAAAA` has bit 31 clear either way here, so
    // this particular pair doesn't yet distinguish "widened unsigned" from
    // "widened signed"; see the dedicated test below for a case where it
    // does (bit 31 set after the logical shift).
    #[test]
    fn logical_shift_right_matches_corpus_examples() {
        let bits_0xaaaaaaaa = -1431655766.0; // 0xAAAAAAAA reinterpreted i32
        let bits_0xffaaaaaa = -5592406.0; // 0xFFAAAAAA reinterpreted i32
        let bits_0x00aaaaaa = 11184810.0; // 0x00AAAAAA, no sign bit either way
        assert_eq!(ev("A>>B", &[bits_0xaaaaaaaa, 8.0]), bits_0xffaaaaaa);
        assert_eq!(ev("A>>>B", &[bits_0xaaaaaaaa, 8.0]), bits_0x00aaaaaa);
    }

    // Critical review fix (Fix round 1): `Op::ShrLogic`'s result must stay
    // in `[0, 4294967295]` - `calcPerform.c:365-368`'s C shift expression
    // has type `epicsUInt32` and Base widens it to `double` DIRECTLY, never
    // re-casting to `epicsInt32` first the way every other bitwise op here
    // does. A prior version of this crate wrongly reinterpreted the
    // `wrapping_shr` result as `i32` before widening, which is
    // indistinguishable from correct on `bits_0x00aaaaaa` above (bit 31
    // clear) but wrong whenever bit 31 of the shifted result is set - e.g.
    // `-1 >>> 0` is `0xFFFFFFFF` unshifted, all bits set: Base returns
    // `4294967295.0`, but the wrong signed-reinterpretation would have
    // returned `-1.0`. Likewise `2863311530.1 >>> 0` (a value whose `d2ui`
    // is already `0xAAAAAAAA`, bit 31 set, per the `d2i`/`d2ui` bit-31
    // asymmetry tests above): Base returns `2863311530.0`, not
    // `-1431655766.0`.
    #[test]
    fn logical_shift_right_result_is_unsigned_unlike_every_other_bitwise_op() {
        assert_eq!(ev("A>>>B", &[-1.0, 0.0]), 4294967295.0);
        assert_eq!(ev("A>>>B", &[2863311530.1, 0.0]), 2863311530.0);
    }

    // Shift counts are masked to `0..=31` (`d2i(top) & 31` /
    // `d2ui(top) & 31u`, `calcPerform.c:357,362,367`) precisely because an
    // unmasked shift count is out of range for a 32-bit shift and would
    // panic under Rust's plain `<<`/`>>` in debug builds. `36` masks to `4`
    // (`36 & 31 == 4`), matching `refs/epicsCalcTest.cpp` shift-with-large-
    // count patterns; this must not panic and must produce the masked
    // result, not the unmasked (and undefined in C) one.
    #[test]
    fn shift_count_is_masked_to_five_bits_and_never_panics() {
        assert_eq!(ev("A<<B", &[1.0, 36.0]), ev("A<<B", &[1.0, 4.0]));
        assert_eq!(ev("A>>B", &[256.0, 36.0]), ev("A>>B", &[256.0, 4.0]));
        assert_eq!(ev("A>>>B", &[256.0, 36.0]), ev("A>>>B", &[256.0, 4.0]));
        // A shift count far outside any plausible bit width - still must not
        // panic (this call itself, not returning, is the assertion).
        let _ = ev("A<<B", &[1.0, 1_000_000.0]);
        let _ = ev("A>>B", &[1.0, -1_000_000.0]);
    }

    // `d2i`/`d2ui` must not panic on any `f64` input, including NaN and both
    // infinities - the values the task instructions call out explicitly.
    #[test]
    fn bitwise_ops_never_panic_on_nan_or_infinity() {
        let vals = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
        for &v in &vals {
            let _ = ev("A&B", &[v, 3.0]);
            let _ = ev("A|B", &[3.0, v]);
            let _ = ev("A XOR B", &[v, 3.0]);
            let _ = ev("~A", &[v]);
            let _ = ev("A<<B", &[v, 3.0]);
            let _ = ev("A<<B", &[3.0, v]);
            let _ = ev("A>>B", &[v, 3.0]);
            let _ = ev("A>>>B", &[v, 3.0]);
        }
    }

    // --- Task 7: named constants, RNDM, NaN/Inf predicates ---

    // `refs/postfix.c:101,111,125,129,132`: `D2R`, `INF`, `NAN`, `PI`, `R2D`
    // are all `OPERAND`/`LITERAL_OPERAND` entries with zero stack effect (push
    // one, pop none) - nullary, not functions. `refs/calcPerform.c:126-136`:
    // `CONST_PI` pushes `PI`, `CONST_D2R` pushes `PI/180.`, `CONST_R2D` pushes
    // `180./PI` - computed as divisions of `PI`, not transcribed decimal
    // literals, so the last bits match exactly.
    #[test]
    fn named_constants() {
        // Bit-identical, not merely close: `constant()` (parse.rs) computes
        // these as divisions of `std::f64::consts::PI`, so the same
        // divisions performed here must round to the identical `f64` bit
        // pattern. A tolerance-based comparison (e.g. `< 1e-15`) would also
        // pass for a transcribed decimal literal that merely rounds to the
        // same value, so it pins nothing about "computed vs. transcribed" -
        // exact bit equality is the only assertion that actually
        // distinguishes them.
        assert_eq!(ev("PI", &[]).to_bits(), std::f64::consts::PI.to_bits());
        assert_eq!(
            ev("D2R", &[]).to_bits(),
            (std::f64::consts::PI / 180.0).to_bits()
        );
        assert_eq!(
            ev("R2D", &[]).to_bits(),
            (180.0 / std::f64::consts::PI).to_bits()
        );
        assert!(ev("INF", &[]).is_infinite());
        assert!(ev("NAN", &[]).is_nan());
    }

    // refs/epicsCalcTest.cpp:336 asserts `testCalc("Infinity", Inf)` -
    // Base's own test suite compiles and evaluates this spelling
    // successfully, even though postfix.c's operands[] table has no
    // distinct "INFINITY" row (see constant()'s doc in parse.rs for the
    // prefix-match + strtod-delegation mechanism that makes it work in
    // Base, and why this crate lists "INFINITY" explicitly instead).
    #[test]
    fn infinity_spelling_is_accepted_like_base() {
        assert!(ev("Infinity", &[]).is_infinite());
        assert!(ev("INFINITY", &[]).is_infinite());
    }

    #[test]
    fn nan_and_inf_predicates() {
        assert_eq!(ev("ISNAN(A)", &[f64::NAN]), 1.0);
        assert_eq!(ev("ISNAN(A)", &[1.0]), 0.0);
        assert_eq!(ev("ISINF(A)", &[f64::INFINITY]), 1.0);
        assert_eq!(ev("ISINF(A)", &[1.0]), 0.0);
        assert_eq!(ev("FINITE(A)", &[1.0]), 1.0);
        assert_eq!(ev("FINITE(A)", &[f64::NAN]), 0.0);
    }

    // `refs/calcPerform.c:267-275` `case FINITE`: `top = finite(*ptop); while
    // (--nargs) top = top && finite(*--ptop);` - an AND-fold. Every argument
    // must be finite for the result to be true; one non-finite argument
    // anywhere makes the whole thing false, regardless of position.
    #[test]
    fn finite_is_variadic_and_folds_with_and() {
        assert_eq!(ev("FINITE(A,B)", &[1.0, f64::NAN]), 0.0);
        assert_eq!(ev("FINITE(A,B)", &[1.0, f64::INFINITY]), 0.0);
        assert_eq!(ev("FINITE(A,B)", &[1.0, 2.0]), 1.0);
        assert_eq!(ev("FINITE(A,B,C)", &[1.0, 2.0, f64::NAN]), 0.0);
        // Non-finite in the FIRST position, not just the last - the fold
        // must not be short-circuited by an implementation that only checks
        // the initial "top" and ignores the rest, nor one that only checks
        // the tail.
        assert_eq!(ev("FINITE(A,B,C)", &[f64::NAN, 2.0, 3.0]), 0.0);
    }

    // `refs/calcPerform.c:281-289` `case ISNAN`: `top = isnan(*ptop); while
    // (--nargs) top = top || isnan(*--ptop);` - an OR-fold. Any argument
    // being NaN, at any position, makes the whole result true.
    #[test]
    fn isnan_is_variadic_and_folds_with_or() {
        assert_eq!(ev("ISNAN(A,B)", &[1.0, f64::NAN]), 1.0);
        assert_eq!(ev("ISNAN(A,B)", &[1.0, 2.0]), 0.0);
        assert_eq!(ev("ISNAN(A,B,C)", &[f64::NAN, 2.0, 3.0]), 1.0);
        assert_eq!(ev("ISNAN(A,B,C)", &[1.0, 2.0, 3.0]), 0.0);
    }

    // RULINGS.md Ruling 6 / `refs/postfix.c:112-113`: `ISINF` is a
    // `UNARY_OPERATOR`, unlike `ISNAN`/`FINITE` (`VARARG_OPERATOR`) - it
    // takes exactly one argument, no `nargs` byte, no fold. `ISINF(A,B)`
    // compiles (fixed-arity functions aren't checked for exact argument
    // count at the closing `)` - see parse.rs's comment on that match arm),
    // but leaves the stack 1 too deep (2 pushed, 1 popped by `IsInf`, net
    // depth 2, not 1) - caught by `check_arity`'s end-of-parse depth check
    // as `ExtraOperand`. A wrongly-variadic `ISINF` (the brief's original,
    // pre-Ruling-6 mistake) would instead accept this and evaluate to `0.0`.
    #[test]
    fn isinf_is_strictly_unary_not_variadic() {
        assert_eq!(
            crate::compile("ISINF(A,B)"),
            Err(crate::CalcError::ExtraOperand)
        );
    }

    // `RNDM` (`refs/postfix.c:133`, opcode `RANDOM`) is nullary - it pushes
    // without popping, a new shape for `check_segment`'s depth accounting
    // (Task 7 introduces the first nullary op besides `Arg`/`Lit`). Draw many
    // times from a freshly (and deterministically, via the crate-internal
    // `seed_rndm` test hook) seeded generator and require both range and
    // variation: a stub that always returns e.g. `0.5` would pass a
    // single-draw range check but fails `seen.len() > 1` here.
    #[test]
    fn rndm_is_in_unit_interval_and_varies_across_draws() {
        seed_rndm(0x1234_5678_9abc_def0);
        let mut a = [0.0f64; 21];
        let e = crate::compile("RNDM").expect("compile");
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            let v = e.eval(&mut a);
            assert!((0.0..=1.0).contains(&v), "RNDM out of [0,1]: {v}");
            seen.insert(v.to_bits());
        }
        assert!(seen.len() > 1, "RNDM did not vary across draws");
    }

    // `RNDM`'s value must genuinely participate in the surrounding
    // expression's arithmetic, not just evaluate in isolation without
    // panicking.
    #[test]
    fn rndm_participates_in_a_larger_expression() {
        seed_rndm(42);
        let e = crate::compile("RNDM*2+A").expect("compile");
        let v = e.eval(&mut [3.0; 21]);
        assert!((3.0..=5.0).contains(&v), "RNDM*2+3 out of [3,5]: {v}");
    }

    // Reseeding to the same value must reproduce the same draw - the
    // crate-internal test hook exists precisely so a nondeterministic op can
    // still be tested deterministically.
    #[test]
    fn rndm_is_reproducible_from_a_fixed_seed() {
        seed_rndm(777);
        let e = crate::compile("RNDM").expect("compile");
        let first = e.eval(&mut [0.0; 21]);
        seed_rndm(777);
        let second = e.eval(&mut [0.0; 21]);
        assert_eq!(first, second);
    }

    // `eval_with_rng` is the public, cross-crate-usable determinism hook -
    // `seed_rndm` is `#[cfg(test)]`/`pub(crate)` and cannot serve callers
    // outside this crate (e.g. a differential-testing oracle in another
    // crate, or a `tests/` integration test, neither of which can see
    // crate-internal items). A caller-supplied closure must be able to pin
    // `RNDM` to an arbitrary, fully-controlled sequence, independent of the
    // thread-local state `eval` uses.
    #[test]
    fn eval_with_rng_uses_the_caller_supplied_generator_not_the_thread_local() {
        let e = crate::compile("RNDM+RNDM").expect("compile");
        let mut calls = 0u32;
        let mut fixed = || {
            calls += 1;
            0.25
        };
        let v = e.eval_with_rng(&mut [0.0; 21], &mut fixed);
        assert_eq!(v, 0.5);
        assert_eq!(calls, 2);
    }

    // A long chain of pure-constant nullary ops exercises `check_segment`'s
    // depth accounting for the new "+1, pop nothing" shape at a scale near
    // `MAX_OPS`, guarding against an off-by-one that only manifests once
    // enough nullary pushes accumulate (e.g. treating `Rndm`/constants as
    // depth-neutral instead of `+1`, which would silently under/over-count
    // rather than panic outright).
    #[test]
    fn many_chained_nullary_constants_compile_and_evaluate() {
        let src = format!("{}PI", "PI+".repeat(300));
        let e = crate::compile(&src).expect("compile");
        assert!((e.eval(&mut [0.0; 21]) - 301.0 * std::f64::consts::PI).abs() < 1e-9);
    }

    // `RNDM`'s generator state lives in a thread-local `Cell<u64>` (see
    // `RNDM_STATE`'s module-level doc), deliberately kept off `Expression`
    // itself so `Expression` keeps every auto trait it had before Task 7.
    // A compile-time-only check: these helper functions never run, but they
    // fail to compile if `Expression` ever stops implementing one of these
    // traits (e.g. if a future change moved `RNDM` state onto the struct as
    // a bare `Cell`, which would make it `!Sync`).
    #[test]
    fn expression_keeps_its_auto_traits_after_adding_rndm() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        fn assert_clone<T: Clone>() {}
        fn assert_partial_eq<T: PartialEq>() {}
        fn assert_debug<T: std::fmt::Debug>() {}
        assert_send::<crate::Expression>();
        assert_sync::<crate::Expression>();
        assert_clone::<crate::Expression>();
        assert_partial_eq::<crate::Expression>();
        assert_debug::<crate::Expression>();
    }

    #[test]
    fn direct_d2i_d2ui_unit_checks() {
        use super::{d2i, d2ui};
        // Base's macro (`calcPerform.c:325-326`), traced by hand:
        assert_eq!(d2i(-1.0), -1);
        assert_eq!(d2i(0.0), 0);
        assert_eq!(d2i(3.9), 3); // truncation, not rounding
        assert_eq!(d2i(-3.9), -3);
        // The bit-31 asymmetry itself.
        assert_eq!(d2i(2863311530.0), -1431655766);
        assert_eq!(d2i(-1431655766.0), -1431655766);
        // Non-finite: no defined C answer; this crate picks 0/saturation.
        assert_eq!(d2i(f64::NAN), 0);
        assert_eq!(d2ui(f64::NAN), 0);
        // d2ui mirrors d2i.
        assert_eq!(d2ui(2863311530.0), 2863311530);
        assert_eq!(d2ui(-1431655766.0), 2863311530);
    }

    // --- Task 8a: store operator `:=` and expression terminator `;` ---

    /// Like `ev`, but returns the (possibly written-back) operand array
    /// alongside the result, so a store's effect on `args` is observable.
    fn ev_args(src: &str, args: &[f64]) -> (f64, [f64; 21]) {
        let mut a = [0.0f64; 21];
        a[..args.len()].copy_from_slice(args);
        let v = crate::compile(src).expect("compile").eval(&mut a);
        (v, a)
    }

    // `refs/epicsCalcTest.cpp:846`: `testCalc("a := 0; a", 0)` - the store
    // takes effect and the later fetch sees it, overwriting the incoming
    // value.
    #[test]
    fn store_then_fetch_sees_the_stored_value() {
        let (v, a) = ev_args("a := 0; a", &[5.0]);
        assert_eq!(v, 0.0);
        assert_eq!(a[0], 0.0);
    }

    // `refs/epicsCalcTest.cpp:868`: `testCalc("a; a := 0", a)` - the result is
    // the value left on the stack by the FIRST fetch; the store's own popped
    // value is not it. The write still happens.
    #[test]
    fn fetch_then_store_returns_the_earlier_fetch_not_the_stored_value() {
        let (v, a) = ev_args("a; a := 0", &[5.0]);
        assert_eq!(v, 5.0);
        assert_eq!(a[0], 0.0);
    }

    // A fetch BEFORE and a fetch AFTER a store to the same slot, in one
    // expression, pinning the ordering. `A := A + 1` reads the incoming `A`
    // on its right-hand side (5), so the stored value is 6; the fetch in the
    // next segment then sees 6, not 5. If the store were applied before its
    // own right-hand side was read, the result would be 10; if the store
    // never took effect, 50.
    #[test]
    fn store_ordering_within_one_expression() {
        // `A := A + 1; A * 10`: reads the incoming A (5), stores 6, then the
        // later fetch sees 6 -> 60.
        let (v, a) = ev_args("A := A + 1; A * 10", &[5.0]);
        assert_eq!(v, 60.0);
        assert_eq!(a[0], 6.0);
    }

    // Stores across the whole A-U range, including `U` (index 20), read back
    // through `args` after `eval` returns. RULINGS.md Ruling 1.
    #[test]
    fn stores_reach_every_slot_including_u() {
        for i in 0..21usize {
            let letter = (b'A' + i as u8) as char;
            let src = format!("{letter} := {}; {letter}", i as f64 + 100.0);
            let (v, a) = ev_args(&src, &[]);
            assert_eq!(v, i as f64 + 100.0, "slot {letter}");
            assert_eq!(a[i], i as f64 + 100.0, "slot {letter}");
            // No other slot was touched.
            for (j, got) in a.iter().enumerate() {
                if j != i {
                    assert_eq!(*got, 0.0, "slot {j} written by a store to {letter}");
                }
            }
        }
    }

    // An expression with no `:=` must leave `args` byte-for-byte untouched.
    // Referenced by `Expression::eval`'s doc comment.
    #[test]
    fn eval_does_not_write_args_without_a_store() {
        let before = [1.5f64; 21];
        let mut a = before;
        let v = crate::compile("A+B*C ? SIN(D) : MAX(E,F)")
            .expect("compile")
            .eval(&mut a);
        assert!(v.is_finite());
        assert_eq!(a, before);
    }

    // Multiple `;`-separated expressions where an earlier store feeds a later
    // one (`refs/epicsCalcTest.cpp:997`'s shape).
    #[test]
    fn chained_stores_feed_later_expressions() {
        let (v, a) = ev_args("A := 2; B := A * 3; C := B + 1; C", &[]);
        assert_eq!(v, 7.0);
        assert_eq!(a[0], 2.0);
        assert_eq!(a[1], 6.0);
        assert_eq!(a[2], 7.0);
    }

    // THE TERNARY-STORE TEST, and it does NOT say what task-8a-brief.md
    // predicted it would.
    //
    // The brief asked for "a store inside each arm of a ternary, showing the
    // untaken arm does not store". That expression does not exist in Base's
    // grammar. A store in the ELSE arm is a compile error (`COND_END` is on
    // the operator stack, tripping `refs/postfix.c:297`'s guard - see
    // parse.rs's `store_in_an_else_branch_is_rejected`), and a store in the
    // THEN arm is not flushed by the `:` (strict `>` against equal
    // priority 0, `refs/postfix.c:402-403` vs `:162`), so it HOISTS OUT of
    // the conditional and runs on BOTH paths.
    //
    // So `A ? B := 1 : 2` is `B := (A ? 1 : 2)`: B is written either way,
    // with whichever arm's value was selected. That is what this test pins,
    // and it is the opposite of the conditional store the brief expected.
    // `:=` is therefore NOT a discriminator for short-circuiting - see
    // `untaken_ternary_branch_never_draws_from_the_rng` below for one that
    // is.
    #[test]
    fn a_store_in_a_ternary_hoists_out_and_runs_on_both_paths() {
        let (v, a) = ev_args("A ? B := 1 : 2; B", &[1.0, 77.0]);
        assert_eq!(v, 1.0);
        assert_eq!(a[1], 1.0);

        let (v, a) = ev_args("A ? B := 1 : 2; B", &[0.0, 77.0]);
        assert_eq!(v, 2.0);
        // The "untaken" arm's store still fired - with the ELSE arm's value.
        assert_eq!(a[1], 2.0);
    }

    // The black-box short-circuit discriminator Task 5's report said did not
    // exist at the time. It is not `:=` (see above) - it is `RNDM` under the
    // public `eval_with_rng`, which lets a caller count draws.
    //
    // `A ? 1 : RNDM` with A == 1: the else-branch's `Op::Rndm` must never
    // execute, so the caller's generator is called ZERO times. Derived
    // failing output under an eager implementation (Task 3's old "pop three,
    // select one" `Op::Cond`, which evaluated both arms and discarded one):
    // the result would still be 1.0, but `calls` would be 1. So the
    // `calls == 0` assertion, and only that assertion, separates the two.
    #[test]
    fn untaken_ternary_branch_never_draws_from_the_rng() {
        let e = crate::compile("A ? 1 : RNDM").expect("compile");

        let mut calls = 0u32;
        let mut rng = || {
            calls += 1;
            0.5
        };
        let v = e.eval_with_rng(&mut [1.0; 21], &mut rng);
        drop(rng);
        assert_eq!(v, 1.0);
        assert_eq!(calls, 0, "else-branch RNDM was evaluated despite A != 0");

        // Control: with A == 0 the else-branch IS taken and the draw happens,
        // proving the zero above is short-circuiting rather than a
        // never-wired-up generator.
        let mut calls = 0u32;
        let mut rng = || {
            calls += 1;
            0.5
        };
        let mut args = [0.0f64; 21];
        let v = e.eval_with_rng(&mut args, &mut rng);
        drop(rng);
        assert_eq!(v, 0.5);
        assert_eq!(calls, 1);
    }

    // `refs/epicsCalcTest.cpp:1037-1047`, transcribed early because they are
    // the corpus's only store-plus-bitwise shapes and they exercise the
    // `;`-flush between two stores. Results are compared as the `epicsUInt32`
    // reinterpretation the corpus uses, since `d2i` widens back as signed.
    #[test]
    fn corpus_uint32_store_shapes() {
        fn as_u32(v: f64) -> u32 {
            (v as i64) as u32
        }
        let (v, _) = ev_args("a:=0xaaaaaaaa; b:=0xffff0000; a AND b", &[]);
        assert_eq!(as_u32(v), 0xaaaa0000u32);
        let (v, _) = ev_args("a:=0xaaaaaaaa; b:=0xffff0000; a OR b", &[]);
        assert_eq!(as_u32(v), 0xffffaaaau32);
        let (v, _) = ev_args("a:=0xaaaaaaaa; b:=0xffff0000; a XOR b", &[]);
        assert_eq!(as_u32(v), 0x5555aaaau32);
        let (v, _) = ev_args("a:=0xaaaaaaaa; ~a", &[]);
        assert_eq!(as_u32(v), 0x55555555u32);
        let (v, _) = ev_args("a:=0xaaaaaaaa; ~~a", &[]);
        assert_eq!(as_u32(v), 0xaaaaaaaau32);
        let (v, _) = ev_args("a:=0xaaaaaaaa; a >> 8", &[]);
        assert_eq!(as_u32(v), 0xffaaaaaau32);
        let (v, _) = ev_args("a:=0xaaaaaaaa; a >>> 8", &[]);
        assert_eq!(as_u32(v), 0x00aaaaaau32);
        let (v, _) = ev_args("a:=0xaaaaaaaa; a << 8", &[]);
        assert_eq!(as_u32(v), 0xaaaaaa00u32);
    }

    // The no-panic invariant, extended to the new shapes: `check_segment`
    // must prove that no input `compile` accepts can underflow `eval`'s
    // stack. A reachable panic on the public API was the Critical finding on
    // Task 3. These are all the store/terminator shapes that pass the
    // checker; each must evaluate to a value.
    #[test]
    fn accepted_store_and_terminator_shapes_never_panic() {
        for src in [
            "A := 0; A",
            "A; A := 0",
            "U := 1; U",
            "A := B := 0; A",           // rejected or evaluated, never panics
            "A ? B := 1 : 2; B",
            "A := (B ? 1 : 2); A",
            "A := 1; B := A; C := B; C",
            "A := A; A",
            "A := NAN; ISNAN(A)",
            "A := 1/0; A",
            "MAX(A,B); A := 1; MIN(A,B)",
        ] {
            if let Ok(e) = crate::compile(src) {
                let mut args = [2.0f64; 21];
                let v = e.eval(&mut args);
                let _ = v;
            }
        }
    }
}
