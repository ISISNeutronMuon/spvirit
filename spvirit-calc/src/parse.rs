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
            Token::Ident(name) => return Err(CalcError::UnknownIdent(name.clone())),
            Token::Op("(") => {
                stack.push(Frame::Paren);
                expect_operand = true;
            }
            Token::Op(")") => {
                loop {
                    match stack.pop() {
                        Some(Frame::Op(op)) => out.push(op),
                        Some(Frame::Paren) => break,
                        Some(Frame::Cond) | None => return Err(CalcError::Unbalanced),
                    }
                }
                expect_operand = false;
            }
            Token::Op("?") => {
                // `?`/`:` carry priority 0 in `postfix.c` (`:161,173`) —
                // loosest of everything else in the table — so pop every
                // pending operator down to the nearest `(` or enclosing
                // `Cond` before opening this one.
                pop_while(&mut stack, &mut out, 0, Assoc::Right);
                stack.push(Frame::Cond);
                expect_operand = true;
            }
            Token::Op(":") => {
                loop {
                    match stack.pop() {
                        Some(Frame::Op(op)) => out.push(op),
                        Some(Frame::Cond) => break,
                        Some(Frame::Paren) | None => return Err(CalcError::BadConditional),
                    }
                }
                stack.push(Frame::Op(Op::Cond));
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
            Frame::Paren => return Err(CalcError::Unbalanced),
            Frame::Cond => return Err(CalcError::BadConditional),
        }
    }

    check_arity(&out)?;
    Ok(Expression { ops: out })
}

enum Frame {
    Op(Op),
    Paren,
    Cond,
}

/// Pop operators that bind at least as tightly as the incoming one.
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
        ("%", false) => Op::Rem,
        ("^" | "**", false) => Op::Pow,
        _ => return Err(CalcError::MissingOperand),
    })
}

/// Simulate the stack depth to reject programs that under- or over-flow.
///
/// Also catches two adjacent operands with no operator between them (e.g.
/// `"A B"`, or the Task 1 known gap where `lex_number` accepts `"1.2.3"` as
/// two adjacent `Num` tokens): each pushes without popping, so depth ends
/// above 1 and this reports `ExtraOperand`.
fn check_arity(ops: &[Op]) -> Result<(), CalcError> {
    let mut depth: usize = 0;
    for op in ops {
        let need = arity(op);
        if depth < need {
            return Err(CalcError::MissingOperand);
        }
        depth = depth - need + 1;
    }
    match depth {
        0 if ops.is_empty() => Ok(()),
        1 => Ok(()),
        0 => Err(CalcError::MissingOperand),
        _ => Err(CalcError::ExtraOperand),
    }
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

    #[test]
    fn conditional_compiles_to_branch() {
        assert_eq!(
            ops("A?B:C"),
            vec![Op::Arg(0), Op::Arg(1), Op::Arg(2), Op::Cond]
        );
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
}
