//! Server-wide lifecycle hooks and named events.

use std::sync::{Arc, RwLock};

/// A synchronous consumer of named events.
///
/// Implementors are called inline on the `post_event` caller's thread and
/// must have finished their work when they return — this is what makes the
/// "records have processed when `post_event` returns" guarantee true.
/// Sub-project B implements this on its `Scanner` to drive `EVNT` scan lists.
pub trait EventSink: Send + Sync {
    /// Handle `event`. Called on the poster's thread; keep it short.
    fn on_event(&self, event: &str);
}

/// Server-wide event registry.
///
/// Owns the synchronous sinks. Deferred handlers are added in a later task.
pub struct Events {
    sinks: RwLock<Vec<Arc<dyn EventSink>>>,
}

impl Events {
    pub fn new() -> Self {
        Self {
            sinks: RwLock::new(Vec::new()),
        }
    }

    /// Register a synchronous sink. Sinks are called in registration order.
    pub fn add_sink(&self, sink: Arc<dyn EventSink>) {
        self.sinks.write().unwrap().push(sink);
    }

    /// Post `event`: call every sink inline, in registration order.
    ///
    /// An unknown event name is a no-op — events are a dynamic namespace.
    pub fn post(&self, event: &str) {
        let sinks = self.sinks.read().unwrap().clone();
        for sink in &sinks {
            sink.on_event(event);
        }
    }
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingSink {
        seen: Mutex<Vec<String>>,
    }

    impl EventSink for RecordingSink {
        fn on_event(&self, event: &str) {
            self.seen.lock().unwrap().push(event.to_string());
        }
    }

    #[test]
    fn post_calls_sinks_in_registration_order() {
        let a = Arc::new(RecordingSink { seen: Mutex::new(Vec::new()) });
        let b = Arc::new(RecordingSink { seen: Mutex::new(Vec::new()) });
        let events = Events::new();
        events.add_sink(a.clone());
        events.add_sink(b.clone());

        events.post("SHUTTER");

        assert_eq!(a.seen.lock().unwrap().as_slice(), &["SHUTTER".to_string()]);
        assert_eq!(b.seen.lock().unwrap().as_slice(), &["SHUTTER".to_string()]);
    }

    #[test]
    fn post_with_no_sinks_is_a_noop() {
        let events = Events::new();
        events.post("NOBODY:LISTENING");
    }
}
