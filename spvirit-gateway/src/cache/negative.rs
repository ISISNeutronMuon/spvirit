//! Negative-search cache.
//!
//! Remembers PV names that recently failed to resolve upstream, so the
//! gateway can avoid re-searching for them on every incoming request until
//! their entry's TTL expires. Bounded in size: when an insert would exceed
//! `capacity`, the soonest-expiring entry is evicted to make room.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Bounded cache of PV names known to have recently missed upstream
/// resolution, keyed by name with a per-entry expiry deadline.
pub struct NegativeCache {
    ttl: Duration,
    capacity: usize,
    entries: Mutex<HashMap<String, Instant>>,
}

impl NegativeCache {
    /// Build a cache that remembers misses for `ttl`, holding at most
    /// `capacity` entries at a time.
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        NegativeCache {
            ttl,
            capacity,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Record `name` as having missed at `now`; it will be considered
    /// missing until `now + ttl`. If this insert would push the cache past
    /// `capacity`, the entry with the soonest deadline is evicted first.
    pub fn record_miss(&self, name: &str, now: Instant) {
        let deadline = now + self.ttl;
        let mut entries = self.entries.lock().unwrap();
        if !entries.contains_key(name)
            && entries.len() >= self.capacity
            && let Some(evict) = entries
                .iter()
                .min_by_key(|(_, deadline)| **deadline)
                .map(|(name, _)| name.clone())
        {
            entries.remove(&evict);
        }
        entries.insert(name.to_string(), deadline);
    }

    /// Returns true if `name` was recorded as missing and its deadline has
    /// not yet passed as of `now`.
    pub fn is_missing(&self, name: &str, now: Instant) -> bool {
        let entries = self.entries.lock().unwrap();
        match entries.get(name) {
            Some(deadline) => now < *deadline,
            None => false,
        }
    }

    /// Current number of stored entries (expired entries are not proactively
    /// removed; they count until overwritten or evicted).
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn miss_hits_within_ttl_then_expires() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let c = NegativeCache::new(Duration::from_millis(1000), 8);
        c.record_miss("X", t0);
        assert!(c.is_missing("X", t0 + Duration::from_millis(500)));
        assert!(!c.is_missing("X", t0 + Duration::from_millis(1500)));
        assert!(!c.is_missing("Y", t0));
    }

    #[test]
    fn capacity_is_bounded() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let c = NegativeCache::new(Duration::from_secs(60), 2);
        for (i, n) in ["A", "B", "C"].iter().enumerate() {
            c.record_miss(n, t0 + Duration::from_millis(i as u64));
        }
        assert!(c.len() <= 2);
    }
}
