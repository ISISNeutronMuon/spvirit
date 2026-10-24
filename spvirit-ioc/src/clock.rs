//! The single source of time for the scan engine.
//!
//! Every scan mechanism reads time through a `Clock`, so tests advance a
//! `ManualClock` explicitly and never call `thread::sleep`.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A source of monotonic time the engine can wait against.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
    /// Block the calling thread until `now() >= deadline`, or until a
    /// `wake_all` releases it early.
    fn sleep_until(&self, deadline: Instant) {
        self.sleep_until_or(deadline, &|| false);
    }
    /// [`Self::sleep_until`], but also return as soon as `cancelled()` is
    /// true. `cancelled` is evaluated under the lock `wake_all` takes, before
    /// each wait, so a caller that changes the state `cancelled` reads and
    /// *then* calls `wake_all` can never have that wake lost, however the two
    /// threads interleave. `cancelled` must not call back into this clock.
    fn sleep_until_or(&self, deadline: Instant, cancelled: &dyn Fn() -> bool);
    /// Wall-clock nanoseconds since the UNIX epoch, for record timestamps.
    fn unix_nanos(&self) -> u64;
    /// Wake every thread currently parked in `sleep_until`, regardless of
    /// whether their deadline has passed. Used by shutdown (and by the
    /// periodic-thread reap) to unpark a thread deterministically without
    /// waiting for a real timeout. A woken sleeper returns early, so a caller
    /// that needs its deadline must re-check `now()` and sleep again.
    fn wake_all(&self);
}

/// Wall-clock nanoseconds since the UNIX epoch, from the real OS clock.
fn real_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        // A clock before the epoch is not worth failing a process pass over.
        .unwrap_or(1)
}

/// The real OS clock. Used in production.
///
/// `sleep_until` parks on a condvar with a timeout rather than calling
/// `thread::sleep`, so `wake_all` can release a sleeper early: a scan thread
/// on a very long period (`SCAN = "1e9 second"` is ~31 years) must not make
/// `Scanner::shutdown`'s join wait out its deadline.
#[derive(Debug, Default)]
pub struct SystemClock {
    /// Bumped by `wake_all`. A sleeper snapshots it on entry and returns as
    /// soon as it changes, whether or not its deadline has passed.
    wake_gen: Mutex<u64>,
    cvar: Condvar,
}

impl SystemClock {
    pub const fn new() -> Self {
        Self {
            wake_gen: Mutex::new(0),
            cvar: Condvar::new(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep_until_or(&self, deadline: Instant, cancelled: &dyn Fn() -> bool) {
        let mut wake_gen = self.wake_gen.lock().unwrap();
        let gen_at_entry = *wake_gen;
        // Looped: `wait_timeout` may return spuriously, or early on a timer
        // that rounds down, and neither is a reason to return.
        loop {
            let now = Instant::now();
            if now >= deadline || *wake_gen != gen_at_entry || cancelled() {
                return;
            }
            wake_gen = self.cvar.wait_timeout(wake_gen, deadline - now).unwrap().0;
        }
    }
    fn unix_nanos(&self) -> u64 {
        real_unix_nanos()
    }
    fn wake_all(&self) {
        let mut wake_gen = self.wake_gen.lock().unwrap();
        *wake_gen = wake_gen.wrapping_add(1);
        self.cvar.notify_all();
    }
}

/// A clock that only advances when told to. Deterministic for tests.
///
/// Anchored on a real `Instant` at construction so the `Instant`s it hands
/// out compare correctly with any captured before it; `now()` is
/// `anchor + accumulated`. Threads parked in `sleep_until` wake the moment
/// `advance` pushes the accumulated total past their deadline.
#[derive(Clone)]
pub struct ManualClock {
    inner: Arc<Inner>,
}

struct Inner {
    anchor: Instant,
    /// Real epoch nanos captured at construction, so `unix_nanos` yields a
    /// plausible wall-clock stamp that still advances only on `advance`.
    epoch_base: u64,
    accumulated: Mutex<Duration>,
    cvar: Condvar,
    /// Bumped only by `wake_all`, never by `advance`. `sleep_until` snapshots
    /// this on entry and folds "has it changed" into its wait condition, so
    /// a `wake_all` unparks a sleeper immediately regardless of whether the
    /// deadline has actually passed — distinct from `advance`, whose notify
    /// must NOT release a sleeper short of its deadline (a partial advance
    /// re-checks the same unchanged generation and goes back to sleep).
    wake_gen: std::sync::atomic::AtomicU64,
}

impl ManualClock {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                anchor: Instant::now(),
                epoch_base: real_unix_nanos(),
                accumulated: Mutex::new(Duration::ZERO),
                cvar: Condvar::new(),
                wake_gen: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    /// Move time forward and wake anyone waiting on a now-passed deadline.
    pub fn advance(&self, by: Duration) {
        let mut acc = self.inner.accumulated.lock().unwrap();
        *acc += by;
        self.inner.cvar.notify_all();
    }

    /// The current wall-clock stamp this clock would hand a record, for tests.
    pub fn now_unix_nanos(&self) -> u64 {
        self.unix_nanos()
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.inner.anchor + *self.inner.accumulated.lock().unwrap()
    }
    fn sleep_until_or(&self, deadline: Instant, cancelled: &dyn Fn() -> bool) {
        let gen_at_entry = self.inner.wake_gen.load(Ordering::SeqCst);
        let mut acc = self.inner.accumulated.lock().unwrap();
        while self.inner.anchor + *acc < deadline
            && self.inner.wake_gen.load(Ordering::SeqCst) == gen_at_entry
            && !cancelled()
        {
            acc = self.inner.cvar.wait(acc).unwrap();
        }
    }
    fn unix_nanos(&self) -> u64 {
        self.inner.epoch_base + self.inner.accumulated.lock().unwrap().as_nanos() as u64
    }
    fn wake_all(&self) {
        // Locking `accumulated` here (even though we don't touch it) gives
        // this call a full memory barrier with `sleep_until`'s own lock/
        // unlock, so a flag a caller set just before calling `wake_all`
        // (e.g. Scanner's `stop`) is guaranteed visible to the woken thread
        // once it re-acquires the lock inside `sleep_until`.
        let _acc = self.inner.accumulated.lock().unwrap();
        self.inner.wake_gen.fetch_add(1, Ordering::SeqCst);
        self.inner.cvar.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn system_clock_sleep_until_past_deadline_returns_immediately() {
        let c = SystemClock::new();
        let start = Instant::now();
        c.sleep_until(start);
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "a past deadline must not block"
        );
    }

    #[test]
    fn system_clock_sleep_until_future_deadline_blocks_approximately() {
        let c = SystemClock::new();
        let start = Instant::now();
        let deadline = start + Duration::from_millis(50);
        c.sleep_until(deadline);
        assert!(
            start.elapsed() >= Duration::from_millis(50),
            "a future deadline must actually block until it passes"
        );
    }

    #[test]
    fn system_clock_wake_all_releases_a_far_future_sleeper() {
        // F3: shutdown relies on `wake_all` to unpark a scan thread; a
        // `thread::sleep` cannot be woken, so a long period hung the join.
        let c = Arc::new(SystemClock::new());
        let c2 = c.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            c2.sleep_until(Instant::now() + Duration::from_secs(1_000_000_000));
            done_tx.send(()).unwrap();
        });
        ready_rx.recv().unwrap();
        // Repeat the wake: the sleeper may not have parked yet on the first.
        for _ in 0..50 {
            c.wake_all();
            if done_rx.recv_timeout(Duration::from_millis(20)).is_ok() {
                return;
            }
        }
        panic!("wake_all did not release a sleeper within 1 s");
    }

    #[test]
    fn manual_clock_sleep_until_actually_blocks_before_deadline() {
        let c = Arc::new(ManualClock::new());
        let deadline = c.now() + Duration::from_millis(200);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let c2 = c.clone();
        let waiter = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            c2.sleep_until(deadline);
            done_tx.send(()).unwrap();
        });
        // Deterministic rendezvous: wait for the waiter to signal that it is
        // about to call sleep_until. No real sleep, no timing tolerance.
        ready_rx.recv().expect("waiter signals before calling sleep_until");
        // A partial advance must not release the waiter.
        c.advance(Duration::from_millis(100));
        assert!(
            done_rx.try_recv().is_err(),
            "must still be blocked before the deadline is crossed"
        );
        // Cross the deadline: the waiter must wake and signal completion.
        c.advance(Duration::from_millis(100));
        done_rx.recv().expect("waiter signals after crossing deadline");
        waiter.join().expect("waiter thread wakes and exits");
    }

    #[test]
    fn manual_clock_now_advances_only_on_advance() {
        let c = ManualClock::new();
        let t0 = c.now();
        assert_eq!(c.now(), t0, "now() must not move on its own");
        c.advance(Duration::from_millis(250));
        assert_eq!(c.now(), t0 + Duration::from_millis(250));
    }

    #[test]
    fn manual_clock_advance_is_cumulative() {
        let c = ManualClock::new();
        let t0 = c.now();
        c.advance(Duration::from_millis(100));
        c.advance(Duration::from_millis(50));
        assert_eq!(c.now(), t0 + Duration::from_millis(150));
    }

    #[test]
    fn sleep_until_returns_immediately_if_deadline_already_passed() {
        let c = ManualClock::new();
        let past = c.now();
        c.advance(Duration::from_secs(1));
        // Deadline is in the past; must not block.
        c.sleep_until(past);
    }

    #[test]
    fn sleep_until_wakes_when_advance_crosses_the_deadline() {
        let c = Arc::new(ManualClock::new());
        let deadline = c.now() + Duration::from_millis(500);
        let c2 = c.clone();
        let waiter = std::thread::spawn(move || {
            c2.sleep_until(deadline); // must block until advance below
        });
        // The waiter is parked; a partial advance must not release it.
        c.advance(Duration::from_millis(499));
        // Cross the deadline: the waiter must wake and the thread must join.
        c.advance(Duration::from_millis(1));
        waiter.join().expect("waiter thread wakes and exits");
    }
}
