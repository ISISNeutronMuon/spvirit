//! Bash-style brace expansion for bulk PV names.

/// Max PVs a single pattern may expand to (safety against typos).
pub const EXPAND_CAP: usize = 1000;

/// Expand bash-style braces. Supports `{m..n}`, `{m..n..step}`, zero-padding
/// inferred from bound widths, and `{a,b,c}` lists. Multiple braces form a
/// cartesian product. Errors on malformed braces or on exceeding `EXPAND_CAP`.
pub fn expand_pattern(s: &str) -> Result<Vec<String>, String> {
    // Find the first top-level `{...}` group.
    let open = match s.find('{') {
        Some(i) => i,
        None => {
            if s.contains('}') {
                return Err(format!("unmatched '}}' in {s:?}"));
            }
            return Ok(vec![s.to_string()]);
        }
    };
    let close = s[open..]
        .find('}')
        .map(|rel| open + rel)
        .ok_or_else(|| format!("unclosed '{{' in {s:?}"))?;

    let prefix = &s[..open];
    let inner = &s[open + 1..close];
    let suffix = &s[close + 1..];

    let alternatives = expand_group(inner)?;

    // Expand the remainder recursively, then cartesian-combine.
    let tails = expand_pattern(suffix)?;
    let mut out = Vec::new();
    for alt in &alternatives {
        for tail in &tails {
            out.push(format!("{prefix}{alt}{tail}"));
            if out.len() > EXPAND_CAP {
                return Err(format!("pattern {s:?} would create over {EXPAND_CAP} PVs"));
            }
        }
    }
    Ok(out)
}

/// Expand the text inside one `{...}` into its alternatives.
fn expand_group(inner: &str) -> Result<Vec<String>, String> {
    // Range form: m..n or m..n..step
    if let Some((lo, rest)) = inner.split_once("..") {
        let (hi, step) = match rest.split_once("..") {
            Some((h, s)) => (h, s.parse::<i64>().map_err(|_| format!("bad step in {{{inner}}}"))?),
            None => (rest, 1),
        };
        if step <= 0 {
            return Err(format!("step must be positive in {{{inner}}}"));
        }
        let start: i64 = lo.parse().map_err(|_| format!("bad range start in {{{inner}}}"))?;
        let end: i64 = hi.parse().map_err(|_| format!("bad range end in {{{inner}}}"))?;
        let width = lo.len().max(hi.len());
        let pad = (lo.starts_with('0') && lo.len() > 1) || (hi.starts_with('0') && hi.len() > 1);

        let mut vals = Vec::new();
        let mut i = start;
        while (start <= end && i <= end) || (start > end && i >= end) {
            let token = if pad {
                format!("{:0width$}", i, width = width)
            } else {
                i.to_string()
            };
            vals.push(token);
            if vals.len() > EXPAND_CAP {
                return Err(format!("range {{{inner}}} would create over {EXPAND_CAP} PVs"));
            }
            i += if start <= end { step } else { -step };
        }
        return Ok(vals);
    }

    // List form: a,b,c
    if inner.contains(',') {
        return Ok(inner.split(',').map(|t| t.to_string()).collect());
    }

    Err(format!("malformed brace {{{inner}}} (want m..n or a,b,c)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_braces_is_identity() {
        assert_eq!(expand_pattern("SIM:X").unwrap(), vec!["SIM:X"]);
    }

    #[test]
    fn numeric_range_ascending_and_descending() {
        assert_eq!(expand_pattern("N{1..3}").unwrap(), vec!["N1", "N2", "N3"]);
        assert_eq!(expand_pattern("N{3..1}").unwrap(), vec!["N3", "N2", "N1"]);
    }

    #[test]
    fn stepped_range() {
        assert_eq!(expand_pattern("N{0..10..5}").unwrap(), vec!["N0", "N5", "N10"]);
    }

    #[test]
    fn zero_padded_range() {
        assert_eq!(
            expand_pattern("BPM{08..11}").unwrap(),
            vec!["BPM08", "BPM09", "BPM10", "BPM11"]
        );
    }

    #[test]
    fn literal_list() {
        assert_eq!(expand_pattern("P:{A,B,C}").unwrap(), vec!["P:A", "P:B", "P:C"]);
    }

    #[test]
    fn cartesian_product() {
        assert_eq!(
            expand_pattern("S{1..2}:{A,B}").unwrap(),
            vec!["S1:A", "S1:B", "S2:A", "S2:B"]
        );
    }

    #[test]
    fn cap_and_malformed() {
        assert!(expand_pattern("X{1..100000}").is_err(), "over cap");
        assert!(expand_pattern("X{1..}").is_err(), "malformed range");
        assert!(expand_pattern("X{").is_err(), "unclosed brace");
    }
}
