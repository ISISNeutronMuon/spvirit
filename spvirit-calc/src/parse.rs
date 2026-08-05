use crate::lex::{Token, lex};
use crate::op::{Assoc, Op, arity, precedence};

/// Compile-time errors from lexing and parsing a CALC expression.
///
/// Reserved strictly for compile-time failures: numeric edge cases (e.g.
/// division by zero, `MODULO` by zero) never produce an `Err` — see
/// `calcPerform.c`, which returns NaN/inf for those instead of failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalcError {
    /// Character not valid anywhere in an expression, with byte offset.
    BadChar(char, usize),
    /// Malformed numeric literal at byte offset.
    BadNumber(usize),
    /// Unknown function or constant name.
    UnknownIdent(String),
    /// Operator or function applied to too few operands.
    MissingOperand,
    /// Operands left on the stack at end of expression.
    ExtraOperand,
    /// Unbalanced parentheses.
    Unbalanced,
    /// `?` without a matching `:`.
    BadConditional,
    /// Expression exceeds the operation limit.
    TooLong,
}

/// Upper bound on postfix length, guarding against pathological input.
const MAX_OPS: usize = 1000;

/// A compiled CALC expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Expression {
    pub(crate) ops: Vec<Op>,
}

/// Compile an infix CALC expression to postfix.
///
/// An empty or all-whitespace expression compiles to an empty program,
/// which the Task 3 evaluator is expected to report as `f64::NAN` —
/// matching a `calc` record with an empty `CALC` field.
pub fn compile(src: &str) -> Result<Expression, CalcError> {
    let tokens = lex(src)?;
    let mut out: Vec<Op> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    // Tracks whether the next `-`/`+` is unary. True at the start of an
    // expression and immediately after any operator or `(`.
    let mut expect_operand = true;
    // Parallel stack of in-progress comma counts for nested function calls,
    // pushed when a `(` immediately follows a `Frame::Func` and popped when
    // that call's `)` closes. Only `MIN`/`MAX` (`Op::Min`/`Op::Max`)
    // actually use the final count (task-4-brief.md, RULINGS.md trap 3),
    // but every function call tracks one uniformly so the stack stays in
    // sync regardless of which function it is. Counts commas seen, NOT
    // arguments - `)` derives the argument count from this plus whether
    // the call body was empty (see the `Token::Op(")")` arm below).
    let mut arg_counts: Vec<usize> = Vec::new();
    // Parallel to `arg_counts`: `out.len()` at the moment each open call's
    // `(` was pushed, so `)` can tell an empty call (`MIN()`, `ABS()`) -
    // where `out` gained nothing between `(` and `)` - from a real one.
    // CALC has no zero-argument calls; without this check `MIN()`/`ABS()`
    // silently compiled as pop-1-push-1 no-ops, which (see fix review)
    // exactly canceled the pre-existing missing-operand-adjacency gap for
    // constructs like `"A MIN()"`.
    let mut call_starts: Vec<usize> = Vec::new();

    for tok in &tokens {
        match tok {
            Token::Arg(i) => {
                out.push(Op::Arg(*i));
                expect_operand = false;
            }
            Token::Num(v) => {
                out.push(Op::Lit(*v));
                expect_operand = false;
            }
            Token::Ident(name) => {
                // A function name is itself a complete operand-producing
                // construct once its call closes, so it's subject to the
                // same adjacency rule as any other operand: two in a row
                // with no operator between them (`"A B"`, `"A SIN(B)"`) is
                // malformed. Checked explicitly here, rather than relying
                // solely on `check_arity`'s end-of-parse depth arithmetic,
                // so the diagnostic doesn't depend on whether the call body
                // happens to be empty (see the `call_starts` comment above).
                if !expect_operand {
                    return Err(CalcError::ExtraOperand);
                }
                let op = ident_to_op(name)?;
                stack.push(Frame::Func(op));
                expect_operand = true;
            }
            Token::Op("(") => {
                // A `(` that immediately follows a bare function name opens
                // that function's argument list; start a comma count (0 -
                // see `arg_counts` above) and record where `out` stood so
                // `)` can detect an empty body.
                if matches!(stack.last(), Some(Frame::Func(_))) {
                    arg_counts.push(0);
                    call_starts.push(out.len());
                }
                stack.push(Frame::Paren);
                expect_operand = true;
            }
            Token::Op(")") => {
                loop {
                    match stack.pop() {
                        Some(Frame::Op(op)) => out.push(op),
                        // A completed ternary (past its `:`) sitting inside
                        // these parens, e.g. `"(A?B:C)"`: finalize it (emit
                        // `CondEnd`, backpatch both jump targets — see
                        // `finalize_cond`), then keep popping down to the
                        // `(`. `pop_while` never does this itself — see its
                        // doc comment — so `)`/`,`/a further `:`/end-of-input
                        // are the only finalizers.
                        Some(Frame::CondTail { if_idx, else_idx }) => {
                            finalize_cond(&mut out, if_idx, else_idx)
                        }
                        Some(Frame::Paren) => break,
                        // A pending `?` with no matching `:` (e.g. `"(A?B)"`)
                        // is a malformed conditional, not a paren mismatch.
                        Some(Frame::CondHead(_)) => return Err(CalcError::BadConditional),
                        Some(Frame::Func(_)) => return Err(CalcError::Unbalanced),
                        None => return Err(CalcError::Unbalanced),
                    }
                }
                // If the paren we just closed belonged to a function call,
                // pop the function frame too and emit its op, fixing up
                // `Min`/`Max`'s arity from the comma count collected above.
                if matches!(stack.last(), Some(Frame::Func(_))) {
                    let Some(Frame::Func(op)) = stack.pop() else {
                        unreachable!()
                    };
                    let commas = arg_counts.pop().unwrap_or(0);
                    let start_len = call_starts.pop().unwrap_or(out.len());
                    // CALC has no zero-argument calls: `MIN()`/`ABS()` must
                    // be rejected, not silently compiled as a pop-1-push-1
                    // no-op (see the `call_starts` comment above).
                    if out.len() == start_len {
                        return Err(CalcError::MissingOperand);
                    }
                    let count = commas + 1;
                    // NOTE: fixed-arity functions (everything but
                    // `Min`/`Max`) discard `count` here - `ABS(A,B)` isn't
                    // rejected by this arm at all. It's still caught, but
                    // only incidentally, by `check_arity`'s end-of-parse
                    // stack-depth arithmetic (the same pre-existing
                    // mechanism, and the same one this fix's empty-call
                    // case had to stop relying on). If a future task wants
                    // a precise "wrong number of arguments to ABS" error,
                    // this is the place to check `count` against a
                    // fixed-arity table.
                    out.push(match op {
                        Op::Min(_) => Op::Min(count),
                        Op::Max(_) => Op::Max(count),
                        other => other,
                    });
                }
                expect_operand = false;
            }
            Token::Op("?") => {
                // `?`/`:` carry priority 0 in `postfix.c` (`:161,173`) —
                // loosest of everything else in the table — so pop every
                // pending operator down to the nearest `(` or enclosing
                // `Cond` before opening this one.
                pop_while(&mut stack, &mut out, 0, Assoc::Right);
                // Emit a `CondIf` placeholder now (see `op.rs`'s `Op::CondIf`
                // doc for the layout): its `else_target` field is unknown
                // until the matching `:` closes and the else-branch is fully
                // emitted, so it's backpatched in place later by
                // `finalize_cond`. Remember where it landed via
                // `Frame::CondHead`.
                let if_idx = out.len();
                out.push(Op::CondIf { else_target: 0 });
                stack.push(Frame::CondHead(if_idx));
                expect_operand = true;
            }
            Token::Op(":") => {
                loop {
                    match stack.pop() {
                        Some(Frame::Op(op)) => out.push(op),
                        // A completed nested ternary between this `?` and
                        // its `:`, e.g. the inner `B?C:D` in `"A?B?C:D:E"`:
                        // finalize it before continuing to search for the
                        // `CondHead` that this `:` actually closes.
                        Some(Frame::CondTail { if_idx, else_idx }) => {
                            finalize_cond(&mut out, if_idx, else_idx)
                        }
                        Some(Frame::CondHead(if_idx)) => {
                            // Emit the `CondElse` placeholder (its
                            // `end_target` is backpatched by
                            // `finalize_cond` once the else-branch and the
                            // ternary as a whole are complete) and carry
                            // both indices forward on the stack until
                            // something finalizes this pair.
                            let else_idx = out.len();
                            out.push(Op::CondElse { end_target: 0 });
                            stack.push(Frame::CondTail { if_idx, else_idx });
                            break;
                        }
                        Some(Frame::Paren) | Some(Frame::Func(_)) | None => {
                            return Err(CalcError::BadConditional);
                        }
                    }
                }
                expect_operand = true;
            }
            Token::Op(",") => {
                // A comma separates arguments within the innermost open
                // function call: pop pending operators down to that call's
                // `(` (same shape as `)`, but without closing the call) and
                // bump its argument count.
                loop {
                    match stack.last() {
                        Some(Frame::Op(_)) => {
                            let Some(Frame::Op(op)) = stack.pop() else {
                                unreachable!()
                            };
                            out.push(op);
                        }
                        Some(Frame::CondTail { .. }) => {
                            let Some(Frame::CondTail { if_idx, else_idx }) = stack.pop() else {
                                unreachable!()
                            };
                            finalize_cond(&mut out, if_idx, else_idx);
                        }
                        Some(Frame::Paren) => break,
                        Some(Frame::CondHead(_)) => return Err(CalcError::BadConditional),
                        _ => return Err(CalcError::Unbalanced),
                    }
                }
                *arg_counts.last_mut().ok_or(CalcError::Unbalanced)? += 1;
                expect_operand = true;
            }
            Token::Op(sym) => {
                let op = to_op(sym, expect_operand)?;
                let (prec, assoc) = precedence(&op);
                pop_while(&mut stack, &mut out, prec, assoc);
                stack.push(Frame::Op(op));
                expect_operand = true;
            }
        }
        if out.len() > MAX_OPS {
            return Err(CalcError::TooLong);
        }
    }

    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Op(op) => out.push(op),
            // A completed ternary at the very top level, e.g. `"A?B:C"`
            // itself: finalize it the same way any other consumer of
            // `Frame::CondTail` does.
            Frame::CondTail { if_idx, else_idx } => finalize_cond(&mut out, if_idx, else_idx),
            Frame::Paren => return Err(CalcError::Unbalanced),
            Frame::CondHead(_) => return Err(CalcError::BadConditional),
            // An unclosed function call, e.g. `"MIN(A,B"`: the `Frame::Paren`
            // above it on the stack is popped (and erred on) first, so this
            // arm is unreachable in practice, but the match must stay
            // exhaustive.
            Frame::Func(_) => return Err(CalcError::Unbalanced),
        }
    }

    check_arity(&out)?;
    Ok(Expression { ops: out })
}

enum Frame {
    Op(Op),
    Paren,
    /// Between `?` and its matching `:`: the condition and then-branch have
    /// been parsed (the then-branch is still being parsed while this sits
    /// on top), holding the index of the `Op::CondIf` placeholder pushed at
    /// `?` so `:` can backfill it once the else-branch's start is known.
    CondHead(usize),
    /// A ternary whose `?` and `:` have both been seen (the else-branch may
    /// still be in progress): `if_idx`/`else_idx` are the indices of the
    /// `Op::CondIf`/`Op::CondElse` placeholders in `out`, both still
    /// carrying dummy targets until something finalizes this frame by
    /// calling `finalize_cond` — one of `)`, `,`, an enclosing `:`, or
    /// end-of-input. NOT `pop_while`: `?:` is the loosest precedence level
    /// in the table, so no incoming operator can ever out-bind a pending
    /// `CondTail` — see `pop_while`'s doc comment for the proof.
    CondTail { if_idx: usize, else_idx: usize },
    /// A pending function call, between the identifier and its `)`.
    Func(Op),
}

/// Backpatch a completed ternary's `CondIf`/`CondElse` placeholders and
/// emit the trailing `CondEnd` marker.
///
/// `else_idx + 1` is the else-branch's first instruction (right after the
/// `CondElse` placeholder at `else_idx`, whose target this sets on the
/// `CondIf`), and `out.len()` after pushing `CondEnd` is the first
/// instruction past the whole conditional (the target `CondElse` gets).
/// Called from every place a `Frame::CondTail` can actually be consumed:
/// `)`, `,`, a further `:`, and the end-of-tokens unwind. NOT `pop_while` —
/// see its doc comment for why a pending `CondTail` can never reach the top
/// of the stack when `pop_while` is the one looking at it.
fn finalize_cond(out: &mut Vec<Op>, if_idx: usize, else_idx: usize) {
    out.push(Op::CondEnd);
    out[if_idx] = Op::CondIf {
        else_target: else_idx + 1,
    };
    out[else_idx] = Op::CondElse {
        end_target: out.len(),
    };
}

/// Pop operators that bind at least as tightly as the incoming one.
///
/// Only matches `Frame::Op` — a pending `Frame::CondTail` (a completed
/// `?...:` whose else-branch may still be in progress) is deliberately NOT
/// popped here, and provably can't be, so there's no arm for it at all
/// rather than an arm that's unreachable but looks load-bearing:
///
/// `?`/`:` carry priority 0 in `postfix.c` (`:161,173`) — the loosest level
/// in the whole table — and `pop_while` has exactly two call sites: `?`
/// itself, calling `pop_while(.., 0, Assoc::Right)`, and the generic
/// operator arm below, calling with `prec` equal to some real operator's
/// precedence (currently `1..=7`, per `op.rs`'s table; Task 6's remaining
/// operators are levels 1-2 and don't change this). A `Frame::CondTail`'s
/// effective priority is 0, so `should_pop` for it would be `0 >= prec`
/// under `Assoc::Left` (false for every `prec >= 1`) or `0 > prec` under
/// `Assoc::Right` (false for every `prec >= 0`, including `?`'s own call
/// with `prec == 0`). No call site can ever make it true — a `CondTail` is
/// only ever finalized by `)`, `,`, an enclosing `:`, or end-of-input (see
/// `finalize_cond`'s doc), never by an incoming operator, which is exactly
/// right: `?:` binding loosest means nothing can out-bind it.
///
/// A `Frame::CondHead` (mid-condition or mid-then-branch, `?` seen but not
/// yet its `:`) is likewise never matched here, for the same reason the old
/// Task-3 `Frame::Cond` wasn't — so an unrelated pending outer ternary is
/// never disturbed while parsing a nested one; see the
/// `chained_ternary_in_else_branch`/`nested_ternary_in_then_branch` tests
/// below for the shapes this protects.
fn pop_while(stack: &mut Vec<Frame>, out: &mut Vec<Op>, prec: u8, assoc: Assoc) {
    while let Some(Frame::Op(top)) = stack.last() {
        let (top_prec, _) = precedence(top);
        let should_pop = match assoc {
            Assoc::Left => top_prec >= prec,
            Assoc::Right => top_prec > prec,
        };
        if !should_pop {
            break;
        }
        let Some(Frame::Op(op)) = stack.pop() else {
            unreachable!()
        };
        out.push(op);
    }
}

/// Map an operator symbol to an `Op`, resolving unary/binary ambiguity.
fn to_op(sym: &str, unary_position: bool) -> Result<Op, CalcError> {
    Ok(match (sym, unary_position) {
        ("-", true) => Op::Neg,
        ("+", true) => return Err(CalcError::MissingOperand),
        ("+", false) => Op::Add,
        ("-", false) => Op::Sub,
        ("*", false) => Op::Mul,
        ("/", false) => Op::Div,
        ("%", false) => Op::Modulo,
        ("^" | "**", false) => Op::Pow,
        // Relational and logical (Task 5). `=`/`==` and `#`/`!=` are true
        // aliases in `refs/postfix.c` (both spellings decode to the same
        // opcode, `EQUAL`/`NOT_EQ` respectively) — not "==` is equality,
        // `=` is something else"; CALC has no assignment operator among
        // these symbols.
        ("!", true) => Op::NotL,
        (">", false) => Op::Gt,
        (">=", false) => Op::Ge,
        ("<", false) => Op::Lt,
        ("<=", false) => Op::Le,
        ("=" | "==", false) => Op::Eq,
        ("#" | "!=", false) => Op::Ne,
        ("&&", false) => Op::AndL,
        ("||", false) => Op::OrL,
        // Real operators the lexer already tokenizes (`&`, `AND`, `<<`, the
        // shifts, etc. — see `lex.rs`'s `OPS` table) but that this task
        // doesn't implement yet land here too, since there's no `Op`
        // variant for them until Task 6 adds one. `MissingOperand` is a
        // placeholder, not a considered error for those symbols; don't read
        // it as meaning "expected an operand and got one of these".
        _ => return Err(CalcError::MissingOperand),
    })
}

/// Map a CALC function name to its operation.
///
/// `SQR` is square *root*, matching EPICS - not squaring
/// (`refs/postfix.c:137-138`). `MIN`/`MAX` are seeded with a placeholder
/// count of 0; the caller fixes it up from the comma-counted argument list
/// when the matching `)` is reached.
fn ident_to_op(name: &str) -> Result<Op, CalcError> {
    Ok(match name {
        "ABS" => Op::Abs,
        "SQR" | "SQRT" => Op::Sqrt,
        "EXP" => Op::Exp,
        "LOG" => Op::Log10,
        "LOGE" | "LN" => Op::Ln,
        "CEIL" => Op::Ceil,
        "FLOOR" => Op::Floor,
        "NINT" => Op::Nint,
        "SIN" => Op::Sin,
        "COS" => Op::Cos,
        "TAN" => Op::Tan,
        "ASIN" => Op::Asin,
        "ACOS" => Op::Acos,
        "ATAN" => Op::Atan,
        "ATAN2" => Op::Atan2,
        "SINH" => Op::Sinh,
        "COSH" => Op::Cosh,
        "TANH" => Op::Tanh,
        "MIN" => Op::Min(0),
        "MAX" => Op::Max(0),
        _ => return Err(CalcError::UnknownIdent(name.to_string())),
    })
}

/// Simulate the stack depth to reject programs that under- or over-flow.
///
/// Also catches two adjacent operands with no operator between them (e.g.
/// `"A B"`, or the Task 1 known gap where `lex_number` accepts `"1.2.3"` as
/// two adjacent `Num` tokens): each pushes without popping, so depth ends
/// above 1 and this reports `ExtraOperand`.
///
/// A flat linear scan (Task 2/3/4's original shape) is no longer sufficient
/// on its own once `Op::CondIf`/`CondElse`/`CondEnd` exist: those three
/// opcodes lay out TWO alternative branches back-to-back in `ops`, only one
/// of which `eval.rs` ever actually executes for a given input. Naively
/// folding depth straight through both branches in sequence, as if they run
/// one after the other, doesn't model that — see `check_segment`, which
/// verifies each branch independently (both starting from the same depth,
/// both required to net exactly +1) and only advances the "real" depth
/// past the whole construct once, matching what a single evaluation
/// actually observes.
fn check_arity(ops: &[Op]) -> Result<(), CalcError> {
    let depth = check_segment(ops, 0, ops.len(), 0)?;
    match depth {
        0 if ops.is_empty() => Ok(()),
        1 => Ok(()),
        0 => Err(CalcError::MissingOperand),
        _ => Err(CalcError::ExtraOperand),
    }
}

/// Simulate `ops[start..end]` assuming the stack already holds `depth_in`
/// values, returning the depth left after all of them run, or an error on
/// underflow.
///
/// `CondIf`'s `else_target` and the paired `CondElse`'s `end_target` are
/// read directly out of the opcodes themselves (rather than re-deriving
/// them by scanning for markers, the way Base's `cond_search` does) and
/// used to slice out exactly the then-branch and else-branch sub-ranges,
/// each checked with its own recursive `check_segment` call starting from
/// the same `depth_in`. Nesting falls out for free: a `CondIf` found while
/// checking one of those sub-ranges is handled by the same recursive call
/// against its OWN stored targets, which can only ever point at its own
/// matching `CondElse`/`CondEnd` (they were computed from that specific
/// `?`/`:` pair's positions when `parse.rs` emitted them - see
/// `finalize_cond`) - there is no shared counter or marker search to
/// confuse an inner pair with an outer one.
///
/// The `Unbalanced` returns on malformed targets are defensive, not reachable
/// from any public API: `Expression.ops` is `pub(crate)`, so the only
/// producer of `Op::CondIf`/`CondElse`/`CondEnd` is `parse.rs` itself, and
/// `finalize_cond` always emits self-consistent targets. Rejecting instead
/// of panicking or mis-simulating keeps that true even if a future bug
/// broke the invariant, per the "no panic on the public API" constraint.
fn check_segment(ops: &[Op], start: usize, end: usize, depth_in: usize) -> Result<usize, CalcError> {
    let mut depth = depth_in;
    let mut i = start;
    while i < end {
        match &ops[i] {
            Op::CondIf { else_target } => {
                let need = arity(&ops[i]); // 1: pops the condition
                if depth < need {
                    return Err(CalcError::MissingOperand);
                }
                let depth_after_cond = depth - need;

                if *else_target == 0 || *else_target > end || *else_target - 1 <= i {
                    return Err(CalcError::Unbalanced);
                }
                let else_op_idx = *else_target - 1;
                let Op::CondElse { end_target } = ops[else_op_idx] else {
                    return Err(CalcError::Unbalanced);
                };
                if end_target == 0 || end_target > end || end_target <= else_op_idx {
                    return Err(CalcError::Unbalanced);
                }
                let end_op_idx = end_target - 1;
                if !matches!(ops.get(end_op_idx), Some(Op::CondEnd)) {
                    return Err(CalcError::Unbalanced);
                }

                let then_depth = check_segment(ops, i + 1, else_op_idx, depth_after_cond)?;
                if then_depth != depth_after_cond + 1 {
                    return Err(if then_depth < depth_after_cond + 1 {
                        CalcError::MissingOperand
                    } else {
                        CalcError::ExtraOperand
                    });
                }
                let else_depth = check_segment(ops, else_op_idx + 1, end_op_idx, depth_after_cond)?;
                if else_depth != depth_after_cond + 1 {
                    return Err(if else_depth < depth_after_cond + 1 {
                        CalcError::MissingOperand
                    } else {
                        CalcError::ExtraOperand
                    });
                }

                depth = depth_after_cond + 1;
                i = end_op_idx + 1;
                continue;
            }
            // Only reachable if a `CondIf` elsewhere pointed here without
            // this being consumed as part of its structured jump handling
            // above - see the doc comment: not reachable from `compile`.
            Op::CondElse { .. } | Op::CondEnd => return Err(CalcError::Unbalanced),
            op => {
                let need = arity(op);
                if depth < need {
                    return Err(CalcError::MissingOperand);
                }
                depth = depth - need + 1;
            }
        }
        i += 1;
    }
    Ok(depth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::Op;

    fn ops(src: &str) -> Vec<Op> {
        compile(src).expect("compile").ops
    }

    #[test]
    fn multiplication_binds_tighter_than_addition() {
        assert_eq!(
            ops("A+B*C"),
            vec![Op::Arg(0), Op::Arg(1), Op::Arg(2), Op::Mul, Op::Add]
        );
    }

    #[test]
    fn parentheses_override_precedence() {
        assert_eq!(
            ops("(A+B)*C"),
            vec![Op::Arg(0), Op::Arg(1), Op::Add, Op::Arg(2), Op::Mul]
        );
    }

    // Brief's `power_is_right_associative` asserted the opposite of Base and
    // is deleted per RULINGS.md Ruling 4: `epicsCalcTest.cpp:822` asserts
    // `2 ^ 2 ^ 3 == pow(pow(2,2),3)` == 64, i.e. `^`/`**` are
    // LEFT-associative in Base's `postfix.c` (both rows carry the same
    // in-stack/in-coming priority, which `postfix()`'s algorithm treats as
    // left-associative — see `refs/postfix.c:156,177`).
    #[test]
    fn power_is_left_associative() {
        assert_eq!(
            ops("A^B^C"),
            vec![Op::Arg(0), Op::Arg(1), Op::Pow, Op::Arg(2), Op::Pow]
        );
    }

    #[test]
    fn subtraction_is_left_associative() {
        assert_eq!(
            ops("A-B-C"),
            vec![Op::Arg(0), Op::Arg(1), Op::Sub, Op::Arg(2), Op::Sub]
        );
    }

    #[test]
    fn unary_minus_distinguished_from_binary() {
        assert_eq!(ops("-A"), vec![Op::Arg(0), Op::Neg]);
        assert_eq!(ops("A*-B"), vec![Op::Arg(0), Op::Arg(1), Op::Neg, Op::Mul]);
    }

    // RULINGS.md Ruling 4 / `refs/postfix.c:76` (unary `-` priority 7) vs.
    // `:156,177` (`**`/`^` priority 6): unary binds TIGHTER than power, so
    // `-A^B` negates `A` before raising to the power of `B` —
    // `-2^2 == pow(-2,2) == +4` per `refs/epicsCalcTest.cpp:945`, not `-4`.
    #[test]
    fn unary_minus_binds_tighter_than_power() {
        assert_eq!(
            ops("-A^B"),
            vec![Op::Arg(0), Op::Neg, Op::Arg(1), Op::Pow]
        );
    }

    // RULINGS.md-adjacent boundary the existing Pow tests all missed: every
    // prior Pow case was Pow-vs-Pow or Pow-vs-unary, so a slip that set
    // Pow's priority equal to Mul/Div's (5) instead of above it (6) would
    // have passed the whole suite. `refs/postfix.c:151,155,156,160,177`:
    // `*`/`/`/`%` are priority 5, `**`/`^` priority 6.
    #[test]
    fn power_binds_tighter_than_multiplication() {
        assert_eq!(
            ops("A*B^C"),
            vec![Op::Arg(0), Op::Arg(1), Op::Arg(2), Op::Pow, Op::Mul]
        );
    }

    // `**` is an alias for `^` (`refs/postfix.c:156,177`, both `POWER`) but
    // every other test here only exercises `^`; a typo in `to_op`'s `"**"`
    // arm would otherwise go uncaught.
    #[test]
    fn double_star_is_an_alias_for_caret() {
        assert_eq!(ops("A**B"), vec![Op::Arg(0), Op::Arg(1), Op::Pow]);
    }

    // Review fix (Important 2): RULINGS.md Ruling 3's placement of `OrL`
    // (prio 1), `AndL` (prio 2), the relationals (prio 3), and `NotL` (prio
    // 7, with unary `Neg`) was, until this fix, entirely unverified — every
    // existing ternary/precedence test only ever pinned a Task 5 operator
    // against `?:` or against another Task 5 operator at the SAME level,
    // never against a different Task 5 or Task 2 level. Swapping `OrL`
    // and `AndL`'s priorities, moving the relationals to 4 or 5, or moving
    // `NotL` down to 1 would all still pass the rest of the suite.
    //
    // Four boundaries, one test each, each hand-traced to fail if the
    // corresponding priority number is wrong (see the comment on each):

    // `||` (prio 1) vs `&&` (prio 2): if this compiled as
    // `(A||B)&&C` instead, the sequence would be
    // `[Arg0, Arg1, OrL, Arg2, AndL]`, not this. Swapping the two
    // priorities (`OrL`=2, `AndL`=1) produces exactly that wrong sequence:
    // `||`'s `pop_while(1, Left)` would then see `&&` pending at Left prio.
    #[test]
    fn or_binds_looser_than_and() {
        assert_eq!(
            ops("A||B&&C"),
            vec![Op::Arg(0), Op::Arg(1), Op::Arg(2), Op::AndL, Op::OrL]
        );
    }

    // `&&` (prio 2) vs relational (prio 3): `(A>B)&&(C>D)`, not `A>(B&&C)>D`
    // (which wouldn't even parse, since `&&`'s RHS `Gt` would then see a
    // fully-reduced boolean, but any wrong priority ordering breaks the
    // clean split into two `Gt` sub-expressions this asserts). If `AndL`
    // were priority 3 or higher (tied with or tighter than relational), the
    // second `>` would fail to pop the pending `Gt` from the first
    // comparison before combining, producing a different op order.
    #[test]
    fn and_binds_looser_than_relational() {
        assert_eq!(
            ops("A>B&&C>D"),
            vec![
                Op::Arg(0),
                Op::Arg(1),
                Op::Gt,
                Op::Arg(2),
                Op::Arg(3),
                Op::Gt,
                Op::AndL,
            ]
        );
    }

    // Relational (prio 3) vs `+` (prio 4): `(A+B)>C`, not `A+(B>C)`. If
    // relational were priority 4 or higher (tied with or tighter than
    // `+`), `>`'s `pop_while` would fail to pop the pending `Add` first,
    // producing `[Arg0, Arg1, Arg2, Gt, Add]` (`A+(B>C)`) instead.
    #[test]
    fn relational_binds_looser_than_addition() {
        assert_eq!(
            ops("A+B>C"),
            vec![Op::Arg(0), Op::Arg(1), Op::Add, Op::Arg(2), Op::Gt]
        );
    }

    // Unary `!` (prio 7, same level as `Neg`) vs relational (prio 3):
    // `(!A)>B`, not `!(A>B)`. If `NotL` were priority 3 or lower (tied with
    // or looser than relational), `>`'s `pop_while` would pop the pending
    // `NotL` too early relative to `A`, or (if looser still) never pop it
    // before combining, producing `[Arg0, Arg1, Gt, NotL]` (`!(A>B)`)
    // instead.
    #[test]
    fn not_binds_tighter_than_relational() {
        assert_eq!(
            ops("!A>B"),
            vec![Op::Arg(0), Op::NotL, Op::Arg(1), Op::Gt]
        );
    }

    // Task 5 replaced the eager, single-opcode `Op::Cond` (pop three,
    // select one) with Base's real three-opcode short-circuit shape
    // (`refs/calcPerform.c:400-411`): `CondIf{else_target}` pops the
    // condition and jumps into the else-branch if it's zero;
    // `CondElse{end_target}` unconditionally jumps past the else-branch
    // once the then-branch finishes; `CondEnd` is a no-op landing pad.
    // `else_target`/`end_target` are absolute indices into this same `ops`
    // vector, pointing one-past the `CondElse`/`CondEnd` respectively (i.e.
    // at the first instruction of the branch/continuation they gate) -
    // see `op.rs`'s `Op::CondIf` doc and `finalize_cond` for how `parse.rs`
    // computes them. Every test below was hand-traced against that layout;
    // the evaluated *results* for these same shapes are unchanged (see the
    // matching tests in `eval.rs`), only the compiled representation moved
    // from eager to short-circuit.
    #[test]
    fn conditional_compiles_to_branch() {
        assert_eq!(
            ops("A?B:C"),
            vec![
                Op::Arg(0),
                Op::CondIf { else_target: 4 },
                Op::Arg(1),
                Op::CondElse { end_target: 6 },
                Op::Arg(2),
                Op::CondEnd,
            ]
        );
    }

    // The ternary interacting with an operator of different precedence on
    // each side: `+` (prio 4) binds tighter than `?:` (prio 0) on both the
    // condition and the else-branch, so this is `(A+B) ? C : (D+E)`, not
    // `A + (B?C:D) + E` or similar. Hand-traced against the `pop_while`
    // priority-0 rule for `?` (`refs/postfix.c:161,173`).
    #[test]
    fn conditional_interacts_with_lower_precedence_operators() {
        assert_eq!(
            ops("A+B?C:D+E"),
            vec![
                Op::Arg(0),
                Op::Arg(1),
                Op::Add,
                Op::CondIf { else_target: 6 },
                Op::Arg(2),
                Op::CondElse { end_target: 10 },
                Op::Arg(3),
                Op::Arg(4),
                Op::Add,
                Op::CondEnd,
            ]
        );
    }

    // Chained (right-associative) ternary in the else-branch:
    // `A ? B : (C ? D : E)`. The inner ternary's `CondIf`/`CondElse` land
    // entirely inside the outer's else-branch (indices 4-9), and the outer
    // `CondElse` at index 3 targets index 11 - one past the INNER
    // `CondEnd`, not the outer's own body - demonstrating that finalizing
    // the inner pair first (at end-of-input, innermost `Frame::CondTail`
    // popped first) does not disturb the outer pair's already-fixed
    // `else_target`.
    #[test]
    fn chained_ternary_in_else_branch() {
        assert_eq!(
            ops("A?B:C?D:E"),
            vec![
                Op::Arg(0),
                Op::CondIf { else_target: 4 },
                Op::Arg(1),
                Op::CondElse { end_target: 11 },
                Op::Arg(2),
                Op::CondIf { else_target: 8 },
                Op::Arg(3),
                Op::CondElse { end_target: 10 },
                Op::Arg(4),
                Op::CondEnd,
                Op::CondEnd,
            ]
        );
    }

    // Nested ternary in the then-branch: `A ? (B?C:D) : E`. The inner
    // ternary (indices 2-7) sits entirely inside the outer's then-branch;
    // the inner `CondElse` at index 5 targets index 8, which is the OUTER's
    // `CondElse` - i.e. finishing the inner ternary (whichever of its two
    // branches actually ran) falls straight through to the outer's
    // "skip the else-branch" jump, without re-entering or re-checking
    // anything. This is the case Base's own `cond_search` comment warns
    // about (skipping nested conditionals correctly rather than mistaking
    // an inner marker for an outer one); the fixed absolute targets here
    // make it correct by construction rather than by careful counting.
    #[test]
    fn nested_ternary_in_then_branch() {
        assert_eq!(
            ops("A?B?C:D:E"),
            vec![
                Op::Arg(0),
                Op::CondIf { else_target: 9 },
                Op::Arg(1),
                Op::CondIf { else_target: 6 },
                Op::Arg(2),
                Op::CondElse { end_target: 8 },
                Op::Arg(3),
                Op::CondEnd,
                Op::CondElse { end_target: 11 },
                Op::Arg(4),
                Op::CondEnd,
            ]
        );
    }

    #[test]
    fn rejects_dangling_question_mark() {
        assert_eq!(compile("A?B"), Err(CalcError::BadConditional));
    }

    // A second `:` with no matching `?` to pair it with (the first `:`
    // already consumed the one `?`).
    #[test]
    fn rejects_dangling_colon() {
        assert_eq!(compile("A?B:C:D"), Err(CalcError::BadConditional));
    }

    #[test]
    fn rejects_colon_without_question_mark() {
        assert_eq!(compile("A:B"), Err(CalcError::BadConditional));
    }

    // Minor 3: a `)` closing over a pending `?` with no matching `:` is a
    // malformed conditional, not a paren mismatch.
    #[test]
    fn rejects_paren_closing_over_dangling_conditional() {
        assert_eq!(compile("(A?B)"), Err(CalcError::BadConditional));
    }

    #[test]
    fn rejects_unbalanced_parens() {
        assert_eq!(compile("(A+B"), Err(CalcError::Unbalanced));
        assert_eq!(compile("A+B)"), Err(CalcError::Unbalanced));
    }

    #[test]
    fn rejects_missing_operand() {
        assert_eq!(compile("A+"), Err(CalcError::MissingOperand));
    }

    #[test]
    fn rejects_extra_operand() {
        assert_eq!(compile("A B"), Err(CalcError::ExtraOperand));
    }

    // Task 1 known gap: `lex_number` accepts `"1.2.3"` as two adjacent `Num`
    // tokens (`1.2` then `.3`) rather than erroring at the lex stage. The
    // parser must still reject it, via the same adjacent-operand check that
    // catches `"A B"`.
    #[test]
    fn rejects_adjacent_number_literals() {
        assert_eq!(compile("1.2.3"), Err(CalcError::ExtraOperand));
    }

    #[test]
    fn empty_expression_compiles_to_nothing() {
        assert_eq!(ops(""), vec![]);
    }

    // Generated, not a literal: 600 repetitions of `"A+"` plus a trailing
    // `A` pushes well over `MAX_OPS` (1000) operands/operators before the
    // expression ends, tripping the guard mid-loop.
    #[test]
    fn rejects_expression_exceeding_max_ops() {
        let src = format!("{}A", "A+".repeat(600));
        assert_eq!(compile(&src), Err(CalcError::TooLong));
    }

    // Review fix (Important 1): CALC has no zero-argument calls. Before the
    // fix, `arg_counts` unconditionally seeded a count of 1 on `(` and
    // `Token::Ident` never checked `expect_operand`, so an empty-parens call
    // compiled as a depth-neutral pop-1-push-1 no-op - which, sitting next
    // to a real operand with no operator between them, exactly canceled the
    // pre-existing missing-operand-adjacency gap (the same gap
    // `rejects_extra_operand`/`rejects_adjacent_number_literals` above
    // exercise for `"A B"`/`"1.2.3"`). `"A MIN()"` compiled to
    // `Ok([Arg(0), Min(1)])` and evaluated as `MIN(A)`; `"A ABS()"` compiled
    // to `Ok([Arg(0), Abs])` and evaluated as `ABS(A)`. Both must be
    // rejected, standalone and in the adjacency shape that used to cancel
    // out.
    #[test]
    fn rejects_empty_argument_list() {
        assert_eq!(compile("MIN()"), Err(CalcError::MissingOperand));
        assert_eq!(compile("ABS()"), Err(CalcError::MissingOperand));
    }

    // These are caught even earlier than the empty-parens check itself: the
    // `expect_operand` guard added to `Token::Ident` sees the function name
    // in operand-adjacent position and rejects it before the parser ever
    // reaches the `(`/`)` pair, so the error is `ExtraOperand` (same
    // diagnostic as `"A B"`), not `MissingOperand`.
    #[test]
    fn rejects_operand_adjacent_to_empty_call() {
        assert_eq!(compile("A MIN()"), Err(CalcError::ExtraOperand));
        assert_eq!(compile("A ABS()"), Err(CalcError::ExtraOperand));
    }

    // The `expect_operand` check added to `Token::Ident` also catches the
    // adjacency case directly, independent of whether the call body is
    // empty (e.g. a non-empty call would otherwise only be caught
    // incidentally by `check_arity`'s end-of-parse depth arithmetic).
    #[test]
    fn rejects_operand_adjacent_to_function_call() {
        assert_eq!(compile("A SIN(B)"), Err(CalcError::ExtraOperand));
    }
}
