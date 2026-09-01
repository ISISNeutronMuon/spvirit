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

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::runtime::Handle;
use tokio::sync::Mutex as AsyncMutex;

/// Coalescing state guarded by a std mutex. Never held across an `.await`:
/// every critical section is O(pending) map/deque work.
#[derive(Default)]
struct Coalesce {
    /// Priority lane: complete control/response frames, FIFO.
    control: VecDeque<Vec<u8>>,
    /// Monitor lane: latest encoded frame per ioid (conflation).
    monitor: HashMap<u32, Vec<u8>>,
    /// Single-flight election: true while some task is the designated flusher.
    flushing: bool,
    /// Set once a socket write has failed; further deposits are dropped so a
    /// dead-but-not-closed peer cannot wedge producers.
    dead: bool,
}

/// The per-connection writer: a socket write half plus its coalescing state.
pub struct ConnWriter {
    /// The socket write half. Held only across `write_all`. By the
    /// single-flight flag at most one flusher exists at a time, so contention
    /// on this lock is only between a flusher and one-shot control sends —
    /// which is exactly the serialization we want.
    writer: AsyncMutex<Box<dyn AsyncWrite + Send + Unpin>>,
    coalesce: StdMutex<Coalesce>,
}

impl ConnWriter {
    /// Wrap a write half (real socket, or any async sink in tests).
    pub fn new<W: AsyncWrite + Send + Unpin + 'static>(w: W) -> Arc<Self> {
        Arc::new(Self {
            writer: AsyncMutex::new(Box::new(w)),
            coalesce: StdMutex::new(Coalesce::default()),
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
    pub async fn send_control(self: &Arc<Self>, bytes: Vec<u8>) {
        {
            let mut c = self.coalesce.lock().unwrap();
            if c.dead {
                return;
            }
            c.control.push_back(bytes);
            if c.flushing {
                return;
            }
            c.flushing = true;
        }
        self.flush().await;
    }

    /// Deposit a monitor frame for `ioid` (monitor lane, latest-wins) and
    /// flush if no flusher is currently active.
    pub async fn send_monitor(self: &Arc<Self>, ioid: u32, bytes: Vec<u8>) {
        {
            let mut c = self.coalesce.lock().unwrap();
            if c.dead {
                return;
            }
            c.monitor.insert(ioid, bytes); // coalesce: replace any stale frame
            if c.flushing {
                return;
            }
            c.flushing = true;
        }
        self.flush().await;
    }

    /// Drain-to-latest loop run by whichever task won the flusher election.
    ///
    /// No lost wakeup: the drain, the empty-check, and clearing `flushing` all
    /// happen under a SINGLE `coalesce` lock acquisition. A depositor that
    /// inserts after the last drain is either seen on the next iteration (it
    /// inserted before the flusher re-locked and found the lanes empty) or
    /// finds `flushing == false` and becomes the new flusher. No interleaving
    /// leaves work pending with no flusher.
    async fn flush(self: &Arc<Self>) {
        // Cancel safety: if this future is dropped (its task cancelled, e.g. a
        // pump aborted or a connection handler torn down) while we hold the
        // flusher election mid-`write_all`, the guard's `Drop` hands the
        // election off so the writer never wedges with `flushing == true`.
        // Every clean return defuses the guard first, so the handoff runs only
        // on an actual cancellation.
        let mut guard = FlushGuard {
            cw: self.clone(),
            armed: true,
        };
        loop {
            let (controls, monitors): (Vec<Vec<u8>>, Vec<Vec<u8>>) = {
                let mut c = self.coalesce.lock().unwrap();
                if c.control.is_empty() && c.monitor.is_empty() {
                    c.flushing = false;
                    guard.armed = false;
                    return;
                }
                let controls = c.control.drain(..).collect();
                let monitors = c.monitor.drain().map(|(_ioid, buf)| buf).collect();
                (controls, monitors)
            };
            // Priority: control lane first, then coalesced monitor frames.
            let mut failed = false;
            {
                let mut w = self.writer.lock().await;
                for buf in controls.iter().chain(monitors.iter()) {
                    if w.write_all(buf).await.is_err() {
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                let mut c = self.coalesce.lock().unwrap();
                c.dead = true;
                c.flushing = false;
                c.control.clear();
                c.monitor.clear();
                guard.armed = false;
                return;
            }
        }
    }
}

/// Hands off the flusher election if a `flush` future is dropped (its task
/// cancelled) while it still holds the election. Defused on every clean return
/// from `flush`; only a cancellation leaves it armed, in which case `Drop`
/// ensures a successor exists so pending bytes are never stranded and no
/// depositor is wedged behind a stale `flushing == true`.
struct FlushGuard {
    cw: Arc<ConnWriter>,
    armed: bool,
}

impl Drop for FlushGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Cancelled while holding the election. Decide the successor under a
        // single lock so a depositor cannot slip work in between the check and
        // the action: EITHER spawn a continuation (keeping `flushing == true`)
        // OR clear the flag — never both, which would create two flushers.
        let mut c = self.cw.coalesce.lock().unwrap();
        if c.dead || (c.control.is_empty() && c.monitor.is_empty()) {
            // Nothing left to write: just release the election.
            c.flushing = false;
            return;
        }
        // Work remains. Keep `flushing == true` and hand off to a continuation
        // that takes over draining. A depositor arriving before it re-locks
        // sees `flushing == true` and simply enqueues; the continuation drains
        // it. If there is no runtime to spawn on (process shutting down), fall
        // back to clearing the flag so a later depositor re-elects.
        match Handle::try_current() {
            Ok(rt) => {
                drop(c);
                let cw = self.cw.clone();
                rt.spawn(async move { cw.flush().await });
            }
            Err(_) => {
                c.flushing = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};
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
            if sink.writes.lock().unwrap().iter().any(|w| w == frame) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
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
        let sink = RecSink::new();
        let cw = ConnWriter::new(sink.clone());
        cw.send_control(vec![1, 2, 3]).await;
        assert_eq!(*sink.writes.lock().unwrap(), vec![vec![1, 2, 3]]);
    }

    #[tokio::test]
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
        let got: std::collections::HashSet<u8> = sink
            .writes
            .lock()
            .unwrap()
            .iter()
            .map(|w| w[0])
            .collect();
        let want: std::collections::HashSet<u8> = (0u32..64).map(|i| i as u8).collect();
        assert_eq!(got, want, "every distinct ioid must be delivered");
    }

    #[tokio::test]
    async fn monitor_frames_coalesce_to_latest() {
        let sink = GateSink::new();
        let cw = ConnWriter::new(sink.clone());

        // Task A becomes flusher, drains {1:A}, and parks inside write_all(A).
        let cwa = cw.clone();
        let a = tokio::spawn(async move { cwa.send_monitor(1, vec![0xAA]).await });
        while !sink.parked() {
            tokio::task::yield_now().await;
        }

        // While A is pinned, deposit two more frames for the same ioid.
        cw.send_monitor(1, vec![0xBB]).await; // coalesced away
        cw.send_monitor(1, vec![0xCC]).await; // latest wins

        sink.release(8); // let A's write and the follow-up drain proceed
        a.await.unwrap();

        let writes = sink.writes.lock().unwrap().clone();
        assert_eq!(writes, vec![vec![0xAA], vec![0xCC]], "B must be conflated away");
    }

    #[tokio::test]
    async fn control_lane_drains_before_monitor_lane() {
        let sink = GateSink::new();
        let cw = ConnWriter::new(sink.clone());

        // Task A becomes flusher on C1 and parks inside write_all(C1).
        let cwa = cw.clone();
        let a = tokio::spawn(async move { cwa.send_control(vec![0xC1]).await });
        while !sink.parked() {
            tokio::task::yield_now().await;
        }

        // Deposit a monitor frame, then a control frame: same batch.
        cw.send_monitor(7, vec![0x77]).await;
        cw.send_control(vec![0xC2]).await;

        sink.release(8);
        a.await.unwrap();

        let writes = sink.writes.lock().unwrap().clone();
        assert_eq!(writes, vec![vec![0xC1], vec![0xC2], vec![0x77]]);
    }

    #[tokio::test]
    async fn dead_socket_drops_further_deposits_without_panic() {
        let cw = ConnWriter::new(ErrSink);
        cw.send_control(vec![1]).await; // write fails -> marks dead
        cw.send_monitor(1, vec![2]).await; // dropped, must not panic
        cw.send_control(vec![3]).await; // dropped, must not panic
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flusher_cancelled_mid_write_does_not_wedge_writer() {
        // If the task holding the flusher election is cancelled mid-write, the
        // election must be released so a later depositor is not wedged with
        // `flushing == true` forever.
        let sink = GateSink::new();
        let cw = ConnWriter::new(sink.clone());

        // Task A becomes flusher and parks inside write_all(A1).
        let cwa = cw.clone();
        let a = tokio::spawn(async move { cwa.send_control(vec![0xA1]).await });
        while !sink.parked() {
            tokio::task::yield_now().await;
        }

        // Cancel A while it holds the election mid-write.
        a.abort();
        let _ = a.await;

        // A fresh deposit must still reach the socket.
        let cwb = cw.clone();
        let b = tokio::spawn(async move { cwb.send_control(vec![0xB2]).await });

        // Grant plenty of permits up front so delivery is never permit-starved,
        // then wait on the actual condition with a real wall-clock deadline:
        // `b` writes on its own task, so completion is not synchronous with this
        // loop and a fixed iteration count could expire before it is scheduled.
        sink.release(100);
        wait_for(&sink, &vec![0xB2]).await;
        let _ = b.await;

        let writes = sink.writes.lock().unwrap().clone();
        assert!(
            writes.contains(&vec![0xB2]),
            "recovery frame must be written; writer wedged: {writes:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flusher_cancelled_with_pending_work_hands_off() {
        // If the flusher is cancelled while other frames are already queued,
        // the pending work must be handed off to a continuation, not stranded.
        let sink = GateSink::new();
        let cw = ConnWriter::new(sink.clone());

        // Task A becomes flusher and parks inside write_all(A1).
        let cwa = cw.clone();
        let a = tokio::spawn(async move { cwa.send_control(vec![0xA1]).await });
        while !sink.parked() {
            tokio::task::yield_now().await;
        }

        // Queue another frame while A is the (parked) flusher — it just enqueues.
        cw.send_control(vec![0xB2]).await;

        // Cancel A mid-write; the queued 0xB2 must be delivered by a handoff.
        a.abort();
        let _ = a.await;

        // The handoff continuation runs on a freshly spawned task, so its write
        // is not synchronous with this loop. Grant permits up front and wait on
        // the delivery condition with a real deadline (a fixed spin count can
        // expire before the continuation is first scheduled under load).
        sink.release(100);
        wait_for(&sink, &vec![0xB2]).await;

        let writes = sink.writes.lock().unwrap().clone();
        assert!(
            writes.contains(&vec![0xB2]),
            "pending frame must be delivered by the handoff continuation; got {writes:?}"
        );
    }
}
