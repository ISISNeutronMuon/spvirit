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
    /// including `?:`, which genuinely short-circuits as of Task 5 (see the
    /// `Op::CondIf`/`Op::CondElse`/`Op::CondEnd` arms below): the untaken
    /// branch's instructions are never reached, matching
    /// `calcPerform.c:400-411`, not evaluated-then-discarded the way Task
    /// 3's placeholder `Op::Cond` used to.
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
            }
            ip += 1;
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
        assert_eq!(taken_then.eval(&[1.0; 21]), 1.0);

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
        assert_eq!(taken_else.eval(&[0.0; 21]), 2.0);
    }
}
