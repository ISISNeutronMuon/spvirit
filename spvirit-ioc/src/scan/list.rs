//! A single scan list: records ordered by PHAS then insertion order.

use crate::lockset::RecordId;

#[derive(Debug, Default)]
pub struct ScanList {
    /// (phas, seq, id). `seq` is a monotonic insertion counter that makes the
    /// PHAS-tie order stable and independent of Vec churn.
    members: Vec<(i32, u64, RecordId)>,
    next_seq: u64,
}

impl ScanList {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `id` at `phas`, or update its `phas` if already present. Order is
    /// realized lazily in `snapshot`, so insert stays O(n) find + push.
    pub fn insert(&mut self, id: RecordId, phas: i32) {
        if let Some(slot) = self.members.iter_mut().find(|(_, _, m)| *m == id) {
            slot.0 = phas;
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.members.push((phas, seq, id));
    }

    /// Remove `id`. Returns whether it was present.
    pub fn remove(&mut self, id: RecordId) -> bool {
        let before = self.members.len();
        self.members.retain(|(_, _, m)| *m != id);
        self.members.len() != before
    }

    /// The processing order for one pass: PHAS ascending, ties by insertion.
    pub fn snapshot(&self) -> Vec<RecordId> {
        let mut ordered = self.members.clone();
        ordered.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        ordered.into_iter().map(|(_, _, id)| id).collect()
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
}
