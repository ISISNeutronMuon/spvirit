use crate::op::Op;
use crate::parse::Expression;

impl Expression {
    /// Evaluate against operands `A`-`U`.
    ///
    /// The array is 21 wide (`CALCPERFORM_NARGS`, `refs/postfix.h:29`), not
    /// the brief's 12 - see RULINGS.md Ruling 1.
    ///
    /// Never fails: numeric edge cases propagate as `inf` or `NaN`, matching
    /// EPICS `calcPerform` (`refs/calcPerform.c`). An empty expression
    /// evaluates to `NaN`. This holds for every input `compile` accepts —
    /// including `?:`, which is evaluated eagerly below (see the `Op::Cond`
    /// arm) rather than deferred, precisely so this guarantee is not a lie
    /// for ternary expressions.
    ///
    /// Takes `&[f64; 21]` by shared reference rather than `&mut`. Ruling 2
    /// flags that a later `:=` (store) operator will need write-back to the
    /// operand array; when that lands this signature will need to become
    /// `&mut [f64; 21]` (or return a separate store set) and every caller of
    /// `eval` will need updating. Task 3 doesn't build `:=` yet, so this
    /// method takes the narrower, safer `&` for now rather than
    /// pre-committing to a mutation strategy Task 5/6 haven't motivated.
    pub fn eval(&self, args: &[f64; 21]) -> f64 {
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

        for op in &self.ops {
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
                // Eager "pop three, select one" per task-3-brief.md:115-120.
                // Op::Cond has arity 3 and the parser (parse.rs) always
                // emits [cond, then, els, Cond], so pop order is
                // els, then, cond (reverse of push order).
                //
                // IMPORTANT for whoever implements Task 5: this is
                // deliberately *not* what Base does. COND_IF/COND_ELSE
                // (calcPerform.c:400-410) short-circuits by jumping over the
                // untaken branch in the instruction stream - it never
                // evaluates both branches and discards one. Eager
                // evaluation here is behaviorally identical to Base's
                // short-circuit *only* because, at Task 3's capability
                // level, there is nothing side-effecting or divergent to
                // observe (no `:=`, no functions, no relationals). Once
                // Task 5/6 add side-effecting or trapping constructs (e.g.
                // `:=`, or anything that could be made to panic/diverge on
                // an operand the untaken branch was guarding against), this
                // arm must be replaced with true short-circuit evaluation
                // that skips the untaken branch's ops entirely rather than
                // evaluating and discarding it.
                Op::Cond => {
                    let els = stack.pop().expect("arity checked at compile time");
                    let then = stack.pop().expect("arity checked at compile time");
                    let cond = stack.pop().expect("arity checked at compile time");
                    stack.push(if cond != 0.0 { then } else { els });
                }

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
                // half-away-from-zero via a truncating int cast, NOT
                // `f64::round`'s general algorithm and NOT banker's
                // rounding (task instructions, trap 2). Rust's `as i32`
                // saturates on out-of-range values instead of C's
                // undefined-behavior cast, the same deliberate, safer
                // divergence Task 3 documented for `%` (see `Op::Modulo`
                // above) - not reachable by any test here.
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
            }
        }

        stack.pop().unwrap_or(f64::NAN)
    }
}

#[cfg(test)]
mod tests {
    use crate::compile;

    /// Operand array is 21 wide (A-U), per RULINGS.md Ruling 1 /
    /// `refs/postfix.h:29` (`CALCPERFORM_NARGS` = 21), not the brief's 12.
    fn ev(src: &str, args: &[f64]) -> f64 {
        let mut a = [0.0f64; 21];
        a[..args.len()].copy_from_slice(args);
        compile(src).expect("compile").eval(&a)
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

    // Op::Cond, evaluated eagerly (task-3-brief.md:115-120). Verified
    // against calcPerform.c:400-410's COND_IF/COND_ELSE semantics: nonzero
    // condition selects the `then` branch, zero selects `else` - eager
    // pop-three-select is behaviorally identical to Base's short-circuit
    // jump here since Task 3 has no side-effecting constructs to diverge
    // on (see the Op::Cond match arm's comment).
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
}
