use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use spvirit_codec::spvd_decode::StructureDesc;
use spvirit_types::NtPayload;

/// One connection's channel tables, shared between the connection task that
/// owns them and the [`MonitorRegistry`](crate::monitor::MonitorRegistry).
///
/// They are shared (rather than staying private to the connection task)
/// because upstream-death teardown runs in the registry: it sends
/// DESTROY_CHANNEL for a monitored channel and must then remove that channel
/// from the connection's tables. `ioid_to_sid` is what lets it do that from a
/// [`MonitorSub`], which carries only `conn_id` and `ioid`.
///
/// A plain `std::sync::Mutex` is correct here — it is only ever held across a
/// map read/write, never across an `.await`.
#[derive(Debug, Default)]
pub struct ChannelTables {
    pub cid_to_sid: HashMap<u32, u32>,
    pub sid_to_cid: HashMap<u32, u32>,
    pub sid_to_pv: HashMap<u32, String>,
    /// Monitor request id → the channel it was created on. Written at monitor
    /// INIT, where both numbers are already in hand.
    pub ioid_to_sid: HashMap<u32, u32>,
}

impl ChannelTables {
    /// Record a newly created channel in all three directions.
    pub fn insert_channel(&mut self, cid: u32, sid: u32, pv: &str) {
        self.cid_to_sid.insert(cid, sid);
        self.sid_to_cid.insert(sid, cid);
        self.sid_to_pv.insert(sid, pv.to_string());
    }

    /// Forget a destroyed channel, along with any monitor bindings on it.
    ///
    /// Leaving any of them behind lets a client that re-searches on the same
    /// TCP connection grow this table without bound, one entry per flap.
    pub fn remove_channel(&mut self, cid: u32, sid: u32) {
        self.cid_to_sid.remove(&cid);
        self.sid_to_cid.remove(&sid);
        self.sid_to_pv.remove(&sid);
        self.ioid_to_sid.retain(|_, s| *s != sid);
    }

    /// Forget destroyed channel `sid`, but leave `cid`'s row alone if that cid
    /// has since been re-bound to a *newer* channel. Returns whether `cid` did
    /// still point back at `sid` — i.e. whether the destroy names a channel the
    /// client still has under that cid.
    ///
    /// A client may re-create a channel on the same cid without first sending
    /// DestroyChannel; `cid_to_sid[cid]` then belongs to the newer sid while
    /// our `sid_to_cid[sid]` row lives on. The plain [`Self::remove_channel`]
    /// strips that cid row unconditionally, after which
    /// `MonitorRegistry::destroy_subs` sees no owner for the cid, declines to
    /// send, and the client is silently skipped on the next upstream death —
    /// the exact silence the destroy path exists to remove. Every caller that
    /// removes a channel it was *told about by the client* (whose sid may
    /// therefore be stale) must come through here.
    pub fn remove_channel_if_current(&mut self, cid: u32, sid: u32) -> bool {
        let current_owner = self.cid_to_sid.get(&cid).copied();
        self.remove_channel(cid, sid);
        if current_owner == Some(sid) {
            return true;
        }
        // `remove_channel` also dropped the cid row, which belongs to the newer
        // channel: put it back.
        if let Some(other) = current_owner {
            self.cid_to_sid.insert(cid, other);
        }
        false
    }

    /// Record that monitor `ioid` runs on channel `sid`.
    pub fn bind_monitor(&mut self, ioid: u32, sid: u32) {
        self.ioid_to_sid.insert(ioid, sid);
    }

    /// Forget a monitor subscription's channel binding.
    pub fn unbind_monitor(&mut self, ioid: u32) {
        self.ioid_to_sid.remove(&ioid);
    }

    /// The client's channel id for one of our server ids, if the channel is
    /// still open.
    pub fn cid_for_sid(&self, sid: u32) -> Option<u32> {
        self.sid_to_cid.get(&sid).copied()
    }

    /// The `(sid, cid)` pair a monitor subscription is running on, if both the
    /// binding and the channel are still live.
    pub fn channel_for_monitor(&self, ioid: u32) -> Option<(u32, u32)> {
        let sid = self.ioid_to_sid.get(&ioid).copied()?;
        let cid = self.sid_to_cid.get(&sid).copied()?;
        Some((sid, cid))
    }
}

/// A [`ChannelTables`] handle shared by a connection task and the registry.
pub type SharedChannelTables = Arc<Mutex<ChannelTables>>;

#[derive(Debug, Default)]
pub struct ConnState {
    pub channels: SharedChannelTables,
    pub ioid_to_desc: HashMap<u32, StructureDesc>,
    pub ioid_to_pv: HashMap<u32, String>,
    pub ioid_to_monitor: HashMap<u32, MonitorState>,
}

#[derive(Debug, Clone)]
pub struct MonitorSub {
    pub conn_id: u64,
    pub ioid: u32,
    pub version: u8,
    pub is_be: bool,
    pub running: bool,
    pub pipeline_enabled: bool,
    pub nfree: u32,
    /// When set, only encode these fields in monitor data responses.
    pub filtered_desc: Option<StructureDesc>,
    /// Last payload sent to this subscriber. Used purely as a change detector:
    /// each monitor frame is a self-contained, fully-filtered snapshot (see
    /// `spvirit-server/src/monitor.rs`), and this baseline decides whether a
    /// new update differs enough to post — it is never emitted as a sparse
    /// delta on the wire. `None` means the next update is the initial snapshot.
    pub last_snapshot: Option<NtPayload>,
}

#[derive(Debug, Clone, Copy)]
pub struct MonitorState {
    pub running: bool,
    pub pipeline_enabled: bool,
    pub nfree: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec section 6's unbounded-growth guard: a destroyed channel must leave
    /// ALL the tables, not just the one the destroy path happens to remember.
    /// A client that re-searches on the same TCP connection gets a fresh
    /// cid/sid each time, so a leftover entry per flap grows the
    /// per-connection tables without bound against the 40000-channel ceiling.
    #[test]
    fn remove_channel_clears_every_table_so_a_flapping_client_cannot_grow_them() {
        let mut t = ChannelTables::default();
        t.insert_channel(11, 22, "PV:X");
        t.bind_monitor(33, 22);
        assert_eq!(t.cid_to_sid.get(&11).copied(), Some(22));
        assert_eq!(t.sid_to_pv.get(&22).map(String::as_str), Some("PV:X"));
        assert_eq!(t.cid_for_sid(22), Some(11));

        t.remove_channel(11, 22);
        assert!(t.cid_to_sid.is_empty(), "cid_to_sid must be cleared");
        assert!(t.sid_to_cid.is_empty(), "sid_to_cid must be cleared");
        assert!(t.sid_to_pv.is_empty(), "sid_to_pv must be cleared");
        assert!(
            t.ioid_to_sid.is_empty(),
            "a destroyed channel must take its monitor bindings with it"
        );
        assert_eq!(t.cid_for_sid(22), None);
    }

    /// A client's *echoed* DestroyChannel names a sid the client chose, so it
    /// may be stale: the client can re-create a channel on the same cid (after
    /// a server-initiated destroy, which is now the designed recovery path)
    /// and only then send the destroy for the old sid. Stripping `cid_to_sid`
    /// unconditionally there loses the live channel's row, after which
    /// `MonitorRegistry::destroy_subs` finds no owner for that cid, declines to
    /// send, and the client goes silent on the next upstream death.
    #[test]
    fn a_stale_destroy_must_not_evict_the_cid_row_of_a_newer_channel() {
        let mut t = ChannelTables::default();
        t.insert_channel(9, 7, "PV:X");
        t.bind_monitor(42, 7);
        // Client re-creates on the same cid, then echoes the old destroy.
        t.insert_channel(9, 8, "PV:X");
        t.bind_monitor(43, 8);

        assert!(
            !t.remove_channel_if_current(9, 7),
            "cid 9 no longer belongs to sid 7, so this destroy is stale"
        );
        assert_eq!(
            t.cid_to_sid.get(&9).copied(),
            Some(8),
            "the live channel's cid row must survive a stale destroy"
        );
        assert_eq!(
            t.channel_for_monitor(43),
            Some((8, 9)),
            "the live monitor must still resolve, or its DESTROY_CHANNEL is \
             never sent"
        );
        assert!(
            !t.sid_to_cid.contains_key(&7)
                && !t.sid_to_pv.contains_key(&7)
                && !t.ioid_to_sid.contains_key(&42),
            "the stale channel's own rows must still be retracted"
        );

        // The ordinary, non-stale case still removes everything.
        assert!(t.remove_channel_if_current(9, 8));
        assert!(t.cid_to_sid.is_empty() && t.sid_to_cid.is_empty());
        assert!(t.sid_to_pv.is_empty() && t.ioid_to_sid.is_empty());
    }

    /// `channel_for_monitor` is how Layer 2 turns a `MonitorSub` (which knows
    /// only conn_id and ioid) into the sid/cid pair DESTROY_CHANNEL needs —
    /// the reason `MonitorSub` itself did not have to grow two fields.
    #[test]
    fn channel_for_monitor_resolves_a_subscription_to_its_sid_and_cid() {
        let mut t = ChannelTables::default();
        t.insert_channel(1, 100, "PV:A");
        t.insert_channel(2, 200, "PV:A");
        // Two channels on the SAME PV over one connection: each subscription
        // must resolve to its own channel, which a name-based scan could not do.
        t.bind_monitor(10, 100);
        t.bind_monitor(20, 200);
        assert_eq!(t.channel_for_monitor(10), Some((100, 1)));
        assert_eq!(t.channel_for_monitor(20), Some((200, 2)));
        assert_eq!(t.channel_for_monitor(99), None, "unknown ioid");

        t.unbind_monitor(10);
        assert_eq!(
            t.channel_for_monitor(10),
            None,
            "an unsubscribed monitor must no longer resolve to a channel"
        );
        assert_eq!(t.channel_for_monitor(20), Some((200, 2)));
    }

    /// Task 6 will register these handles with the `MonitorRegistry` by
    /// `conn_id`, so a per-connection table that turned out to be shared
    /// storage would let one connection's ioids resolve to another's channels.
    /// Each `ConnState` must own an independent `ChannelTables`.
    #[test]
    fn each_connection_gets_its_own_channel_tables() {
        let a = ConnState::default();
        let b = ConnState::default();
        a.channels.lock().unwrap().insert_channel(1, 100, "PV:A");
        a.channels.lock().unwrap().bind_monitor(10, 100);

        let tb = b.channels.lock().unwrap();
        assert!(
            tb.cid_to_sid.is_empty() && tb.sid_to_cid.is_empty() && tb.ioid_to_sid.is_empty(),
            "a second connection must not see the first connection's channels"
        );
        assert_eq!(tb.channel_for_monitor(10), None);
        drop(tb);

        // And dropping a connection drops its tables: nothing survives to be
        // resolved against a later connection reusing the same ids.
        let weak = Arc::downgrade(&a.channels);
        drop(a);
        assert!(
            weak.upgrade().is_none(),
            "a connection's channel tables must not outlive the connection"
        );
    }
}
