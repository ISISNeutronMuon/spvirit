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
    /// `:=` whose left-hand side is not a bare operand `A`-`U`, or which
    /// appears where the operator stack is not empty. Mirrors Base's
    /// `CALC_ERR_BAD_ASSIGNMENT` ("Bad assignment target",
    /// `refs/postfix.c:299,522`) and its guard at `refs/postfix.c:296-301`.
    BadAssignment,
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

/// The two bitmasks `calcArgUsage` reports: bit `n` of `inputs`/`stores` is
/// set if operand `n` (`A` = 0, `U` = 20) is fetched/stored by the
/// expression. Two named fields rather than a `(u32, u32)` tuple, so a call
/// site can't silently swap which mask is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgUsage {
    /// Bitmask of operands read from the argument array.
    pub inputs: u32,
    /// Bitmask of operands written by a `:=` store.
    pub stores: u32,
}

impl Expression {
    /// Which operand slots this expression reads from and writes to,
    /// matching Base's `calcArgUsage(ppostfix, pinputs, pstores)`
    /// (`refs/calcPerform.c:429-507`, `postfix.h:366`). The `calcout` record
    /// uses `inputs` to decide which `INPA`-`INPU` links to fetch before
    /// evaluating - skipping a link Base wouldn't process avoids `PP`
    /// links processing records the real IOC never would.
    ///
    /// This is a flat, linear walk over `self.ops`, mirroring Base's `while
    /// ((op = *pinst++) != END_EXPRESSION)` loop (`:435`) exactly: it does
    /// NOT interpret `Op::CondIf`/`CondElse`/`CondEnd`'s jump targets, so
    /// both arms of a ternary are reported even though only one would run at
    /// evaluation time (`refs/calcPerform.c` has no `COND_IF`/`COND_ELSE`
    /// special-casing in `calcArgUsage` either - the untaken branch's opcodes
    /// are just more entries in the stream it scans). `;` contributes no
    /// opcode at all (Task 8a, `refs/postfix.c:163`, `EXPR_TERMINATOR` is
    /// `NOT_GENERATED`), so segment boundaries are as invisible to this walk
    /// as they are to Base's.
    ///
    /// The two masks are NOT independent of instruction order:
    /// `refs/calcPerform.c:472-473`, `inputs |= (1 << (op - FETCH_A)) &
    /// ~stores;` ("Don't claim to use an arg we already stored to") means a
    /// fetch is only counted as an input if no store to that same slot has
    /// been walked yet. `"A := 1; A"` and `"A; A := 1"` therefore report
    /// different `inputs` masks despite containing the same two opcodes in
    /// the other order - see this module's
    /// `store_then_fetch_of_same_slot_is_not_reported_as_input` and
    /// `fetch_then_store_of_same_slot_is_reported_as_input`.
    ///
    /// Cannot fail: `self.ops` was already validated by `compile`.
    pub fn arg_usage(&self) -> ArgUsage {
        let mut inputs: u32 = 0;
        let mut stores: u32 = 0;
        for op in &self.ops {
            match op {
                // `refs/calcPerform.c:472-473`: mask off any bit already
                // claimed by `stores` before OR-ing the fetch in, so a
                // store followed by a same-slot fetch never sets `inputs`.
                Op::Arg(i) => inputs |= (1u32 << i) & !stores,
                // `refs/calcPerform.c:497`: `stores |= (1 << (op -
                // STORE_A))`. Order relative to the line above matters -
                // this must run in the same left-to-right op order as the
                // `Op::Arg` arm above it, which it does by construction
                // (single forward `for` loop, one op examined per
                // iteration).
                Op::Store(i) => stores |= 1u32 << i,
                _ => {}
            }
        }
        ArgUsage { inputs, stores }
    }
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
                // Task 7: named constants (`PI`/`D2R`/`R2D`/`INF`/`NAN`,
                // `refs/postfix.c:101,111,125,129,132`) compile straight to
                // an `Op::Lit`, and `RNDM` (`:133`) compiles straight to the
                // nullary `Op::Rndm` - neither is a function awaiting a
                // `(...)` call, so both are checked before `word_operator`/
                // `ident_to_op` below, same adjacency contract as any other
                // operand-producing token.
                if let Some(v) = constant(name) {
                    if !expect_operand {
                        return Err(CalcError::ExtraOperand);
                    }
                    out.push(Op::Lit(v));
                    expect_operand = false;
                } else if let Some(op) = nullary_op(name) {
                    if !expect_operand {
                        return Err(CalcError::ExtraOperand);
                    }
                    out.push(op);
                    expect_operand = false;
                } else if let Some(op) = word_operator(name) {
                    // Task 6: `AND`/`OR`/`XOR`/`NOT` are word-spelled
                    // operators (`refs/postfix.c:126,174,175,176`), not
                    // functions - they must slot into the normal
                    // operator-stack machinery (same as `Token::Op(sym)`
                    // below), not be pushed as a `Frame::Func` awaiting a
                    // `(...)` call that will never come.
                    //
                    // `NotB` (`NOT`/`~`) is unary/prefix; `AndB`/`OrB`/
                    // `XorB` (`AND`/`OR`/`XOR`) are binary/infix - same
                    // adjacency contract as `to_op`'s unary_position
                    // parameter for `-`/`!`/`~`.
                    let unary = matches!(op, Op::NotB);
                    if unary != expect_operand {
                        return Err(CalcError::MissingOperand);
                    }
                    let (prec, assoc) = precedence(&op);
                    pop_while(&mut stack, &mut out, prec, assoc);
                    stack.push(Frame::Op(op));
                    expect_operand = true;
                } else {
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
                        // `ISNAN`/`FINITE` are variadic too (`refs/postfix.c:
                        // 113,105`) - same comma-counted fix-up as `Min`/
                        // `Max`. `ISINF` is deliberately absent here: it's
                        // `Op::IsInf` with no count field (RULINGS.md
                        // Ruling 6), so it falls through to `other => other`
                        // below like any other fixed-arity function.
                        Op::IsNan(_) => Op::IsNan(count),
                        Op::Finite(_) => Op::Finite(count),
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
            // Task 8a, `refs/postfix.c:296-307` (`case STORE_OPERATOR`). `:=`
            // is not an ordinary infix operator: it reaches BACKWARDS into
            // the already-emitted output, un-emits the `FETCH_*` it finds
            // there, and pushes the corresponding `STORE_*` onto the operator
            // stack in its place -
            //   `*--pout < FETCH_A || *pout >= FETCH_A + CALCPERFORM_NARGS`
            //   ... `pstacktop->code = STORE_A + *pout - FETCH_A;`
            // - so the left-hand side never contributes a value to the
            // runtime stack, and the store is emitted only when something
            // later flushes the operator stack (a `;`, or end-of-input).
            Token::Op(":=") => {
                // Base reaches this only in operator position: `get_element`
                // looks operand-position tokens up in `operands[]`, where
                // `:=` does not appear, so `"A; := 1"` is `CALC_ERR_SYNTAX`
                // there (`refs/postfix.c:475-478`). This crate's lexer is
                // context-free and has one shared symbol table, so the same
                // rejection is made explicitly here - a divergence in the
                // error variant only (`MissingOperand` vs Base's syntax
                // error), never in the accept/reject verdict.
                if expect_operand {
                    return Err(CalcError::MissingOperand);
                }
                // `pstacktop > stack` (`refs/postfix.c:297`): Base requires
                // the operator stack to be EMPTY. `Frame::CondHead` is the
                // one frame with no Base counterpart on that stack - Base's
                // `?` pushes nothing, tracking an integer `cond_count`
                // instead (`refs/postfix.c:427`) - so a pending `?` must NOT
                // count against this test, or `"A ? B := 1 : 2"` (which Base
                // accepts) would be wrongly rejected. Everything else does
                // count, including `Frame::CondTail`: that IS on Base's
                // stack, as the `COND_END` pushed by `:` (`:422-425`), which
                // is precisely why a store in an ELSE branch is a Base error.
                if !stack.iter().all(|f| matches!(f, Frame::CondHead(_))) {
                    return Err(CalcError::BadAssignment);
                }
                // `*--pout` must be a `FETCH_A..FETCH_A+21`. Only `Op::Arg`
                // qualifies: `Op::Lit` covers Base's `LITERAL_*` and the
                // named constants `PI`/`D2R`/`R2D`/`INF`/`NAN` (all distinct
                // opcodes in Base, none in the fetch range), and every
                // function/operator result is likewise a different opcode.
                let Some(Op::Arg(slot)) = out.last().copied() else {
                    return Err(CalcError::BadAssignment);
                };
                out.pop();
                stack.push(Frame::Op(Op::Store(slot)));
                // `refs/postfix.c:306`: `operand_needed = TRUE`.
                expect_operand = true;
            }
            // Task 8a, `refs/postfix.c:433-458` (`case EXPR_TERMINATOR`).
            // `;` generates NO opcode (`:163`, `NOT_GENERATED`) - hence the
            // absence of any `case EXPR_TERM` in `refs/calcPerform.c`'s
            // evaluator switch, and the absence of an `Op` variant here. All
            // it does is flush the operator stack, so a following expression
            // starts clean and any pending store lands before it.
            //
            // Base additionally checks `runtime_depth > 1` here
            // (`:452-455`, `CALC_ERR_TOOMANY`). This crate does not track
            // depth during parsing and relies on `check_arity`'s end-of-parse
            // check instead. That is not a gap: every `;`-separated segment
            // has a NET stack effect of >= 0 (a store segment such as
            // `A := 0` nets exactly 0 - fetch +1, un-emit -1, value +1,
            // store -1), so a depth that reaches 2 can never come back down
            // to 1, and the final `depth != 1` check rejects the identical
            // set of expressions. Only the error variant differs
            // (`ExtraOperand` vs Base's `CALC_ERR_TOOMANY`).
            Token::Op(";") => {
                // As for `:=` above: `;` is absent from Base's `operands[]`,
                // so it is a syntax error in operand position - which covers
                // a leading `;`, a doubled `;;`, and (via `:457`'s
                // `operand_needed = TRUE` plus `:499`'s end check) a trailing
                // one.
                if expect_operand {
                    return Err(CalcError::MissingOperand);
                }
                while let Some(frame) = stack.pop() {
                    match frame {
                        Frame::Op(op) => out.push(op),
                        Frame::CondTail { if_idx, else_idx } => {
                            finalize_cond(&mut out, if_idx, else_idx)
                        }
                        // `refs/postfix.c:436-439`: CALC_ERR_PAREN_OPEN.
                        Frame::Paren | Frame::Func(_) => return Err(CalcError::Unbalanced),
                        // `refs/postfix.c:448-451`: cond_count != 0 is
                        // CALC_ERR_CONDITIONAL.
                        Frame::CondHead(_) => return Err(CalcError::BadConditional),
                    }
                }
                expect_operand = true;
            }
            Token::Op(":") => {
                // A pending `Op::Store` is deliberately carried across this
                // `:` rather than emitted into the then-branch.
                // `refs/postfix.c:402-403`, the `CONDITIONAL` flush, uses a
                // STRICT comparison - `while (pstacktop->in_stack_pri >
                // pel->in_coming_pri)` - and `:=` and `:` both carry
                // priority 0 (`:162`, `:161`), so `0 > 0` is false and the
                // store survives on Base's stack UNDERNEATH the `COND_END`
                // that this `:` is about to push (`:422-425`).
                //
                // The consequence is genuinely surprising and is Base's real
                // behaviour, not an artifact of this transcription: the store
                // hoists clean out of the conditional, so `A ? B := 1 : 2`
                // means `B := (A ? 1 : 2)` and writes B on BOTH paths. The
                // naive alternative (emitting the store here, inside the
                // then-branch) makes the branch net 0 instead of +1, which
                // `check_segment` rejects - so the two readings differ on
                // whether the expression compiles at all, and Base's accepts.
                // See `store_in_a_then_branch_hoists_out_of_the_conditional`.
                let mut held: Vec<Op> = Vec::new();
                loop {
                    match stack.pop() {
                        Some(Frame::Op(op @ Op::Store(_))) => held.push(op),
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
                            // Restore any store held aside above FIRST, so it
                            // ends up BELOW the `CondTail` and is therefore
                            // emitted after this ternary's `CondEnd` - the
                            // position Base leaves it in (`refs/postfix.c`:
                            // the `COND_END` at `:422-425` is pushed on top
                            // of the un-flushed store). `held` was filled by
                            // `pop`, so replaying it in reverse restores the
                            // original bottom-to-top order; in practice it
                            // holds at most one entry, since `:=` refuses to
                            // parse while any `Frame::Op` is on the stack.
                            for op in std::mem::take(&mut held).into_iter().rev() {
                                stack.push(Frame::Op(op));
                            }
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

    // `refs/postfix.c:499`: `if (operand_needed || runtime_depth != 1)` ->
    // `CALC_ERR_INCOMPLETE`. `check_arity` below covers the depth half, but
    // not the `operand_needed` half: a trailing `;` or `:=` leaves the output
    // in a state whose depth is coincidentally fine (`"A;"` is just `[Arg(0)]`)
    // while the expression is plainly unfinished. Guarded on a non-empty
    // token list so `compile("")` still reaches `empty_expression_compiles_
    // to_nothing`'s `Ok(empty)` rather than this arm - `expect_operand`
    // starts `true`, so an empty token list would otherwise trip it. This is
    // a divergence, not a Base-justified path: Base rejects `""` outright at
    // `refs/postfix.c:235-240` (`*psrc == '\0'` -> `CALC_ERR_NULL_ARG`, before
    // the `get_element` loop this check mirrors even starts at `:246`), so
    // `compile("") -> Ok(empty)` is the pre-existing Task 1/2 behaviour, kept
    // here as-is rather than matched to Base.
    if expect_operand && !tokens.is_empty() {
        return Err(CalcError::MissingOperand);
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
        // Bitwise (Task 6). `~` is unary (prefix); `&`/`|`/`<<`/`>>`/`>>>`
        // are binary. The word forms (`AND`/`OR`/`XOR`/`NOT`) are lexed as
        // identifiers, not symbols, so they're resolved by `word_operator`
        // in the `Token::Ident` arm below, not here.
        ("~", true) => Op::NotB,
        ("&", false) => Op::AndB,
        ("|", false) => Op::OrB,
        ("<<", false) => Op::Shl,
        (">>", false) => Op::Shr,
        (">>>", false) => Op::ShrLogic,
        _ => return Err(CalcError::MissingOperand),
    })
}

/// Named constants and literals that compile straight to `Op::Lit`, matching
/// the exact spelling set in `refs/postfix.c`'s `operands[]` table:
/// `"D2R"` (`:101`), `"INF"` (`:111`), `"NAN"` (`:125`), `"PI"` (`:129`),
/// `"R2D"` (`:132`) - all either `OPERAND` (the trig constants) or
/// `LITERAL_OPERAND`/`LITERAL_DOUBLE` (`INF`/`NAN`), and all zero stack
/// effect (push one, pop none).
///
/// `refs/calcPerform.c:126-136`: `CONST_PI` pushes `PI`, `CONST_D2R` pushes
/// `PI/180.`, `CONST_R2D` pushes `180./PI` - computed here as the same
/// divisions of `std::f64::consts::PI`, not transcribed decimal literals, so
/// the last mantissa bits match exactly.
///
/// `"Inf"` is matched case-insensitively for free (this crate's lexer
/// already uppercases every identifier before it reaches here).
///
/// `"INFINITY"` needed a second look rather than a first-glance no. My first
/// pass over `operands[]` (no distinct `"INFINITY"` row) concluded Base
/// rejects it - but `refs/epicsCalcTest.cpp:336` asserts
/// `testCalc("Infinity", Inf)`, i.e. Base's own test suite compiles it
/// successfully. Reconciling that meant reading `get_element` itself
/// (`refs/postfix.c:190-215`): it matches only a `strlen(pel->name)`-byte
/// *prefix* of the source, case-insensitively, so `"INF"` (3 bytes)
/// prefix-matches the first 3 bytes of `"Infinity"`. For `LITERAL_OPERAND`
/// rows specifically (`refs/postfix.c:255-267`), the matched prefix is only
/// used to *recognize the token as a double literal*; the code then rewinds
/// (`psrc -= strlen(pel->name)`) and hands the *whole* remaining source to
/// `epicsParseDouble` (a `strtod`-family parser), which independently
/// understands the `"infinity"` spelling and consumes all 8 bytes. So Base
/// accepts `"Infinity"` not via a second table row but because the C
/// standard library's double parser does the real consuming. This crate's
/// lexer greedily consumes the whole alphabetic run into one `Ident` token
/// before any of this table logic runs, so the prefix-sniff trick doesn't
/// carry over structurally - `"INFINITY"` is listed here explicitly instead,
/// as the pragmatic equivalent that reproduces the one behavior Base's own
/// tests actually pin.
fn constant(name: &str) -> Option<f64> {
    Some(match name {
        "PI" => std::f64::consts::PI,
        "D2R" => std::f64::consts::PI / 180.0,
        "R2D" => 180.0 / std::f64::consts::PI,
        "INF" | "INFINITY" => f64::INFINITY,
        "NAN" => f64::NAN,
        _ => return None,
    })
}

/// `RNDM` (`refs/postfix.c:133`, opcode `RANDOM`) is the sole nullary
/// operator that isn't a compile-time-known literal - it must still be
/// recognized and emitted directly here (not pushed as a `Frame::Func`
/// awaiting a `(...)` call, since `RNDM` takes no parentheses at all).
///
/// Divergence from the task instructions, verified against `operands[]`
/// rather than assumed: the instructions claim Base also accepts `"RANDOM"`
/// as a spelling for `RNDM`. `refs/postfix.c:72-145`'s `operands[]` table has
/// exactly one row for this opcode - `{"RNDM", 0, 0, 1, OPERAND, RANDOM}`
/// (`:133`) - and no `"RANDOM"` row; `"RANDOM"` is only the opcode's internal
/// C enum/debug name (see the `pinst_names`-style string table around
/// `:599`, used solely for `POSTFIX_DEBUG` tracing), never an accepted input
/// spelling. So only `"RNDM"` is implemented; `"RANDOM"` correctly falls
/// through to `ident_to_op` and is rejected as `CalcError::UnknownIdent`.
fn nullary_op(name: &str) -> Option<Op> {
    Some(match name {
        "RNDM" => Op::Rndm,
        _ => return None,
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
        // Task 7. `ISINF` (`refs/postfix.c:112`) is fixed-arity (unary) -
        // no placeholder count, unlike `ISNAN`/`FINITE` (`:113,105`), which
        // follow `MIN`/`MAX`'s variadic placeholder-then-fix-up pattern
        // (RULINGS.md Ruling 6).
        "ISINF" => Op::IsInf,
        "ISNAN" => Op::IsNan(0),
        "FINITE" => Op::Finite(0),
        _ => return Err(CalcError::UnknownIdent(name.to_string())),
    })
}

/// Map a word-spelled operator name to its `Op`, distinct from
/// `ident_to_op`'s function names: `AND`/`OR`/`XOR`/`NOT`
/// (`refs/postfix.c:174,175,176,126`) are infix/prefix operators, not
/// functions awaiting a `(...)` call, so they're dispatched through the
/// normal operator-stack/precedence machinery in the `Token::Ident` arm
/// above rather than pushed as a `Frame::Func`. Returns `None` for anything
/// else (including unknown identifiers - `ident_to_op` is what reports
/// `UnknownIdent` for those).
fn word_operator(name: &str) -> Option<Op> {
    Some(match name {
        "AND" => Op::AndB,
        "OR" => Op::OrB,
        "XOR" => Op::XorB,
        "NOT" => Op::NotB,
        _ => return None,
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
            // Task 8a. The generic arm below assumes every op pushes exactly
            // one result (`depth - need + 1`), which is true of all 40-odd
            // other opcodes but NOT of a store: `refs/calcPerform.c:102-124`
            // is `parg[op - STORE_A] = *ptop--;` - it pops its value and
            // pushes nothing, the only opcode in Base with a negative net
            // stack effect. Routing it through the generic arm would compute
            // depth 1 where the true depth is 0, which is exactly the class
            // of error this checker exists to prevent: `compile` would accept
            // `"A := 0"` and `eval` would then return whatever `stack.pop()`
            // found - i.e. `.expect("arity checked at compile time")` on an
            // empty stack, a panic on the public API inside an IOC process.
            //
            // `Op::Store` cannot appear inside a ternary arm - `:=` refuses
            // to parse with a `Frame::CondTail` on the stack, and a store
            // pending across a `:` is hoisted out below the `CondTail` - so
            // the arm-by-arm `!= depth_in + 1` checks above are unaffected by
            // this arm. `store_in_a_then_branch_hoists_out_of_the_conditional`
            // and `store_in_an_else_branch_is_rejected` pin both halves.
            Op::Store(_) => {
                if depth < 1 {
                    return Err(CalcError::MissingOperand);
                }
                depth -= 1;
            }
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

    // --- Task 6: bitwise operators, precedence ---
    //
    // RULINGS.md Ruling 3 / `refs/postfix.c` lines 152-178: `|`/`OR`/`XOR`
    // share priority 1 with `||` (already `Op::OrL`); `&`/`AND`/`<<`/`>>`/
    // `>>>` share priority 2 with `&&` (already `Op::AndL`); unary `~`/`NOT`
    // share priority 7 with `Neg`/`NotL`. `op.rs`'s precedence table already
    // documents these numbers (filled in below); each test here is
    // hand-traced against `pop_while`'s algorithm to show it would emit a
    // DIFFERENT op sequence if the corresponding priority were wrong - not
    // just "some assertion holds under the right value".

    // `epicsCalcTest.cpp:915`: `1 | 3 XOR 1 == (1|3)^1` - `|` and `XOR` are
    // equal priority (1) and left-associative, so `XOR` pops the pending
    // `OrB` before pushing itself. If `XorB` were given a TIGHTER priority
    // (e.g. 2, mistakenly grouped with `&`), `pop_while` would see
    // `top_prec(OrB=1) >= 2` as false, leave `OrB` on the stack, and the
    // final unwind would emit `XorB` before `OrB` instead - producing
    // `[Arg0, Arg1, Arg2, XorB, OrB]` (`A|(B XOR C)`) rather than the
    // expected `(A|B) XOR C`.
    #[test]
    fn bitwise_or_and_xor_are_equal_precedence_left_associative() {
        assert_eq!(
            ops("A|B XOR C"),
            vec![Op::Arg(0), Op::Arg(1), Op::OrB, Op::Arg(2), Op::XorB]
        );
    }

    // `|`/`OR`/`XOR` (priority 1) share a level with `||` (already `Op::OrL`,
    // priority 1): `A||B|C` must pop the pending `OrL` before pushing `OrB`,
    // same left-associative tie as above.
    //
    // Fix round 1 (Important 2): a looser `prec` pops MORE under
    // `Assoc::Left` (`top_prec >= prec`), not less, so a mistakenly LOOSER
    // `OrB` (e.g. 0) would still satisfy `top_prec(OrL=1) >= 0` and produce
    // this same expected sequence - this test does not catch that mutation.
    // What it does catch: a mistakenly TIGHTER `OrB` (e.g. 2, wrongly
    // grouped with `&`'s level). Then at `|`, `pop_while(2, Left)` sees
    // `top_prec(OrL=1) >= 2` as false, leaves `OrL` pending, and the final
    // unwind emits `OrB` before `OrL`: `[Arg0, Arg1, Arg2, OrB, OrL]`
    // (`A||(B|C)`) instead of the asserted `(A||B)|C`.
    #[test]
    fn bitwise_or_shares_precedence_with_logical_or() {
        assert_eq!(
            ops("A||B|C"),
            vec![Op::Arg(0), Op::Arg(1), Op::OrL, Op::Arg(2), Op::OrB]
        );
    }

    // `&`/`AND`/shifts (priority 2) share a level with `&&` (already
    // `Op::AndL`, priority 2): mirrors the `OrL`/`OrB` tie above one level
    // up, with the same fix-round-1 correction: a mistakenly LOOSER `AndB`
    // (e.g. 1) would still satisfy `top_prec(AndL=2) >= 1` and produce this
    // same expected sequence, so this test does not catch that mutation.
    // What it catches: a mistakenly TIGHTER `AndB` (e.g. 3, wrongly grouped
    // with relational). Then at `&`, `pop_while(3, Left)` sees
    // `top_prec(AndL=2) >= 3` as false, leaves `AndL` pending, and the
    // unwind emits `AndB` before `AndL`: `[Arg0, Arg1, Arg2, AndB, AndL]`
    // (`A&&(B&C)`) instead of the asserted `(A&&B)&C`.
    #[test]
    fn bitwise_and_shares_precedence_with_logical_and() {
        assert_eq!(
            ops("A&&B&C"),
            vec![Op::Arg(0), Op::Arg(1), Op::AndL, Op::Arg(2), Op::AndB]
        );
    }

    // `epicsCalcTest.cpp:925`: `"18 & 6 << 2"` == `(18 & 6) << 2` - `&` and
    // `<<` are equal priority (2), left-associative, so `<<` pops the
    // pending `AndB` first. If `Shl` were given a TIGHTER priority than
    // `AndB` (e.g. 3), `pop_while` would leave `AndB` pending and the
    // unwind would emit `Shl` before `AndB`, producing `[.., Shl, AndB]`
    // (`A&(B<<C)`) instead of the expected `(A&B)<<C`.
    #[test]
    fn bitwise_and_and_left_shift_are_equal_precedence() {
        assert_eq!(
            ops("A&B<<C"),
            vec![Op::Arg(0), Op::Arg(1), Op::AndB, Op::Arg(2), Op::Shl]
        );
    }

    // `epicsCalcTest.cpp:924`: `"3 << 2 & 10"` == `(3<<2)&10` - the reverse
    // order from the previous test, checking the tie the other direction. If
    // `Shl` were LOOSER than `AndB` (e.g. 1), `pop_while` would leave `Shl`
    // pending when `&` arrives and the unwind would emit `AndB` before
    // `Shl`, producing `[.., AndB, Shl]` (`A<<(B&C)`) instead.
    #[test]
    fn left_shift_and_bitwise_and_are_equal_precedence() {
        assert_eq!(
            ops("A<<B&C"),
            vec![Op::Arg(0), Op::Arg(1), Op::Shl, Op::Arg(2), Op::AndB]
        );
    }

    // `>>>` shares `<<`/`>>`/`&`/`&&`/`AND`'s priority 2 too -
    // `epicsCalcTest.cpp:927-929` exercises the same tie with `>>>`. Same
    // failure mode as the two tests above if `ShrLogic` were given a
    // different priority than `AndB`/`Shl`.
    #[test]
    fn logical_right_shift_shares_precedence_with_bitwise_and() {
        assert_eq!(
            ops("A&B>>>C"),
            vec![Op::Arg(0), Op::Arg(1), Op::AndB, Op::Arg(2), Op::ShrLogic]
        );
    }

    // `epicsCalcTest.cpp:932-934`: `"1 << 2 != 4"` == `1 << (2 != 4)` -
    // relational (priority 3) binds TIGHTER than the shifts (priority 2),
    // the opposite tie-breaking direction from the same-level tests above:
    // here `!=` must NOT pop the pending `Shl`, since `Shl`'s priority (2)
    // is looser than the incoming `Ne`'s (3). If `Shl` were priority 3 or
    // higher (tied with or tighter than relational), `pop_while` would pop
    // it before pushing `Ne`, producing `[Arg0, Arg1, Shl, Arg2, Ne]`
    // (`(1<<2) != 4`) instead of the expected `1 << (2!=4)`.
    #[test]
    fn relational_binds_tighter_than_left_shift() {
        assert_eq!(
            ops("A<<B!=C"),
            vec![Op::Arg(0), Op::Arg(1), Op::Arg(2), Op::Ne, Op::Shl]
        );
    }

    // Unary `~` (priority 7, same level as `Neg`/`NotL`) vs relational
    // (priority 3). `>` is left-associative, so its `pop_while(3, Left)`
    // pops whenever `top_prec >= 3` -- that includes the tie case, so a
    // `NotB` priority of exactly 3 would still (correctly, if coincidentally)
    // pop and pass this test. The boundary this test actually catches is
    // `NotB` priority *strictly below* 3, which would leave `NotB` stuck on
    // the stack when `>` fires, producing `[Arg0, Arg1, Gt, NotB]`
    // (`~(A>B)`) instead of the expected `(~A)>B`. Mirrors
    // `not_binds_tighter_than_relational` above for `NotL`.
    #[test]
    fn bitwise_not_binds_tighter_than_relational() {
        assert_eq!(
            ops("~A>B"),
            vec![Op::Arg(0), Op::NotB, Op::Arg(1), Op::Gt]
        );
    }

    // `~` vs `^` (Pow, priority 6, left-associative -- the tightest binary
    // level). Pins `ops("~A^B") == (~A)^B` per Base (`~` is `postfix.c`
    // prio 7/8, `^`/`**` is 6/6, so `~` binds first). Re-derived carefully
    // rather than assumed: this test does NOT distinguish a `NotB` priority
    // of exactly 6 from the correct 7. `^`'s `pop_while(6, Left)` pops
    // whenever `top_prec >= 6`, which holds for both 6 and 7 alike, so both
    // values produce the identical sequence asserted below. What this test
    // *does* catch is any `NotB` priority below 6 (e.g. accidentally tied
    // with `Mul`/`Add`/relational): `pop_while` would then leave `NotB` on
    // the stack until `^` is pushed, unwinding to `[Arg0, Arg1, Pow, NotB]`
    // (`~(A^B)`) instead of the expected `(~A)^B`. Since 6 is the tightest
    // binary level, no expression built from `pop_while` alone can ever
    // separate 6 from 7 for a right-associative unary prefix op -- that gap
    // is inherent to the single-number precedence model, not a test gap.
    #[test]
    fn bitwise_not_binds_tighter_than_power() {
        assert_eq!(
            ops("~A^B"),
            vec![Op::Arg(0), Op::NotB, Op::Arg(1), Op::Pow]
        );
    }

    // The word-spelled `NOT`/`AND`/`OR`/`XOR` operators must resolve through
    // `word_operator`, not be swallowed by `ident_to_op`'s function-name path
    // (which would push a `Frame::Func` and choke on the missing `(`).
    #[test]
    fn word_spelled_bitwise_operators_compile_like_their_symbols() {
        assert_eq!(ops("A AND B"), ops("A&B"));
        assert_eq!(ops("A OR B"), ops("A|B"));
        assert_eq!(
            ops("A XOR B"),
            vec![Op::Arg(0), Op::Arg(1), Op::XorB]
        );
        assert_eq!(ops("NOT A"), ops("~A"));
    }

    // --- Task 8a: store operator `:=` and expression terminator `;` ---
    //
    // `refs/postfix.c:162`: `{":=", 0, 0, -1, STORE_OPERATOR, STORE_A}`
    // `refs/postfix.c:163`: `{";",  0, 0,  0, EXPR_TERMINATOR, NOT_GENERATED}`
    //
    // `;` is `NOT_GENERATED`: it emits NO opcode (which is why
    // `refs/calcPerform.c` has no `case EXPR_TERM` in its evaluator switch).
    // Its whole job at parse time (`refs/postfix.c:433-458`) is to flush the
    // operator stack. So there is deliberately no `Op` variant for it.

    // The corpus's first pinning shape (`refs/epicsCalcTest.cpp:846`):
    // `"a := 0; a"` == 0. The `:=` retroactively converts the just-emitted
    // FETCH into a STORE and moves it onto the operator stack
    // (`refs/postfix.c:296-307`: `*--pout` un-emits the fetch,
    // `pstacktop->code = STORE_A + *pout - FETCH_A` pushes the store), so the
    // `Op::Arg(0)` that `A` produced does NOT survive into the output.
    #[test]
    fn store_unemits_the_fetch_and_emits_a_store_after_its_value() {
        assert_eq!(
            ops("A := 0; A"),
            vec![Op::Lit(0.0), Op::Store(0), Op::Arg(0)]
        );
    }

    // The corpus's second pinning shape (`refs/epicsCalcTest.cpp:868`):
    // `"a; a := 0"` == a. The leading fetch survives; the store's own fetch is
    // un-emitted and the store lands last, popping the literal it was given -
    // so the value left on the stack is the FIRST fetch, not the stored one.
    #[test]
    fn terminator_emits_nothing_and_leaves_the_earlier_value_on_the_stack() {
        assert_eq!(
            ops("A; A := 0"),
            vec![Op::Arg(0), Op::Lit(0.0), Op::Store(0)]
        );
    }

    // Lowercase reaches the same place: the lexer uppercases identifiers
    // before `Token::Arg` is produced, so the corpus's lowercase spellings
    // compile identically.
    #[test]
    fn lowercase_operands_store_to_the_same_slot() {
        assert_eq!(ops("a := 0; a"), ops("A := 0; A"));
        assert_eq!(ops("u := 0; u"), vec![Op::Lit(0.0), Op::Store(20), Op::Arg(20)]);
    }

    // THE SURPRISE, and the single most important compile-shape test here.
    //
    // `:=` carries in-stack priority 0 (`refs/postfix.c:162`, column 2), and
    // `:`'s flush loop (`refs/postfix.c:400-410`, the `CONDITIONAL` case) uses
    // a STRICT comparison: `while (pstacktop->in_stack_pri > pel->in_coming_pri)`
    // with `:`'s in-coming priority also 0. `0 > 0` is false, so a pending
    // store is NOT flushed by the `:` - it stays on the operator stack,
    // underneath the `COND_END` that `:` pushes (`:422-425`), and is emitted
    // only when the whole expression (or the next `;`) flushes the stack.
    //
    // The store therefore HOISTS OUT of the ternary: `A ? B := 1 : 2` means
    // `B := (A ? 1 : 2)`, and B is written on BOTH paths. Hand-derived, then
    // pinned here. The naive alternative - treating `:=` like any other
    // pending operator and flushing it at the `:` - would instead emit
    // `[Arg0, CondIf{4}, Lit1, Store(1), CondElse{..}, Lit2, CondEnd, Arg1]`,
    // whose then-branch nets 0 rather than +1 and which `check_segment` would
    // reject outright as `MissingOperand`. So this is a real fork in
    // behaviour, not a cosmetic one: Base ACCEPTS this expression and the
    // naive reading REJECTS it.
    #[test]
    fn store_in_a_then_branch_hoists_out_of_the_conditional() {
        assert_eq!(
            ops("A ? B := 1 : 2; B"),
            vec![
                Op::Arg(0),
                Op::CondIf { else_target: 4 },
                Op::Lit(1.0),
                Op::CondElse { end_target: 6 },
                Op::Lit(2.0),
                Op::CondEnd,
                // Outside the conditional entirely - runs whichever arm was
                // taken.
                Op::Store(1),
                Op::Arg(1),
            ]
        );
    }

    // The other half of the same rule: once `:` HAS run, it has pushed
    // `COND_END` onto the operator stack (`refs/postfix.c:422-425`), so the
    // stack is no longer empty - and `refs/postfix.c:297`'s guard
    // (`pout == pdest || pstacktop > stack || *--pout is not a FETCH`)
    // rejects a store whose operator stack is non-empty. A store in an ELSE
    // branch is therefore a compile error in Base, not a conditional store.
    #[test]
    fn store_in_an_else_branch_is_rejected() {
        assert_eq!(compile("A ? 1 : B := 2"), Err(CalcError::BadAssignment));
    }

    // `refs/postfix.c:205-214`: `get_element` scans `operators[]` backwards,
    // so `:=` (row 162) wins over `:` (row 161) and the ternary never gets
    // its else-marker. cond_count stays 1, and the end-of-parse check
    // (`refs/postfix.c:495-498`) reports `CALC_ERR_CONDITIONAL`. Matched here.
    #[test]
    fn store_swallows_the_colon_of_a_ternary_leaving_it_unbalanced() {
        assert_eq!(compile("A?B:=C"), Err(CalcError::BadConditional));
    }

    // `refs/postfix.c:296-301`, the three ways the store guard fires. Each is
    // a compile error, never a panic.
    #[test]
    fn store_requires_a_bare_operand_as_its_target() {
        // `pout == pdest`: nothing emitted yet.
        assert_eq!(compile(":= A"), Err(CalcError::MissingOperand));
        // Last emitted opcode is a literal, not a FETCH. `PI` and a bare
        // number both compile to `Op::Lit` here (see `constant`), matching
        // Base's `CONST_PI`/`LITERAL_*`, none of which is in
        // `FETCH_A..FETCH_A+21`.
        assert_eq!(compile("PI := 1"), Err(CalcError::BadAssignment));
        assert_eq!(compile("1 := A"), Err(CalcError::BadAssignment));
        // Last emitted opcode is a function result, not a FETCH.
        assert_eq!(compile("SIN(A) := 1"), Err(CalcError::BadAssignment));
        // `pstacktop > stack`: an operator is still pending, so the trailing
        // `Op::Arg` in the output is not the whole left-hand side.
        assert_eq!(compile("A + B := 1"), Err(CalcError::BadAssignment));
        // Same guard, via an open paren rather than a pending operator -
        // which is why `(A := 1)` cannot be used to get a store into a
        // ternary arm either.
        assert_eq!(compile("(A := 1)"), Err(CalcError::BadAssignment));
        // A second `:=` with the first still pending: at `B := 0`'s `:=`,
        // `STORE_A` (from `A :=`) is still on the operator stack, so
        // `pstacktop > stack` trips the same guard (`refs/postfix.c:297`).
        // `Frame::Op(Op::Store(_))` is not a `CondHead`, so this crate hits
        // the identical `BadAssignment` rather than treating `B := 0` as an
        // operand for the outer `A :=`.
        assert_eq!(compile("A := B := 0; A"), Err(CalcError::BadAssignment));
    }

    // DEPTH ACCOUNTING. A store pops one value and pushes nothing, so a bare
    // `A := 0` ends at depth 0. Base rejects that at `refs/postfix.c:499`
    // (`if (operand_needed || runtime_depth != 1) CALC_ERR_INCOMPLETE`) - an
    // expression must leave exactly one value. `check_arity` already reports
    // `MissingOperand` for a non-empty program ending at depth 0, so this
    // needs no new machinery, only the `Op::Store` depth arm.
    #[test]
    fn a_bare_store_leaves_no_result_and_is_rejected() {
        assert_eq!(compile("A := 0"), Err(CalcError::MissingOperand));
        assert_eq!(compile("A := 0; B := 1"), Err(CalcError::MissingOperand));
    }

    // `;` lets depth return to 0 mid-expression without that being an
    // underflow: `A := 0` between the terminators nets zero (fetch +1,
    // un-emit -1, literal +1, store -1) and the depth genuinely sits at 0
    // right after each store. Nothing here may error.
    #[test]
    fn depth_may_return_to_zero_between_terminators() {
        assert_eq!(
            ops("A := 1; B := A; C := B; C"),
            vec![
                Op::Lit(1.0),
                Op::Store(0),
                Op::Arg(0),
                Op::Store(1),
                Op::Arg(1),
                Op::Store(2),
                Op::Arg(2),
            ]
        );
    }

    // `refs/postfix.c:457`: `EXPR_TERMINATOR` sets `operand_needed = TRUE`,
    // and `:459`'s end-of-parse check rejects a still-pending operand. So a
    // trailing `;`, a doubled `;`, and a leading `;` are all errors.
    //
    // Base reaches the leading/doubled cases by a different route -
    // `get_element` looks up an operand-position token in `operands[]`, where
    // `;` does not appear, giving `CALC_ERR_SYNTAX` (`:475-478`) - but the
    // accept/reject verdict is the same, which is what matters here (this
    // crate's lexer is context-free and has one shared symbol table).
    #[test]
    fn terminator_in_operand_position_is_rejected() {
        assert_eq!(compile("A;"), Err(CalcError::MissingOperand));
        assert_eq!(compile("A;;B"), Err(CalcError::MissingOperand));
        assert_eq!(compile(";A"), Err(CalcError::MissingOperand));
    }

    // `refs/postfix.c:436-439`: a `(` still open when `;` flushes the stack
    // is `CALC_ERR_PAREN_OPEN`; `:448-451`: an unbalanced `?` at a `;` is
    // `CALC_ERR_CONDITIONAL`.
    #[test]
    fn terminator_rejects_an_open_paren_or_dangling_conditional() {
        assert_eq!(compile("(A;B)"), Err(CalcError::Unbalanced));
        assert_eq!(compile("A?B; C"), Err(CalcError::BadConditional));
    }

    // A completed ternary before a `;` is finalized by the flush, exactly as
    // `)`/`,`/an enclosing `:`/end-of-input already do
    // (`refs/postfix.c:440-445` flushes the pushed `COND_END`).
    #[test]
    fn terminator_finalizes_a_completed_ternary() {
        assert_eq!(
            ops("A?B:C; D := 1"),
            vec![
                Op::Arg(0),
                Op::CondIf { else_target: 4 },
                Op::Arg(1),
                Op::CondElse { end_target: 6 },
                Op::Arg(2),
                Op::CondEnd,
                Op::Lit(1.0),
                Op::Store(3),
            ]
        );
        // The second segment has to be a store, i.e. net zero. `;` does NOT
        // discard the running stack: Base carries `runtime_depth` straight
        // across it (`refs/postfix.c:433-458` never resets it) and the
        // end-of-parse check is `runtime_depth != 1` (`:499`), so `"A?B:C; D"`
        // finishes at depth 2 and is an error in Base too. Written as a live
        // assertion rather than a comment because this is the opposite of
        // what "expression terminator" suggests.
        assert_eq!(compile("A?B:C; D"), Err(CalcError::ExtraOperand));
    }

    // A store whose value is a parenthesised ternary: the store frame sits
    // BELOW the `(` on the operator stack (it was pushed when the stack was
    // empty), so neither `:` nor `)` disturbs it, and the conditional's arms
    // are checked arm-by-arm with no `Op::Store` inside either of them.
    #[test]
    fn store_of_a_parenthesised_conditional_keeps_the_store_outside_the_arms() {
        assert_eq!(
            ops("A := (B ? 1 : 2); A"),
            vec![
                Op::Arg(1),
                Op::CondIf { else_target: 4 },
                Op::Lit(1.0),
                Op::CondElse { end_target: 6 },
                Op::Lit(2.0),
                Op::CondEnd,
                Op::Store(0),
                Op::Arg(0),
            ]
        );
    }

    // Exhaustive over the 21 slots: every letter A-U must map to its own
    // store index, in both cases. Catches an off-by-one or a truncated
    // A-L range (RULINGS.md Ruling 1) in the store path specifically, which
    // the fetch path's own tests would not.
    #[test]
    fn every_operand_letter_has_its_own_store_slot() {
        for i in 0..21usize {
            let upper = (b'A' + i as u8) as char;
            let lower = upper.to_ascii_lowercase();
            let src = format!("{upper} := 0; {upper}");
            assert_eq!(
                ops(&src),
                vec![Op::Lit(0.0), Op::Store(i), Op::Arg(i)],
                "uppercase {upper}"
            );
            assert_eq!(ops(&format!("{lower} := 0; {lower}")), ops(&src), "lowercase {lower}");
        }
    }

    // Task 8. `refs/calcPerform.c:430-506` `calcArgUsage`, corrected to 21
    // slots (RULINGS.md Ruling 1) and two masks (Ruling 2) - the brief's
    // `0b1000_0000_0000` for `"L"` was `u16` bit 11 read as "the top bit";
    // at 21 slots `L` (the 12th letter) is still bit 11, just no longer the
    // top of anything.
    #[test]
    fn arg_usage_reports_referenced_operands() {
        assert_eq!(
            compile("A+C").unwrap().arg_usage(),
            ArgUsage { inputs: 0b101, stores: 0 }
        );
        assert_eq!(
            compile("L").unwrap().arg_usage(),
            ArgUsage { inputs: 1 << 11, stores: 0 }
        );
        assert_eq!(compile("1+2").unwrap().arg_usage(), ArgUsage { inputs: 0, stores: 0 });
        assert_eq!(compile("").unwrap().arg_usage(), ArgUsage { inputs: 0, stores: 0 });
    }

    #[test]
    fn arg_usage_counts_each_operand_once() {
        assert_eq!(
            compile("A+A*A").unwrap().arg_usage(),
            ArgUsage { inputs: 0b1, stores: 0 }
        );
    }

    // `refs/calcPerform.c:472-473`: `inputs |= (1 << (op - FETCH_A)) &
    // ~stores;` - "Don't claim to use an arg we already stored to". A store
    // followed by a fetch of the SAME slot does not count as an input: the
    // value read came from the just-stored expression result, not from an
    // `INPx` link, so `calcout` must not fetch it.
    #[test]
    fn store_then_fetch_of_same_slot_is_not_reported_as_input() {
        assert_eq!(
            compile("A := 1; A").unwrap().arg_usage(),
            ArgUsage { inputs: 0, stores: 0b1 }
        );
    }

    // The reverse order: a fetch that PRECEDES a store to the same slot is a
    // genuine input (the value read still came from the link, before the
    // expression overwrote it), so this is the mirror image of the previous
    // test, not a duplicate of it - the two masks are not independent of
    // instruction order.
    #[test]
    fn fetch_then_store_of_same_slot_is_reported_as_input() {
        assert_eq!(
            compile("A; A := 1").unwrap().arg_usage(),
            ArgUsage { inputs: 0b1, stores: 0b1 }
        );
    }

    // Bit 20 (`U`, the 21st and last slot) on the STORE side specifically -
    // exercises the top of the store mask, mirroring
    // `every_operand_letter_has_its_own_store_slot`'s fetch-side coverage.
    // A bare `"U := 5"` nets zero stack depth (fetch +1, un-emit -1, literal
    // +1, store -1) and `check_arity` rejects any expression that doesn't
    // finish at depth 1 (see `depth_may_return_to_zero_between_terminators`),
    // so a trailing `; 1` segment is needed to make the whole expression
    // valid without touching either mask.
    #[test]
    fn store_to_u_sets_bit_20() {
        assert_eq!(
            compile("U := 5; 1").unwrap().arg_usage(),
            ArgUsage { inputs: 0, stores: 1 << 20 }
        );
    }

    // `calcArgUsage`'s walk is a flat linear scan over the opcode stream
    // (`refs/calcPerform.c:435`, `while ((op = *pinst++) != END_EXPRESSION)`)
    // with a plain `switch` on each opcode - it never inspects `COND_IF`/
    // `COND_ELSE`/`COND_END` to skip the untaken branch the way `eval.rs`'s
    // jump-following interpreter does. This crate's `ops` vector lays both
    // ternary arms out back-to-back the same way Base's instruction stream
    // does (see `op.rs`'s `Op::CondIf` doc), and this walk is likewise a
    // plain `for` over `self.ops` with no jump-target handling, so all three
    // operands of `A?B:C` are reported even though only one of B/C would
    // ever actually be read at runtime.
    #[test]
    fn ternary_reports_both_untaken_and_taken_arm() {
        assert_eq!(
            compile("A?B:C").unwrap().arg_usage(),
            ArgUsage { inputs: 0b111, stores: 0 }
        );
    }

    // An expression that only stores, never fetches: the RHS is a bare
    // literal, so the only opcode touching either mask is the store itself.
    // Like `store_to_u_sets_bit_20` above, a lone `"A := 1"` nets stack
    // depth 0 and `check_arity` rejects it, so `; 1` closes the expression
    // out with a segment that touches neither mask. `inputs` must stay zero.
    #[test]
    fn stores_only_expression_reports_zero_inputs() {
        assert_eq!(
            compile("A := 1; 1").unwrap().arg_usage(),
            ArgUsage { inputs: 0, stores: 0b1 }
        );
    }

    // `;` is `NOT_GENERATED` (`refs/postfix.c:163`) and emits no opcode - Task
    // 8a already established this, and `arg_usage`'s walk is opcode-only (see
    // `Expression::arg_usage`'s doc), so segment boundaries are invisible to
    // it exactly as they are to Base's `calcArgUsage`, which walks the same
    // opcode stream `calcPerform` does.
    #[test]
    fn compile_empty_reports_no_usage() {
        assert_eq!(compile("").unwrap().arg_usage(), ArgUsage { inputs: 0, stores: 0 });
    }
}
