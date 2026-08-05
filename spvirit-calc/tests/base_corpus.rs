//! Conformance corpus transcribed from EPICS Base 7.0's own unit test,
//! `modules/libcom/test/epicsCalcTest.cpp`, on disk at
//! `.superpowers/sdd/2026-08-04-calc-expression-engine/refs/epicsCalcTest.cpp`.
//!
//! **Base is the oracle.** If a case here fails, the bug is in spvirit-calc
//! until proven otherwise; an expected value is only ever adjusted after
//! re-reading the case in `refs/epicsCalcTest.cpp`, and then the line number
//! and reason are named in a comment on that case.
//!
//! # How Base's harness works, and how this file reproduces it
//!
//! Base's four assertion helpers, all reproduced here:
//!
//! - `testCalc(expr, expected)` (`epicsCalcTest.cpp:48-84`) evaluates `expr`
//!   with `args[] = {1.0, 2.0, ... 21.0}` and compares. The comparison rule is
//!   transcribed exactly (`:72-78`): if *both* sides are finite, pass when
//!   `|expected - result| < 1e-8` (absolute, **not** relative and **not**
//!   1e-12); if `expected` is NaN, pass when the result is NaN; otherwise pass
//!   on exact `==` (which is how the infinities are checked).
//! - `testExpr(expr)` (`:165`) is `testCalc(#expr, expr)` - the *same text* is
//!   both the CALC expression and a C expression the C++ compiler evaluates to
//!   produce the expected value. Those are transcribed here as the stringized
//!   text plus the Rust translation of the same expression; Rust and C agree
//!   on IEEE-754 `f64` arithmetic and comparison, including every NaN case, so
//!   the translation is mechanical. Where the C text uses one of the test's
//!   own macros (`ABS`, `LN`, `LOG`, `NINT`, `MAX`, `MIN`, `ATAN2`, `AND`,
//!   `OR`, `XOR`, `D2R`, `R2D`, `:196-297`), the helper below reproduces that
//!   macro, not the CALC opcode - the point of `testExpr` is that the two
//!   independently agree.
//! - `testUInt32Calc(expr, expected)` (`:86-119`) post-converts the `f64`
//!   result to `epicsUInt32` *through* `epicsInt32` when negative
//!   (`:111`) before comparing. `to_uint32` below reproduces that, rather
//!   than comparing as `f64`.
//! - `testArgs(expr, einp, eout)` (`:121-143`) checks `calcArgUsage`, i.e.
//!   this crate's `Expression::arg_usage`.
//! - `testBadExpr(expr, err)` (`:145-162`) asserts a *specific* Base error
//!   code. See the `CALC_ERR_* -> CalcError` mapping table on `bad()` below.
//!
//! Every mismatch is collected into a `Vec` and asserted once at the end, so a
//! single run reports all failures rather than stopping at the first.
//!
//! # Known divergences from Base, ruled on before this task and not fixed here
//!
//! - Base's literal parsing delegates to `epicsParseDouble`/`epicsParseUInt32`
//!   (`refs/postfix.c:263,285`), so strtod spellings such as `nan(0)` and
//!   `0x1p3` are accepted by Base and rejected by this crate. **Not
//!   corpus-tested** - no case below exercises them.
//! - `compile("")` returns `Ok(empty)` here where Base gives
//!   `CALC_ERR_NULL_ARG`. Pre-existing from Tasks 1-2; not corpus-tested
//!   either (Base's test never passes an empty expression).

use spvirit_calc::{CalcError, compile};

// ---------------------------------------------------------------------------
// Constants and helpers mirroring epicsCalcTest.cpp's own macros
// ---------------------------------------------------------------------------

const INF: f64 = f64::INFINITY;
const NAN: f64 = f64::NAN;

/// `epicsCalcTest.cpp:193-195`. Note this is the *test's* PI (14 decimal
/// digits), not `std::f64::consts::PI`; `calcPerform.c:34` uses a third
/// spelling (`3.14159265358979323`). All three agree to ~1e-15, far inside
/// the 1e-8 comparison tolerance, so the difference is unobservable here.
const PI: f64 = 3.14159265358979;
/// `epicsCalcTest.cpp:198`.
const D2R: f64 = PI / 180.0;
/// `epicsCalcTest.cpp:199`.
const R2D: f64 = 180.0 / PI;

/// C's int-valued comparison/logical operators widened to `double`, as they
/// are when `testExpr`'s C expression is passed to `testCalc(const char*,
/// double)`.
fn b(x: bool) -> f64 {
    if x { 1.0 } else { 0.0 }
}

/// `epicsMax` for `double` (`modules/libcom/src/cppStd/epicsAlgorithm.h`,
/// the `template <> const double&` specialization):
/// `return (a < b) || isnan(b) ? b : a;`
///
/// The specialization is what makes `MAX(x, NaN) == NaN`; the generic template
/// would return `a`. This matters for 20+ cases below, so it is reproduced
/// exactly rather than approximated with `f64::max` (which returns the
/// non-NaN operand - the opposite behaviour).
fn emax(a: f64, x: f64) -> f64 {
    if a < x || x.is_nan() { x } else { a }
}

/// `epicsMin` for `double`: `return (b < a) || isnan(b) ? b : a;`
fn emin(a: f64, x: f64) -> f64 {
    if x < a || x.is_nan() { x } else { a }
}

/// The test's variadic `MAX` (`epicsCalcTest.cpp:211-253`) is a left fold of
/// the two-argument `epicsMax`: `MAX(a,b,c) == MAX(MAX(a,b),c)`.
fn emax_n(v: &[f64]) -> f64 {
    v.iter().copied().reduce(emax).expect("MAX needs >= 1 arg")
}

/// The test's variadic `MIN` (`epicsCalcTest.cpp:255-297`), same shape.
fn emin_n(v: &[f64]) -> f64 {
    v.iter().copied().reduce(emin).expect("MIN needs >= 1 arg")
}

/// `epicsCalcTest.cpp:206`:
/// `#define NINT(x) (double)(long)((x) >= 0 ? (x)+0.5 : (x)-0.5)`
fn nint(x: f64) -> f64 {
    (if x >= 0.0 { x + 0.5 } else { x - 0.5 }) as i64 as f64
}

/// `epicsCalcTest.cpp:111`:
/// `uresult = (result < 0.0 ? (epicsUInt32)(epicsInt32)result : (epicsUInt32)result);`
fn to_uint32(result: f64) -> u32 {
    if result < 0.0 {
        (result as i32) as u32
    } else {
        result as u32
    }
}

/// `double args[CALCPERFORM_NARGS] = {1.0, 2.0, ... 21.0}`
/// (`epicsCalcTest.cpp:51-54`). A fresh copy per case, because `eval` takes
/// `&mut` and a `:=` store writes back into it.
fn fresh_args() -> [f64; 21] {
    let mut a = [0.0f64; 21];
    let mut i = 0;
    while i < 21 {
        a[i] = (i + 1) as f64;
        i += 1;
    }
    a
}

// The argument bits for `testArgs` (`epicsCalcTest.cpp:168-188`).
const A_A: u32 = 0x000001;
const A_B: u32 = 0x000002;
const A_C: u32 = 0x000004;
const A_D: u32 = 0x000008;
const A_E: u32 = 0x000010;
const A_F: u32 = 0x000020;
const A_G: u32 = 0x000040;
const A_H: u32 = 0x000080;
const A_I: u32 = 0x000100;
const A_J: u32 = 0x000200;
const A_K: u32 = 0x000400;
const A_L: u32 = 0x000800;
const A_M: u32 = 0x001000;
const A_N: u32 = 0x002000;
const A_O: u32 = 0x004000;
const A_P: u32 = 0x008000;
const A_Q: u32 = 0x010000;
const A_R: u32 = 0x020000;
const A_S: u32 = 0x040000;
const A_T: u32 = 0x080000;
const A_U: u32 = 0x100000;

// ---------------------------------------------------------------------------
// Error-code mapping
// ---------------------------------------------------------------------------

/// A `CalcError` with its payload erased, so a case can name the expected
/// *category* without pinning byte offsets Base never had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ec {
    BadChar,
    BadNumber,
    UnknownIdent,
    MissingOperand,
    ExtraOperand,
    Unbalanced,
    BadConditional,
    BadAssignment,
    TooLong,
}

fn ec(e: &CalcError) -> Ec {
    match e {
        CalcError::BadChar(..) => Ec::BadChar,
        CalcError::BadNumber(..) => Ec::BadNumber,
        CalcError::UnknownIdent(..) => Ec::UnknownIdent,
        CalcError::MissingOperand => Ec::MissingOperand,
        CalcError::ExtraOperand => Ec::ExtraOperand,
        CalcError::Unbalanced => Ec::Unbalanced,
        CalcError::BadConditional => Ec::BadConditional,
        CalcError::BadAssignment => Ec::BadAssignment,
        CalcError::TooLong => Ec::TooLong,
    }
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Corpus {
    failures: Vec<String>,
    skips: Vec<String>,
    ran: usize,
}

impl Corpus {
    /// `testCalc(expr, expected)`, `epicsCalcTest.cpp:48-84`.
    fn calc(&mut self, expr: &'static str, want: f64) {
        self.ran += 1;
        let mut args = fresh_args();
        let got = match compile(expr) {
            Ok(p) => p.eval(&mut args),
            Err(e) => {
                self.failures
                    .push(format!("testCalc {expr:?}: compile error {e:?} (expected {want})"));
                return;
            }
        };
        // `epicsCalcTest.cpp:72-78`, transcribed exactly.
        let pass = if want.is_finite() && got.is_finite() {
            (want - got).abs() < 1e-8
        } else if want.is_nan() {
            got.is_nan()
        } else {
            got == want
        };
        if !pass {
            self.failures
                .push(format!("testCalc {expr:?}: expected {want}, got {got}"));
        }
    }

    /// A case left visible but not run, because of a crate bug that could not
    /// be fixed within this task. Never used to hide a mismatch silently.
    #[allow(dead_code)]
    fn skip(&mut self, expr: &'static str, why: &'static str) {
        self.ran += 1;
        self.skips.push(format!("{expr:?}: {why}"));
    }

    /// `testUInt32Calc(expr, expected)`, `epicsCalcTest.cpp:86-119`.
    fn uint32(&mut self, expr: &'static str, want: u32) {
        self.ran += 1;
        let mut args = fresh_args();
        let got = match compile(expr) {
            Ok(p) => p.eval(&mut args),
            Err(e) => {
                self.failures.push(format!(
                    "testUInt32Calc {expr:?}: compile error {e:?} (expected {want:#x})"
                ));
                return;
            }
        };
        let ugot = to_uint32(got);
        if ugot != want {
            self.failures.push(format!(
                "testUInt32Calc {expr:?}: expected {want:#x}, got {ugot:#x} (raw {got})"
            ));
        }
    }

    /// `testArgs(expr, einp, eout)`, `epicsCalcTest.cpp:121-143`.
    fn args(&mut self, expr: &'static str, einp: u32, eout: u32) {
        self.ran += 1;
        let usage = match compile(expr) {
            Ok(p) => p.arg_usage(),
            Err(e) => {
                self.failures
                    .push(format!("testArgs {expr:?}: compile error {e:?}"));
                return;
            }
        };
        if usage.inputs != einp || usage.stores != eout {
            self.failures.push(format!(
                "testArgs {expr:?}: expected ({einp:x}, {eout:x}) got ({:x}, {:x})",
                usage.inputs, usage.stores
            ));
        }
    }

    /// `testBadExpr(expr, expected_err)`, `epicsCalcTest.cpp:145-162`.
    ///
    /// `base` names the `CALC_ERR_*` constant the corpus asserts, purely for
    /// the failure message; `want` is this crate's corresponding `CalcError`
    /// category. See the mapping table in the test body.
    fn bad(&mut self, expr: &'static str, base: &'static str, want: Ec) {
        self.ran += 1;
        match compile(expr) {
            Ok(_) => self.failures.push(format!(
                "testBadExpr {expr:?}: expected {base} ({want:?}), compiled successfully"
            )),
            Err(e) => {
                if ec(&e) != want {
                    self.failures.push(format!(
                        "testBadExpr {expr:?}: expected {base} -> {want:?}, got {:?}",
                        ec(&e)
                    ));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Slice 1: LITERAL_OPERAND, OPERAND, UNARY_MINUS, UNARY_OPERATOR
// (epicsCalcTest.cpp:317-575)
// ---------------------------------------------------------------------------

fn slice_literals_and_operands(c: &mut Corpus) {
    // LITERAL_OPERAND elements (`:317-337`)
    c.calc("0", 0.0);
    c.calc("1", 1.0);
    c.calc("2", 2.0);
    c.calc("3", 3.0);
    c.calc("4", 4.0);
    c.calc("5", 5.0);
    c.calc("6", 6.0);
    c.calc("7", 7.0);
    c.calc("8", 8.0);
    c.calc("9", 9.0);
    c.calc(".1", 0.1);
    c.calc("0.1", 0.1);
    c.calc("0X0", 0.0);
    c.calc("0x10", 16.0);
    c.calc("0x7fffffff", 2147483647.0);
    // `:333-334`. Base parses hex with `epicsParseUInt32` and stores the bits
    // as a `LITERAL_INT`, which `calcPerform.c:68-71` reloads as `epicsInt32`
    // - so a hex literal with bit 31 set comes out NEGATIVE.
    c.calc("0x80000000", -2147483648.0);
    c.calc("0xffffffff", -1.0);
    c.calc("Inf", INF);
    c.calc("Infinity", INF);
    c.calc("NaN", NAN);

    // OPERAND elements (`:339-363`). args[] is 1.0 ..= 21.0.
    c.calc("a", 1.0);
    c.calc("b", 2.0);
    c.calc("c", 3.0);
    c.calc("d", 4.0);
    c.calc("e", 5.0);
    c.calc("f", 6.0);
    c.calc("g", 7.0);
    c.calc("h", 8.0);
    c.calc("i", 9.0);
    c.calc("j", 10.0);
    c.calc("k", 11.0);
    c.calc("l", 12.0);
    c.calc("m", 13.0);
    c.calc("n", 14.0);
    c.calc("o", 15.0);
    c.calc("p", 16.0);
    c.calc("q", 17.0);
    c.calc("r", 18.0);
    c.calc("s", 19.0);
    c.calc("t", 20.0);
    c.calc("u", 21.0);
    c.calc("PI", PI);
    c.calc("D2R", D2R);
    c.calc("R2D", R2D);

    // `rndm` (`:365-372`) is handled by `rndm_stays_in_unit_interval` below,
    // because it is nondeterministic by design (`calcPerform.c:509-521`) and
    // must go through `eval_with_rng`.

    // UNARY_MINUS element (`:374-378`)
    c.calc("-1", -1.0);
    c.calc("-Inf", -INF);
    c.calc("- -1", 1.0);
    c.calc("-0x80000000", 2147483648.0);

    // UNARY_OPERATOR elements (`:380-...`)
    c.calc("(1)", 1.0);
    c.calc("!0", b(true));
    c.calc("!1", b(false));
    c.calc("!!0", b(false));
    c.calc("ABS(1.0)", 1.0f64.abs());
    c.calc("ABS(-1.)", (-1.0f64).abs());
    c.calc("acos(1.)", 1.0f64.acos());
    c.calc("asin(0.5)", 0.5f64.asin());
    c.calc("atan(0.5)", 0.5f64.atan());
    // `:202`: `#define ATAN2(x,y) atan2(y,x)` - the test's macro swaps the
    // arguments to match `calcPerform.c:225-228`'s self-described
    // "Ouch!: Args backwards!". So the C side computes `atan2(2., 1.)`.
    c.calc("ATAN2(1., 2.)", 2.0f64.atan2(1.0));
    c.calc("ceil(0.5)", 0.5f64.ceil());
    c.calc("cos(0.5)", 0.5f64.cos());
    c.calc("cosh(0.5)", 0.5f64.cosh());
    c.calc("exp(1.)", 1.0f64.exp());
    c.calc("floor(1.5)", 1.5f64.floor());
    // Rust's `%` on f64 is C's `fmod` (truncated, sign of the dividend).
    c.calc("fmod(1.5, 1.0)", 1.5f64 % 1.0);
    c.calc("fmod(-1.5, 1.0)", -1.5f64 % 1.0);
    c.calc("fmod(1.5, -1.0)", 1.5f64 % -1.0);
    c.calc("fmod(-1.5, -1.0)", -1.5f64 % -1.0);
    c.calc("fmod(1.5, 0.0)", 1.5f64 % 0.0);

    c.calc("finite(0.)", b(true));
    c.calc("finite(Inf)", b(false));
    c.calc("finite(-Inf)", b(false));
    c.calc("finite(NaN)", b(false));
    c.calc("finite(0,1,2)", 1.0);
    c.calc("finite(0,1,NaN)", 0.0);
    c.calc("finite(0,NaN,2)", 0.0);
    c.calc("finite(NaN,1,2)", 0.0);
    c.calc("finite(0,1,Inf)", 0.0);
    c.calc("finite(0,Inf,2)", 0.0);
    c.calc("finite(Inf,1,2)", 0.0);
    c.calc("finite(0,1,-Inf)", 0.0);
    c.calc("finite(0,-Inf,2)", 0.0);
    c.calc("finite(-Inf,1,2)", 0.0);
    c.calc("isinf(0.)", b(false));
    c.calc("isinf(Inf)", b(true));
    // `:418`: the `!!` is the corpus's own, because some GCCs' `isinf` returns
    // -1 for -Inf. It normalises both sides, so the expected value is 1.
    c.calc("!!isinf(-Inf)", b(true));
    c.calc("isinf(NaN)", b(false));
    c.calc("isnan(0.)", b(false));
    c.calc("isnan(Inf)", b(false));
    c.calc("isnan(-Inf)", b(false));
    c.calc("!!isnan(NaN)", b(true));
    c.calc("isnan(0,1,2)", 0.0);
    c.calc("isnan(0,1,NaN)", 1.0);
    c.calc("isnan(0,NaN,2)", 1.0);
    c.calc("isnan(NaN,1,2)", 1.0);
    c.calc("isnan(0,1,Inf)", 0.0);
    c.calc("isnan(0,Inf,2)", 0.0);
    c.calc("isnan(Inf,1,2)", 0.0);
    c.calc("isnan(0,1,-Inf)", 0.0);
    c.calc("isnan(0,-Inf,2)", 0.0);
    c.calc("isnan(-Inf,1,2)", 0.0);

    c.calc("LN(5.)", 5.0f64.ln());
    c.calc("LOG(5.)", 5.0f64.log10());
    c.calc("LOGE(2.)", 2.0f64.ln());
}

// ---------------------------------------------------------------------------
// The test entry point
// ---------------------------------------------------------------------------

#[test]
fn base_corpus() {
    let mut c = Corpus::default();

    slice_literals_and_operands(&mut c);

    if !c.skips.is_empty() {
        eprintln!(
            "{} case(s) skipped (crate bugs, see task-9-report.md):\n{}",
            c.skips.len(),
            c.skips.join("\n")
        );
    }
    assert!(
        c.failures.is_empty(),
        "{} of {} case(s) failed:\n{}",
        c.failures.len(),
        c.ran,
        c.failures.join("\n")
    );
}

/// `epicsCalcTest.cpp:365-372`: 100 evaluations of `rndm`, each required to
/// land in `[0, 1]`.
///
/// `RNDM` is nondeterministic by design (`calcPerform.c:509-521` seeds once
/// from monotonic time), so this goes through `eval_with_rng` with a supplied
/// generator rather than `eval`. Note what that does and does not pin: it
/// proves `RNDM` compiles, is nullary, and that the value the generator
/// returns reaches the result unaltered. It does *not* pin the crate's own
/// default generator's range - `eval_with_rng` bypasses it entirely. Base's
/// loop tests its built-in `calcRandom`; there is no non-flaky way to test a
/// time-seeded generator's range from here, so that half is deliberately not
/// claimed.
#[test]
fn rndm_stays_in_unit_interval() {
    let prog = compile("rndm").expect("rndm compiles");
    // A deterministic stand-in for `calcRandom` (xorshift64*, values in
    // [0, 1)), so this test cannot flake.
    let mut state: u64 = 0x2545F4914F6CDD1D;
    let mut rng = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    for repeat in 0..100 {
        let mut args = fresh_args();
        let res = prog.eval_with_rng(&mut args, &mut rng);
        assert!(
            (0.0..=1.0).contains(&res),
            "rndm returned {res} on iteration {repeat}"
        );
    }
}
