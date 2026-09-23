//! A single scan list: records ordered by PHAS then insertion order.

use crate::lockset::RecordId;
use std::collections::HashMap;

/// One scan list: the records to process on a tick, already in processing
/// order.
///
/// `members` is held sorted by `(phas, seq)` at all times, so `snapshot` --
/// which runs on every tick of every period -- is a plain copy of the ids
/// rather than a clone-and-re-sort. `index` maps a member to its sort key so
/// membership tests and removals do not scan the vec.
#[derive(Debug, Default)]
pub struct ScanList {
    /// Sorted ascending by `(phas, seq)`. Invariant: exactly the entries in
    /// `index`, and `binary_search` on `(phas, seq)` finds each of them.
    members: Vec<(i32, u64, RecordId)>,
    /// Each member's `(phas, seq)` sort key.
    index: HashMap<RecordId, (i32, u64)>,
    /// Monotonic insertion counter: ties within a PHAS break by first-added,
    /// and a member that changes PHAS keeps its original seq (so a re-insert
    /// re-orders without jumping to the back of its new phase).
    next_seq: u64,
}

impl ScanList {
    pub fn new() -> Self {
        Self::default()
    }

    /// Position of the member with sort key `(phas, seq)`.
    fn locate(&self, phas: i32, seq: u64) -> Option<usize> {
        self.members
            .binary_search_by(|probe| (probe.0, probe.1).cmp(&(phas, seq)))
            .ok()
    }

    /// Where sort key `key` belongs in `members`.
    fn slot_for(&self, key: (i32, u64)) -> usize {
        self.members.partition_point(|probe| (probe.0, probe.1) < key)
    }

    /// Add `id` at priority `phas`, or re-prioritise it if already a member.
    pub fn insert(&mut self, id: RecordId, phas: i32) {
        if let Some(&(old_phas, seq)) = self.index.get(&id) {
            if old_phas == phas {
                return;
            }
            if let Some(pos) = self.locate(old_phas, seq) {
                self.members.remove(pos);
            }
            let key = (phas, seq);
            let at = self.slot_for(key);
            self.members.insert(at, (phas, seq, id));
            self.index.insert(id, key);
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let key = (phas, seq);
        let at = self.slot_for(key);
        self.members.insert(at, (phas, seq, id));
        self.index.insert(id, key);
    }

    /// Take `id` off this list. Returns whether it was present -- the
    /// signature `remove_takes_it_off` asserts on; do not change it to `()`.
    pub fn remove(&mut self, id: RecordId) -> bool {
        let Some((phas, seq)) = self.index.remove(&id) else {
            return false;
        };
        if let Some(pos) = self.locate(phas, seq) {
            self.members.remove(pos);
        }
        true
    }

    /// The processing order for one tick. Already sorted, so this is a copy.
    pub fn snapshot(&self) -> Vec<RecordId> {
        self.members.iter().map(|&(_, _, id)| id).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
    pub fn len(&self) -> usize {
        self.members.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockset::RecordId;
    use std::time::{Duration, Instant};

    fn rid(slot: usize) -> RecordId {
        RecordId { set: 0, slot }
    }

    #[test]
    fn insertion_order_within_same_phas() {
        let mut l = ScanList::new();
        l.insert(rid(1), 0);
        l.insert(rid(2), 0);
        l.insert(rid(3), 0);
        assert_eq!(l.snapshot(), vec![rid(1), rid(2), rid(3)]);
    }

    #[test]
    fn phas_ascending_across_phases() {
        let mut l = ScanList::new();
        l.insert(rid(1), 5);
        l.insert(rid(2), 0);
        l.insert(rid(3), 10);
        l.insert(rid(4), 0);
        // phas 0 first (in insertion order 2,4), then 5, then 10.
        assert_eq!(l.snapshot(), vec![rid(2), rid(4), rid(1), rid(3)]);
    }

    #[test]
    fn remove_takes_it_off() {
        let mut l = ScanList::new();
        l.insert(rid(1), 0);
        l.insert(rid(2), 0);
        assert!(l.remove(rid(1)));
        assert!(!l.remove(rid(1)), "removing twice reports false");
        assert_eq!(l.snapshot(), vec![rid(2)]);
    }

    #[test]
    fn reinsert_updates_phas_without_duplicating() {
        let mut l = ScanList::new();
        l.insert(rid(1), 10);
        l.insert(rid(2), 5);
        l.insert(rid(1), 0); // PHAS write moves rid(1) ahead
        assert_eq!(l.snapshot(), vec![rid(1), rid(2)]);
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn is_empty_tracks_membership() {
        let mut l = ScanList::new();
        assert!(l.is_empty());
        l.insert(rid(1), 0);
        assert!(!l.is_empty());
        l.remove(rid(1));
        assert!(l.is_empty());
    }

    #[test]
    fn a_large_list_loads_and_snapshots_without_quadratic_blowup() {
        // `insert` used to linear-scan the whole vec (so loading n records was
        // O(n^2)) and `snapshot` used to clone + re-sort the whole list on
        // every tick. Both are now O(1)-lookup / O(n)-copy, so this finishes
        // comfortably inside the bound even in a debug build.
        let started = Instant::now();
        let mut list = ScanList::default();
        for slot in 0..50_000 {
            list.insert(rid(slot), 0);
        }
        for _ in 0..200 {
            assert_eq!(list.snapshot().len(), 50_000);
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "50k inserts + 200 snapshots took {:?}; the list is still quadratic",
            started.elapsed()
        );
    }
}
