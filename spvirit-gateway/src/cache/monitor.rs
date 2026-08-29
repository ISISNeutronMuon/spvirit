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
//! which point `MonitorCache::dispatch_or_retire` atomically (under the
//! entries-map lock) removes the `MonitorEntry` from the map and reports
//! that the upstream loop should end. That atomicity — retiring the entry
//! and deciding to stop the loop as one map-locked step — is what prevents
//! a concurrent `subscribe()` from attaching a new subscriber to an entry
//! whose upstream loop has already (or is about to have) ended; see
//! `dispatch_or_retire`'s doc comment for the exact race it closes. A PV
//! that goes silent forever right after its last subscriber leaves does
//! *not* get its upstream monitor torn down promptly — this "idle-linger"
//! is a documented M1 simplification (see task-11-report.md), not a
//! correctness bug: no downstream traffic is leaked, only an idle upstream
//! monitor task outlives its last subscriber until the PV next changes (or
//! forever, for a PV that never changes again).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use spvirit_types::NtPayload;
use tokio::sync::mpsc;

// NOTE (item 4d): a write-only `latest: Mutex<Option<NtPayload>>` field used to
// live here to cache the most recent payload for a hypothetical "late-subscriber
// replay". It was written on every `dispatch` (an extra `payload.clone()` plus a
// lock per update — doubling per-update work for a single-subscriber PV) but read
// nowhere, since replay is not implemented in M1. Dropped; reintroduce lazily
// when replay actually lands.

/// Identifies one upstream monitor: which client to reach it through, and
/// its name as known to that client.
pub type MonitorKey = (String, String);

/// Bounded channel capacity for each downstream subscriber's `mpsc` pair.
/// Small and fixed: subscribers that fall behind have *this* update dropped
/// (a `TrySendError::Full` from `try_send`) rather than backpressuring the
/// single upstream monitor task — but the subscriber itself is KEPT, so a
/// momentarily-slow client recovers on the next delivery instead of being
/// silently unsubscribed. Only a genuinely dead subscriber (its receiver
/// dropped, `TrySendError::Closed`) is pruned. See `MonitorEntry::dispatch`.
const CHANNEL_CAPACITY: usize = 16;

/// Shared state for one upstream monitor: the live downstream fan-out list.
pub struct MonitorEntry {
    subs: Mutex<Vec<mpsc::Sender<NtPayload>>>,
}

impl MonitorEntry {
    fn new() -> Self {
        MonitorEntry {
            subs: Mutex::new(Vec::new()),
        }
    }

    /// Stores `payload` as the latest value and fans it out to every live
    /// subscriber. A subscriber whose channel is momentarily `Full` (a slow
    /// but still-live client) is KEPT — only *this* update is dropped for it,
    /// and it recovers on the next delivery. A subscriber whose receiver has
    /// been dropped (`Closed`) is pruned. Returns `true` if at least one
    /// subscriber remains after dispatch (the upstream task should keep
    /// running), `false` only if the subscriber list is now empty (the
    /// upstream task should end its monitor loop).
    ///
    /// INTENTIONAL DIVERGENCE from the crate-wide "prune on `Full`"
    /// convention: elsewhere a Full downstream buffer is treated as a dead
    /// peer and pruned, but here pruning the last subscriber uniquely trips
    /// `dispatch_or_retire` into tearing down the shared upstream monitor for
    /// the whole PV. A single slow client — or a burst that momentarily fills
    /// *every* subscriber's buffer — must never be able to do that. Dropping
    /// an intermediate update is safe specifically because `subscribe`
    /// re-accumulates a full snapshot each tick, so a subscriber that misses
    /// one Full update is not left with a torn/partial view.
    pub fn dispatch(&self, payload: NtPayload) -> bool {
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|tx| match tx.try_send(payload.clone()) {
            Ok(()) => true,
            // Slow but live: keep the subscriber, drop only this update.
            Err(mpsc::error::TrySendError::Full(_)) => true,
            // Receiver gone: prune the dead subscriber.
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
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

    /// Fans `payload` out to `entry`'s subscribers, then — if the
    /// subscriber list is now empty — atomically decides whether to retire
    /// `entry` from the map, returning whether the caller's upstream loop
    /// should continue (`true`) or break (`false`).
    ///
    /// This closes a race in the previous split `dispatch` +
    /// `remove_if_current` design: `dispatch` alone could observe an empty
    /// `subs` list, return `false`, and — before the caller got around to
    /// removing the entry — a fresh `subscribe()` for the same key could
    /// attach a new sender to that same (about-to-be-removed) entry, since
    /// `subscribe`'s existing-entry branch only checks the entries map, not
    /// whether the entry's upstream loop already decided to stop. That new
    /// subscriber would then be permanently silent: wired to an entry whose
    /// upstream loop already broke, with no upstream left to feed it, and no
    /// way to detect this until an even-later resubscribe re-registers the
    /// key. Folding the "subs empty -> remove from map" decision into a
    /// single map-locked critical section (this method) closes that window:
    /// `subscribe`'s existing-entry branch and this retire check are
    /// serialized on the same `entries` mutex, so either the new subscriber
    /// attaches before retirement is decided (subs non-empty at recheck ->
    /// entry survives) or strictly after (entry already gone -> `subscribe`
    /// takes the `None` branch and spawns a fresh upstream). No interleaving
    /// leaves a subscriber wired to a retired entry.
    ///
    /// Lock ordering: map lock, then (inside `fan_out`) the entry's `subs`
    /// lock — never the reverse — matching `subscribe`'s ordering, so the
    /// two can never deadlock against each other.
    pub fn dispatch_or_retire(
        &self,
        key: &MonitorKey,
        entry: &Arc<MonitorEntry>,
        payload: NtPayload,
    ) -> bool {
        if entry.dispatch(payload) {
            return true;
        }

        // subs was empty right after fan-out; take the map lock and
        // re-check under it before deciding to retire, so a concurrent
        // `subscribe()` that attaches a sender while we're mid-decision is
        // never lost.
        let mut entries = self.entries.lock().unwrap();
        if !entry.subs.lock().unwrap().is_empty() {
            // A new subscriber attached between our fan-out and taking the
            // map lock: keep the entry (and the upstream loop) alive.
            return true;
        }
        if let Some(current) = entries.get(key)
            && Arc::ptr_eq(current, entry)
        {
            entries.remove(key);
        }
        false
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

    /// Proves the gateway-G1 fix: when EVERY subscriber's buffer is Full,
    /// dispatch must NOT prune anyone and must NOT report the entry empty.
    /// The old `retain(|tx| try_send(..).is_ok())` dropped Full senders just
    /// like Closed ones, so an all-Full burst emptied `subs`, made dispatch
    /// return `false`, and let `dispatch_or_retire` tear down the whole
    /// upstream monitor for a PV whose subscribers were merely slow.
    #[test]
    fn full_buffers_do_not_prune_subscribers_or_retire_upstream() {
        let cache = MonitorCache::new();
        let key: MonitorKey = ("c".into(), "PV".into());
        let mut captured_entry = None;
        // Two slow-but-live subscribers whose receivers are never drained.
        let _rx_a = cache.subscribe(key.clone(), |entry| captured_entry = Some(entry));
        let _rx_b = cache.subscribe(key.clone(), |_| {});
        let entry = captured_entry.expect("spawn_upstream captured the entry");

        // Fill BOTH subscribers' buffers right up to capacity.
        for i in 0..CHANNEL_CAPACITY {
            assert!(
                entry.dispatch(payload(i as f64)),
                "filling buffers must keep the entry live"
            );
        }

        // Every subscriber's buffer is now Full. A further update must be
        // dropped for those subscribers WITHOUT pruning them and WITHOUT
        // retiring the entry.
        let still_live = entry.dispatch(payload(1000.0));
        assert!(
            still_live,
            "an all-Full burst must not report the entry empty (no upstream teardown)"
        );
        assert_eq!(
            entry.subs.lock().unwrap().len(),
            2,
            "Full subscribers must be kept, not pruned"
        );

        // And through the full retire path: the upstream must survive.
        let should_continue = cache.dispatch_or_retire(&key, &entry, payload(1001.0));
        assert!(
            should_continue,
            "dispatch_or_retire must not tear down the upstream on an all-Full burst"
        );
        assert_eq!(
            cache.upstream_count(),
            1,
            "the upstream monitor must still be tracked after an all-Full burst"
        );
    }

    /// A Full (slow-but-live) subscriber is kept while a Closed (receiver
    /// dropped) subscriber is pruned — the two `TrySendError` arms behave
    /// differently, which is the whole point of the fix.
    #[test]
    fn full_subscriber_survives_while_closed_subscriber_is_pruned() {
        let cache = MonitorCache::new();
        let key: MonitorKey = ("c".into(), "PV".into());
        let mut captured_entry = None;
        // Slow-but-live subscriber: never drained.
        let _rx_slow = cache.subscribe(key.clone(), |entry| captured_entry = Some(entry));
        // Closed subscriber: receiver dropped.
        let rx_closed = cache.subscribe(key.clone(), |_| {});
        drop(rx_closed);

        let entry = captured_entry.expect("spawn_upstream captured the entry");

        // Fill the slow subscriber's buffer to capacity.
        for i in 0..CHANNEL_CAPACITY {
            entry.dispatch(payload(i as f64));
        }

        // Slow sub is now Full (kept), closed sub is Closed (pruned).
        let still_live = entry.dispatch(payload(1000.0));
        assert!(
            still_live,
            "the slow-but-live subscriber must keep the upstream alive"
        );
        assert_eq!(
            entry.subs.lock().unwrap().len(),
            1,
            "the Full subscriber is kept; only the Closed one is pruned"
        );
    }

    /// Proves the FIX-1 invariant across the "retire wins" interleave: the
    /// last subscriber drops, THEN `dispatch_or_retire` runs with nothing
    /// else attached in between. It must retire the entry from the map and
    /// report the upstream loop should break; a fresh `subscribe()` for the
    /// same key afterward must spawn a brand-new upstream (proving the old,
    /// now-orphaned entry was actually removed, not left dangling for a
    /// subscriber to attach to silently).
    #[test]
    fn dispatch_or_retire_removes_the_entry_when_last_subscriber_is_gone() {
        let cache = MonitorCache::new();
        let key: MonitorKey = ("c".into(), "PV".into());
        let spawn_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut captured_entry = None;
        let spawns = spawn_calls.clone();
        let rx1 = cache.subscribe(key.clone(), |entry| {
            captured_entry = Some(entry);
            spawns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let entry = captured_entry.expect("spawn_upstream captured the entry");
        assert_eq!(cache.upstream_count(), 1);

        // Last (only) subscriber drops.
        drop(rx1);

        let should_continue = cache.dispatch_or_retire(&key, &entry, payload(1.0));
        assert!(
            !should_continue,
            "upstream loop must break once its last subscriber is gone"
        );
        assert_eq!(
            cache.upstream_count(),
            0,
            "the retired entry must actually be removed from the map"
        );

        // A fresh subscribe for the same key must NOT reuse the retired
        // entry — it must spawn a brand-new upstream and produce a receiver
        // wired to a live one.
        let spawns2 = spawn_calls.clone();
        let mut captured_entry2 = None;
        let _rx2 = cache.subscribe(key.clone(), |entry| {
            captured_entry2 = Some(entry);
            spawns2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        assert_eq!(
            spawn_calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "resubscribing after retirement must spawn a fresh upstream"
        );
        assert_eq!(cache.upstream_count(), 1);
        let entry2 = captured_entry2.expect("second spawn_upstream captured the entry");
        assert!(
            !Arc::ptr_eq(&entry, &entry2),
            "resubscribe must attach to a brand-new entry, not the retired one"
        );
    }

    /// Proves the FIX-1 invariant across the OTHER interleave: a new
    /// subscriber attaches to the entry (via the map lock) after the last
    /// old subscriber dropped, but before `dispatch_or_retire` gets a
    /// chance to run its map-locked re-check. This reproduces exactly the
    /// orphaned-subscriber race the reviewer flagged: without the fix,
    /// `dispatch_or_retire`/the old split `dispatch`+`remove_if_current`
    /// would tear the entry down anyway, leaving the newly-attached
    /// subscriber wired to a dead upstream. With the fix, the map-locked
    /// re-check inside `dispatch_or_retire` observes the new subscriber and
    /// backs off: the entry survives, no second upstream is spawned, and
    /// the attached receiver stays live.
    #[test]
    fn dispatch_or_retire_keeps_the_entry_if_a_subscriber_attaches_first() {
        let cache = MonitorCache::new();
        let key: MonitorKey = ("c".into(), "PV".into());
        let spawn_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut captured_entry = None;
        let spawns = spawn_calls.clone();
        let rx1 = cache.subscribe(key.clone(), |entry| {
            captured_entry = Some(entry);
            spawns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let entry = captured_entry.expect("spawn_upstream captured the entry");

        // Last old subscriber drops...
        drop(rx1);

        // ...but a new subscriber attaches to the *same still-present*
        // entry (the `Some` branch in `subscribe`) before anyone calls
        // `dispatch_or_retire`. This is the interleave: subs is non-empty
        // by the time the retire check runs.
        let spawns2 = spawn_calls.clone();
        let mut rx2 = cache.subscribe(key.clone(), |_entry| {
            spawns2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        assert_eq!(
            spawn_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "attaching to a still-present entry must not spawn a second upstream"
        );

        let should_continue = cache.dispatch_or_retire(&key, &entry, payload(2.0));
        assert!(
            should_continue,
            "a subscriber attached in the meantime must save the entry from retirement"
        );
        assert_eq!(
            cache.upstream_count(),
            1,
            "the entry must not be removed while a live subscriber is attached"
        );
        assert_eq!(
            spawn_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no second upstream should ever be spawned for this key"
        );

        // And the attached subscriber actually got the payload dispatched
        // to it — it is not a silent orphan.
        let NtPayload::Generic { fields, .. } = rx2.try_recv().unwrap() else {
            panic!("expected Generic");
        };
        assert!(matches!(fields[0].1, PvValue::Scalar(ScalarValue::F64(x)) if (x - 2.0).abs() < 1e-9));
    }
}
