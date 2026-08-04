//! EPICS CALC expression compiler and evaluator.
//!
//! Mirrors `postfix()` and `calcPerform()` from EPICS Base. Expressions use
//! operands `A`-`U` (`CALCPERFORM_NARGS` = 21, `postfix.h:29`), supplied to
//! the evaluator as a 21-element array.
//!
//! `Expression`/`compile` land in Task 2; only the lexer and its error type
//! exist so far.

mod lex;

// `parse` currently only defines `CalcError`; `Expression`/`compile` are
// added in Task 2 (the brief's `pub use parse::{CalcError, Expression,
// compile};` can't compile yet because those items don't exist).
mod parse;
mod op;
mod eval;

pub use parse::CalcError;
