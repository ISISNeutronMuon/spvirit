//! Parser for the p4p `pvlist` access-control format: an ordered list of
//! `PATTERN ACTION [args]` rules (first match wins). Pure parsing + matching
//! only — no I/O, no substitution, no enforcement (those land in later
//! tasks).
//!
//! Grammar (p4p spec subset):
//! ```text
//! PATTERN ALLOW [ASG [ASL]] [FROM h1,h2]
//! PATTERN DENY [FROM h1,h2]
//! PATTERN ALIAS TEMPLATE [ASG [ASL]] [FROM h1,h2]
//! ```
//! Lines are trimmed; blank lines and lines starting with `#` are skipped.
//! `PATTERN` is compiled as `^(?:PATTERN)$`. `ASG` defaults to `DEFAULT`,
//! `ASL` to `0`. The `ALIAS` template is stored verbatim (including any
//! `\1`..`\9` capture-group references) — substitution happens at claim
//! time. Any unrecognized token, malformed line, or regex compile failure
//! is a fail-closed `Err` with a line-numbered message.

use regex::Regex;

/// What a matching [`PvlistRule`] does with a claim.
#[derive(Debug, Clone)]
pub enum PvlistAction {
    Allow,
    Deny,
    /// Alias rewrite template, stored verbatim (e.g. `INT:\1`).
    Alias(String),
}

/// A single ordered pvlist rule: an anchored pattern plus its action and
/// optional ASG/ASL/FROM qualifiers.
pub struct PvlistRule {
    pub pattern: Regex,
    pub action: PvlistAction,
    pub asg: String,
    pub asl: u32,
    pub from_hosts: Vec<String>,
}

/// An ordered set of [`PvlistRule`]s; [`Pvlist::match_first`] implements
/// first-match-wins evaluation.
pub struct Pvlist {
    pub rules: Vec<PvlistRule>,
}

impl Pvlist {
    /// Returns the first rule whose pattern matches `pv`, if any.
    pub fn match_first(&self, pv: &str) -> Option<&PvlistRule> {
        self.rules.iter().find(|r| r.pattern.is_match(pv))
    }
}

/// Parses a p4p `pvlist` document into an ordered [`Pvlist`].
///
/// Returns `Err(String)` with a 1-based, line-numbered message on the first
/// unrecognized token, malformed line, or regex compile failure.
pub fn parse_pvlist(text: &str) -> Result<Pvlist, String> {
    let mut rules = Vec::new();

    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut tokens = line.split_whitespace();

        let pattern_src = tokens
            .next()
            .ok_or_else(|| format!("line {line_no}: missing pattern"))?;
        let anchored = format!("^(?:{pattern_src})$");
        let pattern = Regex::new(&anchored)
            .map_err(|e| format!("line {line_no}: invalid pattern {pattern_src:?}: {e}"))?;

        let action_kw = tokens
            .next()
            .ok_or_else(|| format!("line {line_no}: missing action"))?;

        let action = match action_kw {
            "ALLOW" => PvlistAction::Allow,
            "DENY" => PvlistAction::Deny,
            "ALIAS" => {
                let template = tokens
                    .next()
                    .ok_or_else(|| format!("line {line_no}: ALIAS missing template"))?;
                PvlistAction::Alias(template.to_string())
            }
            other => return Err(format!("line {line_no}: unknown action {other:?}")),
        };

        let mut asg = "DEFAULT".to_string();
        let mut asl: u32 = 0;
        let mut from_hosts = Vec::new();

        // Remaining tokens: optional ASG [ASL] (ALLOW/ALIAS only), then
        // optional FROM h1,h2 — in that order.
        let mut remaining: Vec<&str> = tokens.collect();

        // Pull off a trailing "FROM host,host" pair, if present, from
        // wherever it starts in the remaining tokens.
        if let Some(from_pos) = remaining.iter().position(|t| *t == "FROM") {
            let hosts_tok = remaining
                .get(from_pos + 1)
                .ok_or_else(|| format!("line {line_no}: FROM missing host list"))?;
            from_hosts = hosts_tok.split(',').map(|h| h.to_string()).collect();
            if remaining.len() != from_pos + 2 {
                return Err(format!(
                    "line {line_no}: unexpected tokens after FROM host list"
                ));
            }
            remaining.truncate(from_pos);
        }

        if !remaining.is_empty() {
            if matches!(action, PvlistAction::Deny) {
                return Err(format!(
                    "line {line_no}: DENY does not accept ASG/ASL arguments"
                ));
            }
            asg = remaining[0].to_string();
            if remaining.len() > 1 {
                asl = remaining[1]
                    .parse()
                    .map_err(|_| format!("line {line_no}: invalid ASL {:?}", remaining[1]))?;
            }
            if remaining.len() > 2 {
                return Err(format!("line {line_no}: unexpected trailing tokens"));
            }
        }

        rules.push(PvlistRule {
            pattern,
            action,
            asg,
            asl,
            from_hosts,
        });
    }

    Ok(Pvlist { rules })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ordered_allow_deny_alias() {
        let p = parse_pvlist(
            "
        # comment
        .*  ALLOW
        SECRET:.*  DENY
        PUB:(.*)  ALIAS  INT:\\1
    ",
        )
        .unwrap();
        assert_eq!(p.rules.len(), 3);
        assert!(matches!(p.rules[0].action, PvlistAction::Allow));
        assert!(matches!(p.rules[1].action, PvlistAction::Deny));
        match &p.rules[2].action {
            PvlistAction::Alias(t) => assert_eq!(t, "INT:\\1"),
            _ => panic!(),
        }
    }

    #[test]
    fn allow_carries_asg_and_asl_defaults_and_overrides() {
        let p = parse_pvlist("A:.* ALLOW\nB:.* ALLOW RWGROUP 1").unwrap();
        assert_eq!(p.rules[0].asg, "DEFAULT");
        assert_eq!(p.rules[0].asl, 0);
        assert_eq!(p.rules[1].asg, "RWGROUP");
        assert_eq!(p.rules[1].asl, 1);
    }

    #[test]
    fn from_host_qualifier_parses() {
        let p = parse_pvlist("X:.* DENY FROM 10.0.0.1,10.0.0.2").unwrap();
        assert_eq!(p.rules[0].from_hosts, vec!["10.0.0.1", "10.0.0.2"]);
    }

    #[test]
    fn match_first_wins() {
        let p = parse_pvlist("SECRET:.* DENY\n.* ALLOW").unwrap();
        assert!(matches!(
            p.match_first("SECRET:X").unwrap().action,
            PvlistAction::Deny
        ));
        assert!(matches!(
            p.match_first("OTHER").unwrap().action,
            PvlistAction::Allow
        ));
    }

    #[test]
    fn unknown_action_is_error() {
        assert!(parse_pvlist("X:.* FROBNICATE").is_err());
    }

    #[test]
    fn bad_regex_is_error() {
        assert!(parse_pvlist("X:[  ALLOW").is_err());
    }
}
