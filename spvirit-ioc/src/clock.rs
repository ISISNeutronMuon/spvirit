//! The single source of time for the scan engine.
//!
//! Every scan mechanism reads time through a `Clock`, so tests advance a
//! `ManualClock` explicitly and never call `thread::sleep`.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A source of monotonic time the engine can wait against.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
    /// Block the calling thread until `now() >= deadline`.
    fn sleep_until(&self, deadline: Instant);
}

/// The real OS clock. Used in production.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep_until(&self, deadline: Instant) {
        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        }
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
    accumulated: Mutex<Duration>,
    cvar: Condvar,
}

impl ManualClock {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                anchor: Instant::now(),
                accumulated: Mutex::new(Duration::ZERO),
                cvar: Condvar::new(),
            }),
        }
    }

    /// Move time forward and wake anyone waiting on a now-passed deadline.
    pub fn advance(&self, by: Duration) {
        let mut acc = self.inner.accumulated.lock().unwrap();
        *acc += by;
        self.inner.cvar.notify_all();
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
    fn sleep_until(&self, deadline: Instant) {
        let mut acc = self.inner.accumulated.lock().unwrap();
        while self.inner.anchor + *acc < deadline {
            acc = self.inner.cvar.wait(acc).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn system_clock_sleep_until_past_deadline_returns_immediately() {
        let c = SystemClock;
        let start = Instant::now();
        c.sleep_until(start);
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "a past deadline must not block"
        );
    }

    #[test]
    fn system_clock_sleep_until_future_deadline_blocks_approximately() {
        let c = SystemClock;
        let start = Instant::now();
        let deadline = start + Duration::from_millis(50);
        c.sleep_until(deadline);
        assert!(
            start.elapsed() >= Duration::from_millis(50),
            "a future deadline must actually block until it passes"
        );
    }

    #[test]
    fn manual_clock_sleep_until_actually_blocks_before_deadline() {
        let c = Arc::new(ManualClock::new());
        let deadline = c.now() + Duration::from_millis(200);
        let done = Arc::new(AtomicBool::new(false));
        let c2 = c.clone();
        let done2 = done.clone();
        let waiter = std::thread::spawn(move || {
            c2.sleep_until(deadline);
            done2.store(true, Ordering::SeqCst);
        });
        // Give the waiter thread time to actually park in sleep_until.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !done.load(Ordering::SeqCst),
            "must still be blocked before the deadline is crossed"
        );
        c.advance(Duration::from_millis(200));
        waiter.join().expect("waiter thread wakes and exits");
        assert!(done.load(Ordering::SeqCst));
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
