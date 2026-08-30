//! Access control for the gateway: p4p `pvlist` allow/deny/alias rules
//! (this module) and `.acf` ASG/ASL definitions (Task 8, `acf` module).
//!
//! Pure parsing + matching logic only — no I/O, no enforcement. The
//! evaluator (`decide`) lands in a later task once both `pvlist` and `acf`
//! are available.

pub mod acf;
pub mod pvlist;

use self::acf::Acf;
use self::pvlist::{Pvlist, PvlistAction};

/// The three request kinds `decide` evaluates. `Monitor` requests are
/// treated identically to `Get` by callers (both are reads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Get,
    Put,
    Rpc,
}

/// The caller's identity, as known to the gateway at decision time.
///
/// `host` is an exact string (the peer IP or resolved name) — see the
/// host-matching note on [`AccessControl::decide`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub host: Option<String>,
    pub user: Option<String>,
}

/// The outcome of [`AccessControl::decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// The request survived under a rewritten PV name (a pvlist `ALIAS`
    /// rule matched); the caller should proxy the aliased name.
    AllowAliased(String),
    Deny,
}

/// Expands p4p-style `\1`..`\9` backreferences in `template` using `caps`.
/// `\0` and any out-of-range group substitute the empty string rather than
/// panicking. Any other character (including a bare trailing `\`) passes
/// through unchanged.
fn expand_template(template: &str, caps: &regex::Captures) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek().is_some_and(|d| d.is_ascii_digit()) {
            let d = chars.next().unwrap();
            let idx = d.to_digit(10).unwrap() as usize;
            if idx >= 1 && let Some(m) = caps.get(idx) {
                out.push_str(m.as_str());
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The bound ASG/ASL threaded from a surviving pvlist match (or the
/// implicit default) into the ACF step.
struct Binding {
    asg: String,
    asl: u32,
}

/// Combines the p4p `pvlist` and `.acf` access-control inputs with a fixed
/// precedence: **readOnly > pvlist > ACF**. Pure evaluation only — no I/O,
/// no enforcement; callers (Task 11) apply the returned [`Decision`].
pub struct AccessControl {
    read_only: bool,
    pvlist: Option<Pvlist>,
    acf: Option<Acf>,
}

impl AccessControl {
    pub fn new(read_only: bool, pvlist: Option<Pvlist>, acf: Option<Acf>) -> Self {
        Self {
            read_only,
            pvlist,
            acf,
        }
    }

    /// Decides whether `op` on `pv` by `id` is allowed, per the fixed
    /// precedence readOnly > pvlist > ACF (see module docs).
    ///
    /// Host matching (HAG membership, and pvlist `FROM`) is **exact string
    /// match** against `id.host` — the IP or resolved name as provided by
    /// the caller. No CIDR, no DNS resolution. Task 11 supplies the peer's
    /// IP string as `id.host`.
    pub fn decide(&self, op: Op, pv: &str, id: &Identity) -> Decision {
        self.decide_inner(op, pv, id, false)
    }

    /// Decides `op` on a **gateway-local** PV — one this process serves
    /// itself (the status PVs), not a name proxied from upstream.
    ///
    /// Identical to [`decide`](Self::decide) except that a `pvlist` which
    /// does not match `pv` leaves it at the `DEFAULT` binding instead of
    /// denying it. `readOnly`, an explicit pvlist `DENY`, and ACF all still
    /// bind exactly as they do for proxied names.
    ///
    /// A `pvlist` selects which *upstream* names the gateway proxies, so an
    /// ordinary one lists the data PVs and never mentions the status prefix.
    /// Routing status PVs through the fail-closed [`decide`](Self::decide)
    /// made every such deployment deny all of them: `StatusSource::claim`
    /// read the `Deny` as "not mine", search answered `found=false`, and
    /// clients timed out waiting for a search response while `pvlist` still
    /// listed the names.
    pub fn decide_local(&self, op: Op, pv: &str, id: &Identity) -> Decision {
        self.decide_inner(op, pv, id, true)
    }

    /// Shared body of [`decide`](Self::decide) and
    /// [`decide_local`](Self::decide_local). `unmatched_is_default` picks the
    /// treatment of a configured `pvlist` with no rule for `pv`: fail closed
    /// (proxied names) or fall through to the `DEFAULT` binding (local ones).
    fn decide_inner(
        &self,
        op: Op,
        pv: &str,
        id: &Identity,
        unmatched_is_default: bool,
    ) -> Decision {
        // Step 1: readOnly overrides everything for writes/RPCs; Get is
        // unaffected.
        if self.read_only && matches!(op, Op::Put | Op::Rpc) {
            return Decision::Deny;
        }

        // Step 2: pvlist first-match, host-aware (from_hosts filtering is
        // not handled by Pvlist::match_first, so scan manually here).
        let (survived_name, binding) = match &self.pvlist {
            None => (pv.to_string(), Binding {
                asg: "DEFAULT".to_string(),
                asl: 0,
            }),
            Some(pvlist) => {
                let matched = pvlist.rules.iter().find(|r| {
                    r.pattern.is_match(pv)
                        && (r.from_hosts.is_empty()
                            || id
                                .host
                                .as_deref()
                                .is_some_and(|h| r.from_hosts.iter().any(|fh| fh == h)))
                });
                match matched {
                    None if unmatched_is_default => (pv.to_string(), Binding {
                        asg: "DEFAULT".to_string(),
                        asl: 0,
                    }),
                    None => return Decision::Deny,
                    Some(rule) => match &rule.action {
                        PvlistAction::Deny => return Decision::Deny,
                        PvlistAction::Allow => (
                            pv.to_string(),
                            Binding {
                                asg: rule.asg.clone(),
                                asl: rule.asl,
                            },
                        ),
                        PvlistAction::Alias(template) => {
                            let name = match rule.pattern.captures(pv) {
                                Some(caps) => expand_template(template, &caps),
                                None => pv.to_string(),
                            };
                            (
                                name,
                                Binding {
                                    asg: rule.asg.clone(),
                                    asl: rule.asl,
                                },
                            )
                        }
                    },
                }
            }
        };

        let is_aliased = survived_name != pv;
        let allow_decision = || {
            if is_aliased {
                Decision::AllowAliased(survived_name.clone())
            } else {
                Decision::Allow
            }
        };

        // Step 4: Get (and Monitor) are never denied by ACF — only a
        // pvlist DENY can deny a read, and that already returned above.
        if matches!(op, Op::Get) {
            return allow_decision();
        }

        // Step 3: ACF evaluation for Put/Rpc that survived pvlist as
        // ALLOW/ALIAS.
        match &self.acf {
            None => allow_decision(),
            Some(acf) => {
                if self.acf_grants(acf, &binding, op, id) {
                    allow_decision()
                } else {
                    Decision::Deny
                }
            }
        }
    }

    /// Whether some rule in the ASG named `binding.asg` grants `op` to
    /// `id`, given the bound ASL. Fail-closed: an absent ASG, or a
    /// referenced UAG/HAG name absent from the ACF's maps, grants nothing.
    fn acf_grants(&self, acf: &Acf, binding: &Binding, op: Op, id: &Identity) -> bool {
        let Some(rules) = acf.asgs.get(&binding.asg) else {
            return false;
        };

        rules.iter().any(|rule| {
            if rule.asl > binding.asl {
                return false;
            }
            let op_bit = match op {
                Op::Get => rule.ops.get,
                Op::Put => rule.ops.put,
                Op::Rpc => rule.ops.rpc,
            };
            if !op_bit {
                return false;
            }
            let uag_ok = rule.uags.is_empty()
                || id.user.as_deref().is_some_and(|u| {
                    rule.uags
                        .iter()
                        .any(|name| acf.uags.get(name).is_some_and(|g| g.users.iter().any(|x| x == u)))
                });
            if !uag_ok {
                return false;
            }
            rule.hags.is_empty()
                || id.host.as_deref().is_some_and(|h| {
                    rule.hags
                        .iter()
                        .any(|name| acf.hags.get(name).is_some_and(|g| g.hosts.iter().any(|x| x == h)))
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use self::acf::parse_acf;
    use self::pvlist::parse_pvlist;

    fn ac(read_only: bool, pvlist: &str, acf: &str) -> AccessControl {
        let pl = if pvlist.is_empty() {
            None
        } else {
            Some(parse_pvlist(pvlist).unwrap())
        };
        let af = if acf.is_empty() {
            None
        } else {
            Some(parse_acf(acf).unwrap())
        };
        AccessControl::new(read_only, pl, af)
    }
    fn id(user: Option<&str>, host: Option<&str>) -> Identity {
        Identity {
            user: user.map(str::to_string),
            host: host.map(str::to_string),
        }
    }

    #[test]
    fn default_permits_everything() {
        let a = ac(false, "", "");
        assert!(matches!(a.decide(Op::Put, "X", &id(None, None)), Decision::Allow));
        assert!(matches!(a.decide(Op::Get, "X", &id(None, None)), Decision::Allow));
    }

    #[test]
    fn read_only_denies_writes_not_reads() {
        let a = ac(true, "", "");
        assert!(matches!(a.decide(Op::Put, "X", &id(None, None)), Decision::Deny));
        assert!(matches!(a.decide(Op::Get, "X", &id(None, None)), Decision::Allow));
    }

    #[test]
    fn pvlist_deny_hides_reads_and_writes() {
        let a = ac(false, "S:.* DENY\n.* ALLOW", "");
        assert!(matches!(a.decide(Op::Get, "S:x", &id(None, None)), Decision::Deny));
        assert!(matches!(a.decide(Op::Put, "S:x", &id(None, None)), Decision::Deny));
        assert!(matches!(a.decide(Op::Get, "T", &id(None, None)), Decision::Allow));
    }

    #[test]
    fn alias_rewrites_name() {
        let a = ac(false, "PUB:(.*) ALIAS INT:\\1", "");
        match a.decide(Op::Get, "PUB:temp", &id(None, None)) {
            Decision::AllowAliased(n) => assert_eq!(n, "INT:temp"),
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn acf_denies_write_without_grant_but_allows_read() {
        let a = ac(false, ".* ALLOW RW 1", "ASG(RW) { RULE(0, READ) }");
        // Only a READ rule at asl 0; a PUT needs a WRITE grant -> deny.
        assert!(matches!(
            a.decide(Op::Put, "X", &id(Some("alice"), None)),
            Decision::Deny
        ));
        // Reads never denied by ACF.
        assert!(matches!(
            a.decide(Op::Get, "X", &id(Some("alice"), None)),
            Decision::Allow
        ));
    }

    #[test]
    fn acf_grants_write_by_uag_and_hag() {
        let acf = "UAG(ops){alice}\nHAG(ctl){10.0.0.1}\nASG(RW){ RULE(1, WRITE){ UAG(ops), HAG(ctl) } }";
        let a = ac(false, ".* ALLOW RW 1", acf);
        assert!(matches!(
            a.decide(Op::Put, "X", &id(Some("alice"), Some("10.0.0.1"))),
            Decision::Allow
        ));
        assert!(matches!(
            a.decide(Op::Put, "X", &id(Some("bob"), Some("10.0.0.1"))),
            Decision::Deny
        ));
        assert!(matches!(
            a.decide(Op::Put, "X", &id(Some("alice"), Some("10.9.9.9"))),
            Decision::Deny
        ));
    }

    #[test]
    fn unspecified_asg_binds_default_and_evaluates() {
        // pvlist ALLOW with no ASG -> DEFAULT; DEFAULT grants WRITE at asl 0.
        let a = ac(false, ".* ALLOW", "ASG(DEFAULT){ RULE(0, WRITE) }");
        assert!(matches!(a.decide(Op::Put, "X", &id(None, None)), Decision::Allow));
    }

    #[test]
    fn default_asg_absent_from_acf_denies_writes() {
        // ACF present but no DEFAULT ASG; a plain ALLOW binds DEFAULT -> absent -> deny.
        let a = ac(false, ".* ALLOW", "ASG(other){ RULE(0, WRITE) }");
        assert!(matches!(a.decide(Op::Put, "X", &id(None, None)), Decision::Deny));
        assert!(matches!(a.decide(Op::Get, "X", &id(None, None)), Decision::Allow));
    }

    #[test]
    fn from_host_filters_which_pvlist_rule_matches() {
        let a = ac(
            false,
            "X:.* DENY FROM 10.0.0.9\nX:.* ALLOW",
            "",
        );
        // Host in the FROM list: the DENY rule matches.
        assert!(matches!(
            a.decide(Op::Get, "X:a", &id(None, Some("10.0.0.9"))),
            Decision::Deny
        ));
        // Different host: DENY rule doesn't match (from_hosts filters it
        // out), falls through to the catch-all ALLOW.
        assert!(matches!(
            a.decide(Op::Get, "X:a", &id(None, Some("10.0.0.1"))),
            Decision::Allow
        ));
        // No host at all: DENY's FROM can't match either.
        assert!(matches!(
            a.decide(Op::Get, "X:a", &id(None, None)),
            Decision::Allow
        ));
    }

    #[test]
    fn rpc_follows_same_acf_path_as_put() {
        let a = ac(false, ".* ALLOW RW 1", "ASG(RW) { RULE(1, RPC) }");
        assert!(matches!(
            a.decide(Op::Rpc, "X", &id(None, None)),
            Decision::Allow
        ));
        assert!(matches!(
            a.decide(Op::Put, "X", &id(None, None)),
            Decision::Deny
        ));
    }

    #[test]
    fn referenced_uag_absent_from_acf_denies_without_panicking() {
        // RULE references UAG(ghost), which is never defined.
        let a = ac(
            false,
            ".* ALLOW RW 1",
            "ASG(RW) { RULE(1, WRITE) { UAG(ghost) } }",
        );
        assert!(matches!(
            a.decide(Op::Put, "X", &id(Some("alice"), None)),
            Decision::Deny
        ));
    }
}
