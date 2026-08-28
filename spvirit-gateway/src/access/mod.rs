//! Access control for the gateway: p4p `pvlist` allow/deny/alias rules
//! (this module) and `.acf` ASG/ASL definitions (Task 8, `acf` module).
//!
//! Pure parsing + matching logic only — no I/O, no enforcement. The
//! evaluator (`decide`) lands in a later task once both `pvlist` and `acf`
//! are available.

pub mod acf;
pub mod pvlist;
