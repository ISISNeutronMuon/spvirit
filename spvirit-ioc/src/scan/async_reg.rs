//! The binding from a record to its async device support.
//!
//! [`crate::process::AsyncSupport`] is the *contract* for two-phase device
//! support; this registry is how a record finds its implementation during a
//! processing pass. `record_body` threads the registry in through `ProcCtx`
//! (the same way it threads the clock) and looks the record up by
//! [`RecordId`] at the async hook point. A record with no binding processes
//! synchronously, exactly as it did before async support went live.
//!
//! The map is behind an `RwLock` because bindings are established once (at
//! init) and then only read on the hot processing path, which may run from
//! several scan threads at once.

use crate::lockset::RecordId;
use crate::process::AsyncSupport;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Binds records to the async device support that drives them.
pub struct AsyncRegistry {
    map: RwLock<HashMap<RecordId, Arc<dyn AsyncSupport>>>,
}

impl AsyncRegistry {
    /// An empty registry: every record processes synchronously until bound.
    pub fn new() -> AsyncRegistry {
        AsyncRegistry {
            map: RwLock::new(HashMap::new()),
        }
    }

    /// Bind `id` to `support`. A later bind for the same record replaces the
    /// earlier one (last binding wins).
    pub fn bind(&self, id: RecordId, support: Arc<dyn AsyncSupport>) {
        self.map.write().expect("registry lock poisoned").insert(id, support);
    }

    /// The support bound to `id`, if any. Returns a fresh `Arc` clone so the
    /// caller can drop the lock before running arbitrary support code.
    pub fn get(&self, id: RecordId) -> Option<Arc<dyn AsyncSupport>> {
        self.map
            .read()
            .expect("registry lock poisoned")
            .get(&id)
            .cloned()
    }
}

impl Default for AsyncRegistry {
    fn default() -> Self {
        AsyncRegistry::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::ProcCtx;
    use crate::process::AsyncOutcome;

    /// A trivial support whose only job is to be an identifiable `Arc`.
    struct Marker;
    impl AsyncSupport for Marker {
        fn start(&self, _record: &str, _pact: bool, _ctx: &mut ProcCtx) -> AsyncOutcome {
            AsyncOutcome::Complete
        }
    }

    fn id(set: usize, slot: usize) -> RecordId {
        RecordId { set, slot }
    }

    #[test]
    fn get_returns_the_support_that_was_bound() {
        let reg = AsyncRegistry::new();
        let support: Arc<dyn AsyncSupport> = Arc::new(Marker);
        reg.bind(id(0, 0), Arc::clone(&support));
        let got = reg.get(id(0, 0)).expect("a bound record has support");
        assert!(
            Arc::ptr_eq(&got, &support),
            "get must return the exact Arc that was bound"
        );
    }

    #[test]
    fn get_returns_none_for_an_unbound_record() {
        let reg = AsyncRegistry::new();
        reg.bind(id(0, 0), Arc::new(Marker));
        assert!(
            reg.get(id(0, 1)).is_none(),
            "a record that was never bound has no support"
        );
    }

    #[test]
    fn a_second_bind_replaces_the_first() {
        let reg = AsyncRegistry::new();
        let first: Arc<dyn AsyncSupport> = Arc::new(Marker);
        let second: Arc<dyn AsyncSupport> = Arc::new(Marker);
        reg.bind(id(0, 0), Arc::clone(&first));
        reg.bind(id(0, 0), Arc::clone(&second));
        let got = reg.get(id(0, 0)).expect("still bound");
        assert!(
            Arc::ptr_eq(&got, &second),
            "the later bind must win"
        );
        assert!(
            !Arc::ptr_eq(&got, &first),
            "the earlier binding must be gone"
        );
    }
}
