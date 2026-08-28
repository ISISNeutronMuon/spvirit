//! Parser for the classic EPICS Access Security File (ACF) format, the
//! subset p4p's `pvagw` accepts: `UAG(name){...}`, `HAG(name){...}`, and
//! `ASG(name){ RULE(asl, OP) [{ UAG(a), HAG(b) }] }` (including a `DEFAULT`
//! ASG). Pure parsing only — no I/O, no evaluation (the evaluator combining
//! this with `pvlist` lands in a later task).
//!
//! Op-keyword mapping (pinned): `READ`/`GET` -> [`OpSet::get`],
//! `WRITE`/`PUT` -> [`OpSet::put`], `RPC` -> [`OpSet::rpc`]. `GET` is
//! informational only — ACF never denies reads on its own, that policy
//! lives in the evaluator. Any other op keyword is a hard `Err`.
//!
//! `CALC(...)` and any other RULE modifier/keyword outside this subset are
//! hard errors (fail-closed) rather than silently ignored.
//!
//! Lexical rules: brace-and-paren structured, comma-separated,
//! whitespace-insensitive; `#` starts a line/inline comment to end-of-line;
//! tokens are identifiers, numbers, quoted strings, and the punctuation
//! `(){},`.

use std::collections::HashMap;

/// A named user access group: `UAG(name) { user, user }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uag {
    pub name: String,
    pub users: Vec<String>,
}

/// A named host access group: `HAG(name) { host, host }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hag {
    pub name: String,
    pub hosts: Vec<String>,
}

/// The operations a [`AsgRule`] grants. `get` is informational only: ACF
/// never denies reads by itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpSet {
    pub get: bool,
    pub put: bool,
    pub rpc: bool,
}

/// A single `RULE(asl, OP)` inside an ASG, with its optional membership
/// block. When the trailing `{ UAG(...), HAG(...) }` block is absent,
/// `uags`/`hags` are empty (the rule applies unconditionally).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsgRule {
    pub asl: u32,
    pub ops: OpSet,
    pub uags: Vec<String>,
    pub hags: Vec<String>,
}

/// A parsed ACF document: named UAGs, HAGs, and ASGs (each ASG being an
/// ordered list of [`AsgRule`]s).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Acf {
    pub uags: HashMap<String, Uag>,
    pub hags: HashMap<String, Hag>,
    pub asgs: HashMap<String, Vec<AsgRule>>,
}

// ---------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Ident(String),
    Number(u32),
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => {
                i += 1;
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '{' => {
                tokens.push(Token::LBrace);
                i += 1;
            }
            '}' => {
                tokens.push(Token::RBrace);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '"' => {
                let start = i;
                i += 1;
                let mut s = String::new();
                while i < chars.len() && chars[i] != '"' {
                    s.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(format!("unterminated string starting at position {start}"));
                }
                i += 1; // closing quote
                tokens.push(Token::Ident(s));
            }
            _ => {
                let start = i;
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], '(' | ')' | '{' | '}' | ',' | '#' | '"')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                if word.is_empty() {
                    return Err(format!("unexpected character {c:?} at position {start}"));
                }
                if let Ok(n) = word.parse::<u32>() {
                    tokens.push(Token::Number(n));
                } else {
                    tokens.push(Token::Ident(word));
                }
            }
        }
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------
// Recursive-descent parser
// ---------------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Result<Token, String> {
        let tok = self
            .tokens
            .get(self.pos)
            .cloned()
            .ok_or_else(|| "unexpected end of input".to_string())?;
        self.pos += 1;
        Ok(tok)
    }

    fn expect(&mut self, expected: &Token) -> Result<(), String> {
        let tok = self.next()?;
        if &tok == expected {
            Ok(())
        } else {
            Err(format!("expected {expected:?}, found {tok:?}"))
        }
    }

    fn expect_ident(&mut self) -> Result<String, String> {
        match self.next()? {
            Token::Ident(s) => Ok(s),
            other => Err(format!("expected identifier, found {other:?}")),
        }
    }

    fn expect_number(&mut self) -> Result<u32, String> {
        match self.next()? {
            Token::Number(n) => Ok(n),
            other => Err(format!("expected number, found {other:?}")),
        }
    }

    /// Parses a comma-separated `(a, b, c)` list of identifiers/numbers as
    /// strings (used for UAG/HAG member lists).
    fn parse_paren_ident_list(&mut self) -> Result<Vec<String>, String> {
        self.expect(&Token::LParen)?;
        let mut items = Vec::new();
        loop {
            match self.peek() {
                Some(Token::RParen) => break,
                _ => {
                    let item = match self.next()? {
                        Token::Ident(s) => s,
                        Token::Number(n) => n.to_string(),
                        other => return Err(format!("expected list item, found {other:?}")),
                    };
                    items.push(item);
                }
            }
            match self.peek() {
                Some(Token::Comma) => {
                    self.pos += 1;
                }
                Some(Token::RParen) => break,
                other => return Err(format!("expected ',' or ')', found {other:?}")),
            }
        }
        self.expect(&Token::RParen)?;
        Ok(items)
    }

    /// Parses a brace-delimited comma-separated list of identifiers, e.g.
    /// `{ alice, bob }`, used for UAG/HAG bodies.
    fn parse_brace_ident_list(&mut self) -> Result<Vec<String>, String> {
        self.expect(&Token::LBrace)?;
        let mut items = Vec::new();
        loop {
            match self.peek() {
                Some(Token::RBrace) => break,
                _ => {
                    let item = self.expect_ident()?;
                    items.push(item);
                }
            }
            match self.peek() {
                Some(Token::Comma) => {
                    self.pos += 1;
                }
                Some(Token::RBrace) => break,
                other => return Err(format!("expected ',' or '}}', found {other:?}")),
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(items)
    }

    /// Parses one `RULE(asl, OP [, OP...])` (with `CALC(...)` and other
    /// modifiers hard-rejected) plus its optional membership block.
    fn parse_rule(&mut self) -> Result<AsgRule, String> {
        self.expect(&Token::LParen)?;
        let asl = self.expect_number()?;
        self.expect(&Token::Comma)?;

        let mut ops = OpSet::default();
        loop {
            let kw = self.expect_ident()?;
            match kw.as_str() {
                "READ" | "GET" => ops.get = true,
                "WRITE" | "PUT" => ops.put = true,
                "RPC" => ops.rpc = true,
                "CALC" => {
                    return Err(
                        "RULE contains CALC(...), which is not supported by this parser"
                            .to_string(),
                    );
                }
                other => {
                    return Err(format!("unknown RULE operation or modifier {other:?}"));
                }
            }
            match self.peek() {
                Some(Token::Comma) => {
                    self.pos += 1;
                    // A following CALC(...) argument, or a second op, may
                    // appear; peek ahead to see if it looks like CALC.
                    if matches!(self.peek(), Some(Token::Ident(s)) if s == "CALC") {
                        self.pos += 1; // consume CALC ident
                        return Err(
                            "RULE contains CALC(...), which is not supported by this parser"
                                .to_string(),
                        );
                    }
                }
                _ => break,
            }
        }
        self.expect(&Token::RParen)?;

        let mut uags = Vec::new();
        let mut hags = Vec::new();
        if matches!(self.peek(), Some(Token::LBrace)) {
            self.expect(&Token::LBrace)?;
            loop {
                match self.peek() {
                    Some(Token::RBrace) => break,
                    Some(Token::Ident(kw)) if kw == "UAG" => {
                        self.pos += 1;
                        let names = self.parse_paren_ident_list()?;
                        uags.extend(names);
                    }
                    Some(Token::Ident(kw)) if kw == "HAG" => {
                        self.pos += 1;
                        let names = self.parse_paren_ident_list()?;
                        hags.extend(names);
                    }
                    other => {
                        return Err(format!(
                            "expected UAG(...), HAG(...), or '}}' in RULE membership block, found {other:?}"
                        ));
                    }
                }
                match self.peek() {
                    Some(Token::Comma) => {
                        self.pos += 1;
                    }
                    Some(Token::RBrace) => break,
                    other => return Err(format!("expected ',' or '}}', found {other:?}")),
                }
            }
            self.expect(&Token::RBrace)?;
        }

        Ok(AsgRule {
            asl,
            ops,
            uags,
            hags,
        })
    }

    fn parse_asg_body(&mut self) -> Result<Vec<AsgRule>, String> {
        self.expect(&Token::LBrace)?;
        let mut rules = Vec::new();
        loop {
            match self.peek() {
                Some(Token::RBrace) => break,
                Some(Token::Ident(kw)) if kw == "RULE" => {
                    self.pos += 1;
                    rules.push(self.parse_rule()?);
                }
                other => {
                    return Err(format!(
                        "expected RULE(...) or '}}' inside ASG body, found {other:?}"
                    ));
                }
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(rules)
    }

    fn parse_document(&mut self) -> Result<Acf, String> {
        let mut acf = Acf::default();
        while self.peek().is_some() {
            let kw = self.expect_ident()?;
            match kw.as_str() {
                "UAG" => {
                    self.expect(&Token::LParen)?;
                    let name = self.expect_ident()?;
                    self.expect(&Token::RParen)?;
                    let users = self.parse_brace_ident_list()?;
                    acf.uags.insert(name.clone(), Uag { name, users });
                }
                "HAG" => {
                    self.expect(&Token::LParen)?;
                    let name = self.expect_ident()?;
                    self.expect(&Token::RParen)?;
                    let hosts = self.parse_brace_ident_list()?;
                    acf.hags.insert(name.clone(), Hag { name, hosts });
                }
                "ASG" => {
                    self.expect(&Token::LParen)?;
                    let name = self.expect_ident()?;
                    self.expect(&Token::RParen)?;
                    let rules = self.parse_asg_body()?;
                    acf.asgs.insert(name, rules);
                }
                other => return Err(format!("unexpected top-level keyword {other:?}")),
            }
        }
        Ok(acf)
    }
}

/// Parses an ACF document (the `UAG`/`HAG`/`ASG`+`RULE` subset described in
/// the module docs) into an [`Acf`].
///
/// Returns `Err(String)` on any construct outside the supported subset,
/// including `CALC(...)` and unknown RULE operations/modifiers.
pub fn parse_acf(text: &str) -> Result<Acf, String> {
    let tokens = tokenize(text)?;
    let mut parser = Parser { tokens, pos: 0 };
    parser.parse_document()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uag_hag_asg() {
        let acf = parse_acf(
            r#"
            UAG(ops) { alice, bob }
            HAG(control) { 10.0.0.1, ctrl.lab }
            ASG(DEFAULT) { RULE(0, READ) }
            ASG(rw) {
                RULE(1, WRITE) { UAG(ops), HAG(control) }
                RULE(0, READ)
            }
        "#,
        )
        .unwrap();
        assert_eq!(acf.uags["ops"].users, vec!["alice", "bob"]);
        assert_eq!(acf.hags["control"].hosts, vec!["10.0.0.1", "ctrl.lab"]);
        let rw = &acf.asgs["rw"];
        assert_eq!(rw.len(), 2);
        assert_eq!(rw[0].asl, 1);
        assert!(rw[0].ops.put);
        assert_eq!(rw[0].uags, vec!["ops"]);
        assert_eq!(rw[0].hags, vec!["control"]);
    }

    #[test]
    fn calc_rule_is_rejected() {
        let e = parse_acf("ASG(x) { RULE(1, WRITE, CALC(\"A>0\") ) }").unwrap_err();
        assert!(e.to_lowercase().contains("calc"));
    }

    #[test]
    fn unknown_op_is_rejected() {
        assert!(parse_acf("ASG(x) { RULE(1, TELEPORT) }").is_err());
    }

    #[test]
    fn rule_without_membership_block_has_empty_uags_hags() {
        let acf = parse_acf("ASG(DEFAULT) { RULE(0, READ) }").unwrap();
        let rules = &acf.asgs["DEFAULT"];
        assert_eq!(rules.len(), 1);
        assert!(rules[0].uags.is_empty());
        assert!(rules[0].hags.is_empty());
        assert!(rules[0].ops.get);
        assert!(!rules[0].ops.put);
        assert!(!rules[0].ops.rpc);
    }

    #[test]
    fn get_and_put_are_synonyms_of_read_and_write() {
        let acf = parse_acf("ASG(x) { RULE(1, GET) RULE(1, PUT) }").unwrap();
        let rules = &acf.asgs["x"];
        assert!(rules[0].ops.get);
        assert!(!rules[0].ops.put);
        assert!(!rules[1].ops.get);
        assert!(rules[1].ops.put);
    }

    #[test]
    fn rpc_op_is_supported() {
        let acf = parse_acf("ASG(x) { RULE(1, RPC) }").unwrap();
        assert!(acf.asgs["x"][0].ops.rpc);
    }

    #[test]
    fn comments_are_skipped() {
        let acf = parse_acf(
            "# leading comment\nUAG(ops) { alice } # trailing comment\n# another\nHAG(h) { host1 }",
        )
        .unwrap();
        assert_eq!(acf.uags["ops"].users, vec!["alice"]);
        assert_eq!(acf.hags["h"].hosts, vec!["host1"]);
    }

    #[test]
    fn multiple_rules_in_one_asg_are_all_captured() {
        let acf = parse_acf(
            "ASG(multi) { RULE(0, READ) RULE(1, WRITE) { UAG(ops) } RULE(2, RPC) { HAG(h) } }",
        )
        .unwrap();
        let rules = &acf.asgs["multi"];
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].asl, 0);
        assert_eq!(rules[1].asl, 1);
        assert_eq!(rules[1].uags, vec!["ops"]);
        assert_eq!(rules[2].asl, 2);
        assert_eq!(rules[2].hags, vec!["h"]);
    }

    #[test]
    fn empty_uag_hag_bodies_parse_to_empty_vecs() {
        let acf = parse_acf("UAG(empty) {}\nHAG(empty) {}").unwrap();
        assert!(acf.uags["empty"].users.is_empty());
        assert!(acf.hags["empty"].hosts.is_empty());
    }

    #[test]
    fn multi_membership_entries_in_one_rule_are_combined() {
        let acf = parse_acf(
            "ASG(x) { RULE(1, WRITE) { UAG(a), UAG(b), HAG(h1), HAG(h2) } }",
        )
        .unwrap();
        let rule = &acf.asgs["x"][0];
        assert_eq!(rule.uags, vec!["a", "b"]);
        assert_eq!(rule.hags, vec!["h1", "h2"]);
    }

    #[test]
    fn unterminated_string_is_error() {
        assert!(parse_acf("UAG(x) { \"unterminated }").is_err());
    }

    #[test]
    fn unknown_top_level_keyword_is_error() {
        assert!(parse_acf("FOO(x) { }").is_err());
    }
}
