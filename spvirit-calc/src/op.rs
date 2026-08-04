/// A single postfix operation.
///
/// Variants are added by later tasks; the parser and evaluator match
/// exhaustively, so adding one is a compile error until both are updated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    /// Push operand A-U (index 0-20). `CALCPERFORM_NARGS` is 21
    /// (`postfix.h:29`), see RULINGS.md Ruling 1.
    Arg(usize),
    /// Push a literal.
    Lit(f64),

    // Arithmetic (Task 3)
    Add,
    Sub,
    Mul,
    Div,
    /// Integer modulo, matching `calcPerform.c`'s `MODULO` case (both
    /// operands truncate to `epicsInt32`, C's `%` applies, zero divisor
    /// yields `NaN`). Named `Modulo` rather than `Rem` per RULINGS.md
    /// Ruling 5's suggestion, since `Rem` invites readers to assume f64
    /// remainder/fmod semantics, which is exactly the mistake the
    /// original task-3-brief.md test made.
    Modulo,
    Pow,
    Neg,

    // Conditional (Task 5)
    Cond,

    // Algebraic and transcendental (Task 4)
    Abs,
    /// Square root. Covers both `SQR` and `SQRT` (`refs/postfix.c:137-138`
    /// alias the same `SQU_RT` opcode) - `SQR` is square *root*, not
    /// squaring.
    Sqrt,
    Exp,
    /// Base-10 log (`LOG` -> `LOG_10`, `refs/calcPerform.c:183-184`).
    Log10,
    /// Natural log. Covers both `LOGE` and `LN` (`refs/postfix.c:117,119`
    /// both alias `LOG_E`, `refs/calcPerform.c:187-188`).
    Ln,
    Ceil,
    Floor,
    /// Round half-away-from-zero via a truncating `epicsInt32` cast
    /// (`refs/calcPerform.c:291-294`), not `f64::round`'s general behavior
    /// and not banker's rounding. See the `eval.rs` arm for the exact
    /// divergence from C's cast on overflow.
    Nint,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    /// `refs/calcPerform.c:225-228`, commented by Base itself as
    /// "Ouch!: Args backwards!": `ATAN2(A,B)` evaluates to `atan2(B, A)`,
    /// not the mathematically expected `atan2(A, B)`. Do not "fix" this.
    Atan2,
    Sinh,
    Cosh,
    Tanh,
    /// Variadic minimum over the given operand count
    /// (`refs/postfix.c:122`, `VARARG_OPERATOR`). NaN-propagating: any NaN
    /// argument, at any position, makes the whole result NaN
    /// (`refs/calcPerform.c:200-207`) - the opposite of `f64::min`.
    Min(usize),
    /// Variadic maximum over the given operand count
    /// (`refs/postfix.c:121`, `VARARG_OPERATOR`). NaN-propagating, mirroring
    /// `Min` (`refs/calcPerform.c:191-198`).
    Max(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Assoc {
    Left,
    Right,
}

/// Binding power and associativity. Higher binds tighter.
///
/// Verified against EPICS Base `modules/libcom/src/calc/postfix.c`'s
/// `operands[]`/`operators[]` tables (not `calc.y`, which does not exist in
/// Base 7 - see RULINGS.md). Columns 2/3 of each row are the in-stack/
/// in-coming priority pair; Base has 7 levels, not the original task
/// brief's 12:
///
/// | Prio | Operators                                    | `postfix.c` lines |
/// |---|-------------------------------------------------|--------------------|
/// | 1 | `\|` `OR` `XOR` `\|\|`                           | 175,176,178,179    |
/// | 2 | `&` `&&` `<<` `>>` `>>>` `AND`                   | 152,153,165,171,172,174 |
/// | 3 | relational: `<` `<=` `>` `>=` `=` `==` `#` `!=`  | 149,150,164,166-170 |
/// | 4 | `+` `-` (binary)                                 | 157,159            |
/// | 5 | `*` `/` `%`                                      | 151,155,160        |
/// | 6 | `**` `^` (POWER, LEFT-associative)               | 156,177            |
/// | 7 | unary `-` `!` `~` `NOT`, and all functions        | 74,76,90-144       |
///
/// Only the arithmetic/conditional operators introduced by Task 2 are
/// implemented here; levels 1-3 belong to later tasks (bitwise: Task 6;
/// relational: Task 5) and are documented above only so this table stays
/// the single point of truth as those land.
///
/// Two corrections to the original task brief, both required by the
/// corpus (RULINGS.md Ruling 4):
/// - `**`/`^` (POWER) are LEFT-associative, not right. Both rows carry
///   equal in-stack/in-coming priority (6/6), which `postfix()`'s
///   algorithm resolves left-to-right. `refs/epicsCalcTest.cpp:822`:
///   `2^2^3 == pow(pow(2,2),3) == 64`.
/// - Unary `-`/`!`/`~`/`NOT` sit at priority 7, ABOVE power's 6 (the brief
///   put unary below power). `refs/epicsCalcTest.cpp:945`:
///   `-2^2 == pow(-2,2) == +4`, i.e. negation binds to the operand before
///   exponentiation, not to the result.
pub(crate) fn precedence(op: &Op) -> (u8, Assoc) {
    match op {
        // Priority 0: binds loosest of all, matching `postfix.c:161,173`
        // (`:`/`?` both carry in-stack/in-coming priority 0/0). Handled
        // structurally by the parser (see `Frame::Cond`), not via a normal
        // pop_while comparison, but a value is still needed here since a
        // pending `Frame::Op(Op::Cond)` sits on the operator stack between
        // `:` and the closing point of the ternary.
        Op::Cond => (0, Assoc::Right),
        Op::Add | Op::Sub => (4, Assoc::Left),
        Op::Mul | Op::Div | Op::Modulo => (5, Assoc::Left),
        Op::Pow => (6, Assoc::Left),
        Op::Neg => (7, Assoc::Right),
        Op::Arg(_) | Op::Lit(_) => (0, Assoc::Left),
        // Functions never sit on the operator stack as a `Frame::Op` (the
        // parser tracks a pending call via `Frame::Func` instead, and emits
        // the op directly at the closing `)` - see `parse.rs`), so
        // `pop_while` never actually compares against these. They're given
        // the tightest possible binding here purely so this match stays
        // exhaustive as new `Op` variants land; postfix.c:74,90-144 puts
        // every function and unary operator at priority 7 (same as `Neg`),
        // not a distinct level 13, but nothing depends on the exact number.
        Op::Abs
        | Op::Sqrt
        | Op::Exp
        | Op::Log10
        | Op::Ln
        | Op::Ceil
        | Op::Floor
        | Op::Nint
        | Op::Sin
        | Op::Cos
        | Op::Tan
        | Op::Asin
        | Op::Acos
        | Op::Atan
        | Op::Atan2
        | Op::Sinh
        | Op::Cosh
        | Op::Tanh
        | Op::Min(_)
        | Op::Max(_) => (13, Assoc::Right),
    }
}

/// Number of operands each operation pops.
pub(crate) fn arity(op: &Op) -> usize {
    match op {
        Op::Arg(_) | Op::Lit(_) => 0,
        Op::Neg
        | Op::Abs
        | Op::Sqrt
        | Op::Exp
        | Op::Log10
        | Op::Ln
        | Op::Ceil
        | Op::Floor
        | Op::Nint
        | Op::Sin
        | Op::Cos
        | Op::Tan
        | Op::Asin
        | Op::Acos
        | Op::Atan
        | Op::Sinh
        | Op::Cosh
        | Op::Tanh => 1,
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Modulo | Op::Pow | Op::Atan2 => 2,
        Op::Cond => 3,
        // Variadic: arity is carried in the variant itself. `check_arity`
        // (parse.rs) reads this the same way as any other op, so the
        // `.expect("arity checked at compile time")` pops in `eval.rs`
        // stay a sound invariant as long as `parse.rs` populates the count
        // correctly (see the comma-counting logic there).
        Op::Min(n) | Op::Max(n) => *n,
    }
}
