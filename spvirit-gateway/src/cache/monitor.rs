//! Monitor deduplication + fan-out cache.
//!
//! The gateway must never open more than one upstream `pvmonitor` per
//! `(client, real_name)` pair no matter how many downstream subscribers ask
//! for it: the first `subscribe` call starts the single upstream monitor
//! task, every subsequent `subscribe` for the same key just adds another
//! downstream `mpsc::Sender`/`Receiver` pair fed by that one task's updates.
//!
//! Cancellation for M1 is "next-tick": the upstream task notices its
//! `subs` list is empty only when it goes to deliver the *next* update, at
//! which point it returns `ControlFlow::Break(())` and the `MonitorEntry`
//! is dropped from the map on the following `subscribe`/lookup that finds
//! it stale (the entry is removed proactively by the upstream task itself,
//! see `MonitorCache::retain_and_maybe_remove`). A PV that goes silent
//! forever right after its last subscriber leaves does *not* get its
//! upstream monitor torn down promptly — this "idle-linger" is a
//! documented M1 simplification (see task-11-report.md), not a correctness
//! bug: no downstream traffic is leaked, only an idle upstream monitor task
//! outlives its last subscriber until the PV next changes (or forever, for
//! a PV that never changes again).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use spvirit_types::NtPayload;
use tokio::sync::mpsc;

/// Identifies one upstream monitor: which client to reach it through, and
/// its name as known to that client.
pub type MonitorKey = (String, String);

/// Bounded channel capacity for each downstream subscriber's `mpsc` pair.
/// Small and fixed: subscribers that fall behind get updates dropped
/// (`try_send` failure) rather than backpressuring the single upstream
/// monitor task, and a slow/dead subscriber's sender is pruned on the next
/// delivery attempt (see `MonitorEntry::dispatch`).
const CHANNEL_CAPACITY: usize = 16;

/// Shared state for one upstream monitor: its most recent payload (for
/// future late-subscriber replay, unused by M1's `subscribe` but kept for
/// forward compatibility) and the live downstream fan-out list.
pub struct MonitorEntry {
    pub latest: Mutex<Option<NtPayload>>,
    subs: Mutex<Vec<mpsc::Sender<NtPayload>>>,
}

impl MonitorEntry {
    fn new() -> Self {
        MonitorEntry {
            latest: Mutex::new(None),
            subs: Mutex::new(Vec::new()),
        }
    }

    /// Stores `payload` as the latest value and fans it out to every live
    /// subscriber, pruning any sender whose receiver has been dropped or
    /// whose channel is full. Returns `true` if at least one subscriber is
    /// still live after dispatch (the upstream task should keep running),
    /// `false` if the subscriber list is now empty (the upstream task
    /// should end its monitor loop).
    pub fn dispatch(&self, payload: NtPayload) -> bool {
        *self.latest.lock().unwrap() = Some(payload.clone());
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|tx| tx.try_send(payload.clone()).is_ok());
        !subs.is_empty()
    }
}

/// Dedup + fan-out cache for upstream monitors, keyed by `(client_name,
/// real_name)`.
pub struct MonitorCache {
    entries: Mutex<HashMap<MonitorKey, Arc<MonitorEntry>>>,
}

impl Default for MonitorCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorCache {
    pub fn new() -> Self {
        MonitorCache {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Subscribes to `key`. If an upstream monitor for `key` is already
    /// running, just registers a new `Sender` on its existing entry and
    /// returns the paired `Receiver` — no new upstream task is spawned.
    /// Otherwise creates a fresh entry, registers the `Sender`, calls
    /// `spawn_upstream(entry)` exactly once to start the single upstream
    /// monitor task for this key, and returns the `Receiver`.
    pub fn subscribe(
        &self,
        key: MonitorKey,
        spawn_upstream: impl FnOnce(Arc<MonitorEntry>),
    ) -> mpsc::Receiver<NtPayload> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let mut entries = self.entries.lock().unwrap();
        match entries.get(&key) {
            Some(entry) => {
                entry.subs.lock().unwrap().push(tx);
            }
            None => {
                let entry = Arc::new(MonitorEntry::new());
                entry.subs.lock().unwrap().push(tx);
                entries.insert(key, entry.clone());
                // Release the map lock before spawning: `spawn_upstream` is
                // expected to `tokio::spawn` a task that will eventually
                // call back into this cache (e.g. to remove the entry), and
                // must never be invoked while the lock guard is held.
                drop(entries);
                spawn_upstream(entry);
                return rx;
            }
        }
        rx
    }

    /// Removes `key`'s entry if it is still the exact entry passed in (an
    /// `Arc` pointer-equality check guards against races where a new
    /// subscriber recreated the entry between the upstream task observing
    /// an empty `subs` list and calling this).
    pub fn remove_if_current(&self, key: &MonitorKey, entry: &Arc<MonitorEntry>) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(current) = entries.get(key)
            && Arc::ptr_eq(current, entry)
        {
            entries.remove(key);
        }
    }

    /// Number of distinct upstream monitors currently tracked (live
    /// entries, i.e. keys with at least one past-or-present subscriber that
    /// have not yet been torn down). Used by tests to prove dedup.
    pub fn upstream_count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_types::{NtPayload, PvValue, ScalarValue};

    fn payload(v: f64) -> NtPayload {
        NtPayload::Generic {
            struct_id: String::new(),
            fields: vec![("value".to_string(), PvValue::Scalar(ScalarValue::F64(v)))],
        }
    }

    #[test]
    fn second_subscribe_for_same_key_does_not_spawn_a_new_upstream() {
        let cache = MonitorCache::new();
        let key: MonitorKey = ("c".into(), "PV".into());
        let mut spawn_calls = 0;

        let _rx1 = cache.subscribe(key.clone(), |_entry| {
            spawn_calls += 1;
        });
        assert_eq!(cache.upstream_count(), 1);

        let _rx2 = cache.subscribe(key.clone(), |_entry| {
            spawn_calls += 1;
        });
        assert_eq!(
            spawn_calls, 1,
            "spawn_upstream must run exactly once per key"
        );
        assert_eq!(cache.upstream_count(), 1);
    }

    #[test]
    fn dispatch_fans_out_to_every_live_subscriber() {
        let cache = MonitorCache::new();
        let key: MonitorKey = ("c".into(), "PV".into());
        let mut captured_entry = None;
        let mut rx1 = cache.subscribe(key.clone(), |entry| captured_entry = Some(entry));
        let mut rx2 = cache.subscribe(key.clone(), |_| {});

        let entry = captured_entry.expect("spawn_upstream captured the entry");
        let still_live = entry.dispatch(payload(3.5));
        assert!(still_live);

        let NtPayload::Generic { fields, .. } = rx1.try_recv().unwrap() else {
            panic!("expected Generic");
        };
        assert!(matches!(fields[0].1, PvValue::Scalar(ScalarValue::F64(x)) if (x - 3.5).abs() < 1e-9));
        let NtPayload::Generic { fields, .. } = rx2.try_recv().unwrap() else {
            panic!("expected Generic");
        };
        assert!(matches!(fields[0].1, PvValue::Scalar(ScalarValue::F64(x)) if (x - 3.5).abs() < 1e-9));
    }

    #[test]
    fn dispatch_reports_empty_once_all_receivers_dropped() {
        let cache = MonitorCache::new();
        let key: MonitorKey = ("c".into(), "PV".into());
        let mut captured_entry = None;
        let rx = cache.subscribe(key.clone(), |entry| captured_entry = Some(entry));
        drop(rx);

        let entry = captured_entry.expect("spawn_upstream captured the entry");
        let still_live = entry.dispatch(payload(1.0));
        assert!(!still_live, "no live subscribers left after drop");
    }
}
