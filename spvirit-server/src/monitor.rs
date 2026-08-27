//! Monitor subscription management for the PVA server.
//!
//! Tracks per-PV subscriber lists and dispatches monitor update messages.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use tokio::sync::{Mutex, mpsc};
use tracing::debug;

use spvirit_codec::spvirit_encode::{
    encode_monitor_data_response_delta, encode_monitor_data_response_filtered,
    encode_monitor_data_response_payload,
};
use spvirit_types::NtPayload;

use crate::state::MonitorSub;

/// Active connection channels and monitor subscriptions managed by the server.
pub struct MonitorRegistry {
    /// PV name → list of active monitor subscriptions.
    pub monitors: Mutex<HashMap<String, Vec<MonitorSub>>>,
    /// Connection id → message sender.
    pub conns: Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>,
    /// PV name → the task draining a subscribe-only source's update stream
    /// into `notify_monitors`. One pump per PV, shared by every subscriber of
    /// that PV; retired once the last subscriber goes away. Sources that
    /// deliver their own updates ([`Source::pushes_own_updates`]) never get a
    /// pump entry — pumping them would double-deliver.
    pumps: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl MonitorRegistry {
    pub fn new() -> Self {
        Self {
            monitors: Mutex::new(HashMap::new()),
            conns: Mutex::new(HashMap::new()),
            pumps: Mutex::new(HashMap::new()),
        }
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
        let handle = tokio::spawn(async move {
            while let Some(payload) = rx.recv().await {
                let Some(reg) = Weak::upgrade(&weak) else {
                    break;
                };
                reg.notify_monitors(&pv, &payload).await;
            }
        });
        pumps.insert(pv_name.to_string(), handle);
    }

    /// Abort and drop the pump for `pv_name` if no subscribers remain for it.
    ///
    /// Callers must have already removed the relevant subscriptions from
    /// `monitors` (and released that lock) before calling this.
    async fn retire_pump_if_idle(&self, pv_name: &str) {
        let still_active = {
            let monitors = self.monitors.lock().await;
            monitors.get(pv_name).is_some_and(|list| !list.is_empty())
        };
        if still_active {
            return;
        }
        let mut pumps = self.pumps.lock().await;
        if let Some(handle) = pumps.remove(pv_name) {
            handle.abort();
        }
    }

    /// Send a raw message to a connection.
    pub async fn send_msg(&self, conn_id: u64, msg: Vec<u8>) {
        let conns = self.conns.lock().await;
        if let Some(tx) = conns.get(&conn_id) {
            let _ = tx.send(msg).await;
        }
    }

    /// Build the wire bytes (if any) to send for `sub` given `payload`.
    ///
    /// Returns `Some(bytes)` when there is something new to send, in which
    /// case the caller should also update `sub.last_snapshot` and apply any
    /// pipeline credit accounting. Returns `None` when the update is a no-op
    /// (duplicate of the last snapshot under the subscriber's field view) —
    /// in that case the caller must NOT decrement `nfree`.
    fn build_monitor_frame(sub: &MonitorSub, payload: &NtPayload) -> Option<Vec<u8>> {
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
            // Filtered subscribers get a true sparse delta (may be None if the
            // filtered view is unchanged).
            encode_monitor_data_response_delta(
                sub.ioid,
                subcmd,
                prev,
                payload,
                desc,
                sub.version,
                sub.is_be,
            )
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
        let mut to_send: Vec<(u64, Vec<u8>)> = Vec::new();
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
                    to_send.push((sub.conn_id, msg));
                }
            }
        }

        for (conn_id, msg) in to_send {
            self.send_msg(conn_id, msg).await;
            debug!("Monitor update pv='{}' conn={}", pv_name, conn_id);
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
        let mut to_send: Option<(u64, Vec<u8>)> = None;
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
                    to_send = Some((sub.conn_id, msg));
                }
            }
        }

        if let Some((conn_id, msg)) = to_send {
            self.send_msg(conn_id, msg).await;
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
                    sub.nfree = v;
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
    fn filtered_delta_emitted_when_selected_field_changes() {
        let p1 = nt_payload(1.0, 0);
        let full_desc = nt_payload_desc(&p1);
        let filt = filter_structure_desc(&full_desc, &["alarm.severity".to_string()]);
        let mut sub = make_sub(Some(filt));

        let _ = MonitorRegistry::build_monitor_frame(&sub, &p1).expect("first");
        sub.last_snapshot = Some(p1.clone());

        // Severity changes: delta must be emitted.
        let p2 = nt_payload(1.0, 2);
        let delta = MonitorRegistry::build_monitor_frame(&sub, &p2)
            .expect("delta required when selected field changes");
        assert!(!delta.is_empty());
    }
}
