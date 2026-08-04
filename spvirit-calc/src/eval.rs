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
}
