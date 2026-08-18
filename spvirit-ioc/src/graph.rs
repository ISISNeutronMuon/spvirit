//! Load-time diagnostics over the link graph.
//!
//! These answer the questions a `.db` author actually gets wrong: a record
//! that can never process, a link cycle, and a record so heavily depended on
//! that one process pass fans out across the database. None of them is fatal
//! — the engine reports them once at startup and runs anyway.

use crate::lockset::{LinkField, RecordDb, links_of};
use crate::model::{Link, Target};
use std::collections::{HashMap, HashSet};

/// Inbound degree above which a record is worth mentioning: how many other
/// records hold a link that names it. Chosen to be quiet on ordinary
/// databases: a hub with ten dependants is normal, a hundred usually means a
/// `.db` generator ran away.
pub const FAN_OUT_THRESHOLD: usize = 10;

/// Which link field produced an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    Inp,
    Out,
    Dol,
    Flnk,
    Sdis,
}

/// `links_of`/`links_of_mut` (Task 4) are the single place allowed to
/// enumerate a record's five link fields; this exhaustive match — not a
/// second, position-matched table — is what keeps `EdgeKind` from drifting
/// out of sync with that enumeration. Adding a variant to `LinkField`
/// without updating this match fails to compile, rather than silently
/// mislabelling an edge.
impl From<LinkField> for EdgeKind {
    fn from(field: LinkField) -> EdgeKind {
        match field {
            LinkField::Inp => EdgeKind::Inp,
            LinkField::Out => EdgeKind::Out,
            LinkField::Dol => EdgeKind::Dol,
            LinkField::Flnk => EdgeKind::Flnk,
            LinkField::Sdis => EdgeKind::Sdis,
        }
    }
}

/// One directed dependency, from the record holding the link to its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    /// Whether this edge can cause the target to process.
    pub processes_target: bool,
}

/// The whole link graph plus the three load-time checks.
#[derive(Debug, Clone, Default)]
pub struct DependencyGraph {
    pub edges: Vec<Edge>,
    /// Lock-set membership by record name, in lock-set order.
    pub lock_sets: Vec<Vec<String>>,
    /// Records that can never process: passive, no PINI, and no incoming
    /// edge that would process them.
    pub unreachable: Vec<String>,
    /// Strongly connected components of size > 1, or self-loops.
    pub cycles: Vec<Vec<String>>,
    /// Records named by more than [`FAN_OUT_THRESHOLD`] other records' link
    /// fields — i.e. an inbound degree, the number of dependants a single
    /// process pass would fan out to.
    pub high_fan_out: Vec<(String, usize)>,
    /// Link targets naming a record the database does not contain.
    pub unresolved: Vec<String>,
}

impl DependencyGraph {
    /// One line per finding, for the startup log. Empty means clean.
    pub fn report(&self) -> Vec<String> {
        let mut out = Vec::new();
        for name in &self.unreachable {
            out.push(format!(
                "record '{name}' can never process: SCAN is Passive, PINI is NO, \
                 and nothing links to it with PP or FLNK"
            ));
        }
        for cycle in &self.cycles {
            out.push(format!(
                "link cycle among {} (PACT breaks it at runtime; not an error)",
                cycle.join(" -> ")
            ));
        }
        for (name, degree) in &self.high_fan_out {
            out.push(format!(
                "record '{name}' is linked TO by {degree} other records \
                 (threshold {FAN_OUT_THRESHOLD}); one process pass will fan out across the database"
            ));
        }
        for name in &self.unresolved {
            out.push(format!(
                "link target '{name}' names no record in this database; \
                 it reads as a constant zero"
            ));
        }
        out
    }
}

impl RecordDb {
    /// Build the load-time dependency graph.
    pub fn dependency_graph(&self) -> DependencyGraph {
        let mut edges = Vec::new();
        let mut lock_sets: Vec<Vec<String>> = Vec::new();
        let mut self_processing: HashSet<String> = HashSet::new();
        let mut id_name: HashMap<crate::lockset::RecordId, String> = HashMap::new();

        for set_index in 0..self.lock_set_count() {
            self.with_set(set_index, |set| {
                for (slot, record) in set.members.iter().enumerate() {
                    id_name.insert(
                        crate::lockset::RecordId {
                            set: set_index,
                            slot,
                        },
                        record.name.clone(),
                    );
                }
            });
        }

        for set_index in 0..self.lock_set_count() {
            let names = self.with_set(set_index, |set| {
                let mut names = Vec::with_capacity(set.members.len());
                for record in &set.members {
                    names.push(record.name.clone());
                    if record.common.pini || !record.common.scan_raw.eq_ignore_ascii_case("passive")
                    {
                        self_processing.insert(record.name.clone());
                    }
                    for (field, link) in links_of(record) {
                        let kind = EdgeKind::from(field);
                        let Link::Db {
                            target: Target::Id(id),
                            process_passive,
                            ..
                        } = link
                        else {
                            continue;
                        };
                        // A forward link always processes its target; the
                        // others only do when PP is set.
                        let processes_target = kind == EdgeKind::Flnk || *process_passive;
                        edges.push(Edge {
                            from: record.name.clone(),
                            to: id_name.get(id).cloned().unwrap_or_default(),
                            kind,
                            processes_target,
                        });
                    }
                }
                names
            });
            lock_sets.push(names);
        }

        edges.sort_by(|a, b| (&a.from, &a.to, a.kind).cmp(&(&b.from, &b.to, b.kind)));

        let processed_by_someone: HashSet<&str> = edges
            .iter()
            .filter(|e| e.processes_target)
            .map(|e| e.to.as_str())
            .collect();

        let mut unreachable: Vec<String> = self
            .names()
            .into_iter()
            .filter(|n| !self_processing.contains(n) && !processed_by_someone.contains(n.as_str()))
            .collect();
        unreachable.sort();

        let mut fan_out: HashMap<&str, usize> = HashMap::new();
        for edge in &edges {
            *fan_out.entry(edge.to.as_str()).or_insert(0) += 1;
        }
        let mut high_fan_out: Vec<(String, usize)> = fan_out
            .into_iter()
            .filter(|(_, d)| *d > FAN_OUT_THRESHOLD)
            .map(|(n, d)| (n.to_string(), d))
            .collect();
        high_fan_out.sort();

        let cycles = find_cycles(&edges);

        DependencyGraph {
            edges,
            lock_sets,
            unreachable,
            cycles,
            high_fan_out,
            unresolved: self.unresolved_links(),
        }
    }
}

/// Strongly connected components of size > 1, plus self-loops.
///
/// Iterative Tarjan: a pathological `.db` should produce a diagnostic, not a
/// stack overflow inside the diagnostic.
fn find_cycles(edges: &[Edge]) -> Vec<Vec<String>> {
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut nodes: Vec<&str> = Vec::new();
    for edge in edges {
        adjacency
            .entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
        for n in [edge.from.as_str(), edge.to.as_str()] {
            if !nodes.contains(&n) {
                nodes.push(n);
            }
        }
    }
    nodes.sort_unstable();

    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut low: HashMap<&str, usize> = HashMap::new();
    let mut on_stack: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut next_index = 0usize;
    let mut out: Vec<Vec<String>> = Vec::new();

    for &root in &nodes {
        if index.contains_key(root) {
            continue;
        }
        // (node, position in its adjacency list)
        let mut work: Vec<(&str, usize)> = vec![(root, 0)];
        index.insert(root, next_index);
        low.insert(root, next_index);
        next_index += 1;
        stack.push(root);
        on_stack.insert(root);

        while let Some((node, pos)) = work.pop() {
            let neighbours = adjacency.get(node).map(Vec::as_slice).unwrap_or(&[]);
            if pos < neighbours.len() {
                work.push((node, pos + 1));
                let next = neighbours[pos];
                if !index.contains_key(next) {
                    index.insert(next, next_index);
                    low.insert(next, next_index);
                    next_index += 1;
                    stack.push(next);
                    on_stack.insert(next);
                    work.push((next, 0));
                } else if on_stack.contains(next) {
                    let candidate = low[node].min(index[next]);
                    low.insert(node, candidate);
                }
                continue;
            }

            // Finished node: propagate its low-link to its parent.
            if let Some(&(parent, _)) = work.last() {
                let candidate = low[parent].min(low[node]);
                low.insert(parent, candidate);
            }
            if low[node] == index[node] {
                let mut component = Vec::new();
                while let Some(top) = stack.pop() {
                    on_stack.remove(top);
                    component.push(top.to_string());
                    if top == node {
                        break;
                    }
                }
                let self_loop = adjacency.get(node).is_some_and(|ns| ns.contains(&node));
                if component.len() > 1 || self_loop {
                    component.sort();
                    out.push(component);
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::build_records;
    use crate::lockset::RecordDb;
    use spvirit_server::db::parse_db_records;
    use std::collections::HashMap;

    fn graph(text: &str) -> DependencyGraph {
        let raw = parse_db_records(text, "t.db", &HashMap::new()).expect("parse");
        RecordDb::build(build_records(&raw).expect("build")).dependency_graph()
    }

    #[test]
    fn edges_record_which_field_created_them() {
        let g = graph(
            "record(ai, \"PV:A\") {\n    field(INP, \"PV:B\")\n\
                       field(FLNK, \"PV:C\")\n}\n\
                       record(ai, \"PV:B\") {\n}\nrecord(ai, \"PV:C\") {\n}\n",
        );
        let mut kinds: Vec<(String, String, EdgeKind)> = g
            .edges
            .iter()
            .map(|e| (e.from.clone(), e.to.clone(), e.kind))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                ("PV:A".to_string(), "PV:B".to_string(), EdgeKind::Inp),
                ("PV:A".to_string(), "PV:C".to_string(), EdgeKind::Flnk),
            ]
        );
    }

    #[test]
    fn out_dol_and_sdis_edges_also_carry_the_right_kind() {
        // edges_record_which_field_created_them above only exercises INP and
        // FLNK; a mapping shifted by one position (e.g. Out mislabelled as
        // Dol) would pass that test but fail this one.
        let g = graph(
            "record(ao, \"PV:A\") {\n    field(OUT, \"PV:B\")\n\
                       field(DOL, \"PV:C\")\n    field(SDIS, \"PV:D\")\n}\n\
                       record(ai, \"PV:B\") {\n}\nrecord(ai, \"PV:C\") {\n}\n\
                       record(ai, \"PV:D\") {\n}\n",
        );
        let mut kinds: Vec<(String, String, EdgeKind)> = g
            .edges
            .iter()
            .map(|e| (e.from.clone(), e.to.clone(), e.kind))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                ("PV:A".to_string(), "PV:B".to_string(), EdgeKind::Out),
                ("PV:A".to_string(), "PV:C".to_string(), EdgeKind::Dol),
                ("PV:A".to_string(), "PV:D".to_string(), EdgeKind::Sdis),
            ]
        );
    }

    #[test]
    fn a_passive_record_nothing_targets_is_unreachable() {
        let g = graph("record(ai, \"PV:LONELY\") {\n}\n");
        assert_eq!(g.unreachable, vec!["PV:LONELY".to_string()]);
    }

    #[test]
    fn pini_makes_a_record_reachable() {
        let g = graph("record(ai, \"PV:INIT\") {\n    field(PINI, \"YES\")\n}\n");
        assert!(g.unreachable.is_empty(), "PINI records process at startup");
    }

    #[test]
    fn a_non_passive_scan_makes_a_record_reachable() {
        let g = graph("record(ai, \"PV:P\") {\n    field(SCAN, \"1 second\")\n}\n");
        assert!(
            g.unreachable.is_empty(),
            "a scanned record processes itself"
        );
    }

    #[test]
    fn being_a_pp_or_flnk_target_makes_a_record_reachable() {
        let g = graph(
            "record(ai, \"PV:A\") {\n    field(PINI, \"YES\")\n\
                       field(FLNK, \"PV:B\")\n}\nrecord(ai, \"PV:B\") {\n}\n",
        );
        assert!(g.unreachable.is_empty());
    }

    #[test]
    fn an_npp_target_alone_does_not_make_a_record_reachable() {
        // NPP reads the target without processing it, so PV:B still never
        // runs. This is exactly the mistake the check exists to catch.
        let g = graph(
            "record(ai, \"PV:A\") {\n    field(PINI, \"YES\")\n\
                       field(INP, \"PV:B NPP\")\n}\nrecord(ai, \"PV:B\") {\n}\n",
        );
        assert_eq!(g.unreachable, vec!["PV:B".to_string()]);
    }

    #[test]
    fn a_cycle_is_reported_but_is_not_an_error() {
        let g = graph(
            "record(ai, \"PV:A\") {\n    field(FLNK, \"PV:B\")\n}\n\
                       record(ai, \"PV:B\") {\n    field(FLNK, \"PV:A\")\n}\n",
        );
        assert_eq!(g.cycles.len(), 1, "the A-B cycle must be reported");
        let mut members = g.cycles[0].clone();
        members.sort();
        assert_eq!(members, vec!["PV:A".to_string(), "PV:B".to_string()]);
        assert!(
            g.report().iter().any(|l| l.contains("cycle")),
            "the cycle must appear in the startup report"
        );
    }

    #[test]
    fn fan_out_above_the_threshold_is_reported() {
        let mut text = String::from("record(ai, \"PV:HUB\") {\n    field(PINI, \"YES\")\n}\n");
        for i in 0..(FAN_OUT_THRESHOLD + 1) {
            text.push_str(&format!(
                "record(ai, \"PV:{i}\") {{\n    field(INP, \"PV:HUB PP\")\n}}\n"
            ));
        }
        let g = graph(&text);
        let hub = g
            .high_fan_out
            .iter()
            .find(|(name, _)| name == "PV:HUB")
            .expect("PV:HUB must be flagged");
        assert_eq!(hub.1, FAN_OUT_THRESHOLD + 1);
    }

    #[test]
    fn fan_out_exactly_at_the_threshold_is_not_reported() {
        // Pin the boundary: the brief's filter is `d > FAN_OUT_THRESHOLD`, so
        // exactly FAN_OUT_THRESHOLD inbound links must NOT trigger a finding.
        let mut text = String::from("record(ai, \"PV:HUB\") {\n    field(PINI, \"YES\")\n}\n");
        for i in 0..FAN_OUT_THRESHOLD {
            text.push_str(&format!(
                "record(ai, \"PV:{i}\") {{\n    field(INP, \"PV:HUB PP\")\n}}\n"
            ));
        }
        let g = graph(&text);
        assert!(
            g.high_fan_out.iter().all(|(name, _)| name != "PV:HUB"),
            "exactly the threshold must not be flagged, got {:?}",
            g.high_fan_out
        );
    }

    #[test]
    fn lock_sets_are_reported_by_member_name() {
        let g = graph(
            "record(ai, \"PV:A\") {\n    field(INP, \"PV:B PP\")\n}\n\
                       record(ai, \"PV:B\") {\n}\nrecord(ai, \"PV:C\") {\n}\n",
        );
        assert_eq!(g.lock_sets.len(), 2);
        assert!(g.lock_sets.iter().any(|s| s.len() == 2));
    }

    #[test]
    fn a_clean_database_produces_an_empty_report() {
        let g = graph(
            "record(ai, \"PV:A\") {\n    field(PINI, \"YES\")\n\
                       field(FLNK, \"PV:B\")\n}\nrecord(ai, \"PV:B\") {\n}\n",
        );
        assert!(g.report().is_empty(), "got {:?}", g.report());
    }

    // --- Iterative-Tarjan verification -------------------------------
    //
    // These exercise `find_cycles` directly with hand-built edge lists, so
    // each test can isolate one SCC topology without depending on how the
    // rest of `dependency_graph` classifies edges.

    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from: from.to_string(),
            to: to.to_string(),
            kind: EdgeKind::Flnk,
            processes_target: true,
        }
    }

    #[test]
    fn dag_with_no_back_edges_has_no_cycles() {
        // A -> B -> C, a strict chain: inverting cycle-detection to "report
        // everything" would turn this red.
        let edges = vec![edge("A", "B"), edge("B", "C")];
        assert_eq!(find_cycles(&edges), Vec::<Vec<String>>::new());
    }

    #[test]
    fn a_single_self_loop_is_a_cycle() {
        // A -> A alone, plus an unrelated B with no edges at all. A
        // detector that only fires on size > 1 components would miss this.
        let edges = vec![edge("A", "A")];
        assert_eq!(find_cycles(&edges), vec![vec!["A".to_string()]]);
    }

    #[test]
    fn two_disjoint_cycles_are_reported_separately() {
        // A<->B and C<->D, no edges between the two pairs: a detector that
        // merges unrelated components would report one 4-node blob instead
        // of two 2-node ones.
        let edges = vec![
            edge("A", "B"),
            edge("B", "A"),
            edge("C", "D"),
            edge("D", "C"),
        ];
        let cycles = find_cycles(&edges);
        assert_eq!(
            cycles,
            vec![
                vec!["A".to_string(), "B".to_string()],
                vec!["C".to_string(), "D".to_string()],
            ]
        );
    }

    #[test]
    fn a_tail_into_a_cycle_is_not_part_of_it() {
        // X -> A -> B -> A: X reaches the cycle but is never reached back,
        // so X must not appear in the reported component. A naive
        // conversion that treats every node still "on the path" as part of
        // the SCC would incorrectly include X.
        let edges = vec![edge("X", "A"), edge("A", "B"), edge("B", "A")];
        let cycles = find_cycles(&edges);
        assert_eq!(cycles, vec![vec!["A".to_string(), "B".to_string()]]);
    }

    #[test]
    fn nested_cycles_are_reported_as_distinct_components() {
        // A<->B is its own cycle. C<->D is its own cycle. A single edge
        // B -> C links the two chains into one weakly-connected graph
        // without merging them into one SCC (there's no path from C or D
        // back to A or B). A naive "one big component" bug would report a
        // single 4-node SCC instead of two 2-node ones.
        let edges = vec![
            edge("A", "B"),
            edge("B", "A"),
            edge("B", "C"),
            edge("C", "D"),
            edge("D", "C"),
        ];
        let cycles = find_cycles(&edges);
        assert_eq!(
            cycles,
            vec![
                vec!["A".to_string(), "B".to_string()],
                vec!["C".to_string(), "D".to_string()],
            ]
        );
    }

    #[test]
    fn overlapping_paths_that_reconverge_form_one_scc() {
        // A -> B -> D -> A and A -> C -> D: B and C both feed D, D feeds
        // back only to A, so all four nodes are mutually reachable and must
        // collapse into one SCC, not two overlapping ones.
        let edges = vec![
            edge("A", "B"),
            edge("A", "C"),
            edge("B", "D"),
            edge("C", "D"),
            edge("D", "A"),
        ];
        let cycles = find_cycles(&edges);
        assert_eq!(
            cycles,
            vec![vec![
                "A".to_string(),
                "B".to_string(),
                "C".to_string(),
                "D".to_string()
            ]]
        );
    }
}
