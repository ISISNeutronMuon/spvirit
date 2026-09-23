use crate::parse::CalcError;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Token {
    /// Operand `A`-`U`, stored as index 0-20.
    ///
    /// `postfix.c`'s `operands[]` table (`CALCPERFORM_NARGS` = 21,
    /// `postfix.h:29`) treats every letter `A`-`U` as a `FETCH_*` operand,
    /// not just `A`-`L` as the original task brief assumed. See
    /// `RULINGS.md` Ruling 1.
    Arg(usize),
    Num(f64),
    /// Uppercased function or named-constant identifier.
    Ident(String),
    /// Operator or punctuation, as a canonical static string.
    Op(&'static str),
}

/// Multi-character operator/punctuation spellings, ordered so that longer
/// spellings are tried before any shorter prefix of them (`.find()` below
/// returns the first match): `>>>` before `>>` before `>`, `:=` before `:`,
/// `**` before `*`, `<=`/`<<` before `<`, etc. Matches every symbolic entry
/// in `postfix.c`'s `operators[]`/`operands[]` tables (word-operators like
/// `AND`, `OR`, `XOR`, `NOT` are plain identifiers, handled by the
/// alphabetic branch below, not listed here). Includes `>>>`, `:=`, and `;`,
/// which the task brief's original list omitted (RULINGS.md Ruling 2).
const OPS: &[&str] = &[
    ">>>", ">>", ">=", ">", "<<", "<=", "<", "**", "*", ":=", ":", "==", "=",
    "!=", "!", "&&", "&", "||", "|", "+", "-", "/", "%", "^", "~", "#", "(",
    ")", ",", "?", ";",
];

pub(crate) fn lex(src: &str) -> Result<Vec<Token>, CalcError> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let c = bytes[i] as char;

        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        if c.is_ascii_digit() || (c == '.' && i + 1 < bytes.len() && (bytes[i + 1] as char).is_ascii_digit()) {
            let (value, len) = lex_number(&src[i..]).ok_or(CalcError::BadNumber(i))?;
            out.push(Token::Num(value));
            i += len;
            continue;
        }

        if c.is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len() && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = src[start..i].to_ascii_uppercase();
            if word.len() == 1 {
                let ch = word.as_bytes()[0];
                if (b'A'..=b'U').contains(&ch) {
                    out.push(Token::Arg((ch - b'A') as usize));
                    continue;
                }
            }
            out.push(Token::Ident(word));
            continue;
        }

        if let Some(sym) = OPS.iter().find(|s| src[i..].starts_with(**s)) {
            out.push(Token::Op(sym));
            i += sym.len();
            continue;
        }

        return Err(CalcError::BadChar(c, i));
    }

    Ok(out)
}

/// Parse a decimal or `0x`-prefixed hex literal. Returns the value and the
/// number of bytes consumed.
fn lex_number(s: &str) -> Option<(f64, usize)> {
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        let len = rest.chars().take_while(|c| c.is_ascii_hexdigit()).count();
        if len == 0 {
            return None;
        }
        // Base parses hex with `epicsParseUInt32` and stores the result into
        // the postfix stream as raw `epicsUInt32` bits under a `LITERAL_INT`
        // tag (`refs/postfix.c:280-290`); `calcPerform` then reloads those
        // bits as an `epicsInt32` before widening to double
        // (`refs/calcPerform.c:68-71`). So bit 31 is a sign bit: `0xffffffff`
        // is -1, not 4294967295. Values that do not fit in 32 bits are
        // rejected (Base's `epicsParseUInt32` fails -> `CALC_ERR_BAD_LITERAL`,
        // which maps to this crate's `BadNumber`).
        let value = u32::from_str_radix(&rest[..len], 16).ok()?;
        return Some((value as i32 as f64, 2 + len));
    }

    // Scan the float grammar forward, once: digits, at most one `.`, then an
    // exponent only if it is complete. The previous version took the longest
    // run of `[0-9.eE+-]` bytes and re-parsed every shrinking prefix of it --
    // and because `+`/`-` are in that set, the run was the literal plus every
    // operator following it, so `1+1+1+...` was quadratic in the whole
    // expression, not in the literal.
    let b = s.as_bytes();
    let mut end = 0usize;
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    if end < b.len() && b[end] == b'.' {
        let mut after = end + 1;
        while after < b.len() && b[after].is_ascii_digit() {
            after += 1;
        }
        // A `.` counts only if it has a digit on one side or the other:
        // ".5" and "1." are numbers, a bare "." is not.
        if after > end + 1 || end > 0 {
            end = after;
        }
    }
    if end == 0 {
        return None;
    }
    if end < b.len() && (b[end] == b'e' || b[end] == b'E') {
        let mut after = end + 1;
        if after < b.len() && (b[after] == b'+' || b[after] == b'-') {
            after += 1;
        }
        let digits_at = after;
        while after < b.len() && b[after].is_ascii_digit() {
            after += 1;
        }
        // An exponent with no digits is not part of the literal: "1e" is the
        // number 1 followed by the identifier "e".
        if after > digits_at {
            end = after;
        }
    }
    s[..end].parse::<f64>().ok().map(|v| (v, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lex_number_accepts_exactly_the_same_prefixes_as_before() {
        // Behaviour lock for the rewrite below: every one of these is a case
        // the shrink-loop version handled, including the ones where the
        // longest float-ish run is NOT a valid float.
        let cases: &[(&str, Option<(f64, usize)>)] = &[
            ("1", Some((1.0, 1))),
            ("12.5", Some((12.5, 4))),
            (".5", Some((0.5, 2))),
            ("1.", Some((1.0, 2))),
            ("1.5e3", Some((1500.0, 5))),
            ("1e+5", Some((100000.0, 4))),
            ("1e-5", Some((0.00001, 4))),
            ("1e", Some((1.0, 1))),
            ("1e+", Some((1.0, 1))),
            ("1+2", Some((1.0, 1))),
            ("1-2", Some((1.0, 1))),
            ("1..2", Some((1.0, 2))),
            ("0x1f", Some((31.0, 4))),
            ("0xffffffff", Some((-1.0, 10))),
            (".", None),
            ("0x", None),
        ];
        for (src, want) in cases {
            assert_eq!(lex_number(src), *want, "lex_number({src:?})");
        }
    }

    #[test]
    fn lexing_a_number_followed_by_many_operators_is_linear() {
        // `+` and `-` are inside the old "float-ish run", so the run was the
        // literal plus every operator after it, and each shrinking prefix got
        // its own `parse::<f64>()`. This is one token and 20k operators.
        let src = format!("1{}", "+1".repeat(20_000));
        let started = std::time::Instant::now();
        let tokens = lex(&src).expect("lexes");
        assert_eq!(tokens.len(), 40_001, "1, then 20k (+, 1) pairs");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "lexing took {:?}; lex_number is still quadratic",
            started.elapsed()
        );
    }

    #[test]
    fn lexes_operands_numbers_and_operators() {
        let toks = lex("A+1.5*B").expect("lex");
        assert_eq!(
            toks,
            vec![
                Token::Arg(0),
                Token::Op("+"),
                Token::Num(1.5),
                Token::Op("*"),
                Token::Arg(1),
            ]
        );
    }

    #[test]
    fn identifiers_are_case_insensitive() {
        assert_eq!(lex("sin").unwrap(), vec![Token::Ident("SIN".into())]);
        assert_eq!(lex("SIN").unwrap(), vec![Token::Ident("SIN".into())]);
    }

    #[test]
    fn lexes_hex_literals() {
        assert_eq!(lex("0xff").unwrap(), vec![Token::Num(255.0)]);
    }

    #[test]
    fn two_char_operators_beat_one_char() {
        assert_eq!(lex(">=").unwrap(), vec![Token::Op(">=")]);
        assert_eq!(lex("**").unwrap(), vec![Token::Op("**")]);
        assert_eq!(lex("<<").unwrap(), vec![Token::Op("<<")]);
    }

    #[test]
    fn rejects_unknown_character() {
        assert!(matches!(lex("A $ B"), Err(CalcError::BadChar('$', 2))));
    }

    // Task 6 (RULINGS.md Ruling 2): `>>>` (RIGHT_SHIFT_LOGIC) is a distinct
    // operator from `>>` (RIGHT_SHIFT_ARITH) followed by `>`. `OPS`'s
    // longest-match-first ordering (`>>>` before `>>` before `>`) already
    // guards this - see the `OPS` doc comment - but it was never pinned by a
    // test until now. A naive `find` order (or one missing the `>>>` entry)
    // would tokenize `"A>>>B"` as `Arg, Op(">>"), Op(">"), Arg` instead.
    #[test]
    fn triple_char_logical_shift_beats_double_and_single() {
        assert_eq!(lex(">>>").unwrap(), vec![Token::Op(">>>")]);
        assert_eq!(
            lex("A>>>B").unwrap(),
            vec![Token::Arg(0), Token::Op(">>>"), Token::Arg(1)]
        );
        // Sanity: a bare `>>` in isolation still lexes as one token, not
        // dropped or confused with `>>>`'s prefix.
        assert_eq!(lex(">>").unwrap(), vec![Token::Op(">>")]);
    }

    // Task 8a. `:` (the ternary's else-marker, `refs/postfix.c:161`) is a
    // proper prefix of `:=` (the store operator, `:162`), so `OPS`'s
    // longest-match-first ordering is load-bearing here in exactly the way it
    // is for `>>>`/`>>`. Base resolves the same ambiguity the same way and for
    // the same reason: `get_element` (`refs/postfix.c:205-214`) walks
    // `operators[]` BACKWARDS from the last row, and the table is sorted
    // ASCII-ascending, so row 162 (`:=`) is tested before row 161 (`:`).
    //
    // Failing output if `":"` preceded `":="` in `OPS`: `"A:=1"` would
    // tokenize as `[Arg(0), Op(":"), Num(1.0)]` - `compile` would then reject
    // it as `BadConditional` (a `:` with no `?`), not compile a store.
    #[test]
    fn store_operator_beats_the_ternary_colon() {
        assert_eq!(
            lex("A:=1").unwrap(),
            vec![Token::Arg(0), Token::Op(":="), Token::Num(1.0)]
        );
        // A bare `:` is still a `:` - the longer match must not swallow it.
        assert_eq!(
            lex("A?B:C").unwrap(),
            vec![
                Token::Arg(0),
                Token::Op("?"),
                Token::Arg(1),
                Token::Op(":"),
                Token::Arg(2),
            ]
        );
        // The genuinely ambiguous-looking shape. Base lexes this as `:=`
        // (backwards table scan, above), so this crate must too; `compile`
        // then rejects the whole expression as an unbalanced conditional
        // (the `?` never gets its `:`) - see parse.rs's
        // `store_swallows_the_colon_of_a_ternary_leaving_it_unbalanced`.
        assert_eq!(
            lex("A?B:=C").unwrap(),
            vec![
                Token::Arg(0),
                Token::Op("?"),
                Token::Arg(1),
                Token::Op(":="),
                Token::Arg(2),
            ]
        );
    }

    // `;` (`refs/postfix.c:163`) is a single-character operator with no
    // prefix relationship to anything else; this just pins that it lexes at
    // all rather than falling through to `CalcError::BadChar`.
    #[test]
    fn expression_terminator_lexes_as_an_operator() {
        assert_eq!(
            lex("A;B").unwrap(),
            vec![Token::Arg(0), Token::Op(";"), Token::Arg(1)]
        );
    }
}
