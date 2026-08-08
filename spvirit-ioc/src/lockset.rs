//! Lock sets: the unit of mutual exclusion, and the unit of ownership.
//!
//! Records reachable from one another through db links (INP, OUT, DOL,
//! FLNK, SDIS) live in the same lock set. A lock set owns its records
//! outright, so `process()` takes `&mut LockSetData` and the borrow checker
//! enforces that a processing record can only reach records in its own set —
//! the property the whole synchronous design rests on.

use crate::model::{Link, Record, Target};
use std::collections::HashMap;
use std::sync::Mutex;

/// A record's address: which lock set owns it, and where in that set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordId {
    pub set: usize,
    pub slot: usize,
}

/// The records one lock set owns.
#[derive(Debug)]
pub struct LockSetData {
    pub members: Vec<Record>,
}

impl LockSetData {
    pub fn get(&self, id: RecordId) -> &Record {
        &self.members[id.slot]
    }

    pub fn get_mut(&mut self, id: RecordId) -> &mut Record {
        &mut self.members[id.slot]
    }
}

/// Every link a record holds, in a fixed order: INP, OUT, DOL, FLNK, SDIS.
/// This is the single place that enumerates the five link fields — used by
/// the partitioner, link resolution, and (later) the dependency graph — so
/// none of them can drift out of sync with the others. [`links_of_mut`]
/// must enumerate the same five fields in the same order.
pub(crate) fn links_of(record: &Record) -> [&Link; 5] {
    [
        &record.inp,
        &record.out,
        &record.dol,
        &record.common.flnk,
        &record.common.sdis,
    ]
}

/// The mutable counterpart to [`links_of`], for callers that rewrite link
/// targets in place. Disjoint field borrows make returning five simultaneous
/// `&mut Link`s sound. Must enumerate the same five fields in the same order
/// as [`links_of`].
pub(crate) fn links_of_mut(record: &mut Record) -> [&mut Link; 5] {
    [
        &mut record.inp,
        &mut record.out,
        &mut record.dol,
        &mut record.common.flnk,
        &mut record.common.sdis,
    ]
}

/// Group record indices into connected components over their db links.
///
/// Returns components in ascending order of their lowest member index, with
/// each component's members ascending — the partitioning is a pure function
/// of the input order, which the determinism test depends on.
pub fn partition(records: &[Record]) -> Vec<Vec<usize>> {
    let index: HashMap<&str, usize> = records
        .iter()
        .enumerate()
        .map(|(i, r)| (r.name.as_str(), i))
        .collect();

    let mut parent: Vec<usize> = (0..records.len()).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }

    for (i, record) in records.iter().enumerate() {
        for link in links_of(record) {
            let Link::Db {
                target: Target::Name(name),
                ..
            } = link
            else {
                continue;
            };
            let Some(&j) = index.get(name.as_str()) else {
                continue;
            };
            let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
            if ri != rj {
                parent[ri] = rj;
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..records.len() {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }
    let mut out: Vec<Vec<usize>> = groups.into_values().collect();
    for group in &mut out {
        group.sort_unstable();
    }
    out.sort_unstable_by_key(|g| g[0]);
    out
}

/// The whole record database: a name index, the lock sets that own the
/// records, and the definition order PINI needs.
pub struct RecordDb {
    by_name: HashMap<String, RecordId>,
    lock_sets: Vec<Mutex<LockSetData>>,
    order: Vec<RecordId>,
    unresolved: Vec<String>,
}

impl RecordDb {
    /// Partition `records`, move them into their lock sets, and rewrite every
    /// db link's target from a name to a [`RecordId`].
    ///
    /// A link naming a record that does not exist becomes
    /// [`Link::Unresolved`]; the names are collected so the caller can warn
    /// once at startup rather than on every process pass.
    pub fn build(records: Vec<Record>) -> RecordDb {
        let groups = partition(&records);

        // original index -> RecordId
        let mut ids: Vec<RecordId> = vec![RecordId { set: 0, slot: 0 }; records.len()];
        for (set, group) in groups.iter().enumerate() {
            for (slot, &orig) in group.iter().enumerate() {
                ids[orig] = RecordId { set, slot };
            }
        }

        let by_name: HashMap<String, RecordId> = records
            .iter()
            .enumerate()
            .map(|(i, r)| (r.name.clone(), ids[i]))
            .collect();
        let order: Vec<RecordId> = (0..records.len()).map(|i| ids[i]).collect();

        // Move the records into their groups, leaving `records` consumed.
        let mut slots: Vec<Option<Record>> = records.into_iter().map(Some).collect();
        let mut unresolved = Vec::new();
        let mut lock_sets = Vec::with_capacity(groups.len());
        for group in &groups {
            let mut members = Vec::with_capacity(group.len());
            for &orig in group {
                let mut record = slots[orig].take().expect("each index appears in one group");
                resolve_links(&mut record, &by_name, &mut unresolved);
                members.push(record);
            }
            lock_sets.push(Mutex::new(LockSetData { members }));
        }
        unresolved.sort();
        unresolved.dedup();

        RecordDb {
            by_name,
            lock_sets,
            order,
            unresolved,
        }
    }

    pub fn lookup(&self, name: &str) -> Option<RecordId> {
        self.by_name.get(name).copied()
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.by_name.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn lock_set_count(&self) -> usize {
        self.lock_sets.len()
    }

    /// Records in `.db` definition order — the order PINI processes in.
    pub fn order(&self) -> &[RecordId] {
        &self.order
    }

    /// Link targets that named a record the database does not contain.
    pub fn unresolved_links(&self) -> Vec<String> {
        self.unresolved.clone()
    }

    /// Run `f` with the lock set held. A poisoned lock means a previous
    /// `process()` panicked mid-pass; recovering the guard would expose
    /// half-updated records, so this propagates the panic.
    pub fn with_set<T>(&self, set: usize, f: impl FnOnce(&mut LockSetData) -> T) -> T {
        let mut guard = self.lock_sets[set]
            .lock()
            .expect("lock set poisoned by a panicking process() pass");
        f(&mut guard)
    }
}

fn fix_link(link: &mut Link, by_name: &HashMap<String, RecordId>, unresolved: &mut Vec<String>) {
    let Link::Db {
        target,
        field,
        process_passive,
        maximize_severity,
    } = link
    else {
        return;
    };
    let Target::Name(name) = target else { return };
    match by_name.get(name.as_str()) {
        Some(&id) => {
            *link = Link::Db {
                target: Target::Id(id),
                field: *field,
                process_passive: *process_passive,
                maximize_severity: *maximize_severity,
            };
        }
        None => {
            unresolved.push(name.clone());
            *link = Link::Unresolved { name: name.clone() };
        }
    }
}

fn resolve_links(
    record: &mut Record,
    by_name: &HashMap<String, RecordId>,
    unresolved: &mut Vec<String>,
) {
    for link in links_of_mut(record) {
        fix_link(link, by_name, unresolved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::build_records;
    use spvirit_server::db::parse_db_records;
    use std::collections::HashMap;

    fn db(text: &str) -> RecordDb {
        let raw = parse_db_records(text, "t.db", &HashMap::new()).expect("parse");
        RecordDb::build(build_records(&raw).expect("build"))
    }

    #[test]
    fn linked_records_share_a_lock_set() {
        let d = db("record(ai, \"PV:A\") {\n    field(INP, \"PV:B PP\")\n}\n\
                    record(ai, \"PV:B\") {\n}\n");
        let a = d.lookup("PV:A").expect("PV:A exists");
        let b = d.lookup("PV:B").expect("PV:B exists");
        assert_eq!(a.set, b.set, "a db link joins the two lock sets");
    }

    #[test]
    fn unlinked_records_get_separate_lock_sets() {
        let d = db("record(ai, \"PV:A\") {\n}\nrecord(ai, \"PV:B\") {\n}\n");
        let a = d.lookup("PV:A").expect("PV:A exists");
        let b = d.lookup("PV:B").expect("PV:B exists");
        assert_ne!(a.set, b.set);
        assert_eq!(d.lock_set_count(), 2);
    }

    #[test]
    fn flnk_and_sdis_also_join_lock_sets() {
        let d = db("record(ai, \"PV:A\") {\n    field(FLNK, \"PV:B\")\n}\n\
                    record(ai, \"PV:B\") {\n    field(SDIS, \"PV:C\")\n}\n\
                    record(ai, \"PV:C\") {\n}\n");
        let sets: Vec<usize> = ["PV:A", "PV:B", "PV:C"]
            .iter()
            .map(|n| d.lookup(n).expect("record exists").set)
            .collect();
        assert_eq!(sets[0], sets[1]);
        assert_eq!(sets[1], sets[2]);
        assert_eq!(d.lock_set_count(), 1);
    }

    #[test]
    fn links_resolve_to_ids_not_names() {
        let d = db("record(ai, \"PV:A\") {\n    field(INP, \"PV:B PP\")\n}\n\
                    record(ai, \"PV:B\") {\n}\n");
        let a = d.lookup("PV:A").expect("PV:A exists");
        let b = d.lookup("PV:B").expect("PV:B exists");
        d.with_set(a.set, |set| match &set.get(a).inp {
            Link::Db {
                target: Target::Id(id),
                ..
            } => assert_eq!(*id, b),
            other => panic!("INP must resolve to an id, got {other:?}"),
        });
    }

    #[test]
    fn an_unresolvable_target_becomes_unresolved_not_a_panic() {
        let d = db("record(ai, \"PV:A\") {\n    field(INP, \"PV:MISSING PP\")\n}\n");
        let a = d.lookup("PV:A").expect("PV:A exists");
        d.with_set(a.set, |set| match &set.get(a).inp {
            Link::Unresolved { name } => assert_eq!(name, "PV:MISSING"),
            other => panic!("expected Unresolved, got {other:?}"),
        });
        assert_eq!(d.unresolved_links(), vec!["PV:MISSING".to_string()]);
    }

    #[test]
    fn order_preserves_the_db_file_order() {
        let d = db("record(ai, \"PV:Z\") {\n}\nrecord(ai, \"PV:A\") {\n}\n");
        let names: Vec<String> = d
            .order()
            .iter()
            .map(|id| d.with_set(id.set, |s| s.get(*id).name.clone()))
            .collect();
        assert_eq!(
            names,
            vec!["PV:Z".to_string(), "PV:A".to_string()],
            "PINI processes in definition order, so order must be the file's"
        );
    }
}
