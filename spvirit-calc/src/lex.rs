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
        let value = u64::from_str_radix(&rest[..len], 16).ok()?;
        return Some((value as f64, 2 + len));
    }

    // Longest prefix that parses as a float. Bounded by the literal's length,
    // so the quadratic worst case is irrelevant in practice.
    let max = s
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit() || *c == '.' || *c == 'e' || *c == 'E' || *c == '+' || *c == '-')
        .map(|(idx, c)| idx + c.len_utf8())
        .last()?;
    for end in (1..=max).rev() {
        if let Ok(v) = s[..end].parse::<f64>() {
            return Some((v, end));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
