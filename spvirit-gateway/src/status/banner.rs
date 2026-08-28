//! Startup banner listing every PV [`super::StatusSource`] serves under a
//! given prefix — one `info!` line per PV, so an operator can see the full
//! served set at a glance without a `pvlist` round-trip.

use super::served_suffixes;

/// Returns `"Status PV: <prefix><suffix>"` for every PV `StatusSource`
/// serves under `prefix`, in the same stable order `StatusSource::names()`
/// uses. Both draw from the same [`served_suffixes`] iterator so the served
/// set and the banner cannot drift apart.
pub fn status_pv_lines(prefix: &str) -> Vec<String> {
    served_suffixes().map(|s| format!("Status PV: {prefix}{s}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_prefixed_and_stable_in_count() {
        let lines = status_pv_lines("X:");
        assert_eq!(lines.len(), 15);
        assert_eq!(lines[0], "Status PV: X:clients");
        assert!(lines.contains(&"Status PV: X:asTest".to_string()));
    }
}
