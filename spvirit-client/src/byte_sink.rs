//! Optional upstream byte-accounting seam.
//!
//! [`ByteSink`] lets a caller (e.g. the gateway) observe wire bytes sent and
//! received by [`PvaClient`](crate::pva_client::PvaClient) without
//! `spvirit-client` depending on the caller's crate. The client holds an
//! `Option<Arc<dyn ByteSink>>`; when it is `None` (the default) the
//! accounting call sites are pure no-ops.

/// Observes wire-level bytes sent/received by a [`PvaClient`](crate::pva_client::PvaClient).
///
/// Implementations must be cheap and non-blocking: `on_tx`/`on_rx` are called
/// as plain synchronous statements adjacent to the actual send/recv, outside
/// any lock the client holds, and must not block or perform async I/O.
pub trait ByteSink: Send + Sync {
    /// Called once per outbound wire write, with the real PV/channel name,
    /// the server host the bytes were sent to, and the exact wire-byte count.
    fn on_tx(&self, pv: &str, host: &str, n: u64);

    /// Called once per inbound wire read, with the real PV/channel name, the
    /// server host the bytes were received from, and the exact wire-byte
    /// count.
    fn on_rx(&self, pv: &str, host: &str, n: u64);
}
