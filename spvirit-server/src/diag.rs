//! Live registry of connected downstream clients, keyed by connection id.
//!
//! Populated by the connection lifecycle (connect/identity/disconnect/byte
//! counters) and read by the gateway's `clients` diagnostic PV and per-host
//! bandwidth accounting.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// A point-in-time snapshot of one connected client, for diagnostics.
#[derive(Clone, Debug)]
pub struct ClientEntry {
    pub peer: SocketAddr,
    pub user: Option<String>,
    pub host: Option<String>,
    pub tx: u64,
    pub rx: u64,
}

/// Internal per-connection record. Byte counters are atomics so `add_tx`/
/// `add_rx` can update them without taking the registry mutex.
struct ConnRecord {
    peer: SocketAddr,
    user: Option<String>,
    host: Option<String>,
    tx: AtomicU64,
    rx: AtomicU64,
}

/// Live registry of connected downstream clients, keyed by connection id.
pub struct ClientRegistry {
    conns: Mutex<HashMap<u64, ConnRecord>>,
}

impl ClientRegistry {
    pub fn new() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
        }
    }

    /// Register a newly-connected client. Overwrites any existing entry for
    /// `conn_id` (a fresh connect for a reused id).
    pub fn connect(&self, conn_id: u64, peer: SocketAddr) {
        let mut conns = self.conns.lock().unwrap();
        conns.insert(
            conn_id,
            ConnRecord {
                peer,
                user: None,
                host: None,
                tx: AtomicU64::new(0),
                rx: AtomicU64::new(0),
            },
        );
    }

    /// Update the identity of a known connection in place. No-op if the
    /// connection id is unknown.
    pub fn set_identity(&self, conn_id: u64, user: Option<String>, host: Option<String>) {
        let mut conns = self.conns.lock().unwrap();
        if let Some(rec) = conns.get_mut(&conn_id) {
            rec.user = user;
            rec.host = host;
        }
    }

    /// Remove a connection from the registry.
    pub fn disconnect(&self, conn_id: u64) {
        let mut conns = self.conns.lock().unwrap();
        conns.remove(&conn_id);
    }

    /// Add `n` bytes to the transmit counter for `conn_id`. No-op if unknown.
    pub fn add_tx(&self, conn_id: u64, n: u64) {
        let conns = self.conns.lock().unwrap();
        if let Some(rec) = conns.get(&conn_id) {
            rec.tx.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Add `n` bytes to the receive counter for `conn_id`. No-op if unknown.
    pub fn add_rx(&self, conn_id: u64, n: u64) {
        let conns = self.conns.lock().unwrap();
        if let Some(rec) = conns.get(&conn_id) {
            rec.rx.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Snapshot all connections' identity and byte counters.
    pub fn snapshot(&self) -> Vec<ClientEntry> {
        let conns = self.conns.lock().unwrap();
        conns
            .values()
            .map(|rec| ClientEntry {
                peer: rec.peer,
                user: rec.user.clone(),
                host: rec.host.clone(),
                tx: rec.tx.load(Ordering::Relaxed),
                rx: rec.rx.load(Ordering::Relaxed),
            })
            .collect()
    }

    /// Aggregate byte counts by `(account, client_ip)`. Sums `tx` if `tx` is
    /// true, else sums `rx`. Returns `(account, client_ip, bytes)` tuples.
    pub fn byhost(&self, tx: bool) -> Vec<(String, String, u64)> {
        let conns = self.conns.lock().unwrap();
        let mut agg: HashMap<(String, String), u64> = HashMap::new();
        for rec in conns.values() {
            let key = (
                rec.user.clone().unwrap_or_default(),
                rec.peer.ip().to_string(),
            );
            let n = if tx {
                rec.tx.load(Ordering::Relaxed)
            } else {
                rec.rx.load(Ordering::Relaxed)
            };
            *agg.entry(key).or_insert(0) += n;
        }
        agg.into_iter()
            .map(|((account, client_ip), bytes)| (account, client_ip, bytes))
            .collect()
    }
}

impl Default for ClientRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A per-key cumulative byte counter.
pub struct ByteMap {
    counts: Mutex<HashMap<String, u64>>,
}

impl ByteMap {
    pub fn new() -> Self {
        Self {
            counts: Mutex::new(HashMap::new()),
        }
    }

    /// Add `n` bytes to the counter for `key`, creating it if absent.
    pub fn add(&self, key: &str, n: u64) {
        let mut counts = self.counts.lock().unwrap();
        *counts.entry(key.to_string()).or_insert(0) += n;
    }

    /// Snapshot all key/count pairs.
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        let counts = self.counts.lock().unwrap();
        counts.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }
}

impl Default for ByteMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Cumulative wire-byte counters, keyed by PV name or by host, split by
/// downstream (ds, gateway-to-client) and upstream (us, gateway-to-IOC)
/// direction. `ds_byhost_{tx,rx}` are NOT here — they are derived from
/// `ClientRegistry::byhost(..)`; do not add fields for them here.
pub struct BandwidthCounters {
    pub ds_bypv_tx: ByteMap,
    pub ds_bypv_rx: ByteMap,
    pub us_bypv_tx: ByteMap,
    pub us_bypv_rx: ByteMap,
    pub us_byhost_tx: ByteMap,
    pub us_byhost_rx: ByteMap,
}

impl BandwidthCounters {
    pub fn new() -> Self {
        Self {
            ds_bypv_tx: ByteMap::new(),
            ds_bypv_rx: ByteMap::new(),
            us_bypv_tx: ByteMap::new(),
            us_bypv_rx: ByteMap::new(),
            us_byhost_tx: ByteMap::new(),
            us_byhost_rx: ByteMap::new(),
        }
    }
}

impl Default for BandwidthCounters {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn registry_tracks_connect_identity_disconnect() {
        let r = ClientRegistry::new();
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), 40000);
        r.connect(1, peer);
        r.set_identity(1, Some("alice".into()), Some("host-a".into()));
        let snap = r.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].user.as_deref(), Some("alice"));
        assert_eq!(snap[0].peer, peer);
        r.disconnect(1);
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn set_identity_on_unknown_conn_is_ignored() {
        let r = ClientRegistry::new();
        r.set_identity(99, Some("x".into()), None); // no panic, no entry
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn add_tx_and_byhost_aggregate_by_user_and_ip() {
        let r = ClientRegistry::new();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
        let peer1 = SocketAddr::new(ip, 40000);
        let peer2 = SocketAddr::new(ip, 40001);

        r.connect(1, peer1);
        r.connect(2, peer2);
        r.set_identity(1, Some("bob".into()), None);
        r.set_identity(2, Some("bob".into()), None);

        r.add_tx(1, 100);
        r.add_tx(2, 50);

        let snap = r.snapshot();
        let e1 = snap.iter().find(|e| e.peer == peer1).unwrap();
        assert_eq!(e1.tx, 100);

        let by_host = r.byhost(true);
        assert_eq!(by_host.len(), 1);
        let (account, client_ip, bytes) = &by_host[0];
        assert_eq!(account, "bob");
        assert_eq!(client_ip, &ip.to_string());
        assert_eq!(*bytes, 150);
    }

    #[test]
    fn bytemap_add_and_snapshot() {
        let m = ByteMap::new();
        m.add("PV:A", 10);
        m.add("PV:A", 5);
        m.add("PV:B", 3);
        let mut s = m.snapshot();
        s.sort();
        assert_eq!(s, vec![("PV:A".to_string(), 15), ("PV:B".to_string(), 3)]);
    }

    #[test]
    fn bandwidth_counters_have_all_six_bytemaps() {
        let c = BandwidthCounters::new();
        c.ds_bypv_tx.add("P", 1);
        c.us_byhost_rx.add("H", 2);
        assert_eq!(c.ds_bypv_tx.snapshot(), vec![("P".to_string(), 1)]);
        assert_eq!(c.us_byhost_rx.snapshot(), vec![("H".to_string(), 2)]);
    }
}
