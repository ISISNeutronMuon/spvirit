//! The lock-set partitioning must agree with a naive union-find over the
//! same edge set. The engine's partitioner is written for determinism and
//! for a stable slot order; the reference here is written to be obviously
//! correct. Any disagreement is a bug in the engine.

use proptest::prelude::*;
use spvirit_ioc::build::build_records;
use spvirit_ioc::lockset::partition;
use spvirit_server::db::parse_db_records;
use std::collections::{BTreeSet, HashMap};

/// Textbook union-find. Deliberately not the engine's algorithm.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

/// Render `edges` (pairs of record indices) as a `.db` file over `n` records.
fn render_db(n: usize, edges: &[(usize, usize)]) -> String {
    let mut inp: HashMap<usize, usize> = HashMap::new();
    for (from, to) in edges {
        inp.entry(*from).or_insert(*to);
    }
    let mut out = String::new();
    for i in 0..n {
        out.push_str(&format!("record(ai, \"PV:{i}\") {{\n"));
        if let Some(target) = inp.get(&i) {
            out.push_str(&format!("    field(INP, \"PV:{target} PP\")\n"));
        }
        out.push_str("}\n");
    }
    out
}

fn as_sets(groups: Vec<Vec<usize>>) -> BTreeSet<BTreeSet<usize>> {
    groups
        .into_iter()
        .map(|g| g.into_iter().collect::<BTreeSet<usize>>())
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn partitioning_agrees_with_union_find(
        n in 1usize..12,
        raw_edges in prop::collection::vec((0usize..12, 0usize..12), 0..20),
    ) {
        let edges: Vec<(usize, usize)> = raw_edges
            .into_iter()
            .filter(|(a, b)| *a < n && *b < n && a != b)
            .collect();

        let db = render_db(n, &edges);
        let raw = parse_db_records(&db, "prop.db", &HashMap::new()).expect("parse");
        let records = build_records(&raw).expect("build");
        let engine = as_sets(partition(&records));

        // The engine only sees the first INP per record, exactly as
        // render_db wrote it. Mirror that when building the reference.
        let mut first: HashMap<usize, usize> = HashMap::new();
        for (from, to) in &edges {
            first.entry(*from).or_insert(*to);
        }
        let mut uf = UnionFind::new(n);
        for (from, to) in &first {
            uf.union(*from, *to);
        }
        let mut groups: HashMap<usize, BTreeSet<usize>> = HashMap::new();
        for i in 0..n {
            let root = uf.find(i);
            groups.entry(root).or_default().insert(i);
        }
        let reference: BTreeSet<BTreeSet<usize>> = groups.into_values().collect();

        prop_assert_eq!(engine, reference);
    }
}
