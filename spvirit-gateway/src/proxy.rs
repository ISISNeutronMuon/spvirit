//! `GatewaySource` — the [`spvirit_server::pvstore::Source`] implementation
//! that bridges downstream requests to upstream [`spvirit_client::PvaClient`]s.
//!
//! M1 only wires up `claim`: search each configured upstream client in order
//! and cache the first hit. `get`/`put`/`subscribe`/`names` are placeholder
//! stubs replaced in later tasks (10-13).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_types::NtPayload;
use tokio::sync::mpsc;

use crate::cache::negative::NegativeCache;
use crate::loopguard::LoopGuard;
use crate::upstream::UpstreamPool;

/// Record of which upstream client resolved a downstream PV name, and under
/// what name it is known upstream.
///
/// Kept minimal (just the two fields Tasks 10-13 need to route `get`/`put`/
/// `subscribe` back to the right client) so it doubles as the binding cache
/// value with no rework required.
#[derive(Debug, Clone)]
pub struct Binding {
    pub client_name: String,
    pub real_name: String,
}

/// A [`Source`] that resolves and forwards PVs to upstream PVA servers.
pub struct GatewaySource {
    pool: Arc<UpstreamPool>,
    client_order: Vec<String>,
    neg: Arc<NegativeCache>,
    #[allow(dead_code)] // consulted by the server-side search path, wired in Task 14
    guard: Arc<LoopGuard>,
    #[allow(dead_code)] // consumed by `get`'s holdoff logic, wired in Task 10
    getholdoff_ms: u32,
    bindings: Mutex<HashMap<String, Binding>>,
}

impl GatewaySource {
    /// Build a source that searches `client_order` (in order) for each
    /// downstream name, backed by `pool`, remembering upstream misses in
    /// `neg` and (later) refusing to resolve back into our own servers via
    /// `guard`.
    pub fn new(
        pool: Arc<UpstreamPool>,
        client_order: Vec<String>,
        neg: Arc<NegativeCache>,
        guard: Arc<LoopGuard>,
        getholdoff_ms: u32,
    ) -> Self {
        GatewaySource {
            pool,
            client_order,
            neg,
            guard,
            getholdoff_ms,
            bindings: Mutex::new(HashMap::new()),
        }
    }
}

impl Source for GatewaySource {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let now = Instant::now();
            if self.neg.is_missing(&name, now) {
                return None;
            }

            for client_name in &self.client_order {
                let Some(client) = self.pool.client(client_name) else {
                    continue;
                };
                if let Ok(descriptor) = client.pvinfo(&name).await {
                    self.bindings.lock().unwrap().insert(
                        name.clone(),
                        Binding {
                            client_name: client_name.clone(),
                            real_name: name.clone(),
                        },
                    );
                    return Some(PvInfo {
                        descriptor,
                        writable: true,
                    });
                }
            }

            self.neg.record_miss(&name, now);
            None
        })
    }

    fn get(&self, _name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        Box::pin(async { None })
    }

    fn put(
        &self,
        _name: &str,
        _value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        Box::pin(async { Err("not implemented in Task 9".to_string()) })
    }

    fn subscribe(
        &self,
        _name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        Box::pin(async { None })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async { vec![] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;
    use std::time::Duration;

    #[tokio::test]
    async fn claim_returns_none_when_no_clients_configured() {
        let cfg = GatewayConfig::from_json_str(r#"{"version":2,"clients":[],"servers":[]}"#)
            .unwrap();
        let pool = Arc::new(UpstreamPool::from_config(&cfg));
        let neg = Arc::new(NegativeCache::new(Duration::from_secs(30), 128));
        let guard = Arc::new(LoopGuard::build(
            &cfg,
            &crate::config::ServerCfg {
                name: "s".into(),
                clients: vec![],
                interface: vec![],
                addrlist: String::new(),
                ignoreaddr: String::new(),
                autoaddrlist: true,
                serverport: 5075,
                bcastport: 5076,
                getholdoff: 0,
                statusprefix: String::new(),
                access: String::new(),
                pvlist: String::new(),
                acf_client: None,
                x_spvirit: None,
            },
        ));
        let src = GatewaySource::new(pool, vec![], neg, guard, 0);
        assert!(src.claim("ANY:PV").await.is_none());
    }
}
