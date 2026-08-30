//! Monitor subscription management for the PVA server.
//!
//! Tracks per-PV subscriber lists and dispatches monitor update messages.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::debug;

use spvirit_codec::spvirit_encode::{
    encode_monitor_data_response_delta, encode_monitor_data_response_filtered,
    encode_monitor_data_response_payload,
};
use spvirit_types::NtPayload;

use crate::conn_writer::ConnWriter;
use crate::state::MonitorSub;

/// Active connection channels and monitor subscriptions managed by the server.
pub struct MonitorRegistry {
    /// PV name → list of active monitor subscriptions.
    pub monitors: Mutex<HashMap<String, Vec<MonitorSub>>>,
    /// Connection id → its flat-combining writer.
    pub conns: Mutex<HashMap<u64, Arc<ConnWriter>>>,
    /// PV name → the task draining a subscribe-only source's update stream
    /// into `notify_monitors`. One pump per PV, shared by every subscriber of
    /// that PV; retired once the last subscriber goes away. Sources that
    /// deliver their own updates ([`Source::pushes_own_updates`]) never get a
    /// pump entry — pumping them would double-deliver.
    pumps: Mutex<HashMap<String, PumpHandle>>,
    /// The diagnostic [`ClientRegistry`](crate::diag::ClientRegistry) this
    /// registry's connection lifecycle should populate, if one was installed
    /// via [`Self::set_client_registry`]. `None` for servers that don't opt
    /// into downstream-client tracking. A plain `std::sync::Mutex` (not the
    /// `tokio::sync::Mutex` used elsewhere in this struct) because it's only
    /// ever held across a clone/assign, never across an `.await`.
    client_registry: std::sync::Mutex<Option<Arc<crate::diag::ClientRegistry>>>,
    /// The diagnostic [`BandwidthCounters`](crate::diag::BandwidthCounters)
    /// this registry's connection handler and monitor dispatch should record
    /// wire bytes into, if one was installed via
    /// [`Self::set_bandwidth_counters`]. `None` for servers that don't opt
    /// into bandwidth accounting. A plain `std::sync::Mutex` for the same
    /// reason as `client_registry`: only ever held across a clone/assign,
    /// never across an `.await`.
    bandwidth_counters: std::sync::Mutex<Option<Arc<crate::diag::BandwidthCounters>>>,
}

/// A pump task plus its cooperative-shutdown signal.
///
/// Retirement signals `shutdown` rather than aborting the task: an abort could
/// drop the pump future mid-`write_all` inside a shared [`ConnWriter`], wedging
/// that connection's flusher. Cooperative shutdown lets any in-flight
/// `notify_monitors` (and its socket write) finish before the task exits.
struct PumpHandle {
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl MonitorRegistry {
    pub fn new() -> Self {
        Self {
            monitors: Mutex::new(HashMap::new()),
            conns: Mutex::new(HashMap::new()),
            pumps: Mutex::new(HashMap::new()),
            client_registry: std::sync::Mutex::new(None),
            bandwidth_counters: std::sync::Mutex::new(None),
        }
    }

    /// Install the diagnostic [`ClientRegistry`](crate::diag::ClientRegistry)
    /// this registry's connection lifecycle (`cleanup_connection`) should
    /// notify on disconnect. Threaded down from
    /// `PvaServerBuilder::client_registry` via
    /// `PvaServer::resolved_monitor_registry`, which calls this every time
    /// it resolves the registry — so it's safe to call more than once.
    pub fn set_client_registry(&self, registry: Arc<crate::diag::ClientRegistry>) {
        *self.client_registry.lock().unwrap() = Some(registry);
    }

    /// The installed diagnostic client registry, if any.
    pub fn client_registry(&self) -> Option<Arc<crate::diag::ClientRegistry>> {
        self.client_registry.lock().unwrap().clone()
    }

    /// Install the diagnostic
    /// [`BandwidthCounters`](crate::diag::BandwidthCounters) that this
    /// registry's connection handler and monitor dispatch should record wire
    /// bytes into. Threaded down from
    /// `PvaServerBuilder::bandwidth_counters` via
    /// `PvaServer::resolved_monitor_registry`, which calls this every time it
    /// resolves the registry — so it's safe to call more than once.
    pub fn set_bandwidth_counters(&self, counters: Arc<crate::diag::BandwidthCounters>) {
        *self.bandwidth_counters.lock().unwrap() = Some(counters);
    }

    /// The installed diagnostic bandwidth counters, if any.
    pub fn bandwidth_counters(&self) -> Option<Arc<crate::diag::BandwidthCounters>> {
        self.bandwidth_counters.lock().unwrap().clone()
    }

    /// Ensure a single pump task is draining `rx` (a subscribe-only source's
    /// update stream) into `notify_monitors` for `pv_name`.
    ///
    /// If a pump already exists for this PV, `rx` is dropped and this is a
    /// no-op — the existing pump already fans out to every subscriber via the
    /// shared monitor list. Otherwise a task is spawned that forwards each
    /// payload until the source closes the stream or the registry is dropped.
    ///
    /// The task holds only a [`Weak`] reference to the registry so it never
    /// keeps the registry (which owns the pump's `JoinHandle`) alive — that
    /// would be a reference cycle. When the registry is gone, `upgrade` fails
    /// and the task exits.
    pub async fn ensure_pump(self: &Arc<Self>, pv_name: &str, rx: mpsc::Receiver<NtPayload>) {
        let mut pumps = self.pumps.lock().await;
        if pumps.contains_key(pv_name) {
            // Existing pump already feeds all subscribers; drop the extra rx.
            return;
        }
        let weak = Arc::downgrade(self);
        let pv = pv_name.to_string();
        let mut rx = rx;
        let (shutdown, mut shutdown_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Cooperative shutdown: retirement fires this. Cancellation
                    // can only take effect here at the top of the loop, never
                    // inside an in-flight `notify_monitors` — so a socket write
                    // is never dropped mid-frame.
                    _ = &mut shutdown_rx => break,
                    maybe = rx.recv() => {
                        let Some(payload) = maybe else { break };
                        let Some(reg) = Weak::upgrade(&weak) else { break };
                        reg.notify_monitors(&pv, &payload).await;
                    }
                }
            }
        });
        pumps.insert(pv_name.to_string(), PumpHandle { shutdown, handle });
    }

    /// Retire the pump for `pv_name` if no subscribers remain for it.
    ///
    /// Callers must have already removed the relevant subscriptions from
    /// `monitors` (and released that lock) before calling this.
    ///
    /// Shutdown is cooperative (signal, not abort): the pump finishes any
    /// in-flight `notify_monitors` — including its socket write — before
    /// exiting, so a flush is never dropped mid-write to wedge the shared
    /// [`ConnWriter`].
    async fn retire_pump_if_idle(&self, pv_name: &str) {
        let still_active = {
            let monitors = self.monitors.lock().await;
            monitors.get(pv_name).is_some_and(|list| !list.is_empty())
        };
        if still_active {
            return;
        }
        let mut pumps = self.pumps.lock().await;
        if let Some(PumpHandle { shutdown, handle }) = pumps.remove(pv_name) {
            // Signal cooperative shutdown; the task exits at its next loop turn.
            let _ = shutdown.send(());
            // Detach: the task stops on its own. Do NOT abort (would risk
            // dropping a flush mid-write).
            drop(handle);
        }
    }

    /// Look up a connection's flat-combining writer.
    async fn conn_writer(&self, conn_id: u64) -> Option<Arc<ConnWriter>> {
        self.conns.lock().await.get(&conn_id).cloned()
    }

    /// Send a raw control/one-shot frame to a connection (priority lane,
    /// never coalesced).
    pub async fn send_msg(&self, conn_id: u64, msg: Vec<u8>) {
        if let Some(cw) = self.conn_writer(conn_id).await {
            cw.send_control(msg).await;
        }
    }

    /// Build the wire bytes (if any) to send for `sub` given `payload`.
    ///
    /// Returns `Some(bytes)` when there is something new to send, in which
    /// case the caller should also update `sub.last_snapshot` and apply any
    /// pipeline credit accounting. Returns `None` when the update is a no-op
    /// (duplicate of the last snapshot under the subscriber's field view) —
    /// in that case the caller must NOT decrement `nfree`.
    ///
    /// This is a **pure** function (inputs → frame bytes; it reads only its two
    /// arguments and touches no `self`/registry state), which is why it is `pub`:
    /// the server *bin* calls it directly so its monitor-delivery tail can be a
    /// byte-for-byte match of this canonical builder instead of a hand-copied
    /// duplicate (crate audit item 2a). The S1 coalescing fix (0b) lives here,
    /// so centralizing on this copy is what keeps every caller correct.
    pub fn build_monitor_frame(sub: &MonitorSub, payload: &NtPayload) -> Option<Vec<u8>> {
        let subcmd = 0x00;
        // First frame: send the whole (possibly filtered) payload with bit 0 set.
        let Some(prev) = sub.last_snapshot.as_ref() else {
            let bytes = if let Some(ref desc) = sub.filtered_desc {
                encode_monitor_data_response_filtered(
                    sub.ioid,
                    subcmd,
                    payload,
                    desc,
                    sub.version,
                    sub.is_be,
                )
            } else {
                encode_monitor_data_response_payload(
                    sub.ioid,
                    subcmd,
                    payload,
                    sub.version,
                    sub.is_be,
                )
            };
            return Some(bytes);
        };
        // Subsequent frames.
        if let Some(ref desc) = sub.filtered_desc {
            // The monitor lane coalesces (latest-wins) and may drop intermediate
            // frames under load, so every frame must be self-contained. Emit a
            // full filtered frame (safe to drop — the next one fully
            // reconstructs the filtered view), NOT a sparse delta: a dropped
            // delta would silently corrupt the client's value.
            //
            // Still suppress no-op updates (filtered view unchanged) to preserve
            // bandwidth and pipeline credit — the delta encoder returning `None`
            // is the change detector here.
            //
            // PERF follow-up (codec audit): this detects change by building and
            // discarding a delta frame; a lighter filtered-projection equality
            // check would avoid the redundant encode.
            encode_monitor_data_response_delta(
                sub.ioid,
                subcmd,
                prev,
                payload,
                desc,
                sub.version,
                sub.is_be,
            )?;
            Some(encode_monitor_data_response_filtered(
                sub.ioid,
                subcmd,
                payload,
                desc,
                sub.version,
                sub.is_be,
            ))
        } else if prev == payload {
            // Unfiltered subscriber, unchanged payload: suppress.
            None
        } else {
            // Unfiltered subscriber, payload changed: send full.
            Some(encode_monitor_data_response_payload(
                sub.ioid,
                subcmd,
                payload,
                sub.version,
                sub.is_be,
            ))
        }
    }

    /// Broadcast a monitor update for `pv_name` to all running subscribers.
    pub async fn notify_monitors(&self, pv_name: &str, payload: &NtPayload) {
        let mut to_send: Vec<(u64, u32, Vec<u8>, bool)> = Vec::new();
        {
            let mut monitors = self.monitors.lock().await;
            if let Some(list) = monitors.get_mut(pv_name) {
                for sub in list.iter_mut() {
                    if !sub.running {
                        continue;
                    }
                    if sub.pipeline_enabled && sub.nfree == 0 {
                        continue;
                    }
                    let Some(msg) = Self::build_monitor_frame(sub, payload) else {
                        // No-op update — preserve pipeline credit.
                        continue;
                    };
                    if sub.pipeline_enabled && sub.nfree > 0 {
                        sub.nfree -= 1;
                    }
                    sub.last_snapshot = Some(payload.clone());
                    to_send.push((sub.conn_id, sub.ioid, msg, sub.pipeline_enabled));
                }
            }
        }

        // Resolve every connection's writer with a SINGLE `conns` lock rather
        // than re-locking per subscriber. The `monitors` lock is already
        // released (its block ended above), so this does NOT nest `conns`
        // inside `monitors` — preserving the lock ordering the pump relies on.
        let resolved: Vec<(u64, Arc<ConnWriter>, u32, Vec<u8>, bool)> = {
            let conns = self.conns.lock().await;
            to_send
                .into_iter()
                .filter_map(|(conn_id, ioid, msg, pipelined)| {
                    conns
                        .get(&conn_id)
                        .cloned()
                        .map(|cw| (conn_id, cw, ioid, msg, pipelined))
                })
                .collect()
        };

        for (conn_id, cw, ioid, msg, pipelined) in resolved {
            self.route_monitor_frame(&cw, ioid, msg, pipelined).await;
            debug!("Monitor update pv='{}' conn={}", pv_name, conn_id);
        }
    }

    /// Deliver a built monitor frame on the correct lane.
    ///
    /// Non-pipelined subscribers use the coalescing monitor lane (latest-wins
    /// conflation under load). Pipelined subscribers do explicit credit-based
    /// flow control, so every charged frame must be delivered losslessly —
    /// coalescing one away would leak the credit the client already spent and
    /// drift its window until it stalls. They therefore use the FIFO control
    /// lane, which is lossless and, for a pipelined subscriber, bounded by the
    /// credit window (the sender stops at `nfree == 0`).
    ///
    /// Invariant (R1-L1): a task depositing a *pipelined* (control-lane) frame
    /// here must never be `abort`ed mid-deposit. Pipeline credit (`nfree`) is
    /// decremented at build time in `notify_monitors`, before the frame reaches
    /// the wire, so an aborted deposit would spend a credit the client never
    /// receives — drifting its window until it stalls. Pumps today are shut
    /// down cooperatively (not aborted), so this holds; it is a guard against
    /// future changes that might introduce abort-based shutdown.
    async fn route_monitor_frame(
        &self,
        cw: &Arc<ConnWriter>,
        ioid: u32,
        msg: Vec<u8>,
        pipelined: bool,
    ) {
        if pipelined {
            cw.send_control(msg).await;
        } else {
            cw.send_monitor(ioid, msg).await;
        }
    }

    /// Send a monitor update to a specific subscriber.
    pub async fn send_monitor_update_for(
        &self,
        pv_name: &str,
        conn_id: u64,
        ioid: u32,
        payload: &NtPayload,
    ) {
        let mut to_send: Option<(u64, Vec<u8>, bool)> = None;
        {
            let mut monitors = self.monitors.lock().await;
            if let Some(list) = monitors.get_mut(pv_name) {
                if let Some(sub) = list
                    .iter_mut()
                    .find(|s| s.conn_id == conn_id && s.ioid == ioid)
                {
                    if !sub.running {
                        return;
                    }
                    if sub.pipeline_enabled && sub.nfree == 0 {
                        return;
                    }
                    let Some(msg) = Self::build_monitor_frame(sub, payload) else {
                        return;
                    };
                    if sub.pipeline_enabled && sub.nfree > 0 {
                        sub.nfree -= 1;
                    }
                    sub.last_snapshot = Some(payload.clone());
                    to_send = Some((sub.conn_id, msg, sub.pipeline_enabled));
                }
            }
        }

        if let Some((conn_id, msg, pipelined)) = to_send {
            if let Some(cw) = self.conn_writer(conn_id).await {
                self.route_monitor_frame(&cw, ioid, msg, pipelined).await;
            }
        }
    }

    /// Update a monitor subscription's running/pipeline state.
    pub async fn update_monitor_subscription(
        &self,
        conn_id: u64,
        ioid: u32,
        pv_name: &str,
        running: bool,
        nfree: Option<u32>,
        pipeline_enabled: Option<bool>,
    ) -> bool {
        let mut monitors = self.monitors.lock().await;
        if let Some(list) = monitors.get_mut(pv_name) {
            if let Some(sub) = list
                .iter_mut()
                .find(|s| s.conn_id == conn_id && s.ioid == ioid)
            {
                sub.running = running;
                if let Some(v) = nfree {
                    // Clamp to the server ceiling: this `nfree` is the credit
                    // copy that gates `notify_monitors`, so a client-supplied
                    // window above the cap must not reach the control lane
                    // unbounded (R1-H1).
                    sub.nfree = v.min(crate::handler::MAX_PIPELINE_WINDOW);
                }
                if let Some(enabled) = pipeline_enabled {
                    if enabled {
                        sub.pipeline_enabled = true;
                    }
                }
                return true;
            }
        }
        false
    }

    /// Remove a monitor subscription.
    pub async fn remove_monitor_subscription(&self, conn_id: u64, ioid: u32, pv_name: &str) {
        {
            let mut monitors = self.monitors.lock().await;
            if let Some(list) = monitors.get_mut(pv_name) {
                list.retain(|s| s.conn_id != conn_id || s.ioid != ioid);
            }
        }
        // Lock released above; retire the pump if this was the PV's last sub.
        self.retire_pump_if_idle(pv_name).await;
    }

    /// Remove all subscriptions and connection entries for a given connection.
    pub async fn cleanup_connection(&self, conn_id: u64) {
        // Collect the PVs this connection was subscribed to so their pumps can
        // be retired after the monitors lock is released.
        let affected: Vec<String> = {
            let mut monitors = self.monitors.lock().await;
            let mut affected = Vec::new();
            for (pv, list) in monitors.iter_mut() {
                let before = list.len();
                list.retain(|s| s.conn_id != conn_id);
                if list.len() != before {
                    affected.push(pv.clone());
                }
            }
            affected
        };
        {
            let mut conns = self.conns.lock().await;
            conns.remove(&conn_id);
        }
        if let Some(cr) = self.client_registry() {
            cr.disconnect(conn_id);
        }
        for pv in affected {
            self.retire_pump_if_idle(&pv).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_codec::spvd_decode::StructureDesc;
    use spvirit_codec::spvd_encode::{filter_structure_desc, nt_payload_desc};
    use spvirit_types::{NtPayload, NtScalar, ScalarValue};

    fn make_sub(filtered: Option<StructureDesc>) -> MonitorSub {
        MonitorSub {
            conn_id: 1,
            ioid: 42,
            version: 2,
            is_be: false,
            running: true,
            pipeline_enabled: false,
            nfree: 0,
            filtered_desc: filtered,
            last_snapshot: None,
        }
    }

    fn nt_payload(value: f64, severity: i32) -> NtPayload {
        let mut nt = NtScalar::from_value(ScalarValue::F64(value));
        nt.alarm_severity = severity;
        NtPayload::Scalar(nt)
    }

    #[tokio::test]
    async fn update_monitor_subscription_clamps_client_window_to_ceiling() {
        // R1-H1: `sub.nfree` here is the credit copy that gates
        // `notify_monitors`. A client-supplied window above the server ceiling
        // must be clamped so the lossless control lane cannot grow without
        // bound for a stalled pipelined subscriber.
        let reg = MonitorRegistry::new();
        {
            let mut monitors = reg.monitors.lock().await;
            let mut sub = make_sub(None);
            sub.pipeline_enabled = true;
            monitors.entry("pv:x".to_string()).or_default().push(sub);
        }

        // A u32::MAX window clamps to the ceiling.
        let ok = reg
            .update_monitor_subscription(1, 42, "pv:x", true, Some(u32::MAX), Some(true))
            .await;
        assert!(ok, "expected the sub to be found");
        {
            let monitors = reg.monitors.lock().await;
            let sub = &monitors.get("pv:x").unwrap()[0];
            assert_eq!(sub.nfree, crate::handler::MAX_PIPELINE_WINDOW);
            assert!(sub.nfree <= crate::handler::MAX_PIPELINE_WINDOW);
        }

        // A window below the cap is preserved unchanged.
        reg.update_monitor_subscription(1, 42, "pv:x", true, Some(8), Some(true))
            .await;
        {
            let monitors = reg.monitors.lock().await;
            assert_eq!(monitors.get("pv:x").unwrap()[0].nfree, 8);
        }
    }

    // Signature order note: update_monitor_subscription(conn_id, ioid, pv_name,
    // running, nfree, pipeline_enabled). make_sub uses conn_id=1, ioid=42.

    #[test]
    fn unfiltered_first_frame_full_then_suppress_duplicate_then_resend_on_change() {
        let mut sub = make_sub(None);
        let p1 = nt_payload(1.0, 0);

        // First frame: full payload.
        let f1 = MonitorRegistry::build_monitor_frame(&sub, &p1).expect("first frame");
        assert!(!f1.is_empty());
        sub.last_snapshot = Some(p1.clone());

        // Same payload: suppressed.
        assert!(
            MonitorRegistry::build_monitor_frame(&sub, &p1).is_none(),
            "identical unfiltered payload must be suppressed"
        );

        // Changed payload: full again.
        let p2 = nt_payload(2.0, 0);
        let f2 = MonitorRegistry::build_monitor_frame(&sub, &p2).expect("full on change");
        assert!(!f2.is_empty());
    }

    #[test]
    fn build_monitor_frame_first_frame_matches_reference_full_encode() {
        // Reference frame for item 2a: the (now `pub`) canonical builder's
        // output is what the server bin's copy must reproduce byte-for-byte.
        // Pin the representative unfiltered first-frame to a fresh full encode.
        //
        // The payload MUST carry an explicit timeStamp: with `time_stamp == None`
        // the encoder falls back to `SystemTime::now()` at encode time, so the two
        // independent encodes below (taken microseconds apart) would stamp
        // different timeStamps and never match. Pinning it makes the encode
        // deterministic — which is exactly the property the byte-for-byte
        // reference guarantee relies on.
        let sub = make_sub(None);
        let mut p1 = nt_payload(1.0, 0);
        if let NtPayload::Scalar(ref mut nt) = p1 {
            nt.time_stamp = Some(spvirit_types::NtTimeStamp {
                seconds_past_epoch: 1_700_000_000,
                nanoseconds: 123_456_789,
                user_tag: 0,
            });
        }
        let frame =
            MonitorRegistry::build_monitor_frame(&sub, &p1).expect("first frame");
        let expected = encode_monitor_data_response_payload(
            sub.ioid,
            0x00,
            &p1,
            sub.version,
            sub.is_be,
        );
        assert_eq!(
            frame, expected,
            "canonical build_monitor_frame first frame must equal a full payload encode"
        );
    }

    #[test]
    fn filtered_first_frame_then_delta_none_when_selected_fields_unchanged() {
        // Subscriber only cares about alarm.severity.
        let p1 = nt_payload(1.0, 0);
        let full_desc = nt_payload_desc(&p1);
        let filt = filter_structure_desc(&full_desc, &["alarm.severity".to_string()]);
        let mut sub = make_sub(Some(filt));

        let f1 = MonitorRegistry::build_monitor_frame(&sub, &p1).expect("first filtered frame");
        assert!(!f1.is_empty());
        sub.last_snapshot = Some(p1.clone());

        // Value changed, but alarm.severity is unchanged in the filtered view.
        let p2 = nt_payload(2.0, 0);
        assert!(
            MonitorRegistry::build_monitor_frame(&sub, &p2).is_none(),
            "filtered delta must be None when selected fields unchanged"
        );
    }

    #[test]
    fn filtered_frame_emitted_when_selected_field_changes() {
        let p1 = nt_payload(1.0, 0);
        let full_desc = nt_payload_desc(&p1);
        let filt = filter_structure_desc(&full_desc, &["alarm.severity".to_string()]);
        let mut sub = make_sub(Some(filt));

        let _ = MonitorRegistry::build_monitor_frame(&sub, &p1).expect("first");
        sub.last_snapshot = Some(p1.clone());

        // Severity changes: a frame must be emitted.
        let p2 = nt_payload(1.0, 2);
        let frame = MonitorRegistry::build_monitor_frame(&sub, &p2)
            .expect("frame required when selected field changes");
        assert!(!frame.is_empty());
    }

    #[test]
    fn filtered_subsequent_frame_is_self_contained_not_delta() {
        // The monitor lane coalesces (latest-wins) and drops intermediate
        // frames under load. A filtered subscriber's subsequent frame must
        // therefore be a self-contained full filtered frame (safe to drop),
        // NOT a sparse delta relative to `last_snapshot` — a dropped delta
        // would silently corrupt the client's value.
        let p1 = nt_payload(1.0, 0);
        let full_desc = nt_payload_desc(&p1);
        let filt = filter_structure_desc(&full_desc, &["value".to_string()]);
        let mut sub = make_sub(Some(filt.clone()));

        let _ = MonitorRegistry::build_monitor_frame(&sub, &p1).expect("first");
        sub.last_snapshot = Some(p1.clone());

        // `value` changes -> a frame is emitted; it must equal a fresh full
        // filtered encode of the new payload (self-contained), not a delta.
        let p2 = nt_payload(2.0, 0);
        let frame =
            MonitorRegistry::build_monitor_frame(&sub, &p2).expect("frame on change");
        let expected = encode_monitor_data_response_filtered(
            sub.ioid,
            0x00,
            &p2,
            &filt,
            sub.version,
            sub.is_be,
        );
        assert_eq!(
            frame, expected,
            "subsequent filtered frame must be a self-contained full filtered \
             frame (coalesce-safe), not a sparse delta"
        );
    }

    /// Gated recording sink: writes park until the test grants permits, so a
    /// flusher can be pinned mid-write while more frames are deposited.
    #[derive(Clone)]
    struct TestGate {
        writes: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        inner: Arc<std::sync::Mutex<TestGateInner>>,
    }
    struct TestGateInner {
        permits: usize,
        waker: Option<std::task::Waker>,
    }
    impl TestGate {
        fn new() -> Self {
            Self {
                writes: Arc::new(std::sync::Mutex::new(Vec::new())),
                inner: Arc::new(std::sync::Mutex::new(TestGateInner {
                    permits: 0,
                    waker: None,
                })),
            }
        }
        fn release(&self, n: usize) {
            let mut g = self.inner.lock().unwrap();
            g.permits += n;
            if let Some(w) = g.waker.take() {
                w.wake();
            }
        }
        fn parked(&self) -> bool {
            self.inner.lock().unwrap().waker.is_some()
        }
    }
    impl tokio::io::AsyncWrite for TestGate {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let mut g = self.inner.lock().unwrap();
            if g.permits == 0 {
                g.waker = Some(cx.waker().clone());
                return std::task::Poll::Pending;
            }
            g.permits -= 1;
            drop(g);
            self.writes.lock().unwrap().push(buf.to_vec());
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pipelined_monitor_frames_are_not_coalesced() {
        // Pipelined monitors do credit-based flow control: every charged frame
        // must be delivered losslessly. Coalescing would drop a frame the
        // client already spent a credit on, drifting the window until it stalls.
        let reg = Arc::new(MonitorRegistry::new());
        let sink = TestGate::new();
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(1, ConnWriter::new(sink.clone()));
        }
        {
            let mut mons = reg.monitors.lock().await;
            let mut sub = make_sub(None);
            sub.conn_id = 1;
            sub.ioid = 42;
            sub.pipeline_enabled = true;
            sub.nfree = 100;
            mons.insert("pv".to_string(), vec![sub]);
        }

        let p1 = nt_payload(1.0, 0);
        let p2 = nt_payload(2.0, 0);
        let p3 = nt_payload(3.0, 0);

        // Park the flusher on the first frame.
        let r = reg.clone();
        let p1c = p1.clone();
        let t = tokio::spawn(async move { r.notify_monitors("pv", &p1c).await });
        while !sink.parked() {
            tokio::task::yield_now().await;
        }

        // Two more distinct frames for the same ioid, deposited while parked.
        reg.notify_monitors("pv", &p2).await;
        reg.notify_monitors("pv", &p3).await;

        for _ in 0..40 {
            sink.release(4);
            if sink.writes.lock().unwrap().len() >= 3 {
                break;
            }
            tokio::task::yield_now().await;
        }
        t.await.unwrap();

        let writes = sink.writes.lock().unwrap().clone();
        assert_eq!(
            writes.len(),
            3,
            "pipelined monitor frames must be delivered losslessly, not coalesced; got {writes:?}"
        );
    }
}
