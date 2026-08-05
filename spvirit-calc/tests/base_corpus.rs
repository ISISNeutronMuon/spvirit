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
// Slice 2: MAX / MIN (epicsCalcTest.cpp:439-562) and the remaining
// UNARY_OPERATOR cases (:564-575)
// ---------------------------------------------------------------------------

fn slice_max_min(c: &mut Corpus) {
    // MAX (`:439-500`). Every expected value is `emax_n` over exactly the
    // arguments in the expression text, which is what the test's overload set
    // (`:211-253`) computes.
    c.calc("MAX(-99)", emax_n(&[-99.0]));
    c.calc("MAX( 1., 2.)", emax_n(&[1.0, 2.0]));
    c.calc("MAX( 1., Inf)", emax_n(&[1.0, INF]));
    c.calc("MAX( 1.,-Inf)", emax_n(&[1.0, -INF]));
    c.calc("MAX( 1., NaN)", emax_n(&[1.0, NAN]));
    c.calc("MAX( Inf, 1.)", emax_n(&[INF, 1.0]));
    c.calc("MAX(-Inf, 1.)", emax_n(&[-INF, 1.0]));
    c.calc("MAX( NaN, 1.)", emax_n(&[NAN, 1.0]));
    c.calc("MAX( 1., 2.,3.)", emax_n(&[1.0, 2.0, 3.0]));
    c.calc("MAX( 1., 3.,2.)", emax_n(&[1.0, 3.0, 2.0]));
    c.calc("MAX( 2., 1.,3.)", emax_n(&[2.0, 1.0, 3.0]));
    c.calc("MAX( 2., 3.,1.)", emax_n(&[2.0, 3.0, 1.0]));
    c.calc("MAX( 3., 1.,2.)", emax_n(&[3.0, 1.0, 2.0]));
    c.calc("MAX( 3., 2.,1.)", emax_n(&[3.0, 2.0, 1.0]));
    c.calc("MAX( 1., 2., Inf)", emax_n(&[1.0, 2.0, INF]));
    c.calc("MAX( 1., 2.,-Inf)", emax_n(&[1.0, 2.0, -INF]));
    c.calc("MAX( 1., 2., NaN)", emax_n(&[1.0, 2.0, NAN]));
    c.calc("MAX( 1., Inf,2.)", emax_n(&[1.0, INF, 2.0]));
    c.calc("MAX( 1.,-Inf,2.)", emax_n(&[1.0, -INF, 2.0]));
    c.calc("MAX( 1., NaN,2.)", emax_n(&[1.0, NAN, 2.0]));
    c.calc("MAX( Inf, 1.,2.)", emax_n(&[INF, 1.0, 2.0]));
    c.calc("MAX(-Inf, 1.,2.)", emax_n(&[-INF, 1.0, 2.0]));
    c.calc("MAX( NaN, 1.,2.)", emax_n(&[NAN, 1.0, 2.0]));
    c.calc("MAX( 1., 2., 3., 4.)", emax_n(&[1.0, 2.0, 3.0, 4.0]));
    c.calc("MAX( 1., 2., 4., 3.)", emax_n(&[1.0, 2.0, 4.0, 3.0]));
    c.calc("MAX( 1., 4., 3., 2.)", emax_n(&[1.0, 4.0, 3.0, 2.0]));
    c.calc("MAX( 4., 2., 3., 1.)", emax_n(&[4.0, 2.0, 3.0, 1.0]));
    c.calc("MAX( 1., 2., 3.,NaN)", emax_n(&[1.0, 2.0, 3.0, NAN]));
    c.calc("MAX( 1., 2.,NaN, 3.)", emax_n(&[1.0, 2.0, NAN, 3.0]));
    c.calc("MAX( 1.,NaN, 3., 2.)", emax_n(&[1.0, NAN, 3.0, 2.0]));
    c.calc("MAX(NaN, 2., 3., 1.)", emax_n(&[NAN, 2.0, 3.0, 1.0]));
    c.calc("MAX( 1., 2., 3., 4., 5.)", emax_n(&[1.0, 2.0, 3.0, 4.0, 5.0]));
    c.calc("MAX( 1., 2., 3., 5., 4.)", emax_n(&[1.0, 2.0, 3.0, 5.0, 4.0]));
    c.calc("MAX( 1., 2., 5., 4., 3.)", emax_n(&[1.0, 2.0, 5.0, 4.0, 3.0]));
    c.calc("MAX( 1., 5., 3., 4., 2.)", emax_n(&[1.0, 5.0, 3.0, 4.0, 2.0]));
    c.calc("MAX( 5., 2., 3., 4., 1.)", emax_n(&[5.0, 2.0, 3.0, 4.0, 1.0]));
    c.calc("MAX( 1., 2., 3., 4.,NaN)", emax_n(&[1.0, 2.0, 3.0, 4.0, NAN]));
    c.calc("MAX( 1., 2., 3.,NaN, 4.)", emax_n(&[1.0, 2.0, 3.0, NAN, 4.0]));
    c.calc("MAX( 1., 2.,NaN, 4., 3.)", emax_n(&[1.0, 2.0, NAN, 4.0, 3.0]));
    c.calc("MAX( 1.,NaN, 3., 4., 2.)", emax_n(&[1.0, NAN, 3.0, 4.0, 2.0]));
    c.calc("MAX(NaN, 2., 3., 4., 1.)", emax_n(&[NAN, 2.0, 3.0, 4.0, 1.0]));
    c.calc("MAX( 1., 2., 3., 4., 5., 6.)", emax_n(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]));
    c.calc("MAX( 1., 2., 3., 4., 6., 5.)", emax_n(&[1.0, 2.0, 3.0, 4.0, 6.0, 5.0]));
    c.calc("MAX( 1., 2., 3., 6., 5., 4.)", emax_n(&[1.0, 2.0, 3.0, 6.0, 5.0, 4.0]));
    c.calc("MAX( 1., 2., 6., 4., 5., 3.)", emax_n(&[1.0, 2.0, 6.0, 4.0, 5.0, 3.0]));
    c.calc("MAX( 1., 6., 3., 4., 5., 2.)", emax_n(&[1.0, 6.0, 3.0, 4.0, 5.0, 2.0]));
    c.calc("MAX( 6., 2., 3., 4., 5., 1.)", emax_n(&[6.0, 2.0, 3.0, 4.0, 5.0, 1.0]));
    c.calc("MAX( 1., 2., 3., 4., 5.,NaN)", emax_n(&[1.0, 2.0, 3.0, 4.0, 5.0, NAN]));
    c.calc("MAX( 1., 2., 3., 4.,NaN, 5.)", emax_n(&[1.0, 2.0, 3.0, 4.0, NAN, 5.0]));
    c.calc("MAX( 1., 2., 3.,NaN, 5., 4.)", emax_n(&[1.0, 2.0, 3.0, NAN, 5.0, 4.0]));
    c.calc("MAX( 1., 2.,NaN, 4., 5., 3.)", emax_n(&[1.0, 2.0, NAN, 4.0, 5.0, 3.0]));
    c.calc("MAX( 1.,NaN, 3., 4., 5., 2.)", emax_n(&[1.0, NAN, 3.0, 4.0, 5.0, 2.0]));
    c.calc("MAX(NaN, 2., 3., 4., 5., 1.)", emax_n(&[NAN, 2.0, 3.0, 4.0, 5.0, 1.0]));
    c.calc("MAX( 1., 2., 3., 4., 5.,Inf)", emax_n(&[1.0, 2.0, 3.0, 4.0, 5.0, INF]));
    c.calc("MAX( 1., 2., 3., 4.,Inf, 5.)", emax_n(&[1.0, 2.0, 3.0, 4.0, INF, 5.0]));
    c.calc("MAX( 1., 2., 3.,Inf, 5., 4.)", emax_n(&[1.0, 2.0, 3.0, INF, 5.0, 4.0]));
    c.calc("MAX( 1., 2.,Inf, 4., 5., 3.)", emax_n(&[1.0, 2.0, INF, 4.0, 5.0, 3.0]));
    c.calc("MAX( 1.,Inf, 3., 4., 5., 2.)", emax_n(&[1.0, INF, 3.0, 4.0, 5.0, 2.0]));
    c.calc("MAX(Inf, 2., 3., 4., 5., 1.)", emax_n(&[INF, 2.0, 3.0, 4.0, 5.0, 1.0]));
    c.calc(
        "MAX(1,2,3,4,5,6,7,8,9,10,11,12)",
        emax_n(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0]),
    );
    c.calc(
        "MAX(5,4,3,2,1,0,-1,-2,-3,-4,-5,-6)",
        emax_n(&[5.0, 4.0, 3.0, 2.0, 1.0, 0.0, -1.0, -2.0, -3.0, -4.0, -5.0, -6.0]),
    );
    c.calc("MAX(-1,1,0)", emax_n(&[-1.0, 1.0, 0.0]));

    // MIN (`:502-562`)
    c.calc("MIN(99)", emin_n(&[99.0]));
    c.calc("MIN(1.,2.)", emin_n(&[1.0, 2.0]));
    c.calc("MIN(1.,Inf)", emin_n(&[1.0, INF]));
    c.calc("MIN(1.,-Inf)", emin_n(&[1.0, -INF]));
    c.calc("MIN(1.,NaN)", emin_n(&[1.0, NAN]));
    c.calc("MIN(NaN,1.)", emin_n(&[NAN, 1.0]));
    c.calc("MIN( 1., 2.,3.)", emin_n(&[1.0, 2.0, 3.0]));
    c.calc("MIN( 1., 3.,2.)", emin_n(&[1.0, 3.0, 2.0]));
    c.calc("MIN( 2., 1.,3.)", emin_n(&[2.0, 1.0, 3.0]));
    c.calc("MIN( 2., 3.,1.)", emin_n(&[2.0, 3.0, 1.0]));
    c.calc("MIN( 3., 1.,2.)", emin_n(&[3.0, 1.0, 2.0]));
    c.calc("MIN( 3., 2.,1.)", emin_n(&[3.0, 2.0, 1.0]));
    c.calc("MIN( 1., 2., Inf)", emin_n(&[1.0, 2.0, INF]));
    c.calc("MIN( 1., 2.,-Inf)", emin_n(&[1.0, 2.0, -INF]));
    c.calc("MIN( 1., 2., NaN)", emin_n(&[1.0, 2.0, NAN]));
    c.calc("MIN( 1., Inf,2.)", emin_n(&[1.0, INF, 2.0]));
    c.calc("MIN( 1.,-Inf,2.)", emin_n(&[1.0, -INF, 2.0]));
    c.calc("MIN( 1., NaN,2.)", emin_n(&[1.0, NAN, 2.0]));
    c.calc("MIN( Inf, 1.,2.)", emin_n(&[INF, 1.0, 2.0]));
    c.calc("MIN(-Inf, 1.,2.)", emin_n(&[-INF, 1.0, 2.0]));
    c.calc("MIN( NaN, 1.,2.)", emin_n(&[NAN, 1.0, 2.0]));
    c.calc("MIN( 1., 2., 3., 4.)", emin_n(&[1.0, 2.0, 3.0, 4.0]));
    c.calc("MIN( 1., 2., 4., 3.)", emin_n(&[1.0, 2.0, 4.0, 3.0]));
    c.calc("MIN( 1., 4., 3., 2.)", emin_n(&[1.0, 4.0, 3.0, 2.0]));
    c.calc("MIN( 4., 2., 3., 1.)", emin_n(&[4.0, 2.0, 3.0, 1.0]));
    c.calc("MIN( 1., 2., 3.,NaN)", emin_n(&[1.0, 2.0, 3.0, NAN]));
    c.calc("MIN( 1., 2.,NaN, 3.)", emin_n(&[1.0, 2.0, NAN, 3.0]));
    c.calc("MIN( 1.,NaN, 3., 2.)", emin_n(&[1.0, NAN, 3.0, 2.0]));
    c.calc("MIN(NaN, 2., 3., 1.)", emin_n(&[NAN, 2.0, 3.0, 1.0]));
    c.calc("MIN( 1., 2., 3., 4., 5.)", emin_n(&[1.0, 2.0, 3.0, 4.0, 5.0]));
    c.calc("MIN( 1., 2., 3., 5., 4.)", emin_n(&[1.0, 2.0, 3.0, 5.0, 4.0]));
    c.calc("MIN( 1., 2., 5., 4., 3.)", emin_n(&[1.0, 2.0, 5.0, 4.0, 3.0]));
    c.calc("MIN( 1., 5., 3., 4., 2.)", emin_n(&[1.0, 5.0, 3.0, 4.0, 2.0]));
    c.calc("MIN( 5., 2., 3., 4., 1.)", emin_n(&[5.0, 2.0, 3.0, 4.0, 1.0]));
    c.calc("MIN( 1., 2., 3., 4.,NaN)", emin_n(&[1.0, 2.0, 3.0, 4.0, NAN]));
    c.calc("MIN( 1., 2., 3.,NaN, 4.)", emin_n(&[1.0, 2.0, 3.0, NAN, 4.0]));
    c.calc("MIN( 1., 2.,NaN, 4., 3.)", emin_n(&[1.0, 2.0, NAN, 4.0, 3.0]));
    c.calc("MIN( 1.,NaN, 3., 4., 2.)", emin_n(&[1.0, NAN, 3.0, 4.0, 2.0]));
    c.calc("MIN(NaN, 2., 3., 4., 1.)", emin_n(&[NAN, 2.0, 3.0, 4.0, 1.0]));
    c.calc("MIN( 1., 2., 3., 4., 5., 6.)", emin_n(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]));
    c.calc("MIN( 2., 1., 3., 4., 5., 6.)", emin_n(&[2.0, 1.0, 3.0, 4.0, 5.0, 6.0]));
    c.calc("MIN( 3., 2., 1., 4., 5., 6.)", emin_n(&[3.0, 2.0, 1.0, 4.0, 5.0, 6.0]));
    c.calc("MIN( 4., 2., 3., 1., 5., 6.)", emin_n(&[4.0, 2.0, 3.0, 1.0, 5.0, 6.0]));
    c.calc("MIN( 5., 2., 3., 4., 1., 6.)", emin_n(&[5.0, 2.0, 3.0, 4.0, 1.0, 6.0]));
    c.calc("MIN( 6., 2., 3., 4., 5., 1.)", emin_n(&[6.0, 2.0, 3.0, 4.0, 5.0, 1.0]));
    c.calc("MIN( 1., 2., 3., 4., 5.,NaN)", emin_n(&[1.0, 2.0, 3.0, 4.0, 5.0, NAN]));
    c.calc("MIN( 1., 2., 3., 4.,NaN, 5.)", emin_n(&[1.0, 2.0, 3.0, 4.0, NAN, 5.0]));
    c.calc("MIN( 1., 2., 3.,NaN, 5., 4.)", emin_n(&[1.0, 2.0, 3.0, NAN, 5.0, 4.0]));
    c.calc("MIN( 1., 2.,NaN, 4., 5., 3.)", emin_n(&[1.0, 2.0, NAN, 4.0, 5.0, 3.0]));
    c.calc("MIN( 1.,NaN, 3., 4., 5., 2.)", emin_n(&[1.0, NAN, 3.0, 4.0, 5.0, 2.0]));
    c.calc("MIN(NaN, 2., 3., 4., 5., 1.)", emin_n(&[NAN, 2.0, 3.0, 4.0, 5.0, 1.0]));
    c.calc("MIN( 1., 2., 3., 4., 5.,-Inf)", emin_n(&[1.0, 2.0, 3.0, 4.0, 5.0, -INF]));
    c.calc("MIN( 1., 2., 3., 4.,-Inf, 5.)", emin_n(&[1.0, 2.0, 3.0, 4.0, -INF, 5.0]));
    c.calc("MIN( 1., 2., 3.,-Inf, 5., 4.)", emin_n(&[1.0, 2.0, 3.0, -INF, 5.0, 4.0]));
    c.calc("MIN( 1., 2.,-Inf, 4., 5., 3.)", emin_n(&[1.0, 2.0, -INF, 4.0, 5.0, 3.0]));
    c.calc("MIN( 1.,-Inf, 3., 4., 5., 2.)", emin_n(&[1.0, -INF, 3.0, 4.0, 5.0, 2.0]));
    c.calc("MIN(-Inf, 2., 3., 4., 5., 1.)", emin_n(&[-INF, 2.0, 3.0, 4.0, 5.0, 1.0]));
    c.calc(
        "MIN(1,2,3,4,5,6,7,8,9,10,11,12)",
        emin_n(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0]),
    );
    c.calc(
        "MIN(5,4,3,2,1,0,-1,-2,-3,-4,-5,-6)",
        emin_n(&[5.0, 4.0, 3.0, 2.0, 1.0, 0.0, -1.0, -2.0, -3.0, -4.0, -5.0, -6.0]),
    );
    c.calc("MIN(1,-1,0)", emin_n(&[1.0, -1.0, 0.0]));
    // `:562` - nested vararg calls, which also pins that each `)` fixes up its
    // own argument count rather than a shared one.
    c.calc(
        "MAX(MIN(0,2),MAX(0),MIN(3,2,1))",
        emax_n(&[emin_n(&[0.0, 2.0]), emax_n(&[0.0]), emin_n(&[3.0, 2.0, 1.0])]),
    );

    // Remaining UNARY_OPERATOR cases (`:564-575`)
    c.calc("NINT(0.4)", nint(0.4));
    c.calc("NINT(0.6)", nint(0.6));
    c.calc("NINT(-0.4)", nint(-0.4));
    c.calc("NINT(-0.6)", nint(-0.6));
    c.calc("sin(0.5)", 0.5f64.sin());
    c.calc("sinh(0.5)", 0.5f64.sinh());
    // `:208`: `#define SQR(x) sqrt(x)` - SQR is square ROOT in CALC.
    c.calc("SQR(10.)", 10.0f64.sqrt());
    c.calc("sqrt(16.)", 16.0f64.sqrt());
    c.calc("tan(0.5)", 0.5f64.tan());
    c.calc("tanh(0.5)", 0.5f64.tanh());
    c.calc("~5", !5i32 as f64);
    c.calc("~~5", !!5i32 as f64);
}

// ---------------------------------------------------------------------------
// Slice 3: BINARY_OPERATOR elements (epicsCalcTest.cpp:577-829)
// ---------------------------------------------------------------------------

fn slice_binary_operators(c: &mut Corpus) {
    // `!=` (`:578-596`). Rust and C agree on every IEEE-754 comparison,
    // including the NaN rows, so these are direct translations.
    c.calc("0 != 1", b(0.0 != 1.0));
    c.calc("0 != 0", b(0.0 != 0.0));
    c.calc("1 != 0", b(1.0 != 0.0));
    // C parses this as `(1 != 0) != 2`, left-associative, the int 1 vs 2.
    c.calc("1 != 0 != 2", b(b(1.0 != 0.0) != 2.0));
    c.calc("0.0 != Inf", b(0.0 != INF));
    c.calc("0.0 != -Inf", b(0.0 != -INF));
    c.calc("0.0 != NaN", b(0.0 != NAN));
    c.calc("Inf != 0.0", b(INF != 0.0));
    c.calc("Inf != Inf", b(INF != INF));
    c.calc("Inf != -Inf", b(INF != -INF));
    c.calc("Inf != NaN", b(INF != NAN));
    c.calc("-Inf != 0.0", b(-INF != 0.0));
    c.calc("-Inf != Inf", b(-INF != INF));
    c.calc("-Inf != -Inf", b(-INF != -INF));
    c.calc("-Inf != NaN", b(-INF != NAN));
    c.calc("NaN != 0.0", b(NAN != 0.0));
    c.calc("NaN != Inf", b(NAN != INF));
    c.calc("NaN != -Inf", b(NAN != -INF));
    c.calc("NaN != NaN", b(NAN != NAN));

    // `#` is Base's second spelling of `!=` (`refs/postfix.c:148`), `:598-601`
    c.calc("0 # 1", b(0.0 != 1.0));
    c.calc("0 # 0", b(0.0 != 0.0));
    c.calc("1 # 0", b(1.0 != 0.0));
    c.calc("1 # 0 # 2", b(b(1.0 != 0.0) != 2.0));

    // `%` (`:603-606`). RULINGS.md Ruling 5 / `calcPerform.c` `case MODULO`:
    // integer modulo through `epicsInt32`, NaN on a zero divisor - hence the
    // expected values are C *int* `%`, not `fmod`. Rust's `i32 %` truncates
    // toward zero exactly as C99's does, so `-7 % 4 == -3` in both.
    c.calc("7 % 4", (7i32 % 4i32) as f64);
    c.calc("-7 % 4", (-7i32 % 4i32) as f64);
    c.calc("63 % 16 % 6", ((63i32 % 16i32) % 6i32) as f64);
    c.calc("1 % 0", NAN);

    c.calc("7 & 4", (7i32 & 4i32) as f64);

    // `&&` (`:610-613`)
    c.calc("0 && 0", b(false));
    c.calc("0 && 1", b(false));
    c.calc("1 && 0", b(false));
    c.calc("1 && 1", b(true));

    // `*` (`:615-630`)
    c.calc("2 * 2", 2.0 * 2.0);
    c.calc("0.0 * Inf", 0.0 * INF);
    c.calc("0.0 * -Inf", 0.0 * -INF);
    c.calc("0.0 * NaN", 0.0 * NAN);
    c.calc("Inf * 0.0", INF * 0.0);
    c.calc("Inf * Inf", INF * INF);
    c.calc("Inf * -Inf", INF * -INF);
    c.calc("Inf * NaN", INF * NAN);
    c.calc("-Inf * 0.0", -INF * 0.0);
    c.calc("-Inf * Inf", -INF * INF);
    c.calc("-Inf * -Inf", -INF * -INF);
    c.calc("-Inf * NaN", -INF * NAN);
    c.calc("NaN * 0.0", NAN * 0.0);
    c.calc("NaN * Inf", NAN * INF);
    c.calc("NaN * -Inf", NAN * -INF);
    c.calc("NaN * NaN", NAN * NAN);

    // `**` (`:632-636`)
    c.calc("2 ** 0.2", 2.0f64.powf(0.2));
    c.calc("2 ** -0.2", 2.0f64.powf(-0.2));
    c.calc("-0.2 ** 2", (-0.2f64).powf(2.0));
    c.calc("-0.2 ** -2", (-0.2f64).powf(-2.0));
    // `:636` - POWER is LEFT-associative (RULINGS.md Ruling 4):
    // pow(pow(2,2),3) == 64, not pow(2,pow(2,3)) == 256.
    c.calc("2 ** 2 ** 3", 2.0f64.powf(2.0).powf(3.0));

    // `+` (`:638-661`)
    c.calc("0 + 1", 0.0 + 1.0);
    c.calc("0.0 + Inf", 0.0 + INF);
    c.calc("0.0 + -Inf", 0.0 + -INF);
    c.calc("0.0 + NaN", 0.0 + NAN);
    c.calc("Inf + 0.0", INF + 0.0);
    c.calc("Inf + Inf", INF + INF);
    // `:644-648`: guarded by `#if defined(_WIN32) && defined(_MSC_VER)`. Both
    // arms of the `#if` produce the identical expression text "Inf + -Inf" and
    // the identical expected value NaN (the MSVC arm spells it out because
    // MSVC's constant folder mishandles the C-side expression, not because the
    // CALC answer differs), so the two macro invocations collapse to one case
    // here - which is also why the file has 689 invocations but testPlan(687).
    c.calc("Inf + -Inf", NAN);
    c.calc("Inf + NaN", INF + NAN);
    c.calc("-Inf + 0.0", -INF + 0.0);
    // `:651-655`, same collapse as above.
    c.calc("-Inf + Inf", NAN);
    c.calc("-Inf + -Inf", -INF + -INF);
    c.calc("-Inf + NaN", -INF + NAN);
    c.calc("NaN + 0.0", NAN + 0.0);
    c.calc("NaN + Inf", NAN + INF);
    c.calc("NaN + -Inf", NAN + -INF);
    c.calc("NaN + NaN", NAN + NAN);

    // `-` (`:663-679`)
    c.calc("0 - 1", 0.0 - 1.0);
    c.calc("0 - 1 - 2", 0.0 - 1.0 - 2.0);
    c.calc("0.0 - Inf", 0.0 - INF);
    c.calc("0.0 - -Inf", 0.0 - -INF);
    c.calc("0.0 - NaN", 0.0 - NAN);
    c.calc("Inf - 0.0", INF - 0.0);
    c.calc("Inf - Inf", INF - INF);
    c.calc("Inf - -Inf", INF - -INF);
    c.calc("Inf - NaN", INF - NAN);
    c.calc("-Inf - 0.0", -INF - 0.0);
    c.calc("-Inf - Inf", -INF - INF);
    c.calc("-Inf - -Inf", -INF - -INF);
    c.calc("-Inf - NaN", -INF - NAN);
    c.calc("NaN - 0.0", NAN - 0.0);
    c.calc("NaN - Inf", NAN - INF);
    c.calc("NaN - -Inf", NAN - -INF);
    c.calc("NaN - NaN", NAN - NAN);

    // `/` (`:681-697`)
    c.calc("2.0 / 3.0", 2.0 / 3.0);
    c.calc("1.0 / 2.0 / 3.0", 1.0 / 2.0 / 3.0);
    c.calc("0.0 / Inf", 0.0 / INF);
    c.calc("0.0 / -Inf", 0.0 / -INF);
    c.calc("0.0 / NaN", 0.0 / NAN);
    c.calc("Inf / 1.0", INF / 1.0);
    c.calc("Inf / Inf", INF / INF);
    c.calc("Inf / -Inf", INF / -INF);
    c.calc("Inf / NaN", INF / NAN);
    c.calc("-Inf / 1.0", -INF / 1.0);
    c.calc("-Inf / Inf", -INF / INF);
    c.calc("-Inf / -Inf", -INF / -INF);
    c.calc("-Inf / NaN", -INF / NAN);
    c.calc("NaN / 1.0", NAN / 1.0);
    c.calc("NaN / Inf", NAN / INF);
    c.calc("NaN / -Inf", NAN / -INF);
    c.calc("NaN / NaN", NAN / NAN);

    // `<` (`:699-717`)
    c.calc("0 < 1", b(0.0 < 1.0));
    c.calc("0 < 0", b(0.0 < 0.0));
    c.calc("1 < 0", b(1.0 < 0.0));
    c.calc("2 < 0 < 2", b(b(2.0 < 0.0) < 2.0));
    c.calc("0.0 < Inf", b(0.0 < INF));
    c.calc("0.0 < -Inf", b(0.0 < -INF));
    c.calc("0.0 < NaN", b(0.0 < NAN));
    c.calc("Inf < 0.0", b(INF < 0.0));
    c.calc("Inf < Inf", b(INF < INF));
    c.calc("Inf < -Inf", b(INF < -INF));
    c.calc("Inf < NaN", b(INF < NAN));
    c.calc("-Inf < 0.0", b(-INF < 0.0));
    c.calc("-Inf < Inf", b(-INF < INF));
    c.calc("-Inf < -Inf", b(-INF < -INF));
    c.calc("-Inf < NaN", b(-INF < NAN));
    c.calc("NaN < 0.0", b(NAN < 0.0));
    c.calc("NaN < Inf", b(NAN < INF));
    c.calc("NaN < -Inf", b(NAN < -INF));
    c.calc("NaN < NaN", b(NAN < NAN));

    // `<<` (`:719-720`)
    c.calc("1 << 2", (1i32 << 2) as f64);
    c.calc("1 << 3 << 2", ((1i32 << 3) << 2) as f64);

    // `<=` (`:722-740`)
    c.calc("0 <= 1", b(0.0 <= 1.0));
    c.calc("0 <= 0", b(0.0 <= 0.0));
    c.calc("1 <= 0", b(1.0 <= 0.0));
    c.calc("3 <= 2 <= 3", b(b(3.0 <= 2.0) <= 3.0));
    c.calc("0.0 <= Inf", b(0.0 <= INF));
    c.calc("0.0 <= -Inf", b(0.0 <= -INF));
    c.calc("0.0 <= NaN", b(0.0 <= NAN));
    c.calc("Inf <= 0.0", b(INF <= 0.0));
    c.calc("Inf <= Inf", b(INF <= INF));
    c.calc("Inf <= -Inf", b(INF <= -INF));
    c.calc("Inf <= NaN", b(INF <= NAN));
    c.calc("-Inf <= 0.0", b(-INF <= 0.0));
    c.calc("-Inf <= Inf", b(-INF <= INF));
    c.calc("-Inf <= -Inf", b(-INF <= -INF));
    c.calc("-Inf <= NaN", b(-INF <= NAN));
    c.calc("NaN <= 0.0", b(NAN <= 0.0));
    c.calc("NaN <= Inf", b(NAN <= INF));
    c.calc("NaN <= -Inf", b(NAN <= -INF));
    c.calc("NaN <= NaN", b(NAN <= NAN));

    // `=` is Base's second spelling of `==` (`refs/postfix.c:167`), `:742-745`
    c.calc("0 = 1", b(0.0 == 1.0));
    c.calc("0 = 0", b(0.0 == 0.0));
    c.calc("1 = 0", b(1.0 == 0.0));
    c.calc("2 = 2 = 1", b(b(2.0 == 2.0) == 1.0));

    // `==` (`:747-765`)
    c.calc("0 == 1", b(0.0 == 1.0));
    c.calc("0 == 0", b(0.0 == 0.0));
    c.calc("1 == 0", b(1.0 == 0.0));
    c.calc("2 == 2 == 1", b(b(2.0 == 2.0) == 1.0));
    c.calc("0.0 == Inf", b(0.0 == INF));
    c.calc("0.0 == -Inf", b(0.0 == -INF));
    c.calc("0.0 == NaN", b(0.0 == NAN));
    c.calc("Inf == 0.0", b(INF == 0.0));
    c.calc("Inf == Inf", b(INF == INF));
    c.calc("Inf == -Inf", b(INF == -INF));
    c.calc("Inf == NaN", b(INF == NAN));
    c.calc("-Inf == 0.0", b(-INF == 0.0));
    c.calc("-Inf == Inf", b(-INF == INF));
    c.calc("-Inf == -Inf", b(-INF == -INF));
    c.calc("-Inf == NaN", b(-INF == NAN));
    c.calc("NaN == 0.0", b(NAN == 0.0));
    c.calc("NaN == Inf", b(NAN == INF));
    c.calc("NaN == -Inf", b(NAN == -INF));
    c.calc("NaN == NaN", b(NAN == NAN));

    // `>` (`:767-785`)
    c.calc("0 > 1", b(0.0 > 1.0));
    c.calc("0 > 0", b(0.0 > 0.0));
    c.calc("1 > 0", b(1.0 > 0.0));
    c.calc("2 > 0 > 2", b(b(2.0 > 0.0) > 2.0));
    c.calc("0.0 > Inf", b(0.0 > INF));
    c.calc("0.0 > -Inf", b(0.0 > -INF));
    c.calc("0.0 > NaN", b(0.0 > NAN));
    c.calc("Inf > 0.0", b(INF > 0.0));
    c.calc("Inf > Inf", b(INF > INF));
    c.calc("Inf > -Inf", b(INF > -INF));
    c.calc("Inf > NaN", b(INF > NAN));
    c.calc("-Inf > 0.0", b(-INF > 0.0));
    c.calc("-Inf > Inf", b(-INF > INF));
    c.calc("-Inf > -Inf", b(-INF > -INF));
    c.calc("-Inf > NaN", b(-INF > NAN));
    c.calc("NaN > 0.0", b(NAN > 0.0));
    c.calc("NaN > Inf", b(NAN > INF));
    c.calc("NaN > -Inf", b(NAN > -INF));
    c.calc("NaN > NaN", b(NAN > NAN));

    // `>=` (`:787-805`)
    c.calc("0 >= 1", b(0.0 >= 1.0));
    c.calc("0 >= 0", b(0.0 >= 0.0));
    c.calc("1 >= 0", b(1.0 >= 0.0));
    c.calc("3 >= 2 >= 3", b(b(3.0 >= 2.0) >= 3.0));
    c.calc("0.0 >= Inf", b(0.0 >= INF));
    c.calc("0.0 >= -Inf", b(0.0 >= -INF));
    c.calc("0.0 >= NaN", b(0.0 >= NAN));
    c.calc("Inf >= 0.0", b(INF >= 0.0));
    c.calc("Inf >= Inf", b(INF >= INF));
    c.calc("Inf >= -Inf", b(INF >= -INF));
    c.calc("Inf >= NaN", b(INF >= NAN));
    c.calc("-Inf >= 0.0", b(-INF >= 0.0));
    c.calc("-Inf >= Inf", b(-INF >= INF));
    c.calc("-Inf >= -Inf", b(-INF >= -INF));
    c.calc("-Inf >= NaN", b(-INF >= NAN));
    c.calc("NaN >= 0.0", b(NAN >= 0.0));
    c.calc("NaN >= Inf", b(NAN >= INF));
    c.calc("NaN >= -Inf", b(NAN >= -INF));
    c.calc("NaN >= NaN", b(NAN >= NAN));

    // `>>` / `>>>` (`:807-810`). `>>` is arithmetic (i32), `>>>` logical (u32).
    c.calc("8 >> 1", (8i32 >> 1) as f64);
    c.calc("8 >>> 1", (8u32 >> 1u32) as f64);
    c.calc("64 >> 2 >> 1", ((64i32 >> 2) >> 1) as f64);
    c.calc("64 >>> 2 >>> 1", ((64u32 >> 2u32) >> 1u32) as f64);

    // Word forms (`:812-816`). `:201,207,209`: `AND`->`&`, `OR`->`|`,
    // `XOR`->`^` (C's xor; in CALC `^` is POWER and `XOR` is the xor).
    c.calc("7 AND 4", (7i32 & 4i32) as f64);
    c.calc("1 OR 8", (1i32 | 8i32) as f64);
    c.calc("3 XOR 9", (3i32 ^ 9i32) as f64);

    // `^` == POWER (`:818-822`)
    c.calc("2 ^ 0.2", 2.0f64.powf(0.2));
    c.calc("2 ^ -0.2", 2.0f64.powf(-0.2));
    c.calc("(-0.2) ^ 2", (-0.2f64).powf(2.0));
    c.calc("(-0.2) ^ -2", (-0.2f64).powf(-2.0));
    // `:822` - the case RULINGS.md Ruling 4 cites: left-associative, 64.
    c.calc("2 ^ 2 ^ 3", 2.0f64.powf(2.0).powf(3.0));

    c.calc("1 | 8", (1i32 | 8i32) as f64);

    // `||` (`:826-829`)
    c.calc("0 || 0", b(false));
    c.calc("0 || 1", b(true));
    c.calc("1 || 0", b(true));
    c.calc("1 || 1", b(true));
}

// ---------------------------------------------------------------------------
// The test entry point
// ---------------------------------------------------------------------------

#[test]
fn base_corpus() {
    let mut c = Corpus::default();

    slice_literals_and_operands(&mut c);
    slice_max_min(&mut c);
    slice_binary_operators(&mut c);

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
