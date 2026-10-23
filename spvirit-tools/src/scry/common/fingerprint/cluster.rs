//! Single-linkage clustering of fingerprints, for the cluster view of
//! spec §8.
//!
//! Single linkage is the right choice here: a source drifts gradually across
//! a window, so a chain of near-identical fingerprints is one source, and
//! requiring every pair to be close (complete linkage) would split it.
//!
//! Clustering is O(n^2) in the number of fingerprints, so the input is capped
//! — and the number dropped is returned rather than swallowed, because a
//! truncated cluster view that looks complete is worse than no cluster view
//! (spec §4.3, §6.3).
//!
//! Everything here is ordered by [`FingerprintKey`], never by input or hash
//! order: callers hand us the contents of a `HashMap`, and a cluster view
//! that reshuffled between ticks would be unreadable.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use super::similarity::{SimilarityWeights, fingerprint_distance};
use super::types::{FingerprintKey, SourceFingerprint};

/// A group of channels whose fingerprints are mutually reachable.
#[derive(Debug, Clone, PartialEq)]
pub struct FingerprintCluster {
    /// Stable index, assigned in sorted member order.
    pub id: usize,
    /// Members in ascending key order.
    pub members: Vec<FingerprintKey>,
    /// Every distinct server the cluster spans, ascending. More than one is
    /// the finding worth looking at: it means channels from different
    /// addresses look like the same producing process.
    pub servers: Vec<SocketAddr>,
    /// The cluster's lowest-keyed member, used as its display representative.
    pub exemplar: SourceFingerprint,
}

/// Group fingerprints by similarity.
///
/// Returns the clusters and the number of inputs discarded by `max_inputs`.
/// Inputs are considered in sorted key order, so the truncation keeps the
/// same fingerprints whatever order the caller supplied them in.
///
/// A pair whose distance is undefined (`NaN`, see
/// [`super::similarity::fingerprint_distance`]) is never linked: absence of
/// evidence does not merge two sources.
pub fn cluster_fingerprints(
    fps: &[SourceFingerprint],
    threshold: f64,
    max_inputs: usize,
) -> (Vec<FingerprintCluster>, usize) {
    let mut sorted: Vec<&SourceFingerprint> = fps.iter().collect();
    sorted.sort_by_key(|f| f.key());

    let dropped = sorted.len().saturating_sub(max_inputs);
    sorted.truncate(max_inputs);
    let n = sorted.len();
    if n == 0 {
        return (Vec::new(), dropped);
    }

    let mut parent: Vec<usize> = (0..n).collect();
    let weights = SimilarityWeights::default();
    for i in 0..n {
        for j in (i + 1)..n {
            // `<=` on a NaN distance is false, so an incomparable pair stays
            // unlinked rather than merging by accident.
            if fingerprint_distance(sorted[i], sorted[j], &weights) <= threshold {
                union(&mut parent, i, j);
            }
        }
    }

    // BTreeMap keyed by root index keeps cluster order deterministic, and
    // because `sorted` is in key order the root is always the cluster's
    // lowest-keyed member.
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        groups.entry(find(&mut parent, i)).or_default().push(i);
    }

    let clusters = groups
        .into_values()
        .enumerate()
        .map(|(id, idxs)| {
            let mut servers: Vec<SocketAddr> =
                idxs.iter().map(|i| sorted[*i].server_addr).collect();
            servers.sort();
            servers.dedup();
            FingerprintCluster {
                id,
                members: idxs.iter().map(|i| sorted[*i].key()).collect(),
                servers,
                exemplar: sorted[idxs[0]].clone(),
            }
        })
        .collect();

    (clusters, dropped)
}

fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        // Always attach the higher root to the lower, so cluster identity
        // does not depend on the order pairs were examined.
        if ra < rb { parent[rb] = ra } else { parent[ra] = rb }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scry::common::fingerprint::similarity::{DEFAULT_MATCH_THRESHOLD, fingerprints_match};
    use crate::scry::common::fingerprint::types::{SamplingRegime, SourceFingerprint};
    use std::time::UNIX_EPOCH;

    fn fp(channel: &str, server: &str, phase_ms: f64, lsb: f64) -> SourceFingerprint {
        SourceFingerprint {
            channel: channel.to_string(),
            server_addr: server.parse().unwrap(),
            phase_mean_ms: phase_ms,
            phase_spread_us: 85.6,
            phase_circular_var: 3e-7,
            value_lsb: Some(lsb),
            lsb_confidence: 1.0,
            cadence_modal_s: 1.0,
            cadence_p50_s: 1.0,
            cadence_p95_s: 25.0,
            arrival_delta_mean_ms: 4.0,
            arrival_delta_var_ms: Some(0.1),
            sample_count: 40,
            window_start: UNIX_EPOCH,
            window_end: UNIX_EPOCH,
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            struct_id_changed: false,
            has_nan_limits: false,
            observed_count: 40,
            sampling: SamplingRegime::Full,
        }
    }

    /// Spec §8: channels group by fingerprint similarity, and the two
    /// reference sources must not merge.
    #[test]
    fn groups_channels_by_source_and_keeps_the_reference_pair_apart() {
        let fps = vec![
            fp("R55IWATER::AMBIENT:TEMPERATURE", "10.0.0.11:5075", 262.016, 0.0175),
            fp("R55IWATER::AMBIENT:HUMIDITY", "10.0.0.11:5075", 262.020, 0.0175),
            fp("R4WATER::DEMINWAT:PH", "10.0.0.12:5075", 695.973, 0.01),
        ];
        let (clusters, dropped) = cluster_fingerprints(&fps, DEFAULT_MATCH_THRESHOLD, 1000);
        assert_eq!(dropped, 0);
        assert_eq!(clusters.len(), 2);
        let mut sizes: Vec<usize> = clusters.iter().map(|c| c.members.len()).collect();
        sizes.sort_unstable();
        assert_eq!(sizes, vec![1, 2]);
    }

    /// Spec §8: a cluster that spans two servers is the finding worth
    /// surfacing, so the server list must be carried on the cluster.
    #[test]
    fn a_cluster_records_every_server_it_spans() {
        let fps = vec![
            fp("A", "10.0.0.11:5075", 262.016, 0.0175),
            fp("B", "10.0.0.99:5075", 262.018, 0.0175),
        ];
        let (clusters, _) = cluster_fingerprints(&fps, DEFAULT_MATCH_THRESHOLD, 1000);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].servers.len(), 2);
        assert_eq!(clusters[0].members.len(), 2);
    }

    /// Single linkage: A~B and B~C put all three together even though A and C
    /// are further apart than the threshold and would never link directly.
    ///
    /// The 40 ms steps are chosen so each consecutive pair scores 0.04 and the
    /// endpoints score 0.08 against a 0.05 threshold. The premise is asserted
    /// in the body rather than only stated here, so a future change to
    /// `PHASE_SCALE_MS` cannot quietly turn this back into a test that any
    /// linkage rule — complete linkage included — would also pass.
    #[test]
    fn linkage_is_transitive() {
        let fps = vec![
            fp("A", "10.0.0.11:5075", 262.0, 0.0175),
            fp("B", "10.0.0.11:5075", 302.0, 0.0175),
            fp("C", "10.0.0.11:5075", 342.0, 0.0175),
        ];
        assert!(fingerprints_match(&fps[0], &fps[1], DEFAULT_MATCH_THRESHOLD));
        assert!(fingerprints_match(&fps[1], &fps[2], DEFAULT_MATCH_THRESHOLD));
        assert!(
            !fingerprints_match(&fps[0], &fps[2], DEFAULT_MATCH_THRESHOLD),
            "endpoints must not link directly, or this tests nothing"
        );

        let (clusters, _) = cluster_fingerprints(&fps, DEFAULT_MATCH_THRESHOLD, 1000);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members.len(), 3);
    }

    /// Clustering is O(n^2), so the input is capped — but never silently
    /// (spec §4.3 / §6.3).
    #[test]
    fn reports_how_many_inputs_the_cap_dropped() {
        let fps: Vec<SourceFingerprint> = (0..10)
            .map(|i| fp(&format!("PV:{}", i), "10.0.0.11:5075", 262.0, 0.0175))
            .collect();
        let (clusters, dropped) = cluster_fingerprints(&fps, DEFAULT_MATCH_THRESHOLD, 4);
        assert_eq!(dropped, 6);
        assert_eq!(clusters.iter().map(|c| c.members.len()).sum::<usize>(), 4);
    }

    /// Cluster ids and member order must be stable so the TUI does not
    /// reshuffle rows between ticks.
    #[test]
    fn output_is_deterministic() {
        let fps = vec![
            fp("Z", "10.0.0.11:5075", 262.0, 0.0175),
            fp("A", "10.0.0.12:5075", 695.9, 0.01),
            fp("M", "10.0.0.11:5075", 262.1, 0.0175),
        ];
        let (first, _) = cluster_fingerprints(&fps, DEFAULT_MATCH_THRESHOLD, 1000);
        let (second, _) = cluster_fingerprints(&fps, DEFAULT_MATCH_THRESHOLD, 1000);
        assert_eq!(first, second);
        for (i, c) in first.iter().enumerate() {
            assert_eq!(c.id, i);
        }
    }

    /// The real determinism requirement: the same set of fingerprints in a
    /// different order must produce byte-identical clusters, ids and member
    /// order included. Callers hand us the drain of a `HashMap`, so an
    /// order-sensitive implementation would reshuffle the TUI every tick.
    #[test]
    fn output_does_not_depend_on_input_order() {
        let a = fp("Z", "10.0.0.11:5075", 262.0, 0.0175);
        let b = fp("A", "10.0.0.12:5075", 695.9, 0.01);
        let c = fp("M", "10.0.0.11:5075", 262.1, 0.0175);
        let d = fp("Q", "10.0.0.12:5075", 695.95, 0.01);

        let orderings = [
            vec![a.clone(), b.clone(), c.clone(), d.clone()],
            vec![d.clone(), c.clone(), b.clone(), a.clone()],
            vec![c.clone(), a.clone(), d.clone(), b.clone()],
            vec![b.clone(), d.clone(), a.clone(), c.clone()],
        ];

        let (expected, _) = cluster_fingerprints(&orderings[0], DEFAULT_MATCH_THRESHOLD, 1000);
        assert_eq!(expected.len(), 2);
        // Members are listed in key order, not arrival order.
        assert_eq!(
            expected[0].members.iter().map(|k| k.channel.as_str()).collect::<Vec<_>>(),
            vec!["A", "Q"]
        );
        assert_eq!(
            expected[1].members.iter().map(|k| k.channel.as_str()).collect::<Vec<_>>(),
            vec!["M", "Z"]
        );
        for ordering in &orderings[1..] {
            let (got, _) = cluster_fingerprints(ordering, DEFAULT_MATCH_THRESHOLD, 1000);
            assert_eq!(got, expected);
        }
    }

    /// Which inputs the cap keeps must not depend on input order either, or
    /// the truncated view would change identity between ticks.
    #[test]
    fn the_cap_keeps_the_same_inputs_whatever_the_order() {
        let forward: Vec<SourceFingerprint> = (0..6)
            .map(|i| fp(&format!("PV:{}", i), "10.0.0.11:5075", 100.0 * i as f64, 0.0175))
            .collect();
        let mut reversed = forward.clone();
        reversed.reverse();

        let (a, dropped_a) = cluster_fingerprints(&forward, DEFAULT_MATCH_THRESHOLD, 3);
        let (b, dropped_b) = cluster_fingerprints(&reversed, DEFAULT_MATCH_THRESHOLD, 3);
        assert_eq!(dropped_a, 3);
        assert_eq!(dropped_b, 3);
        assert_eq!(a, b);
        let kept: Vec<&str> =
            a.iter().flat_map(|c| c.members.iter().map(|k| k.channel.as_str())).collect();
        assert_eq!(kept, vec!["PV:0", "PV:1", "PV:2"]);
    }

    /// A cluster's exemplar is its lowest-keyed member, so the display
    /// representative does not hop between refreshes.
    #[test]
    fn the_exemplar_is_the_lowest_keyed_member() {
        let fps = vec![
            fp("Z", "10.0.0.11:5075", 262.0, 0.0175),
            fp("M", "10.0.0.11:5075", 262.1, 0.0175),
        ];
        let (clusters, _) = cluster_fingerprints(&fps, DEFAULT_MATCH_THRESHOLD, 1000);
        assert_eq!(clusters[0].exemplar.channel, "M");
    }

    #[test]
    fn empty_input_yields_no_clusters() {
        let (clusters, dropped) = cluster_fingerprints(&[], DEFAULT_MATCH_THRESHOLD, 1000);
        assert!(clusters.is_empty());
        assert_eq!(dropped, 0);
    }

    /// A zero cap drops everything and says so, rather than reporting an
    /// empty-and-therefore-clean cluster view.
    #[test]
    fn a_zero_cap_drops_everything_visibly() {
        let fps = vec![fp("A", "10.0.0.11:5075", 262.0, 0.0175)];
        let (clusters, dropped) = cluster_fingerprints(&fps, DEFAULT_MATCH_THRESHOLD, 0);
        assert!(clusters.is_empty());
        assert_eq!(dropped, 1);
    }
}
