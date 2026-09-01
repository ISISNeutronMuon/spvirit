//! Monitor subscription management for the PVA server.
//!
//! Tracks per-PV subscriber lists and dispatches monitor update messages.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::debug;

use spvirit_codec::spvirit_encode::{
    encode_destroy_channel_response, encode_monitor_data_response_delta,
    encode_monitor_data_response_filtered, encode_monitor_data_response_payload,
};
use spvirit_types::NtPayload;

use crate::conn_writer::ConnWriter;
use crate::state::{MonitorSub, SharedChannelTables};

/// Active connection channels and monitor subscriptions managed by the server.
pub struct MonitorRegistry {
    /// PV name → list of active monitor subscriptions.
    pub monitors: Mutex<HashMap<String, Vec<MonitorSub>>>,
    /// Connection id → its flat-combining writer.
    pub conns: Mutex<HashMap<u64, Arc<ConnWriter>>>,
    /// Connection id → that connection's channel tables.
    ///
    /// Upstream-death teardown sends DESTROY_CHANNEL from *here*, not from the
    /// connection task, so it needs a way to forget the destroyed channel in
    /// the owning connection's tables. Registered when the connection is
    /// accepted, removed by [`Self::cleanup_connection`].
    ///
    /// **Invariant the caller must uphold:** `conn_id` must be unique for the
    /// lifetime of this registry. That is *not* something the server
    /// guarantees on its own — [`run_tcp_server`](crate::handler::run_tcp_server)
    /// allocates its connection counter inside the function, starting at 1, so
    /// it numbers per *invocation*, not per registry. Because
    /// [`run_pva_server_with_registry`](crate::server::run_pva_server_with_registry)
    /// is public and lets a caller share one `Arc<MonitorRegistry>` across
    /// several accept loops, two listeners on one registry would both hand out
    /// conn_id 1: the second registration silently replaces the first
    /// connection's tables, and whichever connection ends first evicts the
    /// survivor's entry.
    ///
    /// No production caller does this today (each server owns one listener),
    /// and the sibling `conns` map has always carried the same hazard — but
    /// this map is worse to alias, because upstream-death teardown resolves a
    /// `MonitorSub`'s `conn_id` through it to choose which socket receives
    /// DESTROY_CHANNEL. An aliased id there is a frame sent to the wrong
    /// client, not merely a dropped one.
    chan_tables: Mutex<HashMap<u64, SharedChannelTables>>,
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
    /// Identifies *which* pump task this handle belongs to, so an exiting task
    /// can retire its own entry without risking the removal of a successor
    /// spawned for the same PV in the meantime. See
    /// [`MonitorRegistry::retire_pump_generation`].
    id: u64,
    /// Set by the pump itself the moment its stream ends, *before* the teardown
    /// it then performs. A handle in this state is still in `pumps` but its task
    /// will never deliver another update, so [`MonitorRegistry::ensure_pump`]
    /// treats it as absent and replaces it rather than dropping the new
    /// subscriber's receiver on the floor. See the end-of-stream branch of the
    /// pump loop.
    exiting: bool,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

/// Source of [`PumpHandle::id`]. Process-global and never recycled; only ever
/// compared for equality, so a `u64` wrap is unreachable in practice. No test
/// reads it (a test that did would have to live in its own binary).
static PUMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl MonitorRegistry {
    pub fn new() -> Self {
        Self {
            monitors: Mutex::new(HashMap::new()),
            conns: Mutex::new(HashMap::new()),
            chan_tables: Mutex::new(HashMap::new()),
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
    /// If a *live* pump already exists for this PV, `rx` is dropped and this is
    /// a no-op — the existing pump already fans out to every subscriber via the
    /// shared monitor list. Otherwise a task is spawned that forwards each
    /// payload until the source closes the stream or the registry is dropped.
    ///
    /// "Live" excludes a pump that has already seen the end of its stream and is
    /// mid-teardown ([`PumpHandle::exiting`]). Deferring to one of those would
    /// drop `rx` in favour of a task that will never speak again, leaving this
    /// subscriber with an established monitor and no data — the very defect this
    /// module's teardown path exists to remove. Replacing it is safe: the
    /// outgoing pump retires *by id*, so it will not remove the successor
    /// installed here.
    ///
    /// The task holds only a [`Weak`] reference to the registry so it never
    /// keeps the registry (which owns the pump's `JoinHandle`) alive — that
    /// would be a reference cycle. When the registry is gone, `upgrade` fails
    /// and the task exits.
    pub async fn ensure_pump(self: &Arc<Self>, pv_name: &str, rx: mpsc::Receiver<NtPayload>) {
        let mut pumps = self.pumps.lock().await;
        if pumps.get(pv_name).is_some_and(|p| !p.exiting) {
            // Existing live pump already feeds all subscribers; drop the extra
            // rx. An *exiting* pump is deliberately not honoured here — see the
            // doc above.
            return;
        }
        let weak = Arc::downgrade(self);
        let pv = pv_name.to_string();
        let mut rx = rx;
        let (shutdown, mut shutdown_rx) = oneshot::channel::<()>();
        let id = PUMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Cooperative shutdown: retirement fires this. Cancellation
                    // can only take effect here at the top of the loop, never
                    // inside an in-flight `notify_monitors` — so a socket write
                    // is never dropped mid-frame.
                    _ = &mut shutdown_rx => break,
                    maybe = rx.recv() => {
                        let Some(payload) = maybe else {
                            // End of stream: the source dropped its sender,
                            // i.e. the data behind this PV is gone. Breaking
                            // silently here is the original defect — every
                            // subscriber would sit on a live-looking monitor
                            // that never speaks again. Tell them instead.
                            //
                            // Calling back into the registry from inside the
                            // pump task is safe: the retirement it ends in
                            // removes this pump's `PumpHandle` and *detaches*
                            // (drops) its `JoinHandle` — it never aborts the
                            // running task — and we break immediately
                            // afterwards.
                            if let Some(reg) = Weak::upgrade(&weak) {
                                // One atomic step flags this handle *and* takes
                                // custody of exactly the subscribers this pump
                                // was serving. The teardown below awaits a
                                // socket write per subscriber, so a MONITOR INIT
                                // can land inside it: the flag makes the racing
                                // `ensure_pump` spawn a replacement instead of
                                // deferring to a task on its way out, and the
                                // snapshot means that newcomer is *not* in the
                                // list we destroy. Re-reading `monitors` here
                                // instead would sweep it up and send it a
                                // DESTROY_CHANNEL for an upstream that is alive.
                                let doomed = reg.begin_pump_teardown(&pv, id).await;
                                reg.destroy_subs(&pv, doomed).await;
                            }
                            break;
                        };
                        let Some(reg) = Weak::upgrade(&weak) else { break };
                        reg.notify_monitors(&pv, &payload).await;
                    }
                }
            }
            // This task is gone; its `PumpHandle` must not outlive it. Leaving
            // it in `pumps` makes every later `ensure_pump(pv)` see a pump that
            // is not flagged as exiting, drop its fresh `rx`, and return — the
            // new subscriber then holds an established monitor that never
            // speaks again, which is the exact defect class this branch exists
            // to remove. This is the *only* retirement on the pump path: the
            // teardown above deliberately does not call the name-based
            // `retire_pump_if_idle`, which would shut down a replacement pump
            // spawned for this PV while the teardown was running. (The `exiting`
            // flag covers the *window*; this covers what is left in the map
            // after it.)
            //
            // Retiring by *id* (not by name) is what makes this safe to do
            // unconditionally: if a successor pump was already spawned for this
            // PV, its id differs and it is left alone.
            if let Some(reg) = Weak::upgrade(&weak) {
                reg.retire_pump_generation(&pv, id).await;
            }
        });
        pumps.insert(
            pv_name.to_string(),
            PumpHandle {
                id,
                exiting: false,
                shutdown,
                handle,
            },
        );
    }

    /// Open pump `id`'s teardown window for `pv_name`: flag its `PumpHandle` as
    /// on its way out and take custody of the subscribers it was serving, in one
    /// atomic step. Returns the list the caller must now destroy.
    ///
    /// Called by a pump the instant its stream ends, before the teardown that
    /// follows. Between that moment and the handle's removal the pump is still
    /// in `pumps` but can no longer deliver anything, and the teardown awaits a
    /// socket write per subscriber, so the window is wide enough for a MONITOR
    /// INIT to land in it. Two things follow, and both need this one function:
    ///
    /// - the flag is what lets [`Self::ensure_pump`] tell that state apart from
    ///   a healthy pump and start a replacement instead of dropping the new
    ///   subscriber's receiver;
    /// - taking the subscriber list *here*, under the same critical section,
    ///   fixes the set of subscribers the teardown may destroy. A newcomer that
    ///   lands after this point goes into a fresh, untouched
    ///   `monitors[pv_name]` entry and is never sent a DESTROY_CHANNEL for the
    ///   live upstream it just subscribed to.
    ///
    /// **Generation guard.** If `pumps[pv_name]` holds a *different* generation,
    /// this pump is a straggler: a successor already owns the PV, its
    /// subscribers belong to a live upstream, and nothing is taken or flagged —
    /// flagging a healthy successor would make the next `ensure_pump` replace it
    /// mid-`select!`, producing two live pumps that both deliver. Covered by
    /// `a_straggling_pump_never_flags_or_tears_down_its_successors_subscribers`.
    /// A *missing* handle is not a straggler (nothing else claims the PV), so
    /// the subscribers are still taken.
    ///
    /// Locks `monitors` then `pumps`, the established order; `pumps` is never
    /// held while `monitors` is taken.
    async fn begin_pump_teardown(&self, pv_name: &str, id: u64) -> Vec<MonitorSub> {
        let mut monitors = self.monitors.lock().await;
        let mut pumps = self.pumps.lock().await;
        match pumps.get_mut(pv_name) {
            Some(p) if p.id != id => Vec::new(),
            Some(p) => {
                p.exiting = true;
                monitors.remove(pv_name).unwrap_or_default()
            }
            None => monitors.remove(pv_name).unwrap_or_default(),
        }
    }

    /// Remove the `PumpHandle` for `pv_name` **only** if it is still the one
    /// belonging to pump `id`.
    ///
    /// Called by a pump task on its way out, so a stale handle can never
    /// suppress a later [`Self::ensure_pump`]. A successor pump for the same PV
    /// carries a different id and is never touched.
    async fn retire_pump_generation(&self, pv_name: &str, id: u64) {
        let mut pumps = self.pumps.lock().await;
        if pumps.get(pv_name).is_some_and(|p| p.id == id) {
            // Detach, never abort: the caller *is* this task.
            pumps.remove(pv_name);
        }
    }

    /// Retire the pump for `pv_name` if no subscribers remain for it.
    ///
    /// Callers must have already removed the relevant subscriptions from
    /// `monitors` (and released that lock) before calling this.
    ///
    /// **"If idle" is the whole point.** Both callers
    /// ([`Self::remove_monitor_subscription`] and [`Self::cleanup_connection`])
    /// name a single departing subscriber, not the PV as a whole; one pump
    /// serves every subscriber of a PV, so retiring it while others remain
    /// silences all of them. Covered by
    /// `one_subscriber_leaving_must_not_silence_the_others`.
    ///
    /// **The idle check and the removal are one atomic step**, and that is
    /// load-bearing: the `monitors` guard is deliberately held across the
    /// `pumps` acquisition. Releasing it first leaves a gap in which a MONITOR
    /// INIT can push its subscriber into `monitors` and have `ensure_pump` drop
    /// its receiver — the pump is perfectly healthy at that instant, so
    /// [`PumpHandle::exiting`] cannot help — after which this function removes
    /// that pump anyway. The subscriber is then permanently silent **and gets no
    /// DESTROY_CHANNEL**, so it never learns to re-search: strictly worse than
    /// the defect this module's teardown path exists to remove. Holding the
    /// guard forces the MONITOR INIT to land wholly before (we see it and
    /// decline) or wholly after (its `ensure_pump` finds no pump and spawns
    /// one). Covered by
    /// `an_idle_retirement_holds_the_monitors_lock_while_it_removes_the_pump`.
    ///
    /// Lock order is `monitors` → `pumps`, the established one; nothing in this
    /// module takes `pumps` before `monitors`, so there is no cycle.
    ///
    /// Shutdown is cooperative (signal, not abort): the pump finishes any
    /// in-flight `notify_monitors` — including its socket write — before
    /// exiting, so a flush is never dropped mid-write to wedge the shared
    /// [`ConnWriter`]. That remains a *signal*: the retired task may still be
    /// inside its `select!` when a later `ensure_pump` spawns a successor, so
    /// two pumps can briefly coexist and one extra update can be delivered.
    /// Duplicate delivery, never silence.
    async fn retire_pump_if_idle(&self, pv_name: &str) {
        let monitors = self.monitors.lock().await;
        if monitors.get(pv_name).is_some_and(|list| !list.is_empty()) {
            return;
        }
        let mut pumps = self.pumps.lock().await;
        if let Some(PumpHandle {
            id: _,
            exiting: _,
            shutdown,
            handle,
        }) = pumps.remove(pv_name)
        {
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

    /// Share a connection's channel tables with the registry, so
    /// upstream-death teardown can retract channels it destroys.
    ///
    /// **Last writer wins.** Registering a `conn_id` that is already present
    /// replaces the stored handle; it does not keep the incumbent. That is the
    /// right way round for the only case that can legitimately reach it — an id
    /// reused after the previous owner has gone — because keeping a stale
    /// handle would have teardown mutate tables nobody reads while the live
    /// connection's channels are never retracted.
    ///
    /// It is **not** a licence to alias ids: see the invariant on the private
    /// `chan_tables` field. `conn_id` must be unique for the lifetime of this
    /// registry, and a caller that shares one registry across two
    /// `run_tcp_server` accept loops breaks that — with last-writer-wins the
    /// consequence is that the second listener's connection silently takes over
    /// the first's entry.
    pub async fn register_channel_tables(&self, conn_id: u64, tables: SharedChannelTables) {
        self.chan_tables.lock().await.insert(conn_id, tables);
    }

    /// A connection's channel tables, if it is still registered.
    pub async fn channel_tables(&self, conn_id: u64) -> Option<SharedChannelTables> {
        self.chan_tables.lock().await.get(&conn_id).cloned()
    }

    /// Send a raw control/one-shot frame to a connection (priority lane,
    /// never coalesced).
    pub async fn send_msg(&self, conn_id: u64, msg: Vec<u8>) {
        let _delivered = self.send_frame(conn_id, msg).await;
    }

    /// [`Self::send_msg`], reporting whether the frame reached a live writer.
    ///
    /// `false` means the connection is no longer registered, or its
    /// [`ConnWriter`] has already recorded a socket failure — in both cases the
    /// frame is dropped and no client will see it. Best-effort by nature: the
    /// socket can still fail during the write this call performs. Byte
    /// accounting is unchanged from `send_msg`'s original behaviour (a
    /// registered connection is credited even if its writer is dead), so the
    /// return value is for reporting, not for retry decisions.
    async fn send_frame(&self, conn_id: u64, msg: Vec<u8>) -> bool {
        let Some(cw) = self.conn_writer(conn_id).await else {
            return false;
        };
        // One-shot reply: the PV (if any) isn't known here, so only
        // credit the per-host byhost attribution, not a per-PV counter.
        if let Some(r) = self.client_registry() {
            r.add_tx(conn_id, msg.len() as u64);
        }
        let live = !cw.is_dead();
        cw.send_control(msg).await;
        live
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
                    // Attribute the actual wire bytes this subscriber will
                    // receive. Both counters take only short internal locks
                    // (no `.await` in this block), so this is safe here.
                    if let Some(c) = self.bandwidth_counters() {
                        c.ds_bypv_tx.add(pv_name, msg.len() as u64);
                    }
                    if let Some(r) = self.client_registry() {
                        r.add_tx(sub.conn_id, msg.len() as u64);
                    }
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
                    // Same accounting as `notify_monitors`: attribute the
                    // actual wire bytes of this (usually initial-snapshot)
                    // frame before it moves into `to_send`. No `.await` in
                    // this block, so no diag lock is held across one.
                    if let Some(c) = self.bandwidth_counters() {
                        c.ds_bypv_tx.add(pv_name, msg.len() as u64);
                    }
                    if let Some(r) = self.client_registry() {
                        r.add_tx(sub.conn_id, msg.len() as u64);
                    }
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

    /// Tear down every downstream channel monitoring `pv_name`, because the
    /// data behind it is gone (its upstream closed).
    ///
    /// Sends DESTROY_CHANNEL to each subscriber, forgets the subscriptions,
    /// retracts the channels from the owning connections' shared
    /// [`ChannelTables`](crate::state::ChannelTables), and retires the pump.
    /// Silence is not an option here: to a PVA client, "monitor established, no
    /// updates" and "monitor established, source dead" look identical, so it
    /// would wait forever. DESTROY_CHANNEL is the message every client already
    /// handles by re-searching — which is the recovery path (pvxs
    /// `Channel::disconnect`, p4p `channelDestroyedOnServer`, Java
    /// DISCONNECTED), and the reason this design has no server-side reconnect
    /// loop.
    ///
    /// The connection itself stays up: other channels on it are unaffected.
    ///
    /// **What is *not* cleaned up.** Only the *shared* tables are reachable
    /// from here. The connection task's private
    /// [`ConnState`](crate::state::ConnState) — `ioid_to_pv`, `ioid_to_desc`,
    /// `ioid_to_monitor` — is cleared only when the client sends a
    /// `DestroyRequest`, which a client reacting to a server-initiated destroy
    /// by re-searching will never send for that ioid. On a long-lived
    /// connection against a flapping upstream those three maps therefore grow
    /// one entry per flap. Closing that needs the connection task to be
    /// reachable from the registry (the Task 5 treatment applied to the rest of
    /// `ConnState`), which is deliberately out of scope here.
    ///
    /// **Caller contract.** Safe to call from *inside* the PV's own pump task
    /// (the end-of-stream path does). From anywhere else, call it only when the
    /// PV's upstream is already dead and no new pump can be started for it
    /// concurrently: the trailing retirement detaches the pump task, which
    /// keeps running until its next `select!` turn, so an `ensure_pump` racing
    /// in that window could spawn a second pump for the same PV. (The pump's own
    /// end-of-stream path avoids this by flagging its handle
    /// [`exiting`](PumpHandle::exiting) first; an out-of-pump caller has no such
    /// handle to flag.) There is no such caller today.
    ///
    /// **Idempotence — only on a quiescent PV.** A second call on a PV that
    /// nothing has touched in between finds no subscription list, sends nothing,
    /// and (the pump having gone with the first call) retires nothing. Under
    /// concurrency none of the three holds: this function sweeps whatever is in
    /// `monitors[pv_name]` at the moment it looks, and retires whatever
    /// `PumpHandle` is in `pumps[pv_name]` at the moment it looks, **regardless
    /// of generation** — so a subscriber that arrived between the two calls is
    /// destroyed, and a replacement pump installed between them is shut down.
    /// The pump's own end-of-stream path is not exposed to either: it goes
    /// through [`Self::begin_pump_teardown`], which fixes the subscriber set
    /// atomically, and it retires by id from its exit path instead of calling
    /// [`Self::retire_pump_if_idle`] here.
    pub async fn destroy_channels_for_pv(&self, pv_name: &str) {
        let subs = {
            let mut monitors = self.monitors.lock().await;
            monitors.remove(pv_name).unwrap_or_default()
        };
        self.destroy_subs(pv_name, subs).await;
        // Best-effort, and name-based: see the idempotence paragraph above. The
        // pump path does not come through here precisely because this call
        // cannot tell a replacement pump from the one that is dying.
        self.retire_pump_if_idle(pv_name).await;
    }

    /// Send DESTROY_CHANNEL to an already-taken set of subscribers and retract
    /// their channels. Retires no pump — the caller owns that decision, because
    /// only the caller knows whether the `PumpHandle` now in `pumps` is the one
    /// that died ([`Self::destroy_channels_for_pv`], name-based and best-effort)
    /// or possibly a live replacement (the pump path, which retires by id from
    /// its own exit).
    async fn destroy_subs(&self, pv_name: &str, subs: Vec<MonitorSub>) {
        let mut destroyed = 0usize;
        for sub in &subs {
            let Some(tables) = self.channel_tables(sub.conn_id).await else {
                // The connection is already gone; nothing to send it and
                // nothing to clean up.
                continue;
            };
            // Resolve AND retract under one lock acquisition, then drop the
            // guard before any `.await`: this is a `std::sync::Mutex`, and
            // holding it across an await is the crate's hardest invariant to
            // break (see the same discipline at every lock site in
            // `handler.rs` and `gateway/src/proxy.rs`).
            //
            // Retracting before sending also follows the Task 3 teardown rule:
            // every internal state change completes first, and the observable
            // "it's gone" signal is last.
            let to_send = {
                let mut t = tables.lock().unwrap();
                // A `MonitorSub` knows only conn_id and ioid, so the channel
                // comes from the connection's tables. A miss means the channel
                // was already destroyed (a DestroyChannel racing this teardown,
                // or a connection being torn down): skip it. NEVER substitute a
                // guessed sid — that would tell the client to drop an unrelated
                // channel.
                match t.channel_for_monitor(sub.ioid) {
                    None => None,
                    Some((sid, cid)) => {
                        // `sid` is authoritative: it is never recycled and is
                        // reached from the ioid, which uniquely identifies this
                        // subscription. `cid` is only ADVISORY — a client that
                        // re-creates a channel on the same cid without first
                        // sending DestroyChannel rebinds `cid_to_sid[cid]` to a
                        // newer sid while our stale `sid_to_cid[sid]` row lives
                        // on. Sending DESTROY_CHANNEL with that cid would tell
                        // the client to drop its *new* channel, so when the cid
                        // no longer points back at our sid we send nothing and
                        // only retract our own rows.
                        let current_owner = t.cid_to_sid.get(&cid).copied();
                        t.remove_channel(cid, sid);
                        if current_owner == Some(sid) {
                            Some((sid, cid))
                        } else {
                            // `remove_channel` also dropped the cid row, which
                            // belongs to the newer channel: put it back.
                            if let Some(other) = current_owner {
                                t.cid_to_sid.insert(cid, other);
                            }
                            None
                        }
                    }
                }
            };
            let Some((sid, cid)) = to_send else {
                continue;
            };
            let msg = encode_destroy_channel_response(sid, cid, sub.version, sub.is_be);
            // Count deposits that actually reached a live writer: a frame for a
            // connection that has since gone (or whose socket is already dead)
            // never reaches a client, and logging it as destroyed would
            // overstate what the peer was told.
            if self.send_frame(sub.conn_id, msg).await {
                destroyed += 1;
            }
        }
        if destroyed > 0 {
            debug!(
                pv = pv_name,
                channels = destroyed,
                "upstream gone: destroyed downstream channels"
            );
        }
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
        {
            let mut tables = self.chan_tables.lock().await;
            tables.remove(&conn_id);
        }
        // ORDERING (Task 3 principle): `cr.disconnect` is the observable "this
        // connection is gone" signal, so it must stay AFTER every internal
        // state change above — `monitors`, `conns`, and `chan_tables`. Do not
        // move it up, and do not move any of them down past it.
        //
        // Deliberately not covered by a test: there is no `.await` between the
        // `chan_tables` removal and this call, so no other task can ever
        // observe the intermediate state, and any test claiming to check the
        // relative order would be asserting nothing. Keep the constraint here,
        // where a reorder is made.
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

    /// `make_sub` with the fields the DESTROY_CHANNEL frame is built from left
    /// to the caller. The defaults in `make_sub` (`conn_id: 1`, `ioid: 42`,
    /// `version: 2`, `is_be: false`) are exactly the literals a mutation would
    /// substitute, so any test that pins the *subscriber's* negotiated values
    /// must use non-default ones.
    fn make_sub_on(conn_id: u64, ioid: u32, version: u8, is_be: bool) -> MonitorSub {
        MonitorSub {
            conn_id,
            ioid,
            version,
            is_be,
            ..make_sub(None)
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

    #[tokio::test]
    async fn notify_monitors_accounts_downstream_tx_bytes_per_pv_and_host() {
        // Task 8: `notify_monitors` must attribute the actual wire bytes
        // delivered to each subscriber into both the per-PV `BandwidthCounters`
        // and the per-connection `ClientRegistry` (which derives byhost tx).
        use crate::diag::{BandwidthCounters, ClientRegistry};

        let reg = MonitorRegistry::new();
        let counters = Arc::new(BandwidthCounters::new());
        let client_registry = Arc::new(ClientRegistry::new());
        let peer: std::net::SocketAddr = "127.0.0.1:5555".parse().unwrap();
        client_registry.connect(1, peer);

        reg.set_bandwidth_counters(counters.clone());
        reg.set_client_registry(client_registry.clone());

        {
            let mut conns = reg.conns.lock().await;
            conns.insert(1, ConnWriter::new(tokio::io::sink()));
        }
        {
            let mut mons = reg.monitors.lock().await;
            let mut sub = make_sub(None);
            sub.conn_id = 1;
            mons.insert("pv:acct".to_string(), vec![sub]);
        }

        let payload = nt_payload(1.0, 0);
        reg.notify_monitors("pv:acct", &payload).await;

        let expected_len =
            MonitorRegistry::build_monitor_frame(&make_sub(None), &payload)
                .expect("first frame")
                .len() as u64;

        let pv_snap = counters.ds_bypv_tx.snapshot();
        assert_eq!(
            pv_snap,
            vec![("pv:acct".to_string(), expected_len)],
            "ds_bypv_tx must be credited with the delivered frame's byte length"
        );

        let byhost = client_registry.byhost(true);
        assert_eq!(byhost.len(), 1, "expected exactly one host aggregate");
        assert_eq!(
            byhost[0].2, expected_len,
            "registry conn tx must reflect the delivered frame's byte length"
        );
    }

    #[tokio::test]
    async fn send_monitor_update_for_accounts_initial_snapshot_tx_bytes() {
        // Task 8b: `send_monitor_update_for` delivers the INITIAL snapshot
        // frame on every `MonitorRequest{start:true}` (handler.rs:1671), a
        // separate delivery path from `notify_monitors` that Task 8 missed.
        // It must attribute the same way: per-PV `ds_bypv_tx` and per-conn
        // registry `tx`.
        use crate::diag::{BandwidthCounters, ClientRegistry};

        let reg = MonitorRegistry::new();
        let counters = Arc::new(BandwidthCounters::new());
        let client_registry = Arc::new(ClientRegistry::new());
        let peer: std::net::SocketAddr = "127.0.0.1:5556".parse().unwrap();
        client_registry.connect(1, peer);

        reg.set_bandwidth_counters(counters.clone());
        reg.set_client_registry(client_registry.clone());

        {
            let mut conns = reg.conns.lock().await;
            conns.insert(1, ConnWriter::new(tokio::io::sink()));
        }
        {
            let mut mons = reg.monitors.lock().await;
            let mut sub = make_sub(None);
            sub.conn_id = 1;
            sub.ioid = 42;
            mons.insert("pv:initial".to_string(), vec![sub]);
        }

        let payload = nt_payload(1.0, 0);
        reg.send_monitor_update_for("pv:initial", 1, 42, &payload)
            .await;

        let expected_len =
            MonitorRegistry::build_monitor_frame(&make_sub(None), &payload)
                .expect("first frame")
                .len() as u64;

        let pv_snap = counters.ds_bypv_tx.snapshot();
        assert_eq!(
            pv_snap,
            vec![("pv:initial".to_string(), expected_len)],
            "ds_bypv_tx must be credited with the initial-snapshot frame's byte length"
        );

        let byhost = client_registry.byhost(true);
        assert_eq!(byhost.len(), 1, "expected exactly one host aggregate");
        assert_eq!(
            byhost[0].2, expected_len,
            "registry conn tx must reflect the initial-snapshot frame's byte length"
        );
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

    /// The registry must be able to reach a connection's channel tables, and
    /// must let go of them when the connection dies — otherwise every
    /// connection the server ever accepted leaks an Arc for the process's
    /// lifetime.
    #[tokio::test]
    async fn registered_channel_tables_are_reachable_and_dropped_with_the_connection() {
        let reg = Arc::new(MonitorRegistry::new());
        let tables: crate::state::SharedChannelTables = Default::default();
        tables.lock().unwrap().insert_channel(5, 6, "PV:Q");
        reg.register_channel_tables(1, Arc::clone(&tables)).await;

        let found = reg.channel_tables(1).await.expect("tables registered");
        assert_eq!(
            found.lock().unwrap().sid_to_pv.get(&6).map(String::as_str),
            Some("PV:Q")
        );
        assert!(
            Arc::ptr_eq(&found, &tables),
            "must be the same handle, not a copy"
        );

        // A second connection: the map must be keyed, not a single slot.
        // Task 7 resolves a `MonitorSub` (conn_id + ioid) through this lookup,
        // so returning some *other* connection's tables would have it destroy
        // a channel on the wrong socket.
        let other: crate::state::SharedChannelTables = Default::default();
        other.lock().unwrap().insert_channel(5, 6, "PV:OTHER");
        reg.register_channel_tables(2, Arc::clone(&other)).await;
        assert!(
            Arc::ptr_eq(&reg.channel_tables(2).await.unwrap(), &other),
            "each conn_id must resolve to its own tables"
        );
        assert!(
            Arc::ptr_eq(&reg.channel_tables(1).await.unwrap(), &tables),
            "registering a second connection must not displace the first"
        );
        assert!(
            reg.channel_tables(3).await.is_none(),
            "an unregistered conn_id must not resolve to someone else's tables"
        );

        // Re-registering a live conn_id is last-writer-wins, not
        // first-writer-sticks. Only a reused id can legitimately reach this,
        // and keeping the incumbent there would have teardown mutate a dead
        // connection's tables while the live connection's channels are never
        // retracted — a DESTROY_CHANNEL aimed at nothing.
        let replacement: crate::state::SharedChannelTables = Default::default();
        replacement.lock().unwrap().insert_channel(9, 10, "PV:NEW");
        reg.register_channel_tables(1, Arc::clone(&replacement)).await;
        let after = reg.channel_tables(1).await.expect("still registered");
        assert!(
            Arc::ptr_eq(&after, &replacement),
            "re-registering a conn_id must replace the stored handle, not keep the stale one"
        );
        assert!(
            !Arc::ptr_eq(&after, &tables),
            "the superseded handle must no longer be reachable through the registry"
        );
        assert!(
            Arc::ptr_eq(&reg.channel_tables(2).await.unwrap(), &other),
            "replacing one conn_id must not disturb another"
        );

        reg.cleanup_connection(1).await;
        assert!(
            reg.channel_tables(1).await.is_none(),
            "cleanup_connection must unregister the tables"
        );
        assert!(
            reg.channel_tables(2).await.is_some(),
            "cleanup_connection must unregister only the connection that died"
        );
    }

    /// The non-vacuous half of the leak proof: Task 5's `Arc::downgrade` check
    /// could not fail, because the only strong reference was the one being
    /// dropped. Here the registry genuinely holds a second strong reference —
    /// asserted while the connection's own handle is gone — so the final
    /// `upgrade().is_none()` can only pass if `cleanup_connection` actually
    /// released it.
    #[tokio::test]
    async fn cleanup_connection_releases_the_registry_reference_to_the_tables() {
        let reg = Arc::new(MonitorRegistry::new());
        let weak = {
            let conn = crate::state::ConnState::default();
            let weak = Arc::downgrade(&conn.channels);
            reg.register_channel_tables(7, Arc::clone(&conn.channels))
                .await;
            // Connection task ends: its own handle goes away.
            drop(conn);
            weak
        };
        // Non-vacuity: with the connection gone, the tables are still alive
        // *only* because the registry is holding them.
        let alive = weak.upgrade().expect("registry must still hold the tables");
        assert_eq!(
            Arc::strong_count(&alive),
            2,
            "exactly the registry's handle plus this test's temporary upgrade"
        );
        drop(alive);

        reg.cleanup_connection(7).await;
        assert!(
            weak.upgrade().is_none(),
            "cleanup_connection must drop the registry's handle, not just hide it"
        );
    }

    /// Always-ready recording sink (unlike `TestGate`, no permits needed):
    /// every write lands synchronously so the test can assert exact bytes.
    #[derive(Clone)]
    struct PlainRec {
        writes: Arc<std::sync::Mutex<Vec<u8>>>,
    }
    impl PlainRec {
        fn new() -> Self {
            Self {
                writes: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }
    impl tokio::io::AsyncWrite for PlainRec {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.writes.lock().unwrap().extend_from_slice(buf);
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

    /// Spec section 2: when the PV's upstream is gone, a subscriber must be
    /// told — a silent server looks identical to an idle PV and the client
    /// waits forever. DESTROY_CHANNEL is the message every PVA client already
    /// reacts to by re-searching.
    #[tokio::test]
    async fn destroy_channels_for_pv_sends_destroy_channel_and_clears_all_state() {
        let reg = Arc::new(MonitorRegistry::new());
        let sink = PlainRec::new();
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(1, ConnWriter::new(sink.clone()));
        }
        let tables: crate::state::SharedChannelTables = Default::default();
        // make_sub uses conn_id 1, ioid 42, version 2, is_be false. The sid and
        // cid come from the tables, not from the sub.
        {
            let mut t = tables.lock().unwrap();
            t.insert_channel(9, 7, "PV:D");
            t.bind_monitor(42, 7);
        }
        reg.register_channel_tables(1, Arc::clone(&tables)).await;
        {
            let mut monitors = reg.monitors.lock().await;
            monitors.insert("PV:D".to_string(), vec![make_sub(None)]);
        }
        let (tx, rx) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:D", rx).await;
        assert!(reg.pumps.lock().await.contains_key("PV:D"));

        reg.destroy_channels_for_pv("PV:D").await;

        let expected =
            spvirit_codec::spvirit_encode::encode_destroy_channel_response(7, 9, 2, false);
        assert_eq!(
            *sink.writes.lock().unwrap(),
            expected,
            "subscriber must receive exactly one DESTROY_CHANNEL for sid=7 cid=9"
        );
        assert!(
            !reg.monitors.lock().await.contains_key("PV:D"),
            "the PV's subscription list must be gone"
        );
        {
            let t = tables.lock().unwrap();
            assert!(
                t.cid_to_sid.is_empty()
                    && t.sid_to_cid.is_empty()
                    && t.sid_to_pv.is_empty()
                    && t.ioid_to_sid.is_empty(),
                "the destroyed channel and its monitor binding must both be gone"
            );
        }
        assert!(
            !reg.pumps.lock().await.contains_key("PV:D"),
            "the pump must be retired once its last subscriber is destroyed"
        );
        drop(tx);
    }

    /// A subscription whose channel was already destroyed (a DestroyChannel
    /// that raced this teardown) must be dropped silently. Guessing a sid
    /// would tell the client to destroy an unrelated channel.
    #[tokio::test]
    async fn a_subscription_with_no_live_channel_is_skipped_not_guessed() {
        let reg = Arc::new(MonitorRegistry::new());
        let sink = PlainRec::new();
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(1, ConnWriter::new(sink.clone()));
        }
        let tables: crate::state::SharedChannelTables = Default::default();
        // A DIFFERENT, unrelated channel is open on this connection, and the
        // subscription's own ioid (42) is bound to nothing.
        tables.lock().unwrap().insert_channel(500, 600, "PV:OTHER");
        reg.register_channel_tables(1, Arc::clone(&tables)).await;
        {
            let mut monitors = reg.monitors.lock().await;
            monitors.insert("PV:D".to_string(), vec![make_sub(None)]);
        }

        reg.destroy_channels_for_pv("PV:D").await;

        assert!(
            sink.writes.lock().unwrap().is_empty(),
            "nothing may be sent for a subscription with no live channel"
        );
        assert!(
            !reg.monitors.lock().await.contains_key("PV:D"),
            "the subscription is still dropped"
        );
        let t = tables.lock().unwrap();
        assert_eq!(
            t.sid_to_pv.get(&600).map(String::as_str),
            Some("PV:OTHER"),
            "the unrelated channel must be left completely alone"
        );
    }

    /// The cid is advisory: a client that re-creates a channel on the same cid
    /// without destroying the old one rebinds `cid_to_sid[cid]` to a newer sid.
    /// Destroying the dead subscription must not tell that client to drop its
    /// live channel, and must not evict the live cid row either.
    #[tokio::test]
    async fn a_cid_rebound_to_a_newer_channel_is_neither_destroyed_nor_evicted() {
        let reg = Arc::new(MonitorRegistry::new());
        let sink = PlainRec::new();
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(1, ConnWriter::new(sink.clone()));
        }
        let tables: crate::state::SharedChannelTables = Default::default();
        {
            let mut t = tables.lock().unwrap();
            t.insert_channel(9, 7, "PV:D");
            t.bind_monitor(42, 7);
            // The client re-uses cid 9 for a brand new channel (sid 8) without
            // ever destroying sid 7.
            t.insert_channel(9, 8, "PV:D");
        }
        reg.register_channel_tables(1, Arc::clone(&tables)).await;
        {
            let mut monitors = reg.monitors.lock().await;
            monitors.insert("PV:D".to_string(), vec![make_sub(None)]);
        }

        reg.destroy_channels_for_pv("PV:D").await;

        assert!(
            sink.writes.lock().unwrap().is_empty(),
            "no DESTROY_CHANNEL may be sent for a cid the client has re-bound"
        );
        let t = tables.lock().unwrap();
        assert_eq!(
            t.cid_to_sid.get(&9).copied(),
            Some(8),
            "the live channel's cid row must survive"
        );
        assert!(
            !t.sid_to_cid.contains_key(&7)
                && !t.sid_to_pv.contains_key(&7)
                && t.ioid_to_sid.is_empty(),
            "the dead channel's own rows must still be retracted"
        );
    }

    /// The pump's end-of-stream arm must do the teardown, not `break`
    /// silently: this is the actual defect at the `rx.recv()` `None` arm.
    #[tokio::test]
    async fn a_closed_upstream_stream_destroys_the_pvs_channels() {
        let reg = Arc::new(MonitorRegistry::new());
        let sink = PlainRec::new();
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(1, ConnWriter::new(sink.clone()));
        }
        let tables: crate::state::SharedChannelTables = Default::default();
        {
            let mut t = tables.lock().unwrap();
            t.insert_channel(9, 7, "PV:D");
            t.bind_monitor(42, 7);
        }
        reg.register_channel_tables(1, Arc::clone(&tables)).await;
        {
            let mut monitors = reg.monitors.lock().await;
            monitors.insert("PV:D".to_string(), vec![make_sub(None)]);
        }
        let (tx, rx) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:D", rx).await;

        // Upstream dies: the source drops its sender.
        drop(tx);

        let expected =
            spvirit_codec::spvirit_encode::encode_destroy_channel_response(7, 9, 2, false);
        for _ in 0..200 {
            if *sink.writes.lock().unwrap() == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!(
            "pump closed without sending DESTROY_CHANNEL; got {:?}",
            *sink.writes.lock().unwrap()
        );
    }

    /// The frame must be built from the *subscriber's* negotiated version and
    /// endianness, not from the values every other fixture happens to use. A
    /// big-endian client that received a little-endian DESTROY_CHANNEL would
    /// silently fail to recover, and nothing else in the suite would notice.
    /// The sid/cid here are also distinct from each other and from the version
    /// byte, so an argument swap cannot pass by coincidence.
    #[tokio::test]
    async fn the_destroy_frame_uses_the_subscribers_own_version_and_endianness() {
        let reg = Arc::new(MonitorRegistry::new());
        let sink = PlainRec::new();
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(4, ConnWriter::new(sink.clone()));
        }
        let tables: crate::state::SharedChannelTables = Default::default();
        {
            let mut t = tables.lock().unwrap();
            t.insert_channel(23, 11, "PV:BE");
            t.bind_monitor(77, 11);
        }
        reg.register_channel_tables(4, Arc::clone(&tables)).await;
        {
            let mut monitors = reg.monitors.lock().await;
            // version 1, big-endian: neither is `make_sub`'s default.
            monitors.insert("PV:BE".to_string(), vec![make_sub_on(4, 77, 1, true)]);
        }

        reg.destroy_channels_for_pv("PV:BE").await;

        let expected =
            spvirit_codec::spvirit_encode::encode_destroy_channel_response(11, 23, 1, true);
        let got = sink.writes.lock().unwrap().clone();
        assert_eq!(
            got, expected,
            "the frame must carry sid=11, cid=23, version=1, big-endian"
        );
        // Independent of the encoder: a big-endian frame must not be
        // byte-identical to the little-endian one for the same channel.
        let le = spvirit_codec::spvirit_encode::encode_destroy_channel_response(11, 23, 1, false);
        assert_ne!(
            got, le,
            "fixture regression: this case must actually distinguish endianness"
        );
    }

    /// Fan-out is the point of a gateway: when the upstream dies, EVERY
    /// downstream subscriber must be destroyed, each on its own connection with
    /// its own sid/cid. A teardown that stops after the first subscriber leaves
    /// the rest holding monitors that never speak again — the original defect,
    /// merely narrowed.
    #[tokio::test]
    async fn every_subscriber_on_every_connection_is_destroyed() {
        let reg = Arc::new(MonitorRegistry::new());
        let sink_a = PlainRec::new();
        let sink_b = PlainRec::new();
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(1, ConnWriter::new(sink_a.clone()));
            conns.insert(2, ConnWriter::new(sink_b.clone()));
        }
        let tables_a: crate::state::SharedChannelTables = Default::default();
        {
            let mut t = tables_a.lock().unwrap();
            t.insert_channel(9, 7, "PV:F");
            t.bind_monitor(42, 7);
        }
        let tables_b: crate::state::SharedChannelTables = Default::default();
        {
            let mut t = tables_b.lock().unwrap();
            t.insert_channel(33, 21, "PV:F");
            t.bind_monitor(43, 21);
        }
        reg.register_channel_tables(1, Arc::clone(&tables_a)).await;
        reg.register_channel_tables(2, Arc::clone(&tables_b)).await;
        {
            let mut monitors = reg.monitors.lock().await;
            monitors.insert(
                "PV:F".to_string(),
                vec![
                    make_sub_on(1, 42, 2, false),
                    make_sub_on(2, 43, 2, false),
                ],
            );
        }

        reg.destroy_channels_for_pv("PV:F").await;

        assert_eq!(
            *sink_a.writes.lock().unwrap(),
            spvirit_codec::spvirit_encode::encode_destroy_channel_response(7, 9, 2, false),
            "connection 1 must be told about its own channel"
        );
        assert_eq!(
            *sink_b.writes.lock().unwrap(),
            spvirit_codec::spvirit_encode::encode_destroy_channel_response(21, 33, 2, false),
            "connection 2 must be told about ITS own channel, not connection 1's"
        );
        assert!(
            tables_a.lock().unwrap().channel_for_monitor(42).is_none()
                && tables_b.lock().unwrap().channel_for_monitor(43).is_none(),
            "both channels must be retracted"
        );
    }

    /// A sink that re-subscribes to the PV from inside the write, simulating a
    /// MONITOR INIT that lands *inside* the teardown window — after
    /// `begin_pump_teardown` has taken the doomed subscriber list and before the
    /// pump has finished writing to it. The `try_lock` is deterministic:
    /// `destroy_subs` holds no `monitors` guard while it writes.
    #[derive(Clone)]
    struct ResubscribeOnWrite {
        reg: Weak<MonitorRegistry>,
        pv: String,
        fired: Arc<std::sync::atomic::AtomicBool>,
    }
    impl tokio::io::AsyncWrite for ResubscribeOnWrite {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if !self
                .fired
                .swap(true, std::sync::atomic::Ordering::SeqCst)
                && let Some(reg) = self.reg.upgrade()
            {
                let mut monitors = reg
                    .monitors
                    .try_lock()
                    .expect("teardown must not hold the monitors lock while writing");
                monitors
                    .entry(self.pv.clone())
                    .or_default()
                    .push(make_sub(None));
            }
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

    /// An exiting pump must never leave its `PumpHandle` behind, not even when a
    /// MONITOR INIT re-populates `monitors` mid-teardown. A stale handle makes
    /// every later `ensure_pump` drop its fresh receiver, so the new subscriber
    /// holds a monitor that never speaks again. Retiring its own generation on
    /// the way out is the pump's only retirement, and it is unconditional —
    /// which is what closes that hole.
    #[tokio::test]
    async fn an_exiting_pump_never_strands_its_handle_even_if_a_new_subscriber_arrives() {
        let reg = Arc::new(MonitorRegistry::new());
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(
                1,
                ConnWriter::new(ResubscribeOnWrite {
                    reg: Arc::downgrade(&reg),
                    pv: "PV:S".to_string(),
                    fired: Arc::clone(&fired),
                }),
            );
        }
        let tables: crate::state::SharedChannelTables = Default::default();
        {
            let mut t = tables.lock().unwrap();
            t.insert_channel(9, 7, "PV:S");
            t.bind_monitor(42, 7);
        }
        reg.register_channel_tables(1, Arc::clone(&tables)).await;
        {
            let mut monitors = reg.monitors.lock().await;
            monitors.insert("PV:S".to_string(), vec![make_sub(None)]);
        }
        let (tx, rx) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:S", rx).await;
        assert!(reg.pumps.lock().await.contains_key("PV:S"));

        // Upstream dies; the write of the DESTROY_CHANNEL re-subscribes.
        drop(tx);

        let mut stranded = true;
        for _ in 0..200 {
            if !reg.pumps.lock().await.contains_key("PV:S") {
                stranded = false;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            "fixture regression: the re-subscribe never ran, so nothing was raced"
        );
        assert!(
            reg.monitors.lock().await.contains_key("PV:S"),
            "fixture regression: the injected subscriber must still be registered \
             for this to be the mid-teardown race at all"
        );
        assert!(
            !stranded,
            "the exiting pump left its PumpHandle behind; the next ensure_pump \
             would drop its receiver and the new subscriber would go silent"
        );
    }

    /// The other half of self-retirement: an exiting pump must remove **its
    /// own** handle and nothing else. If a successor pump was already spawned
    /// for the same PV, retiring by name would silently kill the live one and
    /// leave that PV unpumped forever.
    #[tokio::test]
    async fn a_late_exiting_pump_never_retires_its_successor() {
        let reg = Arc::new(MonitorRegistry::new());
        let (tx1, rx1) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:G", rx1).await;
        let id1 = reg.pumps.lock().await.get("PV:G").expect("pump 1").id;

        // Pump 1 exits and retires itself.
        reg.retire_pump_generation("PV:G", id1).await;
        assert!(!reg.pumps.lock().await.contains_key("PV:G"));

        // A new subscriber arrives and pump 2 takes over.
        let (tx2, rx2) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:G", rx2).await;
        let id2 = reg.pumps.lock().await.get("PV:G").expect("pump 2").id;
        assert_ne!(id1, id2, "each pump must have its own identity");

        // A straggling call from pump 1 must not touch pump 2.
        reg.retire_pump_generation("PV:G", id1).await;
        assert_eq!(
            reg.pumps.lock().await.get("PV:G").map(|p| p.id),
            Some(id2),
            "the successor pump must survive its predecessor's retirement"
        );
        drop((tx1, tx2));
    }

    /// One pump serves every subscriber of a PV, so `retire_pump_if_idle`'s
    /// `still_active` guard is the only thing standing between "client A
    /// unsubscribes" and "every other client on that PV goes permanently
    /// silent" — the same defect class this branch exists to remove, reached
    /// from the ordinary unsubscribe path rather than from upstream death.
    #[tokio::test]
    async fn one_subscriber_leaving_must_not_silence_the_others() {
        let reg = Arc::new(MonitorRegistry::new());
        let sink_a = PlainRec::new();
        let sink_b = PlainRec::new();
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(1, ConnWriter::new(sink_a.clone()));
            conns.insert(2, ConnWriter::new(sink_b.clone()));
        }
        {
            let mut monitors = reg.monitors.lock().await;
            monitors.insert(
                "PV:K".to_string(),
                vec![make_sub_on(1, 42, 2, false), make_sub_on(2, 43, 2, false)],
            );
        }
        let (tx, rx) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:K", rx).await;
        let pump_id = reg.pumps.lock().await.get("PV:K").expect("pump").id;

        // Client A unsubscribes. B is still monitoring the same PV.
        reg.remove_monitor_subscription(1, 42, "PV:K").await;

        assert_eq!(
            reg.pumps.lock().await.get("PV:K").map(|p| p.id),
            Some(pump_id),
            "the pump must survive: it is shared, and B is still subscribed"
        );

        // The real proof: B still receives updates.
        tx.send(nt_payload(1.25, 0)).await.expect("pump alive");
        let mut got_b = Vec::new();
        for _ in 0..200 {
            got_b = sink_b.writes.lock().unwrap().clone();
            if !got_b.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            !got_b.is_empty(),
            "the remaining subscriber went silent when the other one left"
        );
        assert!(
            sink_a.writes.lock().unwrap().is_empty(),
            "the departed subscriber must receive nothing"
        );
        drop(tx);
    }

    /// The residue of the pump-retirement race: a MONITOR INIT that lands while
    /// the outgoing pump is still in `pumps` (its teardown awaits a socket write
    /// per subscriber, so the window spans real I/O). Deferring to that handle
    /// would drop the new subscriber's receiver and leave it holding a monitor
    /// that never speaks. `ensure_pump` must replace an exiting pump instead.
    #[tokio::test]
    async fn a_subscriber_arriving_mid_teardown_gets_a_live_pump_not_a_dying_one() {
        let reg = Arc::new(MonitorRegistry::new());
        let sink = PlainRec::new();
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(2, ConnWriter::new(sink.clone()));
        }
        // The outgoing pump, in exactly the state its end-of-stream branch puts
        // it in before running the teardown.
        let (old_tx, old_rx) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:R", old_rx).await;
        let old_id = reg.pumps.lock().await.get("PV:R").expect("pump").id;
        let doomed = reg.begin_pump_teardown("PV:R", old_id).await;
        assert!(doomed.is_empty(), "fixture: no subscribers yet");

        // The racing MONITOR INIT: a new subscriber plus a fresh stream.
        {
            let mut monitors = reg.monitors.lock().await;
            monitors.insert("PV:R".to_string(), vec![make_sub_on(2, 43, 2, false)]);
        }
        let (new_tx, new_rx) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:R", new_rx).await;
        assert_ne!(
            reg.pumps.lock().await.get("PV:R").map(|p| p.id),
            Some(old_id),
            "an exiting pump must be replaced, not deferred to"
        );

        new_tx.send(nt_payload(2.5, 0)).await.expect("pump alive");
        let mut got = Vec::new();
        for _ in 0..200 {
            got = sink.writes.lock().unwrap().clone();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            !got.is_empty(),
            "the new subscriber's receiver was dropped on the floor; it holds a \
             monitor that will never speak"
        );

        // And the outgoing pump must not take its replacement down with it.
        reg.retire_pump_generation("PV:R", old_id).await;
        assert!(
            reg.pumps.lock().await.contains_key("PV:R"),
            "the replacement pump must survive its predecessor's retirement"
        );
        drop((old_tx, new_tx));
    }

    /// A sink that samples the PV pump state from inside the teardown write,
    /// which is the only moment the `exiting` window is observable. The
    /// `try_lock` is deterministic: `destroy_channels_for_pv` holds no `pumps`
    /// guard while it writes.
    #[derive(Clone)]
    struct SamplePumpStateOnWrite {
        reg: Weak<MonitorRegistry>,
        pv: String,
        seen: Arc<std::sync::Mutex<Vec<Option<bool>>>>,
    }
    impl tokio::io::AsyncWrite for SamplePumpStateOnWrite {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if let Some(reg) = self.reg.upgrade() {
                let pumps = reg
                    .pumps
                    .try_lock()
                    .expect("teardown must not hold the pumps lock while writing");
                self.seen
                    .lock()
                    .unwrap()
                    .push(pumps.get(&self.pv).map(|p| p.exiting));
            }
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

    /// The wiring half of the previous test: the pump must flag its own handle
    /// *before* it starts the teardown, because the teardown is the window a
    /// MONITOR INIT lands in. Sampling the flag from inside the teardown write
    /// is the only way to observe that ordering.
    #[tokio::test]
    async fn a_pump_flags_itself_exiting_before_it_tears_the_pv_down() {
        let reg = Arc::new(MonitorRegistry::new());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(
                1,
                ConnWriter::new(SamplePumpStateOnWrite {
                    reg: Arc::downgrade(&reg),
                    pv: "PV:X".to_string(),
                    seen: Arc::clone(&seen),
                }),
            );
        }
        let tables: crate::state::SharedChannelTables = Default::default();
        {
            let mut t = tables.lock().unwrap();
            t.insert_channel(9, 7, "PV:X");
            t.bind_monitor(42, 7);
        }
        reg.register_channel_tables(1, Arc::clone(&tables)).await;
        {
            let mut monitors = reg.monitors.lock().await;
            monitors.insert("PV:X".to_string(), vec![make_sub(None)]);
        }
        let (tx, rx) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:X", rx).await;

        // Upstream dies; the DESTROY_CHANNEL write samples the pump state.
        drop(tx);

        let mut sampled = None;
        for _ in 0..200 {
            sampled = seen.lock().unwrap().first().copied();
            if sampled.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let sampled = sampled.expect("fixture regression: the teardown never wrote anything");
        assert_eq!(
            sampled,
            Some(true),
            "during its teardown the pump must be present and flagged exiting; a \
             concurrent ensure_pump seeing anything else would drop the new \
             subscriber receiver"
        );
    }

    /// The generation guard on `begin_pump_teardown`, which is the mirror of
    /// `a_late_exiting_pump_never_retires_its_successor` for the *other*
    /// id-guarded function. Without it a straggling pump flags its successor's
    /// handle as exiting — so the next `ensure_pump` replaces a perfectly
    /// healthy pump while it is still inside `select!` and still delivering,
    /// the one construction that yields two genuinely live pumps for one PV —
    /// and it also hands that successor's subscribers to a teardown that would
    /// send them DESTROY_CHANNEL for an upstream that is alive.
    #[tokio::test]
    async fn a_straggling_pump_never_flags_or_tears_down_its_successors_subscribers() {
        let reg = Arc::new(MonitorRegistry::new());
        let (tx1, rx1) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:Q", rx1).await;
        let id1 = reg.pumps.lock().await.get("PV:Q").expect("pump 1").id;

        // Pump 1's handle goes (the idle path removes it by name); a new
        // subscriber then brings pump 2 in for the same PV.
        reg.retire_pump_generation("PV:Q", id1).await;
        let (tx2, rx2) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:Q", rx2).await;
        let id2 = reg.pumps.lock().await.get("PV:Q").expect("pump 2").id;
        assert_ne!(id1, id2, "fixture: two distinct generations");
        {
            let mut monitors = reg.monitors.lock().await;
            monitors.insert("PV:Q".to_string(), vec![make_sub(None)]);
        }

        // Only now does pump 1's stream end.
        let doomed = reg.begin_pump_teardown("PV:Q", id1).await;

        assert!(
            doomed.is_empty(),
            "a straggling pump must not take custody of its successor's \
             subscribers; they belong to a live upstream"
        );
        assert_eq!(
            reg.pumps
                .lock()
                .await
                .get("PV:Q")
                .map(|p| (p.id, p.exiting)),
            Some((id2, false)),
            "a straggling pump must not flag its successor as exiting; the next \
             ensure_pump would replace a live, delivering pump"
        );
        assert_eq!(
            reg.monitors.lock().await.get("PV:Q").map(|l| l.len()),
            Some(1),
            "the successor's subscriber must still be registered"
        );
        drop((tx1, tx2));
    }

    /// A sink that adds a *different* connection's subscriber to the PV from
    /// inside the teardown write — the MONITOR INIT that lands after the pump
    /// flagged itself exiting. That subscriber's upstream is alive (a
    /// replacement pump is what `ensure_pump` gives it), so the outgoing
    /// teardown must not touch it.
    #[derive(Clone)]
    struct SubscribeOtherConnOnWrite {
        reg: Weak<MonitorRegistry>,
        pv: String,
        fired: Arc<std::sync::atomic::AtomicBool>,
    }
    impl tokio::io::AsyncWrite for SubscribeOtherConnOnWrite {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if !self
                .fired
                .swap(true, std::sync::atomic::Ordering::SeqCst)
                && let Some(reg) = self.reg.upgrade()
            {
                let mut monitors = reg
                    .monitors
                    .try_lock()
                    .expect("teardown must not hold the monitors lock while writing");
                monitors
                    .entry(self.pv.clone())
                    .or_default()
                    .push(make_sub_on(2, 43, 2, false));
            }
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

    /// A subscriber that arrives *after* the pump began exiting was never served
    /// by that pump, and `ensure_pump` hands it a live replacement. Destroying
    /// it would send DESTROY_CHANNEL for an upstream that is alive: the client
    /// loses a working channel and has to re-search for no reason. The teardown
    /// must therefore act on the subscriber set fixed at
    /// `begin_pump_teardown`, not on whatever `monitors` holds when it gets
    /// round to looking.
    #[tokio::test]
    async fn a_subscriber_arriving_mid_teardown_is_never_destroyed_by_it() {
        let reg = Arc::new(MonitorRegistry::new());
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sink_b = PlainRec::new();
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(
                1,
                ConnWriter::new(SubscribeOtherConnOnWrite {
                    reg: Arc::downgrade(&reg),
                    pv: "PV:T".to_string(),
                    fired: Arc::clone(&fired),
                }),
            );
            conns.insert(2, ConnWriter::new(sink_b.clone()));
        }
        let tables_a: crate::state::SharedChannelTables = Default::default();
        {
            let mut t = tables_a.lock().unwrap();
            t.insert_channel(9, 7, "PV:T");
            t.bind_monitor(42, 7);
        }
        reg.register_channel_tables(1, Arc::clone(&tables_a)).await;
        // The newcomer's channel is fully resolvable, so if the teardown did
        // sweep it up it *would* produce a frame — the assertion below is not
        // vacuous.
        let tables_b: crate::state::SharedChannelTables = Default::default();
        {
            let mut t = tables_b.lock().unwrap();
            t.insert_channel(33, 21, "PV:T");
            t.bind_monitor(43, 21);
        }
        reg.register_channel_tables(2, Arc::clone(&tables_b)).await;
        {
            let mut monitors = reg.monitors.lock().await;
            monitors.insert("PV:T".to_string(), vec![make_sub(None)]);
        }
        let (tx, rx) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:T", rx).await;

        // Upstream dies; the write of A's DESTROY_CHANNEL injects B.
        drop(tx);
        for _ in 0..200 {
            if fired.load(std::sync::atomic::Ordering::SeqCst)
                && !reg.pumps.lock().await.contains_key("PV:T")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            "fixture regression: the mid-teardown subscribe never ran"
        );

        assert_eq!(
            reg.monitors
                .lock()
                .await
                .get("PV:T")
                .map(|l| l.iter().map(|s| (s.conn_id, s.ioid)).collect::<Vec<_>>()),
            Some(vec![(2u64, 43u32)]),
            "the mid-teardown subscriber must survive the teardown that did not \
             serve it"
        );
        assert!(
            sink_b.writes.lock().unwrap().is_empty(),
            "the mid-teardown subscriber was sent a DESTROY_CHANNEL for an \
             upstream that is alive"
        );
        assert_eq!(
            tables_b.lock().unwrap().channel_for_monitor(43),
            Some((21, 33)),
            "the mid-teardown subscriber's channel must not be retracted"
        );
    }

    /// `retire_pump_if_idle` must hold the `monitors` guard across its `pumps`
    /// acquisition. If it releases the guard first, a MONITOR INIT can land in
    /// the gap: the subscriber is pushed into `monitors`, `ensure_pump` sees a
    /// pump that is present and perfectly healthy (so `exiting` cannot help)
    /// and drops the fresh receiver, and this function then removes that pump.
    /// The subscriber is permanently silent **and gets no DESTROY_CHANNEL**, so
    /// unlike every other failure on this branch it never learns to re-search.
    ///
    /// The interleaving itself is not deterministically constructible from a
    /// test — on a current-thread runtime an uncontended `pumps.lock().await`
    /// is not even a scheduling point, and contending it to force one makes
    /// tokio's FIFO-fair mutex serve the retirement first. So this asserts the
    /// invariant that removes the gap: while the retirement waits for `pumps`,
    /// `monitors` must be unavailable to anyone else.
    #[tokio::test]
    async fn an_idle_retirement_holds_the_monitors_lock_while_it_removes_the_pump() {
        let reg = Arc::new(MonitorRegistry::new());
        let (tx, rx) = mpsc::channel::<NtPayload>(4);
        reg.ensure_pump("PV:N", rx).await;
        assert!(reg.pumps.lock().await.contains_key("PV:N"));

        // Park the retirement on `pumps`, exactly where the gap used to open.
        let gate = reg.pumps.lock().await;
        let r = Arc::clone(&reg);
        let retiring = tokio::spawn(async move { r.retire_pump_if_idle("PV:N").await });
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        let monitors_held = reg.monitors.try_lock().is_err();
        drop(gate);
        retiring.await.expect("retirement task");

        assert!(
            monitors_held,
            "retire_pump_if_idle released `monitors` before taking `pumps`; a \
             MONITOR INIT landing in that gap goes permanently silent with no \
             DESTROY_CHANNEL"
        );
        assert!(
            !reg.pumps.lock().await.contains_key("PV:N"),
            "an idle PV's pump must still actually be retired"
        );
        drop(tx);
    }

    /// A sink whose every write fails, to reach the `ConnWriter`'s dead path.
    struct FailSink;
    impl tokio::io::AsyncWrite for FailSink {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "dead",
            )))
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

    /// `send_frame`'s bool is what the teardown counts, so it must be false in
    /// both undeliverable cases: no such connection, and a writer that has
    /// already failed.
    #[tokio::test]
    async fn send_frame_reports_undeliverable_frames() {
        let reg = Arc::new(MonitorRegistry::new());
        assert!(
            !reg.send_frame(99, vec![1, 2, 3]).await,
            "an unregistered connection cannot receive anything"
        );
        let cw = ConnWriter::new(FailSink);
        {
            let mut conns = reg.conns.lock().await;
            conns.insert(1, Arc::clone(&cw));
        }
        assert!(
            reg.send_frame(1, vec![1, 2, 3]).await,
            "the first frame is deposited before the socket failure is known"
        );
        assert!(cw.is_dead(), "the failed write must mark the writer dead");
        assert!(
            !reg.send_frame(1, vec![4, 5, 6]).await,
            "a dead writer drops the frame, so it must not be counted"
        );
    }
}
