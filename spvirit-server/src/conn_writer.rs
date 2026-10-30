//! Per-connection flat-combining writer.
//!
//! Replaces the old delivery tail of a per-connection `mpsc` plus a
//! dedicated writer task. Producers deposit complete PVA frames and one task
//! is elected "flusher" to drain them to the socket, so the common case
//! writes inline on the producing task with **no cross-thread wakeup**.
//!
//! This is flat combining (Hendler, Incze, Shavit & Tzafrir, SPAA 2010)
//! specialized to a single write resource: the `flushing` flag is the
//! combiner election, and the `monitor` map is a conflating (latest-wins)
//! slot per ioid — the N-key generalization of [`tokio::sync::watch`].
//!
//! Two lanes give priority scheduling the combiner can honor because it
//! holds the whole pending batch at drain time:
//! * `control` — FIFO, never coalesced, never dropped: handshake, GET/PUT/RPC
//!   and monitor-init responses, errors, control frames. Drained first.
//! * `monitor` — latest encoded frame per ioid; a newer frame replaces the
//!   older one, so intermediate monitor values are dropped under load
//!   (conflation, exactly like pvxs/pvagw).
//!
//! Cross-operation reordering is protocol-legal: PVA frames are correlated by
//! request id, not stream position. Control-first ordering also guarantees a
//! monitor INIT response precedes that ioid's DATA frames.
//!
//! **Depositing never waits on socket I/O.** One task fans a monitor update
//! out to every subscriber's writer in turn, so a deposit that awaited a slow
//! peer's socket would stall delivery to every subscriber after it. The
//! elected flusher therefore only *tries* the socket inline: it polls the
//! write with a no-op waker, and the moment the socket is not ready it hands
//! the unwritten remainder — and the election with it — to a spawned
//! continuation task, then returns. The continuation is the only code that
//! awaits the socket, and it does so under a per-write stall deadline
//! ([`DEFAULT_WRITE_STALL_TIMEOUT`]): a peer that accepts no bytes for that
//! long is marked dead, its queues are freed, and [`ConnWriter::closed`]
//! resolves so the connection task can tear it down.
//!
//! The connection's *own* request loop still gets backpressure from a peer
//! that does not read: [`ConnWriter::backpressure`] waits while the unsent
//! backlog is over [`BACKLOG_HIGH_WATER`]. Only that peer's own task calls
//! it, so it throttles the peer's requests without touching anyone else.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::runtime::Handle;
use tokio::sync::{Mutex as AsyncMutex, Notify};

/// How long a write may make no progress (the peer accepts no bytes) before
/// the connection is marked dead.
pub const DEFAULT_WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Unsent bytes above which [`ConnWriter::backpressure`] makes the
/// connection's own request loop wait.
pub const BACKLOG_HIGH_WATER: usize = 4 * 1024 * 1024;

/// Batches the inline flusher may write before it hands a continuous stream
/// of deposits to a continuation task, so a depositor that won the election
/// is not kept busy writing other producers' frames.
const INLINE_BATCH_LIMIT: usize = 8;

/// Coalescing state guarded by a std mutex. Never held across an `.await` or
/// a socket poll: every critical section is O(pending) map/deque work.
#[derive(Default)]
struct Coalesce {
    /// Priority lane: complete control/response frames, FIFO.
    control: VecDeque<Vec<u8>>,
    /// Monitor lane: latest encoded frame per ioid (conflation).
    monitor: HashMap<u32, Vec<u8>>,
    /// Single-flight election: true while some task is the designated flusher
    /// — an inline depositor or a spawned continuation, never both.
    flushing: bool,
    /// Set once a socket write has failed or stalled past the deadline;
    /// further deposits are dropped so a dead-but-not-closed peer cannot wedge
    /// producers or grow the queues.
    dead: bool,
    /// Bytes deposited and not yet accepted by the socket: both lanes plus
    /// the batch in flight. Drives [`ConnWriter::backpressure`].
    backlog: usize,
}

impl Coalesce {
    /// Drain both lanes into one batch, control lane first. With nothing
    /// pending, release the election instead and return `None` — the drain,
    /// the empty-check and the release happen under the caller's single lock
    /// acquisition, which is what rules out a lost wake-up.
    fn take_batch(&mut self) -> Option<Vec<u8>> {
        if self.control.is_empty() && self.monitor.is_empty() {
            self.flushing = false;
            return None;
        }
        // Concatenated into ONE buffer so the batch reaches the kernel in a
        // single write. Doing a write per frame throws away the whole point
        // of collecting a batch, and with TCP_NODELAY set (which the server
        // does on every accepted socket) each such write can become its own
        // packet.
        let total: usize = self
            .control
            .iter()
            .chain(self.monitor.values())
            .map(Vec::len)
            .sum();
        let mut batch = Vec::with_capacity(total);
        for buf in self.control.drain(..) {
            batch.extend_from_slice(&buf);
        }
        for (_ioid, buf) in self.monitor.drain() {
            batch.extend_from_slice(&buf);
        }
        Some(batch)
    }
}

/// The per-connection writer: a socket write half plus its coalescing state.
pub struct ConnWriter {
    /// The socket write half. Only the holder of the flusher election touches
    /// it, so the lock is uncontended except across an election handover.
    writer: AsyncMutex<Box<dyn AsyncWrite + Send + Unpin>>,
    coalesce: StdMutex<Coalesce>,
    /// No-progress deadline for one socket write in the continuation.
    write_timeout: Duration,
    /// Woken when the backlog falls to [`BACKLOG_HIGH_WATER`] or the
    /// connection dies.
    drained: Notify,
    /// Woken once, when the connection is marked dead.
    died: Notify,
}

impl ConnWriter {
    /// Wrap a write half (real socket, or any async sink in tests).
    pub fn new<W: AsyncWrite + Send + Unpin + 'static>(w: W) -> Arc<Self> {
        Self::with_write_timeout(w, DEFAULT_WRITE_STALL_TIMEOUT)
    }

    /// [`Self::new`] with a custom write-stall deadline.
    pub fn with_write_timeout<W: AsyncWrite + Send + Unpin + 'static>(
        w: W,
        write_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            writer: AsyncMutex::new(Box::new(w)),
            coalesce: StdMutex::new(Coalesce::default()),
            write_timeout,
            drained: Notify::new(),
            died: Notify::new(),
        })
    }

    /// Whether a socket write has already failed on this connection, i.e. any
    /// further deposit will be dropped rather than written.
    ///
    /// A snapshot, not a guarantee: the socket can fail immediately after this
    /// returns `false`. Callers use it for reporting (did this frame have any
    /// chance of reaching the peer?), never as a delivery receipt.
    pub fn is_dead(&self) -> bool {
        self.coalesce.lock().unwrap().dead
    }

    /// Deposit a control/one-shot frame (priority lane, never coalesced) and
    /// flush if no flusher is currently active.
    ///
    /// Never waits on the socket: it returns once the frame is queued and, if
    /// this call won the election, written as far as the socket accepts
    /// without blocking.
    pub async fn send_control(self: &Arc<Self>, bytes: Vec<u8>) {
        {
            let mut c = self.coalesce.lock().unwrap();
            if c.dead {
                return;
            }
            c.backlog += bytes.len();
            c.control.push_back(bytes);
            if c.flushing {
                return;
            }
            c.flushing = true;
        }
        self.flush_inline();
    }

    /// Deposit a monitor frame for `ioid` (monitor lane, latest-wins) and
    /// flush if no flusher is currently active. Never waits on the socket.
    pub async fn send_monitor(self: &Arc<Self>, ioid: u32, bytes: Vec<u8>) {
        {
            let mut c = self.coalesce.lock().unwrap();
            if c.dead {
                return;
            }
            c.backlog += bytes.len();
            // coalesce: replace any stale frame
            if let Some(stale) = c.monitor.insert(ioid, bytes) {
                c.backlog -= stale.len();
            }
            if c.flushing {
                return;
            }
            c.flushing = true;
        }
        self.flush_inline();
    }

    /// Wait while this connection's unsent backlog is above
    /// [`BACKLOG_HIGH_WATER`]; returns at once if it is not, or if the
    /// connection is dead.
    ///
    /// For the connection's own request loop only: it restores the TCP
    /// backpressure a non-reading peer used to get from a blocking write, so
    /// such a peer cannot make the server queue replies without bound. Never
    /// call it from a task that serves other connections.
    pub async fn backpressure(&self) {
        // Fast path for every reply: under the mark, skip registering.
        {
            let c = self.coalesce.lock().unwrap();
            if c.dead || c.backlog <= BACKLOG_HIGH_WATER {
                return;
            }
        }
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            // Register before checking, so a drain between the check and the
            // await still wakes us.
            notified.as_mut().enable();
            {
                let c = self.coalesce.lock().unwrap();
                if c.dead || c.backlog <= BACKLOG_HIGH_WATER {
                    return;
                }
            }
            notified.await;
        }
    }

    /// Resolves once the connection has been marked dead (a write failed or
    /// stalled past the deadline). The connection task selects on this so a
    /// peer that stopped reading is torn down even while it keeps sending.
    pub async fn closed(&self) {
        loop {
            let notified = self.died.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_dead() {
                return;
            }
            notified.await;
        }
    }

    /// Account `n` bytes as accepted by the socket.
    fn wrote(&self, n: usize) {
        let mut c = self.coalesce.lock().unwrap();
        let was_over = c.backlog > BACKLOG_HIGH_WATER;
        c.backlog = c.backlog.saturating_sub(n);
        if was_over && c.backlog <= BACKLOG_HIGH_WATER {
            self.drained.notify_waiters();
        }
    }

    /// Record a failed or stalled socket: drop everything queued, release the
    /// election, and wake anyone waiting on backpressure or [`Self::closed`].
    fn mark_dead(&self) {
        {
            let mut c = self.coalesce.lock().unwrap();
            c.dead = true;
            c.flushing = false;
            c.control.clear();
            c.monitor.clear();
            c.backlog = 0;
        }
        self.drained.notify_waiters();
        self.died.notify_waiters();
    }

    /// The inline half of the flusher, run by the depositor that won the
    /// election. Synchronous on purpose: it has no `.await`, so it can neither
    /// wait on the socket nor be cancelled while it holds the election.
    ///
    /// Each drained batch is written by polling with a no-op waker until it
    /// completes or reports `Pending`. On `Pending` the unwritten remainder
    /// and the election pass to [`Self::hand_off`]; the no-op registration is
    /// harmless, as the continuation's first poll replaces it with its own.
    ///
    /// No lost wake-up: [`Coalesce::take_batch`] drains, checks for empty and
    /// releases the election under one lock acquisition. A depositor that
    /// inserts after the last drain is either seen on the next iteration or
    /// finds `flushing == false` and becomes the new flusher; a hand-off keeps
    /// `flushing == true` throughout, so its deposits are drained by the
    /// continuation. No interleaving leaves work pending with no flusher.
    fn flush_inline(self: &Arc<Self>) {
        // Only the election holder takes this lock, so it is free unless a
        // continuation that just released the election is still dropping its
        // guard. Rather than wait, let a continuation take over.
        let Ok(mut w) = self.writer.try_lock() else {
            self.hand_off(Vec::new(), 0);
            return;
        };
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..INLINE_BATCH_LIMIT {
            let Some(batch) = self.coalesce.lock().unwrap().take_batch() else {
                return;
            };
            let mut off = 0;
            while off < batch.len() {
                match Pin::new(&mut **w).poll_write(&mut cx, &batch[off..]) {
                    Poll::Ready(Ok(n)) if n > 0 => off += n,
                    Poll::Ready(_) => {
                        // An error, or a zero-length write: the peer is gone.
                        drop(w);
                        self.mark_dead();
                        return;
                    }
                    Poll::Pending => {
                        drop(w);
                        self.hand_off(batch, off);
                        return;
                    }
                }
            }
            self.wrote(batch.len());
        }
        // Still being fed after INLINE_BATCH_LIMIT batches: let a continuation
        // carry on so this depositor can return to its own work.
        drop(w);
        self.hand_off(Vec::new(), 0);
    }

    /// Pass the election, and `batch[off..]` (bytes that must reach the
    /// socket before anything else), to a spawned continuation. The caller
    /// holds the election and leaves `flushing == true` for the continuation.
    fn hand_off(self: &Arc<Self>, batch: Vec<u8>, off: usize) {
        match Handle::try_current() {
            Ok(rt) => {
                let cw = self.clone();
                rt.spawn(async move { cw.flush_async(batch, off).await });
            }
            // No runtime to continue on, so the remainder can never be
            // written, and a partial frame already on the wire would desync
            // the stream. All that is left is to give the connection up.
            Err(_) => self.mark_dead(),
        }
    }

    /// The continuation: write `batch[off..]`, then keep draining until the
    /// lanes are empty. The only code that awaits the socket, and every such
    /// await is bounded by the write-stall deadline.
    async fn flush_async(self: Arc<Self>, mut batch: Vec<u8>, mut off: usize) {
        // Cancel safety: if this task is cancelled (runtime shutdown) while it
        // holds the election, the guard's `Drop` marks the writer dead, which
        // releases the election. Every clean return defuses the guard first.
        let mut guard = FlushGuard {
            cw: self.clone(),
            armed: true,
        };
        let mut w = self.writer.lock().await;
        loop {
            if off < batch.len() {
                if self
                    .write_with_deadline(&mut **w, &batch[off..])
                    .await
                    .is_err()
                {
                    drop(w);
                    self.mark_dead();
                    guard.armed = false;
                    return;
                }
                self.wrote(batch.len());
            }
            let Some(next) = self.coalesce.lock().unwrap().take_batch() else {
                guard.armed = false;
                return;
            };
            batch = next;
            off = 0;
        }
    }

    /// `write_all`, except that each individual write must make progress
    /// within `write_timeout`. A deadline per write rather than per batch
    /// keeps a large batch on a slow-but-live link going while still catching
    /// a peer that has stopped reading.
    async fn write_with_deadline(
        &self,
        w: &mut (dyn AsyncWrite + Send + Unpin),
        mut buf: &[u8],
    ) -> io::Result<()> {
        while !buf.is_empty() {
            match tokio::time::timeout(self.write_timeout, w.write(buf)).await {
                Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(Ok(n)) => buf = &buf[n..],
                Ok(Err(e)) => return Err(e),
                Err(_elapsed) => {
                    tracing::warn!(
                        "peer accepted no bytes for {:?}; marking the connection dead",
                        self.write_timeout
                    );
                    return Err(io::ErrorKind::TimedOut.into());
                }
            }
        }
        Ok(())
    }
}

/// Gives the connection up if the continuation task is dropped (cancelled)
/// while it holds the election. Defused on every clean return.
///
/// Nothing keeps the continuation's `JoinHandle`, so only a runtime shutdown
/// can cancel it, and then no successor could run anyway. The batch it was
/// writing is gone, possibly half on the wire, so the stream cannot be
/// resumed. Marking the writer dead releases the election, drops the queues
/// and wakes every waiter, so nothing is wedged behind a stale
/// `flushing == true`.
struct FlushGuard {
    cw: Arc<ConnWriter>,
    armed: bool,
}

impl Drop for FlushGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cw.mark_dead();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::pin::Pin;
    use tokio::io::AsyncWrite;

    /// A sink that records each `write_all` payload and is always ready.
    #[derive(Clone)]
    struct RecSink {
        writes: Arc<StdMutex<Vec<Vec<u8>>>>,
    }
    impl RecSink {
        fn new() -> Self {
            Self {
                writes: Arc::new(StdMutex::new(Vec::new())),
            }
        }
    }
    impl AsyncWrite for RecSink {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.writes.lock().unwrap().push(buf.to_vec());
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Sink whose writes block until the test grants permits, so a flusher can
    /// be pinned mid-write while other tasks deposit.
    #[derive(Clone)]
    struct GateSink {
        writes: Arc<StdMutex<Vec<Vec<u8>>>>,
        inner: Arc<StdMutex<GateInner>>,
    }
    struct GateInner {
        permits: usize,
        waker: Option<Waker>,
    }
    impl GateSink {
        fn new() -> Self {
            Self {
                writes: Arc::new(StdMutex::new(Vec::new())),
                inner: Arc::new(StdMutex::new(GateInner {
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
    impl AsyncWrite for GateSink {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let mut g = self.inner.lock().unwrap();
            if g.permits == 0 {
                g.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
            g.permits -= 1;
            drop(g);
            self.writes.lock().unwrap().push(buf.to_vec());
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Poll a `GateSink` until `frame` has been written, up to a generous
    /// wall-clock deadline. Used instead of a fixed spin count because delivery
    /// may happen on a separately scheduled task (a handoff continuation or a
    /// re-elected flusher), so completion is not synchronous with the caller.
    async fn wait_for(sink: &GateSink, frame: &Vec<u8>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            // A frame may now share a write with others in the same batch, so
            // look for it as a subsequence rather than as a whole write.
            if sink
                .writes
                .lock()
                .unwrap()
                .iter()
                .any(|w| w.windows(frame.len()).any(|c| c == frame.as_slice()))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }

    /// Sink that accepts the first `first` bytes immediately, then parks every
    /// write until released, after which it accepts everything. Pins a
    /// flusher mid-batch, with part of the batch already on the wire.
    #[derive(Clone)]
    struct TrickleSink {
        bytes: Arc<StdMutex<Vec<u8>>>,
        gate: GateSink,
        first: Arc<StdMutex<usize>>,
    }
    impl TrickleSink {
        fn new(first: usize) -> Self {
            Self {
                bytes: Arc::new(StdMutex::new(Vec::new())),
                gate: GateSink::new(),
                first: Arc::new(StdMutex::new(first)),
            }
        }
    }
    impl AsyncWrite for TrickleSink {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let mut first = self.first.lock().unwrap();
            let n = if *first > 0 {
                let n = (*first).min(buf.len());
                *first -= n;
                n
            } else {
                drop(first);
                let mut g = self.gate.inner.lock().unwrap();
                if g.permits == 0 {
                    g.waker = Some(cx.waker().clone());
                    return Poll::Pending;
                }
                buf.len()
            };
            self.bytes.lock().unwrap().extend_from_slice(&buf[..n]);
            Poll::Ready(Ok(n))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// `fut` must finish without the socket making progress; a deposit that
    /// waited on I/O would hang here instead.
    async fn promptly<F: std::future::Future>(fut: F) -> F::Output {
        tokio::time::timeout(std::time::Duration::from_secs(1), fut)
            .await
            .expect("must not wait on the socket")
    }

    /// Sink that always fails, to exercise the dead-socket path.
    struct ErrSink;
    impl AsyncWrite for ErrSink {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "dead")))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn single_frame_is_written() {
        // A ready socket is written inline, before the deposit returns.
        let sink = RecSink::new();
        let cw = ConnWriter::new(sink.clone());
        cw.send_control(vec![1, 2, 3]).await;
        assert_eq!(*sink.writes.lock().unwrap(), vec![vec![1, 2, 3]]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn distinct_ioids_all_delivered_under_concurrency() {
        // No lost wakeup: every distinct deposit is eventually written even
        // when many tasks race to become the flusher.
        let sink = RecSink::new();
        let cw = ConnWriter::new(sink.clone());
        let mut handles = Vec::new();
        for i in 0u32..64 {
            let cw = cw.clone();
            handles.push(tokio::spawn(async move {
                cw.send_monitor(i, vec![i as u8]).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let want: std::collections::HashSet<u8> = (0u32..64).map(|i| i as u8).collect();
        // A long run of deposits may end on a continuation task, so wait for
        // the condition rather than assume the last depositor wrote it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let got: std::collections::HashSet<u8> = sink
                .writes
                .lock()
                .unwrap()
                .iter()
                .flat_map(|w| w.iter().copied())
                .collect();
            if got == want || std::time::Instant::now() > deadline {
                assert_eq!(got, want, "every distinct ioid must be delivered");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn monitor_frames_coalesce_to_latest() {
        let sink = GateSink::new();
        let cw = ConnWriter::new(sink.clone());

        // A finds the socket not ready and hands {1:A} to a continuation,
        // which parks inside the write. A itself returns at once.
        promptly(cw.send_monitor(1, vec![0xAA])).await;
        while !sink.parked() {
            tokio::task::yield_now().await;
        }

        // While A's write is pending, deposit two more frames for the ioid.
        promptly(cw.send_monitor(1, vec![0xBB])).await; // coalesced away
        promptly(cw.send_monitor(1, vec![0xCC])).await; // latest wins

        sink.release(8); // let A's write and the follow-up drain proceed
        wait_for(&sink, &vec![0xCC]).await;

        let writes = sink.writes.lock().unwrap().clone();
        assert_eq!(
            writes,
            vec![vec![0xAA], vec![0xCC]],
            "B must be conflated away"
        );
    }

    #[tokio::test]
    async fn control_lane_drains_before_monitor_lane_in_one_write() {
        let sink = GateSink::new();
        let cw = ConnWriter::new(sink.clone());

        // C1 is handed to a continuation that parks inside the write.
        promptly(cw.send_control(vec![0xC1])).await;
        while !sink.parked() {
            tokio::task::yield_now().await;
        }

        // Deposit a monitor frame, then a control frame: same batch.
        cw.send_monitor(7, vec![0x77]).await;
        cw.send_control(vec![0xC2]).await;

        sink.release(8);
        wait_for(&sink, &vec![0xC2, 0x77]).await;

        let writes = sink.writes.lock().unwrap().clone();
        assert_eq!(
            writes,
            vec![vec![0xC1], vec![0xC2, 0x77]],
            "a batch must reach the socket in ONE write, control lane \
             first: the flat-combining writer collects a batch and then must \
             not hand it to the kernel one frame at a time"
        );
    }

    #[tokio::test]
    async fn dead_socket_drops_further_deposits_without_panic() {
        let cw = ConnWriter::new(ErrSink);
        cw.send_control(vec![1]).await; // write fails -> marks dead
        assert!(cw.is_dead());
        cw.send_monitor(1, vec![2]).await; // dropped, must not panic
        cw.send_control(vec![3]).await; // dropped, must not panic
        promptly(cw.closed()).await;
    }

    #[tokio::test]
    async fn deposit_never_waits_on_a_stalled_socket() {
        // The socket accepts nothing until released, and a frame stays
        // queued meanwhile; neither lane's deposit may wait on it.
        let sink = GateSink::new();
        let cw = ConnWriter::new(sink.clone());
        promptly(cw.send_control(vec![0x01])).await;
        promptly(cw.send_monitor(5, vec![0x05])).await;
        promptly(cw.send_control(vec![0x02])).await;
        assert!(sink.writes.lock().unwrap().is_empty());
        assert!(!cw.is_dead());

        sink.release(100);
        wait_for(&sink, &vec![0x02, 0x05]).await;
        assert_eq!(
            *sink.writes.lock().unwrap(),
            vec![vec![0x01], vec![0x02, 0x05]]
        );
    }

    #[tokio::test]
    async fn a_partially_written_batch_finishes_before_anything_else() {
        // The inline attempt gets two bytes of [1,2,3,4] onto the wire before
        // the socket fills. The continuation must write the remainder before
        // any later frame, or the peer sees a torn frame (lossless FIFO).
        let sink = TrickleSink::new(2);
        let cw = ConnWriter::new(sink.clone());
        promptly(cw.send_control(vec![1, 2, 3, 4])).await;
        assert_eq!(*sink.bytes.lock().unwrap(), vec![1, 2]);
        promptly(cw.send_control(vec![5, 6])).await;
        promptly(cw.send_monitor(9, vec![9])).await;

        sink.gate.release(1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while sink.bytes.lock().unwrap().len() < 7 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(*sink.bytes.lock().unwrap(), vec![1, 2, 3, 4, 5, 6, 9]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_that_stalls_past_the_deadline_marks_the_connection_dead() {
        let sink = GateSink::new();
        let cw = ConnWriter::with_write_timeout(sink.clone(), std::time::Duration::from_secs(3));
        cw.send_control(vec![0x01]).await;
        cw.send_monitor(1, vec![0x02]).await;
        assert!(!cw.is_dead(), "not before the deadline");

        // Paused clock: auto-advances to the deadline once all tasks idle.
        tokio::time::timeout(std::time::Duration::from_secs(60), cw.closed())
            .await
            .expect("a stalled write must mark the connection dead");
        assert!(cw.is_dead());

        // Everything queued was dropped, and later deposits are refused.
        cw.send_control(vec![0x03]).await;
        sink.release(100);
        tokio::task::yield_now().await;
        assert!(sink.writes.lock().unwrap().is_empty());
        assert_eq!(cw.coalesce.lock().unwrap().backlog, 0);
    }

    #[tokio::test]
    async fn backpressure_waits_for_the_backlog_to_drain() {
        let sink = GateSink::new();
        let cw = ConnWriter::new(sink.clone());
        promptly(cw.backpressure()).await; // empty backlog: no wait

        cw.send_control(vec![0u8; BACKLOG_HIGH_WATER + 1]).await;
        let waiting = tokio::time::timeout(std::time::Duration::from_millis(50), cw.backpressure());
        assert!(
            waiting.await.is_err(),
            "the connection's own loop must wait while it is over the high-water mark"
        );

        sink.release(1);
        promptly(cw.backpressure()).await;
        assert_eq!(cw.coalesce.lock().unwrap().backlog, 0);
    }

    #[tokio::test]
    async fn coalesced_monitor_frames_are_not_counted_twice() {
        let sink = GateSink::new();
        let cw = ConnWriter::new(sink.clone());
        cw.send_control(vec![0; 3]).await; // parks in the continuation
        cw.send_monitor(1, vec![0; 10]).await;
        cw.send_monitor(1, vec![0; 4]).await; // replaces the 10-byte frame
        assert_eq!(cw.coalesce.lock().unwrap().backlog, 3 + 4);
    }
}
