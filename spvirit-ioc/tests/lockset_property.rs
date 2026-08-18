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

/// The five link fields the engine partitions over, in the order
/// [`spvirit_ioc::lockset::links_of`] enumerates them. The model does not
/// restrict which of these a given record kind may carry — `build_records`
/// parses all five uniformly regardless of kind — so any field is always
/// meaningful to write on an `ai` record and no edge is ever dropped for
/// being "the wrong field for this kind".
const LINK_FIELDS: [&str; 5] = ["INP", "OUT", "DOL", "FLNK", "SDIS"];

/// Render `edges` (from-index, to-index, field-selector triples) as a `.db`
/// file over `n` records. Each record gets at most one target per field
/// (matching `.db`'s one-value-per-field-name shape); when more than one
/// edge names the same (from, field) pair, the first one in iteration order
/// wins, exactly as `field_targets` below computes it for the oracle.
fn render_db(n: usize, field_targets: &HashMap<(usize, usize), usize>) -> String {
    let mut out = String::new();
    for i in 0..n {
        out.push_str(&format!("record(ai, \"PV:{i}\") {{\n"));
        for (field_ix, field_name) in LINK_FIELDS.iter().enumerate() {
            if let Some(target) = field_targets.get(&(i, field_ix)) {
                out.push_str(&format!("    field({field_name}, \"PV:{target} PP\")\n"));
            }
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
        raw_edges in prop::collection::vec((0usize..12, 0usize..12, 0usize..LINK_FIELDS.len()), 0..20),
    ) {
        let edges: Vec<(usize, usize, usize)> = raw_edges
            .into_iter()
            .filter(|(a, b, _)| *a < n && *b < n && a != b)
            .collect();

        // The engine's `.db` model holds one value per field name, so a
        // record can carry at most one target per field. Mirror that here
        // with a first-edge-per-(from, field) rule, which both the
        // renderer and the union-find reference use — this is the one
        // place the "which edge wins" decision is made, so the two can
        // never see a different edge set.
        let mut field_targets: HashMap<(usize, usize), usize> = HashMap::new();
        for (from, to, field) in &edges {
            field_targets.entry((*from, *field)).or_insert(*to);
        }

        let db = render_db(n, &field_targets);
        let raw = parse_db_records(&db, "prop.db", &HashMap::new()).expect("parse");
        let records = build_records(&raw).expect("build");
        let engine = as_sets(partition(&records));

        let mut uf = UnionFind::new(n);
        for (&(from, _field), &to) in &field_targets {
            uf.union(from, to);
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
