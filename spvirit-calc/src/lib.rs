//! EPICS CALC expression compiler and evaluator.
//!
//! Mirrors `postfix()` and `calcPerform()` from EPICS Base. Expressions use
//! operands `A`-`U` (`CALCPERFORM_NARGS` = 21, `postfix.h:29`), supplied to
//! the evaluator as a 21-element array.
//!
//! `Expression`/`compile` land in Task 2 wiring `lex`/`Token` and the
//! `op` operator table together; evaluation (`Expression::eval`) lands in
//! Task 3.

mod lex;
mod parse;
mod op;
mod eval;

pub use parse::{CalcError, Expression, compile};
