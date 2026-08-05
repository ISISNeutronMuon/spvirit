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
    //
    // `refs/calcPerform.c:400-411`: Base's `COND_IF`/`COND_ELSE`/`COND_END`
    // are three opcodes, not one, and the untaken branch is never executed -
    // `cond_search` scans the instruction stream forward to the matching
    // marker. Task 3's `Op::Cond` (pop three, select one - see the removed
    // arm's comment in `eval.rs`'s git history) was an explicitly-flagged
    // placeholder, correct only because nothing in Task 3/4's feature set
    // could observe the difference between eager and short-circuit
    // evaluation.
    //
    // This crate represents Base's linear marker-scan as compile-time
    // jump offsets instead: `else_target`/`end_target` are absolute indices
    // into the owning `Expression.ops`, computed once by `parse.rs` and
    // baked into the opcode, so `eval.rs` does an O(1) jump rather than an
    // O(n) rescan per evaluation. This is a deliberate, documented
    // divergence from Base's *mechanism* (a linear scan vs. a precomputed
    // offset), not from its *semantics*: both leave the untaken branch
    // wholly unexecuted, and `parse.rs`'s `check_arity` proves every
    // jump target is in range and every branch leaves exactly one value on
    // the stack, so no malformed offset is ever reachable by public API
    // (`Expression.ops` is `pub(crate)`, so nothing outside this crate can
    // construct one anyway).
    //
    // Layout emitted by `parse.rs` for `A?B:C`:
    //   [..cond ops..] CondIf{else_target} [..then ops..]
    //   CondElse{end_target} [..else ops..] CondEnd
    // `else_target` points one past the `CondElse` (the first instruction
    // of the else-branch); `end_target` points one past the matching
    // `CondEnd` (the first instruction after the whole conditional).
    // Nesting is handled for free: each `CondIf`/`CondElse` pair's targets
    // are computed from that specific pair's own position, so a nested
    // `CondIf` inside a skipped branch can never have its `CondElse`
    // mistaken for an enclosing one - there is no scanning-for-a-marker
    // step at eval time to get confused, and the targets are absolute
    // rather than relative-and-counted.
    /// Pop the condition; if it's `0.0`, jump to `else_target` (the first op
    /// of the else-branch). Otherwise fall through into the then-branch.
    /// Mirrors `calcPerform.c:400-403`'s `*ptop-- == 0.0`.
    CondIf { else_target: usize },
    /// Reached only by falling through the end of a then-branch (never by a
    /// `CondIf` jump, which lands past it): unconditionally jump to
    /// `end_target` (the first op after the whole conditional), skipping
    /// the else-branch. Mirrors `calcPerform.c:405-407`.
    CondElse { end_target: usize },
    /// No-op marker: the first op after an else-branch, and the jump target
    /// of the owning `CondElse`. Mirrors `calcPerform.c:409-410`.
    CondEnd,

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

    // Relational and logical (Task 5)
    /// `refs/calcPerform.c:395-398` `GR_THAN`: `*ptop > top`. Every
    /// comparison against NaN is false in IEEE 754 (and thus in C), so
    /// `NaN > x` and `x > NaN` are both `0.0` — Rust's `f64::>` already
    /// matches this, no special-casing needed.
    Gt,
    /// `refs/calcPerform.c:390-393` `GR_OR_EQ`: `*ptop >= top`.
    Ge,
    /// `refs/calcPerform.c:375-378` `LESS_THAN`: `*ptop < top`.
    Lt,
    /// `refs/calcPerform.c:380-383` `LESS_OR_EQ`: `*ptop <= top`.
    Le,
    /// `refs/calcPerform.c:385-388` `EQUAL`: `*ptop == top`. `refs/postfix.c`
    /// aliases `=` and `==` to the same opcode — both spellings map here.
    /// `NaN == NaN` is `0.0`, matching IEEE 754/C, not the alias's surface
    /// resemblance to assignment.
    Eq,
    /// `refs/calcPerform.c:370-373` `NOT_EQ`: `*ptop != top`. `refs/postfix.c`
    /// aliases `#` and `!=` to the same opcode. The sole comparison where
    /// NaN operands yield `1.0` rather than `0.0`: `NaN != x` is true for
    /// every `x`, NaN included.
    Ne,
    /// `refs/calcPerform.c:305-308` `REL_AND`: `*ptop = *ptop && top`. Both
    /// operands are already on the stack by the time this opcode runs — it
    /// does NOT short-circuit at the operand-evaluation level (unlike the
    /// ternary above, which does). C's `&&` treats any non-zero double as
    /// true, NaN included, so `NaN && 1.0` is `1.0`. See RULINGS.md and
    /// task-5-brief.md: this is Base's actual behavior, not a shortcut to
    /// "improve".
    AndL,
    /// `refs/calcPerform.c:300-303` `REL_OR`: `*ptop = *ptop || top`. Same
    /// non-short-circuiting, NaN-is-truthy behavior as `AndL`.
    OrL,
    /// `refs/calcPerform.c:310-312` `REL_NOT`: `*ptop = ! *ptop`. `!NaN` is
    /// `0.0` (NaN is truthy, so its negation is false), matching C.
    NotL,

    // Bitwise (Task 6). Every case here converts its `f64` operand(s) to a
    // 32-bit integer via `d2i`/`d2ui` (`eval.rs`, mirroring
    // `calcPerform.c:325-326`'s `d2i`/`d2ui` macros), applies the C-integer
    // operation, and widens the (always signed) result back to `f64` -
    // `calcPerform.c:314-324`'s twelve-line comment explains why the cast is
    // asymmetric by sign rather than by magnitude.
    /// `refs/calcPerform.c:333-336` `BIT_AND`: `d2i(*ptop) & d2i(top)`.
    /// `refs/postfix.c:152,174` - both `&` and the word form `AND`.
    AndB,
    /// `refs/calcPerform.c:328-331` `BIT_OR`: `d2i(*ptop) | d2i(top)`.
    /// `refs/postfix.c:178,175` - both `|` and the word form `OR`.
    OrB,
    /// `refs/calcPerform.c:338-341` `BIT_EXCL_OR`: `d2i(*ptop) ^ d2i(top)`.
    /// `refs/postfix.c:176` - only the word form `XOR`; `^` is `Pow`
    /// (exponentiation), not xor, per RULINGS.md.
    XorB,
    /// `refs/calcPerform.c:343-345` `BIT_NOT`: `~d2i(*ptop)`.
    /// `refs/postfix.c:144,126` - both `~` and the word form `NOT`.
    NotB,
    /// `refs/calcPerform.c:360-363` `LEFT_SHIFT_ARITH`:
    /// `d2i(*ptop) << (d2i(top) & 31)`. `refs/postfix.c:165` - `<<`.
    Shl,
    /// `refs/calcPerform.c:355-358` `RIGHT_SHIFT_ARITH`:
    /// `d2i(*ptop) >> (d2i(top) & 31)` - arithmetic (sign-extending) shift.
    /// `refs/postfix.c:171` - `>>`. Rust's `>>` on `i32` is arithmetic by
    /// definition, same as C's on every real platform (both note this
    /// explicitly, since C's is technically implementation-defined) - no
    /// divergence to paper over here.
    Shr,
    /// `refs/calcPerform.c:365-368` `RIGHT_SHIFT_LOGIC`:
    /// `*ptop = (double)(d2ui(*ptop) >> (d2ui(top) & 31u));` - logical
    /// (zero-filling) shift of the UNSIGNED reinterpretation. Unlike every
    /// other bitwise op here, the result is widened to `f64` DIRECTLY from
    /// `epicsUInt32`, not re-cast to signed first: the "result is always
    /// signed" rule from the `d2i`/`d2ui` twelve-line comment
    /// (`calcPerform.c:314-324`) is stated about `d2i`, and
    /// `RIGHT_SHIFT_LOGIC` is Base's documented exception to it, not
    /// another instance of it - so this is the one op whose result can be
    /// as large as `4294967295.0`, never negative. `refs/postfix.c:172` -
    /// `>>>`, required by RULINGS.md Ruling 2 even though
    /// task-6-brief.md's text never mentions it.
    ShrLogic,
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
/// Levels 1-7 are fully implemented as of Task 6: level 1 (`|`/`OR`/`XOR`,
/// alongside `||`) and level 2's remaining members (`&`/`AND`/`<<`/`>>`/
/// `>>>`, alongside `&&`) land the bitwise operators; level 3-7 (relational
/// through unary/functions) were already implemented as of Task 5.
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
        // `?`/`:` still carry priority 0 (loosest), matching
        // `postfix.c:161,173`, and are handled structurally by the parser
        // (see `Frame::CondHead`/`Frame::CondTail` in `parse.rs`) rather
        // than via a normal `pop_while` comparison against one of these
        // `Op` variants directly — `CondIf`/`CondElse`/`CondEnd` are never
        // pushed onto the operator stack as a `Frame::Op` (they're written
        // straight into the output as placeholders and backpatched), so
        // this function is never actually called on them at runtime. The
        // arm exists only so this match stays exhaustive as the enum grows;
        // the value is arbitrary but chosen to match the old placeholder.
        Op::CondIf { .. } | Op::CondElse { .. } | Op::CondEnd => (0, Assoc::Right),
        // `|`/`OR`/`XOR` share priority 1 with `||` (`refs/postfix.c:175,
        // 176,178` vs `:179`). `epicsCalcTest.cpp:915`: `1 | 3 XOR 1 ==
        // (1|3)^1` - equal priority, left-associative (see
        // `bitwise_or_and_xor_are_equal_precedence_left_associative` and
        // `bitwise_or_shares_precedence_with_logical_or` in parse.rs's tests).
        Op::OrL | Op::OrB | Op::XorB => (1, Assoc::Left),
        // `&`/`AND`/`<<`/`>>`/`>>>` share priority 2 with `&&`
        // (`refs/postfix.c:152,174,165,171,172` vs `:153`).
        // `epicsCalcTest.cpp:924-929`: same-priority left-associative ties
        // between `&`/`AND` and each shift; `:932-934`: relational (3) binds
        // TIGHTER than these (2), the opposite tie-break direction - see
        // `parse.rs`'s Task 6 precedence tests for both directions,
        // hand-traced against `pop_while`.
        Op::AndL | Op::AndB | Op::Shl | Op::Shr | Op::ShrLogic => (2, Assoc::Left),
        // RULINGS.md Ruling 3 corrects task-5-brief.md's precedence numbers
        // (which used a stale 12-level scheme putting relationals at 7,
        // `&&`/`||` at 2/3, and `NotL` at 11): Base has 7 levels, and this
        // table's existing 4-7 (Add/Sub..unary) were already built against
        // Ruling 3's numbering, so relationals slot in at 3, `&&` at 2
        // (grouped with bitwise `&` and the shifts, Task 6), and `||` at 1
        // (grouped with bitwise `|`/`OR`/`XOR`, Task 6) rather than getting
        // levels of their own.
        Op::Gt | Op::Ge | Op::Lt | Op::Le | Op::Eq | Op::Ne => (3, Assoc::Left),
        Op::Add | Op::Sub => (4, Assoc::Left),
        Op::Mul | Op::Div | Op::Modulo => (5, Assoc::Left),
        Op::Pow => (6, Assoc::Left),
        // Unary `~`/`NOT` share priority 7 with unary `-`/`!`
        // (`refs/postfix.c:144,126` vs `:76,90`). See
        // `bitwise_not_binds_tighter_than_relational` (parse.rs).
        Op::Neg | Op::NotL | Op::NotB => (7, Assoc::Right),
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
        | Op::Tanh
        | Op::NotL
        | Op::NotB => 1,
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Modulo
        | Op::Pow
        | Op::Atan2
        | Op::Gt
        | Op::Ge
        | Op::Lt
        | Op::Le
        | Op::Eq
        | Op::Ne
        | Op::AndL
        | Op::OrL
        | Op::AndB
        | Op::OrB
        | Op::XorB
        | Op::Shl
        | Op::Shr
        | Op::ShrLogic => 2,
        // `CondIf` pops the condition value (`calcPerform.c:400-401`,
        // `*ptop--`); `check_arity` (parse.rs) uses this directly when it
        // simulates the depth just before entering the then/else branches.
        // `CondElse`/`CondEnd` never touch the stack — they're pure control
        // flow (`calcPerform.c:405-410`) — so both are 0, even though the
        // ternary as a whole still nets exactly one value, same as the old
        // 3-in/1-out `Op::Cond` did; `parse.rs::check_segment` is what
        // proves that net effect now, since it's no longer a single op's
        // arity to read off this table.
        Op::CondIf { .. } => 1,
        Op::CondElse { .. } | Op::CondEnd => 0,
        // Variadic: arity is carried in the variant itself. `check_arity`
        // (parse.rs) reads this the same way as any other op, so the
        // `.expect("arity checked at compile time")` pops in `eval.rs`
        // stay a sound invariant as long as `parse.rs` populates the count
        // correctly (see the comma-counting logic there).
        Op::Min(n) | Op::Max(n) => *n,
    }
}
