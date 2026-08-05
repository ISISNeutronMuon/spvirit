//! EPICS CALC expression compiler and evaluator.
//!
//! Mirrors `postfix()` and `calcPerform()` from EPICS Base. Expressions use
//! operands `A`-`U` (`CALCPERFORM_NARGS` = 21, `postfix.h:29`), supplied to
//! the evaluator as a `&mut` 21-element array: evaluation only ever reads it
//! back through fetches, except that a `:=` store writes the assigned value
//! into the corresponding slot before returning (RULINGS.md Ruling 2).
//!
//! `Expression`/`compile` (`parse.rs`) wire `lex`/`Token` and the `op`
//! operator table together; `Expression::eval`/`eval_with_rng` (`eval.rs`)
//! perform the evaluation.

mod lex;
mod parse;
mod op;
mod eval;

pub use parse::{CalcError, Expression, compile};
