//! Task-local request context, visible to [`crate::pvstore::Source`] implementations
//! for the duration of a single connection's async task tree.
//!
//! This is a purely additive seam: it does **not** change the `Source` trait
//! signature. Sources that need to know the downstream peer (and, from M2
//! onward, the decoded `ca`-credential user/host) can call
//! [`current_request`] from anywhere within the task spawned for a
//! connection, since `tokio::task_local!` values are inherited by everything
//! that runs on that task (including nested `.await`s), but not by other
//! tasks.
//!
//! Peer address is populated at accept time via [`scope`]. `user`/`host`
//! remain `None` in M1; `set_credentials` exists for M2, which will call it
//! after `ConnectionValidation` decodes the peer's `ca` credentials.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// Snapshot of the current connection's identity, as seen by a [`Source`](crate::pvstore::Source).
#[derive(Clone, Debug)]
pub struct RequestContext {
    pub peer: SocketAddr,
    pub user: Option<String>,
    pub host: Option<String>,
}

/// Internal per-connection identity cell. Held behind an `Arc` in the task-local
/// so that `set_credentials` (called later in the same task, e.g. after
/// `ConnectionValidation`) is visible to any concurrently-running clone of the
/// task-local without needing to re-enter `scope`.
struct ConnIdentity {
    peer: SocketAddr,
    creds: Mutex<Option<(Option<String>, Option<String>)>>,
}

tokio::task_local! {
    static CONN_IDENTITY: Arc<ConnIdentity>;
}

/// Run `fut` with a fresh [`RequestContext`] (peer only) bound to it and
/// everything it `.await`s.
pub(crate) fn scope<F>(peer: SocketAddr, fut: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    let identity = Arc::new(ConnIdentity {
        peer,
        creds: Mutex::new(None),
    });
    CONN_IDENTITY.scope(identity, fut)
}

/// Update the user/host credentials for the current connection's context.
///
/// Intended for use after `ConnectionValidation` decodes `ca` credentials
/// (M2). In M1 this is unused but kept as part of the seam's public surface.
#[allow(dead_code)]
pub(crate) fn set_credentials(user: Option<String>, host: Option<String>) {
    let _ = CONN_IDENTITY.try_with(|identity| {
        *identity.creds.lock().unwrap() = Some((user, host));
    });
}

/// Read the current task's [`RequestContext`], if any.
///
/// Returns `None` when called outside a [`scope`]d task (e.g. from a task
/// that isn't handling a connection).
pub fn current_request() -> Option<RequestContext> {
    CONN_IDENTITY
        .try_with(|identity| {
            let (user, host) = identity
                .creds
                .lock()
                .unwrap()
                .clone()
                .unwrap_or((None, None));
            RequestContext {
                peer: identity.peer,
                user,
                host,
            }
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[tokio::test]
    async fn context_visible_within_scope() {
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let seen = scope(peer, async { current_request() }).await;
        assert_eq!(seen.unwrap().peer, peer);
    }

    #[tokio::test]
    async fn context_absent_outside_scope() {
        assert!(current_request().is_none());
    }
}
