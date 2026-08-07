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
//!   `OR`, `XOR`, `D2R`, `R2D`, `:193-297` - the `#define`s run to `:209` and
//!   the `MAX`/`MIN` overload sets to `:297`), the helper below reproduces that
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
//!   (`refs/postfix.c:263,283`), so strtod spellings such as `nan(0)` and
//!   `0x1p3` are accepted by Base and rejected by this crate. **Not
//!   corpus-tested** - no case below exercises them.
//! - `compile("")` returns `Ok(empty)` here where Base gives
//!   `CALC_ERR_NULL_ARG`. Pre-existing from Tasks 1-2; not corpus-tested
//!   either (Base's test never passes an empty expression).

// Every expected value below is written as a Rust expression that MIRRORS the
// CALC expression text it is checking, so that the two can be read side by
// side and the transcription audited against `epicsCalcTest.cpp` line by line.
// That is the whole method of this file, and it puts it structurally at odds
// with three lints:
//
// - `eq_op`: Base's corpus deliberately tests self-comparisons (`"A-A"`,
//   `"A=A"`, `"1/1"`) to pin NaN/identity behaviour, so the mirrored
//   expectation is genuinely `1.0 - 1.0`, `4.0 == 4.0`, and so on. Folding
//   them to constants would silently drop the correspondence.
// - `identity_op`: `"3 | 1 & 2"` exists to pin `&` binding tighter than `|`;
//   the mirror must keep the parenthesisation (`3 | (1 & 2)`) even though the
//   result happens to reduce to `3`. Reducing it would erase the very
//   precedence claim the case makes.
// - `approx_constant`: `PI` here is Base's own 14-digit test macro
//   (`epicsCalcTest.cpp:194`), not `std::f64::consts::PI`. Substituting the
//   std constant would mean this file no longer compares against the oracle.
//   See the doc comment on `PI` below.
//
// Scoped to this test target only; `spvirit-calc/src/` is clippy-clean
// without any allows.
#![allow(clippy::eq_op, clippy::identity_op, clippy::approx_constant)]

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
// Slice 4: CONDITIONAL, STORE_OPERATOR/EXPR_TERM, relative precedence,
// parentheses (epicsCalcTest.cpp:831-950)
// ---------------------------------------------------------------------------

fn slice_conditional_store_precedence(c: &mut Corpus) {
    // CONDITIONAL elements (`:831-843`). Note `NaN ? 1 : 2` is 1: C (and
    // `calcPerform.c:400-403`'s `*ptop-- == 0.0`) treat any non-zero double,
    // NaN included, as true.
    c.calc("0 ? 1 : 2", 2.0);
    c.calc("1 ? 1 : 2", 1.0);
    c.calc("Inf ? 1 : 2", 1.0);
    c.calc("NaN ? 1 : 2", 1.0);
    c.calc("0 ? 0 ? 2 : 3 : 4", 4.0);
    c.calc("0 ? 1 ? 2 : 3 : 4", 4.0);
    c.calc("1 ? 0 ? 2 : 3 : 4", 3.0);
    c.calc("1 ? 1 ? 2 : 3 : 4", 2.0);
    c.calc("0 ? 2 : 0 ? 3 : 4", 4.0);
    c.calc("0 ? 2 : 1 ? 3 : 4", 3.0);
    c.calc("1 ? 2 : 0 ? 3 : 4", 2.0);
    c.calc("1 ? 2 : 1 ? 3 : 4", 2.0);

    // STORE_OPERATOR and EXPR_TERM elements (`:845-888`).
    //
    // Two shapes, 21 letters each. `"x := 0; x"` stores then fetches, so the
    // result is 0 regardless of the slot's initial value; `"x; x := 0"`
    // fetches first, so the result is the slot's initial value (a=1 .. u=21).
    // Both leave runtime depth 1 overall, which is Base's only end-of-parse
    // requirement (`refs/postfix.c:499`, `runtime_depth != 1`) - note the
    // second shape's first segment is NOT a store, so "every segment but the
    // last must be a store" is not Base's rule and these 21 cases are what
    // show it.
    c.calc("a := 0; a", 0.0);
    c.calc("b := 0; b", 0.0);
    c.calc("c := 0; c", 0.0);
    c.calc("d := 0; d", 0.0);
    c.calc("e := 0; e", 0.0);
    c.calc("f := 0; f", 0.0);
    c.calc("g := 0; g", 0.0);
    c.calc("h := 0; h", 0.0);
    c.calc("i := 0; i", 0.0);
    c.calc("j := 0; j", 0.0);
    c.calc("k := 0; k", 0.0);
    c.calc("l := 0; l", 0.0);
    c.calc("m := 0; m", 0.0);
    c.calc("n := 0; n", 0.0);
    c.calc("o := 0; o", 0.0);
    c.calc("p := 0; p", 0.0);
    c.calc("q := 0; q", 0.0);
    c.calc("r := 0; r", 0.0);
    c.calc("s := 0; s", 0.0);
    c.calc("t := 0; t", 0.0);
    c.calc("u := 0; u", 0.0);

    c.calc("a; a := 0", 1.0);
    c.calc("b; b := 0", 2.0);
    c.calc("c; c := 0", 3.0);
    c.calc("d; d := 0", 4.0);
    c.calc("e; e := 0", 5.0);
    c.calc("f; f := 0", 6.0);
    c.calc("g; g := 0", 7.0);
    c.calc("h; h := 0", 8.0);
    c.calc("i; i := 0", 9.0);
    c.calc("j; j := 0", 10.0);
    c.calc("k; k := 0", 11.0);
    c.calc("l; l := 0", 12.0);
    c.calc("m; m := 0", 13.0);
    c.calc("n; n := 0", 14.0);
    c.calc("o; o := 0", 15.0);
    c.calc("p; p := 0", 16.0);
    c.calc("q; q := 0", 17.0);
    c.calc("r; r := 0", 18.0);
    c.calc("s; s := 0", 19.0);
    c.calc("t; t := 0", 20.0);
    c.calc("u; u := 0", 21.0);

    // Relative precedences (`:890-945`). The trailing `// n m` comments in the
    // corpus name the two priority levels each case pits against each other.
    c.calc("0 ? 1 : 2 | 4", (2i32 | 4i32) as f64); // 0 1
    c.calc("1 ? 1 : 2 | 4", 1.0); // 0 1
    c.calc("0 ? 2 | 4 : 1", 1.0); // 0 1
    c.calc("1 ? 2 | 4 : 1", (2i32 | 4i32) as f64); // 0 1
    c.calc("0 ? 1 : 2 & 3", (2i32 & 3i32) as f64); // 0 2
    c.calc("1 ? 1 : 2 & 3", 1.0); // 0 2
    c.calc("0 ? 2 & 3 : 1", 1.0); // 0 2
    c.calc("1 ? 2 & 3 : 1", (2i32 & 3i32) as f64); // 0 2
    c.calc("0 ? 2 : 3 >= 1", b(3.0 >= 1.0)); // 0 3
    c.calc("0 ? 3 >= 1 : 2", 2.0); // 0 3
    c.calc("1 ? 0 == 1 : 2", b(0.0 == 1.0)); // 0 3
    c.calc("1 ? 2 : 0 == 1", 2.0); // 0 3
    c.calc("0 ? 1 : 2 + 4", 2.0 + 4.0); // 0 4
    c.calc("1 ? 1 : 2 + 4", 1.0); // 0 4
    c.calc("0 ? 2 + 4 : 1", 1.0); // 0 4
    c.calc("1 ? 2 + 4 : 1", 2.0 + 4.0); // 0 4
    c.calc("0 ? 1 : 2 * 4", 2.0 * 4.0); // 0 5
    c.calc("1 ? 1 : 2 * 4", 1.0); // 0 5
    c.calc("0 ? 2 * 4 : 1", 1.0); // 0 5
    c.calc("1 ? 2 * 4 : 1", 2.0 * 4.0); // 0 5
    c.calc("0 ? 1 : 2 ** 3", 8.0); // 0 6
    c.calc("1 ? 1 : 2 ** 3", 1.0); // 0 6
    c.calc("0 ? 2 ** 3 : 1", 1.0); // 0 6
    c.calc("1 ? 2 ** 3 : 1", 8.0); // 0 6
    // `:915` - the case RULINGS.md Ruling 3 cites for `|` and `XOR` sharing
    // one priority level, left-associative: `(1|3)^1`, not `1|(3^1)`.
    c.calc("1 | 3 XOR 1", ((1i32 | 3i32) ^ 1i32) as f64); // 1 1
    c.calc("1 XOR 3 | 1", ((1i32 ^ 3i32) | 1i32) as f64); // 1 1
    c.calc("3 | 1 & 2", (3i32 | (1i32 & 2i32)) as f64); // 1 2
    c.calc("2 | 4 > 3", (2i32 | (4.0 > 3.0) as i32) as f64); // 1 3
    c.calc("2 OR 4 > 3", (2i32 | (4.0 > 3.0) as i32) as f64); // 1 3
    c.calc("2 XOR 3 >= 0", (2i32 ^ (3.0 >= 0.0) as i32) as f64); // 1 3
    c.calc("2 | 1 - 3", (2i32 | (1i32 - 3i32)) as f64); // 1 4
    c.calc("2 | 4 / 2", (2i32 | (4i32 / 2i32)) as f64); // 1 5
    c.calc("1 | 2 ** 3", (1i32 | 2.0f64.powf(3.0) as i32) as f64); // 1 6
    c.calc("3 << 2 & 10", ((3i32 << 2) & 10i32) as f64); // 2 2
    c.calc("18 & 6 << 2", ((18i32 & 6i32) << 2) as f64); // 2 2
    c.calc("36 >> 2 & 10", ((36i32 >> 2) & 10i32) as f64); // 2 2
    c.calc("36 >>> 2 & 10", ((36u32 >> 2u32) & 10u32) as f64); // 2 2
    c.calc("18 & 20 >> 2", ((18i32 & 20i32) >> 2) as f64); // 2 2
    c.calc("18 & 20 >>> 2", ((18u32 & 20u32) >> 2) as f64); // 2 2
    c.calc("3 & 4 == 4", (3i32 & (4.0 == 4.0) as i32) as f64); // 2 3
    c.calc("3 AND 4 == 4", (3i32 & (4.0 == 4.0) as i32) as f64); // 2 3
    c.calc("1 << 2 != 4", (1i32 << (2.0 != 4.0) as i32) as f64); // 2 3
    c.calc("16 >> 2 != 4", (16i32 >> (2.0 != 4.0) as i32) as f64); // 2 3
    c.calc("16 >>> 2 != 4", (16u32 >> (2.0 != 4.0) as u32) as f64); // 2 3
    c.calc("3 AND -2", (3i32 & -2i32) as f64); // 2 8
    c.calc("0 < 1 ? 2 : 3", 2.0); // 3 0
    c.calc("1 <= 0 ? 2 : 3", 3.0); // 3 0
    c.calc("0 + -1", 0.0 + -1.0); // 4 8
    c.calc("0 - -1", 0.0 - -1.0); // 4 8
    c.calc("10 + 10 * 2", 10.0 + 10.0 * 2.0); // 4 5
    c.calc("20 + 20 / 2", 20.0 + 20.0 / 2.0); // 4 5
    c.calc("-1 + 1", -1.0 + 1.0); // 7 4
    c.calc("-1 - 2", -1.0 - 2.0); // 7 4
    // `:944-945` - the cases RULINGS.md Ruling 4 cites for unary minus binding
    // TIGHTER than power: `pow(-2, 2) == +4`, not `-(2**2) == -4`.
    c.calc("-2 ** 2", (-2.0f64).powf(2.0)); // 7 6
    c.calc("-2 ^ 2", (-2.0f64).powf(2.0)); // 7 6

    // Parentheses (`:947-950`)
    c.calc("(1 | 2) ** 3", ((1i32 | 2i32) as f64).powf(3.0)); // 8 6
    c.calc("1+(1|2)**3", 1.0 + ((1i32 | 2i32) as f64).powf(3.0)); // 8 6
    c.calc(
        "1+(1?(1<2):(1>2))*2",
        1.0 + (if 1.0 != 0.0 { b(1.0 < 2.0) } else { b(1.0 > 2.0) }) * 2.0,
    );
}

// ---------------------------------------------------------------------------
// Slice 5a: testArgs (epicsCalcTest.cpp:952-998)
// ---------------------------------------------------------------------------

fn slice_args(c: &mut Corpus) {
    c.args("a", A_A, 0);
    c.args("A", A_A, 0);
    c.args("B", A_B, 0);
    c.args("C", A_C, 0);
    c.args("D", A_D, 0);
    c.args("E", A_E, 0);
    c.args("F", A_F, 0);
    c.args("G", A_G, 0);
    c.args("H", A_H, 0);
    c.args("I", A_I, 0);
    c.args("J", A_J, 0);
    c.args("K", A_K, 0);
    c.args("L", A_L, 0);
    c.args("M", A_M, 0);
    c.args("N", A_N, 0);
    c.args("O", A_O, 0);
    c.args("P", A_P, 0);
    c.args("Q", A_Q, 0);
    c.args("R", A_R, 0);
    c.args("S", A_S, 0);
    c.args("T", A_T, 0);
    c.args("U", A_U, 0);
    c.args(
        "A+B+C+D+E+F+G+H+I+J+K+L+M+N+O+P+Q+R+S+T+U",
        A_A | A_B | A_C | A_D | A_E | A_F | A_G | A_H | A_I | A_J | A_K | A_L | A_M | A_N | A_O
            | A_P | A_Q | A_R | A_S | A_T | A_U,
        0,
    );
    c.args("0.1;A:=0", 0, A_A);
    c.args("1.1;B:=0", 0, A_B);
    c.args("2.1;C:=0", 0, A_C);
    c.args("3.1;D:=0", 0, A_D);
    c.args("4.1;E:=0", 0, A_E);
    c.args("5.1;F:=0", 0, A_F);
    c.args("6.1;G:=0", 0, A_G);
    c.args("7.1;H:=0", 0, A_H);
    c.args("8.1;I:=0", 0, A_I);
    c.args("9.1;J:=0", 0, A_J);
    c.args("10.1;K:=0", 0, A_K);
    c.args("11.1;L:=0", 0, A_L);
    c.args("12.1;M:=0", 0, A_M);
    c.args("13.1;N:=0", 0, A_N);
    c.args("14.1;O:=0", 0, A_O);
    c.args("15.1;P:=0", 0, A_P);
    c.args("16.1;Q:=0", 0, A_Q);
    c.args("17.1;R:=0", 0, A_R);
    c.args("18.1;S:=0", 0, A_S);
    c.args("19.1;T:=0", 0, A_T);
    c.args("20.1;U:=0", 0, A_U);
    // `:997` - chained stores: B:=A reads A, but A was already stored to, so
    // `calcPerform.c:472-473`'s `& ~stores` suppresses the input claim.
    c.args("12.1;A:=0;B:=A;C:=B;D:=C", 0, A_A | A_B | A_C | A_D);
    // `:998` - B:=A reads A *before* A:=B stores it, so A is claimed as an
    // input; C:=D likewise reads D before D:=C. This pair is the case that
    // pins the order-dependence of the two masks.
    c.args("13.1;B:=A;A:=B;C:=D;D:=C", A_A | A_D, A_A | A_B | A_C | A_D);
}

// ---------------------------------------------------------------------------
// Slice 5b: testBadExpr (epicsCalcTest.cpp:1000-1021)
// ---------------------------------------------------------------------------

/// # `CALC_ERR_* -> CalcError` mapping
///
/// Base asserts a specific `short` error code; this crate's `CalcError` has a
/// different, finer-grained partition of the same failure space. The mapping
/// used below, derived case by case from `refs/postfix.c`:
///
/// | Base code | `CalcError` | note |
/// |---|---|---|
/// | `CALC_ERR_INCOMPLETE` (`postfix.c:500`, `operand_needed \|\| runtime_depth != 1` at end; also `:394`, a zero-argument call) | `MissingOperand` **or** `ExtraOperand` | **one-to-many**: the `operand_needed` half is `MissingOperand`, the `runtime_depth != 1` half is `ExtraOperand` |
/// | `CALC_ERR_CONDITIONAL` (`:496` at end of parse; also `:419`, `:449`) | `BadConditional` | one-to-one |
/// | `CALC_ERR_PAREN_NOT_OPEN` (`:370`, `:376`, `)` with no `(`) | `Unbalanced` | one-to-one |
/// | `CALC_ERR_BAD_SEPERATOR` (`:348`, `:354`, `,` outside a call) | `Unbalanced` | **many-to-one**: joins `PAREN_NOT_OPEN` in `Unbalanced`, since this crate reports both "a `,` with no enclosing call" and "a `)` with no `(`" as one unbalanced-grouping error |
/// | `CALC_ERR_SYNTAX` (`:476`, input left over after the parse loop) | `MissingOperand`, `ExtraOperand`, or `UnknownIdent` | **one-to-many**, see below |
/// | `CALC_ERR_TOOMANY` (`:453`, depth > 1 at a `;`) | `ExtraOperand` | one-to-one; not reached by any corpus case |
///
/// Two of these are easy to get backwards, so both are spelled out:
///
/// - **`TOOMANY` is not the end-of-parse "too many operands" code.** `:453`
///   sits inside `case EXPR_TERMINATOR`, so it fires only at a `;` that leaves
///   more than one value on the stack. At end of parse Base has no `TOOMANY`
///   path at all - `:499` is `operand_needed || runtime_depth != 1`, i.e.
///   `INCOMPLETE` covers *both* directions. So this crate's `ExtraOperand` at
///   end of parse corresponds to Base's `INCOMPLETE`, not to `TOOMANY`.
/// - `INCOMPLETE` therefore fans out as well; `SYNTAX` is not the only row
///   that does.
///
/// `CALC_ERR_SYNTAX` is Base's catch-all. Base reaches it from three distinct
/// situations that this crate keeps separate, and the split is *finer*, not
/// different - every expression below is rejected by both, and the crate's
/// variant names the more specific reason:
///
/// - an operand appearing where an operator was expected (`0x0.1`) -> this
///   crate finishes the parse with two values on the stack: `ExtraOperand`;
/// - an operator appearing where an operand was expected (`*1`, `:1`,
///   `MIN()`, `MIN(A,)`) -> `MissingOperand`;
/// - a name that is not an operand or function (`V`..`Z`) -> `UnknownIdent`.
///
/// None of these is a category disagreement: in every case both
/// implementations reject, and the crate's category is a refinement of Base's
/// single `SYNTAX` bucket, not a contradiction of it.
fn slice_bad_exprs(c: &mut Corpus) {
    // `:1001`. Base: `epicsParseUInt32` consumes "0x0" and leaves ".1", which
    // is then a LITERAL_OPERAND with `operand_needed == FALSE` -> SYNTAX.
    // Here: `lex` yields `Num(0), Num(0.1)`, two operands and no operator, so
    // the parse ends with depth 2.
    c.bad("0x0.1", "CALC_ERR_SYNTAX", Ec::ExtraOperand);
    c.bad("1*", "CALC_ERR_INCOMPLETE", Ec::MissingOperand);
    c.bad("*1", "CALC_ERR_SYNTAX", Ec::MissingOperand);
    c.bad("MIN", "CALC_ERR_INCOMPLETE", Ec::MissingOperand);
    c.bad("MIN()", "CALC_ERR_SYNTAX", Ec::MissingOperand);
    c.bad("MIN(A,)", "CALC_ERR_SYNTAX", Ec::MissingOperand);
    c.bad("MIN(A,B,)", "CALC_ERR_SYNTAX", Ec::MissingOperand);
    c.bad("MAX", "CALC_ERR_INCOMPLETE", Ec::MissingOperand);
    c.bad("MAX()", "CALC_ERR_SYNTAX", Ec::MissingOperand);
    c.bad("MAX(A,)", "CALC_ERR_SYNTAX", Ec::MissingOperand);
    c.bad("MAX(A,B,)", "CALC_ERR_SYNTAX", Ec::MissingOperand);
    c.bad("1?", "CALC_ERR_CONDITIONAL", Ec::BadConditional);
    c.bad("1?1", "CALC_ERR_CONDITIONAL", Ec::BadConditional);
    c.bad(":1", "CALC_ERR_SYNTAX", Ec::MissingOperand);
    c.bad("0,", "CALC_ERR_BAD_SEPERATOR", Ec::Unbalanced);
    c.bad("0)", "CALC_ERR_PAREN_NOT_OPEN", Ec::Unbalanced);
    // `:1017-1021`. `V`..`Z` are past `U` (`CALCPERFORM_NARGS` = 21), so they
    // are not operands. Base has no entry for them at all -> SYNTAX; this
    // crate's lexer emits `Ident("V")` and the parser rejects the name.
    // (`VAL` *is* in Base's `operands[]` at `postfix.c:143`, but it is a
    // calcout-only `FETCH_VAL` and is not exercised by any corpus case.)
    c.bad("V", "CALC_ERR_SYNTAX", Ec::UnknownIdent);
    c.bad("W", "CALC_ERR_SYNTAX", Ec::UnknownIdent);
    c.bad("X", "CALC_ERR_SYNTAX", Ec::UnknownIdent);
    c.bad("Y", "CALC_ERR_SYNTAX", Ec::UnknownIdent);
    c.bad("Z", "CALC_ERR_SYNTAX", Ec::UnknownIdent);
}

// ---------------------------------------------------------------------------
// Slice 5c: testUInt32Calc (epicsCalcTest.cpp:1023-1077)
// ---------------------------------------------------------------------------

fn slice_uint32(c: &mut Corpus) {
    // Bit manipulations wrt bit 31 (Base bug lp:1514520), integer literals
    c.uint32("0xaaaaaaaa AND 0xffff0000", 0xaaaa0000);
    c.uint32("0xaaaaaaaa OR 0xffff0000", 0xffffaaaa);
    c.uint32("0xaaaaaaaa XOR 0xffff0000", 0x5555aaaa);
    c.uint32("~0xaaaaaaaa", 0x55555555);
    c.uint32("~~0xaaaaaaaa", 0xaaaaaaaa);
    c.uint32("0xaaaaaaaa >> 8", 0xffaaaaaa);
    c.uint32("0x55555555 >> 8", 0x00555555);
    c.uint32("0xaaaaaaaa >>> 8", 0x00aaaaaa);
    c.uint32("0x55555555 >>> 8", 0x00555555);
    c.uint32("0xaaaaaaaa << 8", 0xaaaaaa00);
    c.uint32("0x55555555 << 8", 0x55555500);

    // ... the same, via variables assigned by `:=`
    c.uint32("a:=0xaaaaaaaa; b:=0xffff0000; a AND b", 0xaaaa0000);
    c.uint32("a:=0xaaaaaaaa; b:=0xffff0000; a OR b", 0xffffaaaa);
    c.uint32("a:=0xaaaaaaaa; b:=0xffff0000; a XOR b", 0x5555aaaa);
    c.uint32("a:=0xaaaaaaaa; ~a", 0x55555555);
    c.uint32("a:=0xaaaaaaaa; ~~a", 0xaaaaaaaa);
    c.uint32("a:=0xaaaaaaaa; a >> 8", 0xffaaaaaa);
    c.uint32("a:=0xaaaaaaaa; a >>> 8", 0x00aaaaaa);
    c.uint32("a:=0xaaaaaaaa; a << 8", 0xaaaaaa00);
    c.uint32("a:=0x55555555; a >> 8", 0x00555555);
    c.uint32("a:=0x55555555; a >>> 8", 0x00555555);
    c.uint32("a:=0x55555555; a << 8", 0x55555500);

    // Conversion of double values used as bitwise inputs (`:1049-1077`). The
    // `+ 0.1` forces a double literal; `0xaaaaaaaa` is -1431655766 signed or
    // 2863311530 unsigned, and `d2i`/`d2ui` (`calcPerform.c:314-326`) must
    // reach the same 32 bits from either spelling.
    c.uint32("-1431655766.1 OR 0", 0xaaaaaaaa);
    c.uint32("2863311530.1 OR 0", 0xaaaaaaaa);
    c.uint32("0 OR -1431655766.1", 0xaaaaaaaa);
    c.uint32("0 OR 2863311530.1", 0xaaaaaaaa);
    c.uint32("-1431655766.1 XOR 0", 0xaaaaaaaa);
    c.uint32("2863311530.1 XOR 0", 0xaaaaaaaa);
    c.uint32("0 XOR -1431655766.1", 0xaaaaaaaa);
    c.uint32("0 XOR 2863311530.1", 0xaaaaaaaa);
    c.uint32("-1431655766.1 AND 0xffffffff", 0xaaaaaaaa);
    c.uint32("2863311530.1 AND 0xffffffff", 0xaaaaaaaa);
    c.uint32("0xffffffff AND -1431655766.1", 0xaaaaaaaa);
    c.uint32("0xffffffff AND 2863311530.1", 0xaaaaaaaa);
    c.uint32("~ -1431655766.1", 0x55555555);
    c.uint32("~ 2863311530.1", 0x55555555);
    c.uint32("-1431655766.1 >> 0", 0xaaaaaaaa);
    c.uint32("-1431655766.1 >>> 0", 0xaaaaaaaa);
    c.uint32("2863311530.1 >> 0", 0xaaaaaaaa);
    c.uint32("2863311530.1 >>> 0", 0xaaaaaaaa);
    c.uint32("-1431655766.1 >> 0.1", 0xaaaaaaaa);
    c.uint32("-1431655766.1 >>> 0.1", 0xaaaaaaaa);
    c.uint32("2863311530.1 >> 0.1", 0xaaaaaaaa);
    c.uint32("2863311530.1 >>> 0.1", 0xaaaaaaaa);
    c.uint32("-1431655766.1 << 0", 0xaaaaaaaa);
    c.uint32("2863311530.1 << 0", 0xaaaaaaaa);
    c.uint32("-1431655766.1 << 0.1", 0xaaaaaaaa);
    c.uint32("2863311530.1 << 0.1", 0xaaaaaaaa);
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
    slice_conditional_store_precedence(&mut c);
    slice_args(&mut c);
    slice_bad_exprs(&mut c);
    slice_uint32(&mut c);

    // A skip FAILS the run. `eprintln!` would not be enough: cargo swallows a
    // passing test's stderr, so a skipped case would be invisible in CI, which
    // is exactly what the task addendum forbids. Currently zero cases skip; if
    // one ever has to, the failure text is the record of it and the case must
    // be argued in the task report before the assertion is relaxed.
    assert!(
        c.skips.is_empty(),
        "{} case(s) skipped - a skip is never silent:\n{}",
        c.skips.len(),
        c.skips.join("\n")
    );
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
