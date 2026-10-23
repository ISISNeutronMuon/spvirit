//! Poison-tolerant locking for the gateway data path.
//!
//! Every `std::sync::Mutex` in the data path guards a short, panic-free
//! critical section (a `HashMap` insert, a `Vec` push, an `Option` store), so
//! a poisoned lock carries no broken invariant — at worst a slightly stale
//! entry. Propagating the poison with `.unwrap()` instead turns one panic
//! anywhere in the crate into a permanent whole-gateway outage: every later
//! task that touches the bindings map or the monitor cache panics too. These
//! helpers recover the guard instead, and document the reasoning once rather
//! than at every call site.

use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

/// Locks `m`, recovering the guard from a poisoned lock rather than panicking.
pub fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `try_lock` that treats a poisoned-but-uncontended lock as available,
/// mirroring [`lock_or_recover`]. `None` means genuinely contended — the only
/// case a caller doing opportunistic work should back off from.
pub fn try_lock_or_recover<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_lock_is_still_usable() {
        let m = Mutex::new(vec![1]);

        // The panic below is expected and prints a backtrace line to the test
        // output; the hook is silenced so it does not look like a failure.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = m.lock().unwrap();
            panic!("poison it");
        }));
        std::panic::set_hook(previous);

        assert!(m.is_poisoned());
        lock_or_recover(&m).push(2);
        assert_eq!(*lock_or_recover(&m), vec![1, 2]);
        assert!(try_lock_or_recover(&m).is_some());
    }

    #[test]
    fn try_lock_or_recover_backs_off_only_when_contended() {
        let m = Mutex::new(0);
        let held = m.lock().unwrap();
        assert!(try_lock_or_recover(&m).is_none());
        drop(held);
        assert!(try_lock_or_recover(&m).is_some());
    }
}
