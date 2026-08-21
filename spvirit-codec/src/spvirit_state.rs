//! PVA Connection State Tracker
//!
//! Tracks channel mappings (CID ↔ SID ↔ PV name) and operation states
//! to enable full decoding of MONITOR packets.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tracing::debug;

use crate::spvd_decode::StructureDesc;

/// Configuration for the PVA state tracker
#[derive(Debug, Clone)]
pub struct PvaStateConfig {
    /// Maximum number of channels to track (default: 40000)
    pub max_channels: usize,
    /// Time-to-live for channel entries (default: 5 minutes)
    pub channel_ttl: Duration,
    /// Maximum number of operations to track per connection
    pub max_operations: usize,
    /// Maximum update timestamps kept per connection for rate calculation (default: 10000)
    pub max_update_rate: usize,
    /// Ceiling on tracked state in bytes, as reported by [`PvaStateTracker::memory_estimate`].
    ///
    /// When the estimate exceeds this, `cleanup_expired` sheds state in
    /// priority order -- debug affordances first, decoded introspection last
    /// -- so that a long-running passive observer accumulates coverage
    /// without growing without bound. `0` disables the ceiling.
    pub max_memory_bytes: usize,
    /// Hard cap on SEARCH name-cache entries, enforced independently of the
    /// memory ceiling so the caches cannot dominate a small budget.
    pub max_search_cache_entries: usize,
}

impl Default for PvaStateConfig {
    fn default() -> Self {
        Self {
            max_channels: 40_000,
            channel_ttl: Duration::from_secs(5 * 60), // 5 minutes
            max_operations: 10_000,
            max_update_rate: 10_000,
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_search_cache_entries: DEFAULT_MAX_SEARCH_CACHE_ENTRIES,
        }
    }
}

/// 1 GiB. Chosen to be generous enough that a busy facility never sheds
/// introspection in practice, while still bounding a multi-day passive run.
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 1024 * 1024 * 1024;
/// Enough for a large facility's PV set several times over.
pub const DEFAULT_MAX_SEARCH_CACHE_ENTRIES: usize = 200_000;

impl PvaStateConfig {
    pub fn new(max_channels: usize, ttl_secs: u64) -> Self {
        Self {
            max_channels,
            channel_ttl: Duration::from_secs(ttl_secs),
            max_operations: 10_000,
            max_update_rate: 10_000,
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_search_cache_entries: DEFAULT_MAX_SEARCH_CACHE_ENTRIES,
        }
    }

    pub fn with_max_update_rate(mut self, max_update_rate: usize) -> Self {
        self.max_update_rate = max_update_rate;
        self
    }

    /// Set the memory ceiling. `0` disables shedding entirely.
    pub fn with_max_memory_bytes(mut self, max_memory_bytes: usize) -> Self {
        self.max_memory_bytes = max_memory_bytes;
        self
    }

    /// Set the per-connection operation cap.
    pub fn with_max_operations(mut self, max_operations: usize) -> Self {
        self.max_operations = max_operations;
        self
    }
}

/// Heap bytes held by a `VecDeque<String>` ring, contents included.
fn ring_bytes(ring: &VecDeque<String>) -> usize {
    ring.capacity() * std::mem::size_of::<String>()
        + ring.iter().map(String::capacity).sum::<usize>()
}

/// Heap bytes held by a `VecDeque<Instant>`.
fn instants_bytes(times: &VecDeque<Instant>) -> usize {
    times.capacity() * std::mem::size_of::<Instant>()
}

/// Approximate bytes a hashbrown table occupies for `n` slots of `(K, V)`.
/// One control byte per slot on top of the key/value pair.
fn table_bytes<K, V>(capacity: usize) -> usize {
    capacity * (std::mem::size_of::<K>() + std::mem::size_of::<V>() + 1)
}

/// Unique key for a TCP connection (canonical - order independent)
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ConnectionKey {
    /// Lower address (lexicographically sorted for consistency)
    pub addr_a: SocketAddr,
    /// Higher address
    pub addr_b: SocketAddr,
}

impl ConnectionKey {
    /// Create a canonical connection key (order independent)
    pub fn new(addr1: SocketAddr, addr2: SocketAddr) -> Self {
        // Always store in sorted order for consistent hashing
        if addr1 <= addr2 {
            Self {
                addr_a: addr1,
                addr_b: addr2,
            }
        } else {
            Self {
                addr_a: addr2,
                addr_b: addr1,
            }
        }
    }

    /// Create from IP strings and ports (convenience method)
    /// Order of arguments doesn't matter - will be canonicalized
    pub fn from_parts(ip1: &str, port1: u16, ip2: &str, port2: u16) -> Option<Self> {
        let addr1: SocketAddr = format!("{}:{}", ip1, port1).parse().ok()?;
        let addr2: SocketAddr = format!("{}:{}", ip2, port2).parse().ok()?;
        Some(Self::new(addr1, addr2))
    }
}

/// Information about a channel (PV)
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    /// PV name
    pub pv_name: String,
    /// Client Channel ID
    pub cid: u32,
    /// Server Channel ID (assigned by server in CREATE_CHANNEL response)
    pub sid: Option<u32>,
    /// When this channel was created/last accessed
    pub last_seen: Instant,
    /// Whether we saw the full CREATE_CHANNEL exchange
    pub fully_established: bool,
    pub update_times: VecDeque<Instant>,
    pub recent_messages: VecDeque<String>,
}

impl ChannelInfo {
    pub fn new_pending(cid: u32, pv_name: String) -> Self {
        Self {
            pv_name,
            cid,
            sid: None,
            last_seen: Instant::now(),
            fully_established: false,
            update_times: VecDeque::new(),
            recent_messages: VecDeque::new(),
        }
    }

    pub fn touch(&mut self) {
        self.last_seen = Instant::now();
    }

    pub fn is_expired(&self, ttl: Duration) -> bool {
        self.last_seen.elapsed() > ttl
    }

    /// Bytes this channel owns on the heap, excluding the struct itself
    /// (its container accounts for that).
    pub fn heap_bytes(&self) -> usize {
        self.pv_name.capacity()
            + instants_bytes(&self.update_times)
            + ring_bytes(&self.recent_messages)
    }
}

/// State for an active operation (GET/PUT/MONITOR etc.)
#[derive(Debug, Clone)]
pub struct OperationState {
    /// Server channel ID this operation is on
    pub sid: u32,
    /// Operation ID
    pub ioid: u32,
    /// Command type (10=GET, 11=PUT, 13=MONITOR, etc.)
    pub command: u8,
    /// PV name (resolved from channel state)
    pub pv_name: Option<String>,
    /// Field description from INIT response (parsed introspection)
    pub field_desc: Option<StructureDesc>,
    /// Heap bytes held by `field_desc`, cached at assignment.
    ///
    /// The introspection tree is the one large term in the memory estimate
    /// whose measurement is recursive, so it is walked once when the INIT
    /// response lands rather than on every accounting pass.
    field_desc_bytes: usize,
    /// Whether INIT phase completed
    pub initialized: bool,
    /// Last activity
    pub last_seen: Instant,
    pub update_times: VecDeque<Instant>,
    pub recent_messages: VecDeque<String>,
}

impl OperationState {
    pub fn new(sid: u32, ioid: u32, command: u8, pv_name: Option<String>) -> Self {
        Self {
            sid,
            ioid,
            command,
            pv_name,
            field_desc: None,
            field_desc_bytes: 0,
            initialized: false,
            last_seen: Instant::now(),
            update_times: VecDeque::new(),
            recent_messages: VecDeque::new(),
        }
    }

    pub fn touch(&mut self) {
        self.last_seen = Instant::now();
    }

    pub fn is_expired(&self, ttl: Duration) -> bool {
        self.last_seen.elapsed() > ttl
    }

    /// Attach introspection and cache its measured size.
    pub fn set_field_desc(&mut self, field_desc: Option<StructureDesc>) {
        self.field_desc_bytes = field_desc.as_ref().map_or(0, |d| d.heap_size());
        self.field_desc = field_desc;
    }

    /// Heap bytes held by this operation's introspection, if any.
    pub fn field_desc_bytes(&self) -> usize {
        self.field_desc_bytes
    }

    /// True when this operation was auto-created from mid-stream traffic and
    /// carries nothing the decoder can use: no completed INIT, no
    /// introspection. Shed first under memory pressure.
    pub fn is_placeholder(&self) -> bool {
        !self.initialized && self.field_desc.is_none()
    }

    /// Bytes this operation owns on the heap, excluding the struct itself.
    pub fn heap_bytes(&self) -> usize {
        self.pv_name.as_ref().map_or(0, |s| s.capacity())
            + self.field_desc_bytes
            + instants_bytes(&self.update_times)
            + ring_bytes(&self.recent_messages)
    }
}

/// Per-connection state
#[derive(Debug)]
pub struct ConnectionState {
    /// Channels indexed by Client ID
    pub channels_by_cid: HashMap<u32, ChannelInfo>,
    /// Server ID → Client ID mapping
    pub sid_to_cid: HashMap<u32, u32>,
    /// Operations indexed by IOID
    pub operations: HashMap<u32, OperationState>,
    /// Byte order for this connection (true = big endian)
    pub is_be: bool,
    /// Last activity on this connection
    pub last_seen: Instant,
    pub update_times: VecDeque<Instant>,
    pub recent_messages: VecDeque<String>,
}

impl ConnectionState {
    pub fn new() -> Self {
        Self {
            channels_by_cid: HashMap::new(),
            sid_to_cid: HashMap::new(),
            operations: HashMap::new(),
            is_be: false, // Default to little endian
            last_seen: Instant::now(),
            update_times: VecDeque::new(),
            recent_messages: VecDeque::new(),
        }
    }

    pub fn touch(&mut self) {
        self.last_seen = Instant::now();
    }

    /// Get channel info by Server ID
    pub fn get_channel_by_sid(&self, sid: u32) -> Option<&ChannelInfo> {
        self.sid_to_cid
            .get(&sid)
            .and_then(|cid| self.channels_by_cid.get(cid))
    }

    /// Get mutable channel info by Server ID
    pub fn get_channel_by_sid_mut(&mut self, sid: u32) -> Option<&mut ChannelInfo> {
        if let Some(&cid) = self.sid_to_cid.get(&sid) {
            self.channels_by_cid.get_mut(&cid)
        } else {
            None
        }
    }

    /// Get PV name for a Server ID
    pub fn get_pv_name_by_sid(&self, sid: u32) -> Option<&str> {
        self.get_channel_by_sid(sid).map(|ch| ch.pv_name.as_str())
    }

    /// Get PV name for an operation IOID
    pub fn get_pv_name_by_ioid(&self, ioid: u32) -> Option<&str> {
        self.operations
            .get(&ioid)
            .and_then(|op| op.pv_name.as_deref())
    }

    /// Bytes this connection owns on the heap, excluding the struct itself:
    /// its three tables, everything inside them, and its own rings.
    pub fn heap_bytes(&self) -> usize {
        table_bytes::<u32, ChannelInfo>(self.channels_by_cid.capacity())
            + table_bytes::<u32, OperationState>(self.operations.capacity())
            + table_bytes::<u32, u32>(self.sid_to_cid.capacity())
            + self
                .channels_by_cid
                .values()
                .map(ChannelInfo::heap_bytes)
                .sum::<usize>()
            + self
                .operations
                .values()
                .map(OperationState::heap_bytes)
                .sum::<usize>()
            + instants_bytes(&self.update_times)
            + ring_bytes(&self.recent_messages)
    }
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::new()
    }
}

/// Global PVA state tracker across all connections
#[derive(Debug)]
pub struct PvaStateTracker {
    /// Configuration
    config: PvaStateConfig,
    /// Per-connection state
    connections: HashMap<ConnectionKey, ConnectionState>,
    /// Total channel count across all connections (for limit enforcement)
    total_channels: usize,
    /// Statistics
    pub stats: PvaStateStats,
    /// (client_ip, CID) → PV name cache from SEARCH messages
    /// Scoped by client IP to prevent CID collisions across different clients
    search_cache: HashMap<(IpAddr, u32), String>,
    /// Flat CID → PV name fallback (last-writer-wins, used when client IP is unknown)
    search_cache_flat: HashMap<u32, String>,
}

/// Statistics for monitoring
#[derive(Debug, Default, Clone)]
pub struct PvaStateStats {
    pub channels_created: u64,
    pub channels_destroyed: u64,
    pub channels_expired: u64,
    pub channels_evicted: u64,
    pub operations_created: u64,
    pub operations_completed: u64,
    pub create_channel_requests: u64,
    pub create_channel_responses: u64,
    pub search_responses_resolved: u64,
    pub search_cache_entries: u64,
    pub search_retroactive_resolves: u64,
    /// PVA messages with is_server=false (sent by client)
    pub client_messages: u64,
    /// PVA messages with is_server=true (sent by server)
    pub server_messages: u64,
    /// Most recent value of [`PvaStateTracker::memory_estimate`], refreshed
    /// by `cleanup_expired` so exporters need not re-walk the state.
    pub memory_bytes: u64,
    /// Operations aged out on their own `last_seen`, independently of any
    /// channel. This is what reclaims mid-stream placeholders.
    pub operations_expired: u64,
    /// Placeholder operations discarded to stay inside the memory ceiling.
    pub shed_placeholder_ops: u64,
    /// Debug message rings cleared to stay inside the memory ceiling.
    pub shed_message_rings: u64,
    /// SEARCH cache entries discarded, by cap or by memory ceiling.
    pub shed_search_entries: u64,
    /// Channels evicted specifically to stay inside the memory ceiling.
    /// These carried introspection, so this rising means coverage is being lost.
    pub shed_channels: u64,
}

#[derive(Debug, Clone)]
pub struct ConnectionSnapshot {
    pub addr_a: SocketAddr,
    pub addr_b: SocketAddr,
    pub channel_count: usize,
    pub operation_count: usize,
    pub last_seen: Duration,
    pub pv_names: Vec<String>,
    pub updates_per_sec: f64,
    pub recent_messages: Vec<String>,
    pub mid_stream: bool,
    pub is_beacon: bool,
    pub is_broadcast: bool,
}

#[derive(Debug, Clone)]
pub struct ChannelSnapshot {
    pub addr_a: SocketAddr,
    pub addr_b: SocketAddr,
    pub cid: u32,
    pub sid: Option<u32>,
    pub pv_name: String,
    pub last_seen: Duration,
    pub updates_per_sec: f64,
    pub recent_messages: Vec<String>,
    pub mid_stream: bool,
    pub is_beacon: bool,
    pub is_broadcast: bool,
}

impl PvaStateTracker {
    fn is_broadcast_addr(addr: &SocketAddr) -> bool {
        match addr.ip() {
            std::net::IpAddr::V4(v4) => {
                if v4.is_broadcast() {
                    return true;
                }
                v4.octets()[3] == 255
            }
            std::net::IpAddr::V6(v6) => {
                // IPv6 has no broadcast; treat multicast as equivalent for PVA
                v6.is_multicast()
            }
        }
    }
    pub fn new(config: PvaStateConfig) -> Self {
        Self {
            config,
            connections: HashMap::new(),
            total_channels: 0,
            stats: PvaStateStats::default(),
            search_cache: HashMap::new(),
            search_cache_flat: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(PvaStateConfig::default())
    }

    /// Get or create connection state
    fn get_or_create_connection(&mut self, key: &ConnectionKey) -> &mut ConnectionState {
        if !self.connections.contains_key(key) {
            self.connections.insert(key.clone(), ConnectionState::new());
        }
        self.connections.get_mut(key).unwrap()
    }

    /// Get connection state (read-only)
    pub fn get_connection(&self, key: &ConnectionKey) -> Option<&ConnectionState> {
        self.connections.get(key)
    }

    /// Get PV name by SID for a connection
    pub fn get_pv_name_by_sid(&self, conn_key: &ConnectionKey, sid: u32) -> Option<String> {
        self.connections
            .get(conn_key)
            .and_then(|conn| conn.get_pv_name_by_sid(sid))
            .map(|s| s.to_string())
    }

    /// Handle CREATE_CHANNEL request (client → server)
    /// Called when we see cmd=7 from client with CID and PV name
    pub fn on_create_channel_request(
        &mut self,
        conn_key: &ConnectionKey,
        cid: u32,
        pv_name: String,
    ) {
        self.stats.create_channel_requests += 1;

        // Also cache in search_cache so it's available as fallback
        // Extract client IP from connection key (client is the one sending the request)
        let client_ip = conn_key.addr_a.ip(); // either side works as flat fallback
        self.search_cache.insert((client_ip, cid), pv_name.clone());
        self.search_cache_flat.insert(cid, pv_name.clone());

        // Check channel limit
        if self.total_channels >= self.config.max_channels {
            self.evict_oldest_channels(100); // Evict 100 oldest
        }

        let conn = self.get_or_create_connection(conn_key);
        conn.touch();

        // Only add if not already present
        if !conn.channels_by_cid.contains_key(&cid) {
            conn.channels_by_cid
                .insert(cid, ChannelInfo::new_pending(cid, pv_name));
            self.total_channels += 1;
            self.stats.channels_created += 1;
            debug!("CREATE_CHANNEL request: cid={}", cid);
        }
    }

    /// Handle CREATE_CHANNEL response (server → client)
    /// Called when we see cmd=7 from server with CID and SID
    pub fn on_create_channel_response(&mut self, conn_key: &ConnectionKey, cid: u32, sid: u32) {
        self.stats.create_channel_responses += 1;

        // Look up search cache BEFORE borrowing self mutably via get_or_create_connection
        // Try scoped cache first (both sides of the connection key), then flat fallback
        let cached_pv_name = self
            .search_cache
            .get(&(conn_key.addr_a.ip(), cid))
            .or_else(|| self.search_cache.get(&(conn_key.addr_b.ip(), cid)))
            .or_else(|| self.search_cache_flat.get(&cid))
            .cloned();

        let conn = self.get_or_create_connection(conn_key);
        conn.touch();

        if let Some(channel) = conn.channels_by_cid.get_mut(&cid) {
            channel.sid = Some(sid);
            channel.fully_established = true;
            channel.touch();
            conn.sid_to_cid.insert(sid, cid);
            debug!(
                "CREATE_CHANNEL response: cid={}, sid={}, pv={}",
                cid, sid, channel.pv_name
            );
        } else {
            // We missed the request - try search cache first, then create placeholder
            let pv_name = cached_pv_name.unwrap_or_else(|| format!("<unknown:cid={}>", cid));
            let is_resolved = !pv_name.starts_with("<unknown");
            debug!(
                "CREATE_CHANNEL response without request: cid={}, sid={}, resolved={}",
                cid, sid, is_resolved
            );
            let mut channel = ChannelInfo::new_pending(cid, pv_name);
            channel.sid = Some(sid);
            channel.fully_established = is_resolved;
            conn.channels_by_cid.insert(cid, channel);
            conn.sid_to_cid.insert(sid, cid);
            self.total_channels += 1;
        }
    }

    /// Handle DESTROY_CHANNEL (cmd=8)
    pub fn on_destroy_channel(&mut self, conn_key: &ConnectionKey, cid: u32, sid: u32) {
        if let Some(conn) = self.connections.get_mut(conn_key) {
            conn.touch();

            // Remove by CID
            if conn.channels_by_cid.remove(&cid).is_some() {
                self.total_channels = self.total_channels.saturating_sub(1);
                self.stats.channels_destroyed += 1;
            }

            // Remove SID mapping
            conn.sid_to_cid.remove(&sid);

            // Remove any operations on this channel
            conn.operations.retain(|_, op| op.sid != sid);

            debug!("DESTROY_CHANNEL: cid={}, sid={}", cid, sid);
        }
    }

    /// Handle operation INIT request (client → server)
    /// subcmd & 0x08 indicates INIT
    pub fn on_op_init_request(
        &mut self,
        conn_key: &ConnectionKey,
        sid: u32,
        ioid: u32,
        command: u8,
    ) {
        let max_ops = self.config.max_operations;
        let conn = self.get_or_create_connection(conn_key);
        conn.touch();

        let pv_name = conn.get_pv_name_by_sid(sid).map(|s| s.to_string());

        if conn.operations.len() < max_ops {
            conn.operations
                .insert(ioid, OperationState::new(sid, ioid, command, pv_name));
            self.stats.operations_created += 1;
            debug!(
                "Operation INIT: sid={}, ioid={}, cmd={}",
                sid, ioid, command
            );
        }
    }

    /// Handle operation INIT response (server → client)
    /// Contains type introspection data
    pub fn on_op_init_response(
        &mut self,
        conn_key: &ConnectionKey,
        ioid: u32,
        field_desc: Option<StructureDesc>,
    ) {
        if let Some(conn) = self.connections.get_mut(conn_key) {
            conn.touch();

            if let Some(op) = conn.operations.get_mut(&ioid) {
                op.set_field_desc(field_desc);
                op.initialized = true;
                op.touch();
                debug!("Operation INIT response: ioid={}", ioid);
            }
        }
    }

    /// Handle operation DESTROY (subcmd & 0x10)
    pub fn on_op_destroy(&mut self, conn_key: &ConnectionKey, ioid: u32) {
        if let Some(conn) = self.connections.get_mut(conn_key) {
            if conn.operations.remove(&ioid).is_some() {
                self.stats.operations_completed += 1;
            }
        }
    }

    /// Touch connection, operation, and channel activity for any op message (data updates, etc.)
    /// If the IOID is unknown (mid-stream join), auto-creates a placeholder operation
    /// so the connection appears on the Connections page.
    pub fn on_op_activity(&mut self, conn_key: &ConnectionKey, sid: u32, ioid: u32, command: u8) {
        let max_update_rate = self.config.max_update_rate;
        let max_ops = self.config.max_operations;
        let mut created_placeholder = false;

        let conn = self.get_or_create_connection(conn_key);
        conn.touch();

        Self::record_update(&mut conn.update_times, max_update_rate);

        let mut channel_sid = if sid != 0 { Some(sid) } else { None };
        if let Some(op) = conn.operations.get_mut(&ioid) {
            op.touch();
            Self::record_update(&mut op.update_times, max_update_rate);
            if channel_sid.is_none() {
                channel_sid = Some(op.sid);
            }
        } else if conn.operations.len() < max_ops {
            // Mid-stream: we missed the INIT exchange, create a placeholder operation
            // so this connection/channel is visible on the Connections page.
            let pv_name = if sid != 0 {
                conn.get_pv_name_by_sid(sid).map(|s| s.to_string())
            } else if conn.channels_by_cid.len() == 1 && conn.operations.is_empty() {
                // Server Op messages have sid=0; only use single-channel fallback
                // when this is the very first operation (no other ops yet).
                // If there are already other operations, this is likely a
                // multiplexed connection and the fallback would be wrong.
                conn.channels_by_cid
                    .values()
                    .next()
                    .map(|ch| ch.pv_name.clone())
                    .filter(|n| !n.starts_with("<unknown"))
            } else {
                None
            };
            conn.operations
                .insert(ioid, OperationState::new(sid, ioid, command, pv_name));
            created_placeholder = true;
        }

        if let Some(sid_val) = channel_sid {
            if let Some(channel) = conn.get_channel_by_sid_mut(sid_val) {
                channel.touch();
                Self::record_update(&mut channel.update_times, max_update_rate);
            }
        }

        // Deferred stat update — can't touch self.stats while conn borrows self
        if created_placeholder {
            self.stats.operations_created += 1;
            debug!(
                "Auto-created placeholder operation for mid-stream traffic: sid={}, ioid={}, cmd={}",
                sid, ioid, command
            );
        }
    }

    /// Cache PV name mappings from SEARCH messages (CID → PV name)
    /// These serve as fallback when the client's CREATE_CHANNEL request is missed.
    /// Also retroactively resolves any existing `<unknown:cid=N>` channels and
    /// placeholder operations that match the CIDs in this SEARCH.
    /// `source_ip` is the IP of the client that sent the SEARCH request.
    pub fn on_search(&mut self, pv_requests: &[(u32, String)], source_ip: Option<IpAddr>) {
        // Build a lookup map for this batch
        let cid_to_pv: HashMap<u32, String> = pv_requests.iter().cloned().collect();

        for (cid, pv_name) in pv_requests {
            if let Some(ip) = source_ip {
                self.search_cache.insert((ip, *cid), pv_name.clone());
            }
            // Always populate flat fallback
            self.search_cache_flat.insert(*cid, pv_name.clone());
        }

        // Retroactively resolve existing unknown channels and operations.
        // Walk all connections and fix any <unknown:cid=N> entries whose CID
        // matches a CID from this SEARCH request.
        let mut retroactive_count: u64 = 0;
        for conn in self.connections.values_mut() {
            for (cid, channel) in conn.channels_by_cid.iter_mut() {
                if channel.pv_name.starts_with("<unknown") {
                    if let Some(pv_name) = cid_to_pv.get(cid) {
                        debug!(
                            "Retroactive PV resolve from SEARCH: cid={} {} -> {}",
                            cid, channel.pv_name, pv_name
                        );
                        channel.pv_name = pv_name.clone();
                        channel.fully_established = true;
                        retroactive_count += 1;
                    }
                }
            }

            // Also update placeholder operations that have pv_name=None
            // or stale <unknown...> names, and whose SID maps to a resolved channel
            for op in conn.operations.values_mut() {
                let needs_update = match &op.pv_name {
                    None => true,
                    Some(name) => name.starts_with("<unknown"),
                };
                if needs_update && op.sid != 0 {
                    if let Some(&cid) = conn.sid_to_cid.get(&op.sid) {
                        if let Some(pv_name) = cid_to_pv.get(&cid) {
                            op.pv_name = Some(pv_name.clone());
                        }
                    }
                }
            }
        }
        if retroactive_count > 0 {
            self.stats.search_retroactive_resolves += retroactive_count;
            debug!(
                "Retroactively resolved {} unknown channels from SEARCH cache",
                retroactive_count
            );
        }

        // Update search cache size stat
        self.stats.search_cache_entries = self.search_cache_flat.len() as u64;

        // Cap cache sizes to prevent unbounded growth
        while self.search_cache.len() > 50_000 {
            if let Some(key) = self.search_cache.keys().next().cloned() {
                self.search_cache.remove(&key);
            }
        }
        while self.search_cache_flat.len() > 50_000 {
            if let Some(key) = self.search_cache_flat.keys().next().cloned() {
                self.search_cache_flat.remove(&key);
            }
        }
    }

    /// Resolve PV names from SEARCH_RESPONSE CIDs using the search cache.
    /// Returns a list of (CID, resolved_pv_name) pairs for all CIDs that could be resolved.
    /// `source_ip` is optionally the IP of the server that sent the response;
    /// we try scoped lookups using peer IPs, then fall back to flat cache.
    pub fn resolve_search_cids(
        &mut self,
        cids: &[u32],
        peer_ip: Option<IpAddr>,
    ) -> Vec<(u32, String)> {
        let mut resolved = Vec::new();
        for &cid in cids {
            // Try scoped cache with peer IP (the client that originally searched),
            // then fall back to flat cache
            let pv_name = peer_ip
                .and_then(|ip| self.search_cache.get(&(ip, cid)))
                .or_else(|| self.search_cache_flat.get(&cid))
                .cloned();
            if let Some(name) = pv_name {
                resolved.push((cid, name));
                self.stats.search_responses_resolved += 1;
            }
        }
        resolved
    }

    /// Count a PVA message direction (for messages not routed through on_message)
    pub fn count_direction(&mut self, is_server: bool) {
        if is_server {
            self.stats.server_messages += 1;
        } else {
            self.stats.client_messages += 1;
        }
    }

    pub fn on_message(
        &mut self,
        conn_key: &ConnectionKey,
        sid: u32,
        ioid: u32,
        request_type: &str,
        message: String,
        is_server: bool,
    ) {
        let conn = self.get_or_create_connection(conn_key);
        conn.touch();
        let dir = if is_server { "S>" } else { "C>" };
        let full_message = format!("{} {} {}", dir, request_type, message);
        Self::push_message(&mut conn.recent_messages, full_message.clone());

        let mut channel_sid = if sid != 0 { Some(sid) } else { None };
        if let Some(op) = conn.operations.get_mut(&ioid) {
            Self::push_message(&mut op.recent_messages, full_message.clone());
            if channel_sid.is_none() {
                channel_sid = Some(op.sid);
            }
        }
        if let Some(sid_val) = channel_sid {
            if let Some(channel) = conn.get_channel_by_sid_mut(sid_val) {
                Self::push_message(&mut channel.recent_messages, full_message);
            }
        }
    }

    fn record_update(times: &mut VecDeque<Instant>, max_update_rate: usize) {
        let now = Instant::now();
        times.push_back(now);
        Self::trim_times(times, now);
        while times.len() > max_update_rate {
            times.pop_front();
        }
    }

    fn trim_times(times: &mut VecDeque<Instant>, now: Instant) {
        while let Some(front) = times.front() {
            if now.duration_since(*front) > Duration::from_secs(1) {
                times.pop_front();
            } else {
                break;
            }
        }
    }

    fn updates_per_sec(times: &VecDeque<Instant>) -> f64 {
        times.len() as f64
    }

    fn push_message(messages: &mut VecDeque<String>, message: String) {
        messages.push_back(message);
        while messages.len() > 30 {
            messages.pop_front();
        }
    }

    /// Resolve PV name for a MONITOR/GET/PUT packet
    pub fn resolve_pv_name(&self, conn_key: &ConnectionKey, sid: u32, ioid: u32) -> Option<String> {
        let conn = self.connections.get(conn_key)?;

        // First try by IOID (operation state) - works for server responses
        if let Some(op) = conn.operations.get(&ioid) {
            if let Some(ref name) = op.pv_name {
                if !name.starts_with("<unknown") {
                    return Some(name.clone());
                }
            }
        }

        // Fall back to SID lookup - works for client requests
        if sid != 0 {
            if let Some(name) = conn.get_pv_name_by_sid(sid) {
                return Some(name.to_string());
            }
        }

        // Last resort: if there's exactly one channel AND at most one operation,
        // use that channel's PV name. This handles simple single-PV connections
        // where the server Op message has sid_or_cid=0.
        //
        // IMPORTANT: Do NOT use this fallback when there are multiple operations,
        // because PVA multiplexes many channels over one TCP connection (e.g.
        // Phoebus). If we only captured one CREATE_CHANNEL but there are many
        // ops, the other ops likely belong to different PVs that were established
        // before our capture started.
        if conn.channels_by_cid.len() == 1 && conn.operations.len() <= 1 {
            if let Some(ch) = conn.channels_by_cid.values().next() {
                if !ch.pv_name.starts_with("<unknown") {
                    return Some(ch.pv_name.clone());
                }
            }
        }

        None
    }

    /// Get the number of active tracked channels
    pub fn active_channel_count(&self) -> usize {
        self.total_channels
    }

    /// Get the number of active tracked connections
    pub fn active_connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Check if a connection is mid-stream (incomplete channel state)
    pub fn is_connection_mid_stream(&self, conn_key: &ConnectionKey) -> bool {
        self.connections
            .get(conn_key)
            .map(|conn| {
                // Operations exist but no channels tracked → definitely mid-stream
                if conn.channels_by_cid.is_empty() && !conn.operations.is_empty() {
                    return true;
                }
                // Any channel not fully established → mid-stream
                conn.channels_by_cid
                    .values()
                    .any(|ch| !ch.fully_established)
            })
            .unwrap_or(false)
    }

    /// Get operation state for decoding values
    pub fn get_operation(&self, conn_key: &ConnectionKey, ioid: u32) -> Option<&OperationState> {
        self.connections
            .get(conn_key)
            .and_then(|conn| conn.operations.get(&ioid))
    }

    /// Evict oldest channels when at capacity
    /// Evict the `count` least-recently-seen channels. Returns how many went.
    ///
    /// Removing a channel must also remove the operations riding on it. When
    /// it did not, those operations were orphaned permanently -- nothing else
    /// reclaims them, and a connection is only dropped once it holds neither
    /// channels nor operations, so every leaked operation also pinned its
    /// connection for the life of the process.
    fn evict_oldest_channels(&mut self, count: usize) -> usize {
        let mut oldest: Vec<(ConnectionKey, u32, Instant)> = Vec::new();

        for (conn_key, conn) in &self.connections {
            for (cid, channel) in &conn.channels_by_cid {
                oldest.push((conn_key.clone(), *cid, channel.last_seen));
            }
        }

        // Sort by last_seen (oldest first)
        oldest.sort_by_key(|(_, _, t)| *t);

        // Remove oldest
        let mut evicted = 0;
        for (conn_key, cid, _) in oldest.into_iter().take(count) {
            if let Some(conn) = self.connections.get_mut(&conn_key) {
                if let Some(channel) = conn.channels_by_cid.remove(&cid) {
                    if let Some(sid) = channel.sid {
                        conn.sid_to_cid.remove(&sid);
                        conn.operations.retain(|_, op| op.sid != sid);
                    }
                    self.total_channels = self.total_channels.saturating_sub(1);
                    self.stats.channels_evicted += 1;
                    evicted += 1;
                }
            }
        }
        evicted
    }

    /// Periodic cleanup of expired entries
    pub fn cleanup_expired(&mut self) {
        let ttl = self.config.channel_ttl;
        let mut expired_count = 0;

        for conn in self.connections.values_mut() {
            let expired_cids: Vec<u32> = conn
                .channels_by_cid
                .iter()
                .filter(|(_, ch)| ch.is_expired(ttl))
                .map(|(cid, _)| *cid)
                .collect();

            for cid in expired_cids {
                if let Some(channel) = conn.channels_by_cid.remove(&cid) {
                    if let Some(sid) = channel.sid {
                        conn.sid_to_cid.remove(&sid);
                        conn.operations.retain(|_, op| op.sid != sid);
                    }
                    expired_count += 1;
                }
            }
        }

        // Age operations out on their own `last_seen`, independently of any
        // channel. Channel-driven cleanup alone never reclaims a mid-stream
        // placeholder: it is created with sid=0, so it matches no expiring
        // channel's sid, and because a connection is retained while it holds
        // any operation, each such placeholder pinned its connection for the
        // life of the process. An operation carrying live traffic is touched
        // on every update, so it is never caught here.
        let mut expired_ops: u64 = 0;
        for conn in self.connections.values_mut() {
            let before = conn.operations.len();
            conn.operations.retain(|_, op| !op.is_expired(ttl));
            expired_ops += (before - conn.operations.len()) as u64;
        }
        if expired_ops > 0 {
            self.stats.operations_expired += expired_ops;
            debug!("Cleaned up {} expired operations", expired_ops);
        }

        if expired_count > 0 {
            self.total_channels = self.total_channels.saturating_sub(expired_count);
            self.stats.channels_expired += expired_count as u64;
            debug!("Cleaned up {} expired channels", expired_count);
        }

        // Remove empty connections
        self.connections
            .retain(|_, conn| !conn.channels_by_cid.is_empty() || !conn.operations.is_empty());

        self.enforce_search_cache_cap();
        self.shed_to_budget();
        self.stats.memory_bytes = self.memory_estimate() as u64;
    }

    /// Approximate bytes of live tracked state.
    ///
    /// Every term is measured on each call except introspection, which is
    /// walked once when the INIT response lands and cached on the operation
    /// -- it is the only recursive term, and the only one worth caching. A
    /// full pass at the channel ceiling costs single-digit milliseconds, and
    /// it runs once per second from `cleanup_expired`.
    pub fn memory_estimate(&self) -> usize {
        let conns = table_bytes::<ConnectionKey, ConnectionState>(self.connections.capacity())
            + self
                .connections
                .values()
                .map(ConnectionState::heap_bytes)
                .sum::<usize>();
        let search = table_bytes::<(IpAddr, u32), String>(self.search_cache.capacity())
            + self
                .search_cache
                .values()
                .map(String::capacity)
                .sum::<usize>()
            + table_bytes::<u32, String>(self.search_cache_flat.capacity())
            + self
                .search_cache_flat
                .values()
                .map(String::capacity)
                .sum::<usize>();
        conns + search
    }

    /// Bring tracked state back inside `max_memory_bytes`, discarding in
    /// ascending order of how much the decoder needs it.
    ///
    /// The ordering is the whole point. `field_desc` is expensive to acquire:
    /// it arrives only on a MONITOR INIT response, which a passive observer
    /// sees only when it happens to witness a channel being created. Debug
    /// affordances refill in seconds; introspection may not return for days.
    /// So message rings go before caches, and caches before channels.
    fn shed_to_budget(&mut self) {
        let budget = self.config.max_memory_bytes;
        if budget == 0 {
            return;
        }
        let mut est = self.memory_estimate();
        if est <= budget {
            return;
        }

        // Tier 1: mid-stream placeholders -- no INIT seen, no introspection.
        if self.shed_placeholder_ops(est - budget) > 0 {
            self.shrink_containers();
            est = self.memory_estimate();
            if est <= budget {
                return;
            }
        }

        // Tier 2: debug message rings. Pure UI affordance; dropping them
        // leaves every field_desc intact.
        if self.shed_message_rings(est - budget) > 0 {
            est = self.memory_estimate();
            if est <= budget {
                return;
            }
        }

        // Tier 3: SEARCH name caches. Future SEARCH traffic repopulates them.
        if self.shed_search_caches() > 0 {
            est = self.memory_estimate();
            if est <= budget {
                return;
            }
        }

        // Tier 4: oldest channels, introspection and all. Reaching here means
        // the budget cannot hold the live channel set, so coverage is being
        // lost -- `shed_channels` is the metric that says so.
        let mut guard = 0;
        while est > budget && self.total_channels > 0 && guard < 16 {
            let per_channel = (est / self.total_channels.max(1)).max(1);
            let batch = (((est - budget) / per_channel) + 1).clamp(100, self.total_channels);
            let evicted = self.evict_oldest_channels(batch);
            if evicted == 0 {
                break;
            }
            self.stats.shed_channels += evicted as u64;
            self.shrink_containers();
            est = self.memory_estimate();
            guard += 1;
        }
    }

    /// Hand back the table capacity that removals left behind.
    ///
    /// A `HashMap` never shrinks on `remove`, so shedding entries releases
    /// their contents but not the slots that held them. Skipping this would
    /// leave both the estimate and the real allocation nearly where they
    /// started -- the tiers would report progress they had not made, and the
    /// loop would keep evicting live channels chasing a figure that could not
    /// come down. Only reached while over budget, so the rehash is not on any
    /// steady-state path.
    fn shrink_containers(&mut self) {
        for conn in self.connections.values_mut() {
            conn.operations.shrink_to_fit();
            conn.channels_by_cid.shrink_to_fit();
            conn.sid_to_cid.shrink_to_fit();
        }
        self.connections.shrink_to_fit();
    }

    /// Discard placeholder operations, least recently seen first, until
    /// `target` bytes are freed. Returns the bytes actually freed.
    fn shed_placeholder_ops(&mut self, target: usize) -> usize {
        let mut cands: Vec<(ConnectionKey, u32, Instant, usize)> = Vec::new();
        for (ck, conn) in &self.connections {
            for (ioid, op) in &conn.operations {
                if op.is_placeholder() {
                    cands.push((
                        ck.clone(),
                        *ioid,
                        op.last_seen,
                        op.heap_bytes() + std::mem::size_of::<OperationState>(),
                    ));
                }
            }
        }
        cands.sort_by_key(|(_, _, t, _)| *t);

        let mut freed = 0;
        for (ck, ioid, _, bytes) in cands {
            if freed >= target {
                break;
            }
            let removed = self
                .connections
                .get_mut(&ck)
                .and_then(|conn| conn.operations.remove(&ioid));
            if removed.is_some() {
                freed += bytes;
                self.stats.shed_placeholder_ops += 1;
            }
        }
        freed
    }

    /// Release `recent_messages` rings, least recently seen first, until
    /// `target` bytes are freed. Returns the bytes actually freed.
    fn shed_message_rings(&mut self, target: usize) -> usize {
        let mut cands: Vec<(ConnectionKey, u32, Instant, usize)> = Vec::new();
        for (ck, conn) in &self.connections {
            for (ioid, op) in &conn.operations {
                let bytes = ring_bytes(&op.recent_messages);
                if bytes > 0 {
                    cands.push((ck.clone(), *ioid, op.last_seen, bytes));
                }
            }
        }
        cands.sort_by_key(|(_, _, t, _)| *t);

        let mut freed = 0;
        for (ck, ioid, _, bytes) in cands {
            if freed >= target {
                break;
            }
            if let Some(op) = self
                .connections
                .get_mut(&ck)
                .and_then(|conn| conn.operations.get_mut(&ioid))
            {
                // Assign a fresh ring rather than `clear()`, which keeps the
                // allocation and would free nothing.
                op.recent_messages = VecDeque::new();
                freed += bytes;
                self.stats.shed_message_rings += 1;
            }
        }
        if freed >= target {
            return freed;
        }

        for conn in self.connections.values_mut() {
            if freed >= target {
                break;
            }
            let bytes = ring_bytes(&conn.recent_messages);
            if bytes > 0 {
                conn.recent_messages = VecDeque::new();
                freed += bytes;
                self.stats.shed_message_rings += 1;
            }
        }
        freed
    }

    /// Drop both SEARCH name caches wholesale. Returns the bytes freed.
    ///
    /// They are unordered best-effort lookups with no natural eviction order,
    /// and losing one costs at most a name that a later SEARCH supplies
    /// again, so a partial eviction would buy nothing over a clean reset.
    fn shed_search_caches(&mut self) -> usize {
        let freed = table_bytes::<(IpAddr, u32), String>(self.search_cache.capacity())
            + self
                .search_cache
                .values()
                .map(String::capacity)
                .sum::<usize>()
            + table_bytes::<u32, String>(self.search_cache_flat.capacity())
            + self
                .search_cache_flat
                .values()
                .map(String::capacity)
                .sum::<usize>();
        let dropped = (self.search_cache.len() + self.search_cache_flat.len()) as u64;
        self.search_cache = HashMap::new();
        self.search_cache_flat = HashMap::new();
        self.stats.shed_search_entries += dropped;
        freed
    }

    /// Keep the SEARCH caches inside their entry cap, independently of the
    /// memory ceiling so they cannot dominate a small budget.
    fn enforce_search_cache_cap(&mut self) {
        let cap = self.config.max_search_cache_entries;
        if cap == 0 {
            return;
        }
        if self.search_cache.len() > cap || self.search_cache_flat.len() > cap {
            self.shed_search_caches();
        }
    }

    /// Get summary statistics
    pub fn summary(&self) -> String {
        format!(
            "PVA State: {} connections, {} channels (created={}, destroyed={}, expired={}, evicted={})",
            self.connections.len(),
            self.total_channels,
            self.stats.channels_created,
            self.stats.channels_destroyed,
            self.stats.channels_expired,
            self.stats.channels_evicted,
        )
    }

    /// Get current channel count
    pub fn channel_count(&self) -> usize {
        self.total_channels
    }

    /// Get current connection count
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    pub fn connection_snapshots(&self) -> Vec<ConnectionSnapshot> {
        let mut snapshots = Vec::new();
        let now = Instant::now();
        for (conn_key, conn) in &self.connections {
            let mut update_times = conn.update_times.clone();
            Self::trim_times(&mut update_times, now);
            let mut pv_names: Vec<String> = conn
                .channels_by_cid
                .values()
                .map(|ch| ch.pv_name.clone())
                .collect();
            pv_names.sort();
            pv_names.truncate(8);
            let mut messages: Vec<String> = conn.recent_messages.iter().cloned().collect();
            if messages.len() > 20 {
                messages = messages.split_off(messages.len() - 20);
            }
            let is_beacon = messages.iter().any(|m| m.starts_with("BEACON "));
            let is_broadcast = Self::is_broadcast_addr(&conn_key.addr_a)
                || Self::is_broadcast_addr(&conn_key.addr_b);
            let mut mid_stream = false;
            if conn.channels_by_cid.is_empty() && !conn.operations.is_empty() {
                mid_stream = true;
            }
            if conn
                .channels_by_cid
                .values()
                .any(|ch| !ch.fully_established || ch.pv_name.starts_with("<unknown"))
            {
                mid_stream = true;
            }

            snapshots.push(ConnectionSnapshot {
                addr_a: conn_key.addr_a,
                addr_b: conn_key.addr_b,
                channel_count: conn.channels_by_cid.len(),
                operation_count: conn.operations.len(),
                last_seen: conn.last_seen.elapsed(),
                pv_names,
                updates_per_sec: Self::updates_per_sec(&update_times),
                recent_messages: messages,
                mid_stream,
                is_beacon,
                is_broadcast,
            });
        }
        snapshots
    }

    pub fn channel_snapshots(&self) -> Vec<ChannelSnapshot> {
        let mut snapshots = Vec::new();
        let now = Instant::now();
        for (conn_key, conn) in &self.connections {
            for channel in conn.channels_by_cid.values() {
                let mut update_times = channel.update_times.clone();
                Self::trim_times(&mut update_times, now);
                let mut messages: Vec<String> = channel.recent_messages.iter().cloned().collect();
                if messages.len() > 20 {
                    messages = messages.split_off(messages.len() - 20);
                }
                let is_beacon = messages.iter().any(|m| m.starts_with("BEACON "));
                let is_broadcast = Self::is_broadcast_addr(&conn_key.addr_a)
                    || Self::is_broadcast_addr(&conn_key.addr_b);
                snapshots.push(ChannelSnapshot {
                    addr_a: conn_key.addr_a,
                    addr_b: conn_key.addr_b,
                    cid: channel.cid,
                    sid: channel.sid,
                    pv_name: channel.pv_name.clone(),
                    last_seen: channel.last_seen.elapsed(),
                    updates_per_sec: Self::updates_per_sec(&update_times),
                    recent_messages: messages,
                    mid_stream: !channel.fully_established
                        || channel.pv_name.starts_with("<unknown"),
                    is_beacon,
                    is_broadcast,
                });
            }

            // Avoid emitting duplicate fallback rows when multiple operations
            // reference the same unresolved SID/PV on one connection.
            let mut seen_virtual = HashSet::new();
            for op in conn.operations.values() {
                if conn.get_channel_by_sid(op.sid).is_none() {
                    let mut update_times = op.update_times.clone();
                    Self::trim_times(&mut update_times, now);
                    let mut messages: Vec<String> = op.recent_messages.iter().cloned().collect();
                    if messages.len() > 20 {
                        messages = messages.split_off(messages.len() - 20);
                    }
                    let is_beacon = messages.iter().any(|m| m.starts_with("BEACON "));
                    let is_broadcast = Self::is_broadcast_addr(&conn_key.addr_a)
                        || Self::is_broadcast_addr(&conn_key.addr_b);
                    let pv_name = op
                        .pv_name
                        .clone()
                        .unwrap_or_else(|| format!("<unknown:sid={}>", op.sid));
                    if !seen_virtual.insert((op.sid, pv_name.clone())) {
                        continue;
                    }
                    snapshots.push(ChannelSnapshot {
                        addr_a: conn_key.addr_a,
                        addr_b: conn_key.addr_b,
                        cid: 0,
                        sid: Some(op.sid),
                        pv_name,
                        last_seen: op.last_seen.elapsed(),
                        updates_per_sec: Self::updates_per_sec(&update_times),
                        recent_messages: messages,
                        mid_stream: true,
                        is_beacon,
                        is_broadcast,
                    });
                }
            }
        }
        snapshots
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FieldDesc, FieldType};

    fn test_conn_key() -> ConnectionKey {
        ConnectionKey::from_parts("192.168.1.1", 12345, "192.168.1.2", 5075).unwrap()
    }

    /// An NTScalar-shaped description with `nested` sub-fields under
    /// `timeStamp`, so the recursive term of `heap_size` is exercised.
    fn desc_with_fields(nested: usize) -> StructureDesc {
        let mut inner = StructureDesc::new();
        for i in 0..nested {
            inner.fields.push(FieldDesc {
                name: format!("nested_field_name_{i}"),
                field_type: FieldType::String,
            });
        }
        let mut outer = StructureDesc::new();
        outer.struct_id = Some("epics:nt/NTScalar:1.0".to_string());
        outer.fields.push(FieldDesc {
            name: "value".to_string(),
            field_type: FieldType::String,
        });
        outer.fields.push(FieldDesc {
            name: "timeStamp".to_string(),
            field_type: FieldType::Structure(inner),
        });
        outer
    }

    #[test]
    fn test_heap_size_walks_nested_fields() {
        let flat = desc_with_fields(0);
        let nested = desc_with_fields(20);

        assert!(
            flat.heap_size() > 0,
            "struct_id and two field names are heap"
        );
        // Each nested field contributes at least its own name; a walk that
        // stopped at the top level would miss all twenty.
        let names_only = 20 * "nested_field_name_0".len();
        assert!(
            nested.heap_size() > flat.heap_size() + names_only,
            "nested={} flat={}",
            nested.heap_size(),
            flat.heap_size()
        );
    }

    #[test]
    fn test_expired_placeholder_operations_are_reclaimed() {
        let mut tracker = PvaStateTracker::new(PvaStateConfig::new(100, 0));
        let key = test_conn_key();

        // Mid-stream server MONITOR update: sid=0, unseen ioid. This mints a
        // placeholder operation and no channel at all, so channel-driven
        // cleanup alone has nothing to hang the reclamation on.
        tracker.on_op_activity(&key, 0, 42, 13);
        assert_eq!(tracker.connection_count(), 1);

        tracker.cleanup_expired();

        assert_eq!(tracker.stats.operations_expired, 1);
        assert_eq!(
            tracker.connection_count(),
            0,
            "connection must drop once its last operation ages out"
        );
    }

    #[test]
    fn test_active_operation_is_not_aged_out() {
        let mut tracker = PvaStateTracker::new(PvaStateConfig::new(100, 3600));
        let key = test_conn_key();

        tracker.on_op_activity(&key, 0, 42, 13);
        tracker.cleanup_expired();

        assert_eq!(tracker.stats.operations_expired, 0);
        assert_eq!(tracker.connection_count(), 1);
    }

    #[test]
    fn test_eviction_removes_operations_riding_the_channel() {
        let mut tracker = PvaStateTracker::new(PvaStateConfig::new(1, 3600));
        let key = test_conn_key();

        tracker.on_create_channel_request(&key, 1, "PV:ONE".to_string());
        tracker.on_create_channel_response(&key, 1, 100);
        tracker.on_op_init_request(&key, 100, 7, 13);
        assert_eq!(tracker.connections[&key].operations.len(), 1);

        // A second channel puts us at the ceiling and evicts the first.
        tracker.on_create_channel_request(&key, 2, "PV:TWO".to_string());

        assert!(tracker.stats.channels_evicted >= 1);
        assert!(
            tracker.connections[&key].operations.is_empty(),
            "an operation whose channel was evicted is unreachable; nothing \
             else reclaims it, and it pins its connection forever"
        );
    }

    #[test]
    fn test_shedding_drops_placeholders_before_introspection() {
        let mut tracker = PvaStateTracker::new(PvaStateConfig::new(10_000, 3600));
        let key = test_conn_key();

        // One fully-witnessed operation carrying introspection...
        tracker.on_create_channel_request(&key, 1, "PV:KEEP".to_string());
        tracker.on_create_channel_response(&key, 1, 100);
        tracker.on_op_init_request(&key, 100, 1, 13);
        tracker.on_op_init_response(&key, 1, Some(desc_with_fields(30)));

        // ...and a crowd of mid-stream placeholders carrying nothing.
        for ioid in 100..400 {
            tracker.on_op_activity(&key, 0, ioid, 13);
        }

        tracker.config.max_memory_bytes = tracker.memory_estimate() / 2;
        tracker.cleanup_expired();

        assert!(tracker.stats.shed_placeholder_ops > 0);
        assert_eq!(
            tracker.stats.shed_channels, 0,
            "no channel should be shed while worthless placeholders remain"
        );
        let op = &tracker.connections[&key].operations[&1];
        assert!(
            op.field_desc.is_some(),
            "introspection is the expensive thing to reacquire; it sheds last"
        );
        assert!(tracker.memory_estimate() <= tracker.config.max_memory_bytes);
    }

    #[test]
    fn test_cleanup_holds_state_under_the_memory_ceiling() {
        let mut tracker = PvaStateTracker::new(PvaStateConfig::new(40_000, 3600));
        let key = test_conn_key();

        for cid in 0..4_000u32 {
            let sid = cid + 1;
            tracker.on_create_channel_request(&key, cid, format!("SR:DI:BPM:{cid:04}:X:MEAN"));
            tracker.on_create_channel_response(&key, cid, sid);
            tracker.on_op_init_request(&key, sid, sid, 13);
            tracker.on_op_init_response(&key, sid, Some(desc_with_fields(30)));
        }

        let unbounded = tracker.memory_estimate();
        let budget = unbounded / 4;
        tracker.config.max_memory_bytes = budget;
        tracker.cleanup_expired();

        assert!(
            tracker.stats.shed_channels > 0,
            "a quarter of the state cannot hold this channel set; coverage \
             has to be given up, and shed_channels is what says so"
        );
        assert!(
            tracker.memory_estimate() <= budget,
            "estimate {} still over budget {}",
            tracker.memory_estimate(),
            budget
        );
        assert_eq!(
            tracker.stats.memory_bytes as usize,
            tracker.memory_estimate()
        );
    }

    #[test]
    fn test_zero_budget_disables_shedding() {
        let mut tracker = PvaStateTracker::new(PvaStateConfig::new(10_000, 3600));
        let key = test_conn_key();

        for ioid in 0..200 {
            tracker.on_op_activity(&key, 0, ioid, 13);
        }
        tracker.config.max_memory_bytes = 0;
        tracker.cleanup_expired();

        assert_eq!(tracker.stats.shed_placeholder_ops, 0);
        assert_eq!(tracker.stats.shed_channels, 0);
        assert_eq!(tracker.connections[&key].operations.len(), 200);
    }

    #[test]
    fn test_search_cache_cap_is_enforced() {
        let mut tracker = PvaStateTracker::new(PvaStateConfig::new(10_000, 3600));
        tracker.config.max_search_cache_entries = 50;

        let ip = Some("192.168.1.1".parse::<IpAddr>().unwrap());
        let reqs: Vec<(u32, String)> = (0..200u32).map(|i| (i, format!("PV:{i}"))).collect();
        tracker.on_search(&reqs, ip);
        assert!(tracker.search_cache.len() > 50);

        tracker.cleanup_expired();

        assert!(tracker.search_cache.is_empty());
        assert!(tracker.search_cache_flat.is_empty());
        assert!(tracker.stats.shed_search_entries >= 200);
    }

    #[test]
    fn test_create_channel_flow() {
        let mut tracker = PvaStateTracker::with_defaults();
        let key = test_conn_key();

        // Client sends CREATE_CHANNEL
        tracker.on_create_channel_request(&key, 1, "TEST:PV:VALUE".to_string());
        assert_eq!(tracker.channel_count(), 1);

        // Server responds
        tracker.on_create_channel_response(&key, 1, 100);

        // Verify we can resolve the PV name
        let pv_name = tracker.resolve_pv_name(&key, 100, 0);
        assert_eq!(pv_name, Some("TEST:PV:VALUE".to_string()));
    }

    #[test]
    fn test_channel_limit() {
        let config = PvaStateConfig::new(100, 300);
        let mut tracker = PvaStateTracker::new(config);
        let key = test_conn_key();

        // Add 150 channels (exceeds limit of 100)
        for i in 0..150 {
            tracker.on_create_channel_request(&key, i, format!("PV:{}", i));
        }

        // Should have evicted some
        assert!(tracker.channel_count() <= 100);
    }

    #[test]
    fn test_destroy_channel() {
        let mut tracker = PvaStateTracker::with_defaults();
        let key = test_conn_key();

        tracker.on_create_channel_request(&key, 1, "TEST:PV".to_string());
        tracker.on_create_channel_response(&key, 1, 100);
        assert_eq!(tracker.channel_count(), 1);

        tracker.on_destroy_channel(&key, 1, 100);
        assert_eq!(tracker.channel_count(), 0);
    }

    #[test]
    fn test_channel_snapshots_dedup_unresolved_sid_rows() {
        let mut tracker = PvaStateTracker::with_defaults();
        let key = test_conn_key();

        // Two operations on same unresolved SID should collapse to one virtual channel row.
        tracker.on_op_init_request(&key, 777, 1001, 13);
        tracker.on_op_init_request(&key, 777, 1002, 13);
        tracker.on_op_activity(&key, 777, 1001, 13);
        tracker.on_op_activity(&key, 777, 1002, 13);

        let snapshots = tracker.channel_snapshots();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].sid, Some(777));
    }

    #[test]
    fn test_single_channel_fallback_works_for_simple_connection() {
        // When there is truly one channel and zero/one operations, the
        // single-channel fallback should resolve the PV name from sid=0.
        let mut tracker = PvaStateTracker::with_defaults();
        let key = test_conn_key();

        tracker.on_create_channel_request(&key, 1, "SIMPLE:PV".to_string());
        tracker.on_create_channel_response(&key, 1, 100);

        // sid=0, ioid=99 — no matching operation
        let pv = tracker.resolve_pv_name(&key, 0, 99);
        assert_eq!(pv, Some("SIMPLE:PV".to_string()));
    }

    #[test]
    fn test_no_false_attribution_on_multiplexed_connection() {
        // Phoebus scenario: one TCP connection carries many channels, but we
        // only captured one CREATE_CHANNEL.  When additional ops arrive with
        // sid=0 (server direction), the single-channel fallback must NOT
        // attribute them to the one known channel.
        let mut tracker = PvaStateTracker::with_defaults();
        let key = test_conn_key();

        // Capture one channel
        tracker.on_create_channel_request(&key, 1, "CAPTURED:PV".to_string());
        tracker.on_create_channel_response(&key, 1, 100);

        // Simulate many ops arriving (as happens with multiplexed connections).
        // First op via on_op_init_request with sid known:
        tracker.on_op_init_request(&key, 100, 1, 13); // MONITOR for the known channel

        // Additional ops with different SIDs (channels we never saw created):
        for ioid in 2..=10 {
            tracker.on_op_activity(&key, 0, ioid, 13);
        }

        // The known IOID=1 should resolve (via its op's pv_name from INIT)
        let pv1 = tracker.resolve_pv_name(&key, 100, 1);
        assert_eq!(pv1, Some("CAPTURED:PV".to_string()));

        // Unknown ioids should NOT resolve to CAPTURED:PV
        for ioid in 2..=10 {
            let pv = tracker.resolve_pv_name(&key, 0, ioid);
            assert_eq!(
                pv, None,
                "ioid={} should not resolve to the single captured channel",
                ioid
            );
        }
    }

    #[test]
    fn test_on_op_activity_placeholder_not_created_for_multiplexed() {
        // When one channel is known but operations already exist, activity
        // with sid=0 should create a placeholder WITHOUT a PV name (not
        // inheriting from the single captured channel).
        let mut tracker = PvaStateTracker::with_defaults();
        let key = test_conn_key();

        tracker.on_create_channel_request(&key, 1, "KNOWN:PV".to_string());
        tracker.on_create_channel_response(&key, 1, 100);

        // First op — establishes that operations exist
        tracker.on_op_init_request(&key, 100, 1, 13);

        // Second op via on_op_activity with sid=0 — should NOT inherit PV name
        tracker.on_op_activity(&key, 0, 2, 13);

        let pv = tracker.resolve_pv_name(&key, 0, 2);
        assert_eq!(
            pv, None,
            "placeholder for ioid=2 should not inherit PV from single-channel fallback"
        );
    }

    #[test]
    fn test_search_cache_populates_and_resolves() {
        let mut tracker = PvaStateTracker::with_defaults();
        let client_ip: IpAddr = "192.168.1.10".parse().unwrap();

        // Simulate SEARCH request with CID → PV name pairs
        let pv_requests = vec![
            (100, "MOTOR:X:POSITION".to_string()),
            (101, "MOTOR:Y:POSITION".to_string()),
            (102, "TEMP:SENSOR:1".to_string()),
        ];
        tracker.on_search(&pv_requests, Some(client_ip));

        // Resolve CIDs from a SEARCH_RESPONSE
        let resolved = tracker.resolve_search_cids(&[100, 101, 102], Some(client_ip));
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0], (100, "MOTOR:X:POSITION".to_string()));
        assert_eq!(resolved[1], (101, "MOTOR:Y:POSITION".to_string()));
        assert_eq!(resolved[2], (102, "TEMP:SENSOR:1".to_string()));
    }

    #[test]
    fn test_search_cache_partial_resolve() {
        let mut tracker = PvaStateTracker::with_defaults();
        let client_ip: IpAddr = "192.168.1.10".parse().unwrap();

        let pv_requests = vec![(100, "MOTOR:X:POSITION".to_string())];
        tracker.on_search(&pv_requests, Some(client_ip));

        // Resolve with some CIDs that were never cached
        let resolved = tracker.resolve_search_cids(&[100, 999], Some(client_ip));
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0], (100, "MOTOR:X:POSITION".to_string()));
    }

    #[test]
    fn test_search_cache_scoped_by_ip() {
        let mut tracker = PvaStateTracker::with_defaults();
        let client_a: IpAddr = "192.168.1.10".parse().unwrap();
        let client_b: IpAddr = "192.168.1.20".parse().unwrap();

        // Both clients use the same CID=1 but different PV names
        tracker.on_search(&[(1, "CLIENT_A:PV".to_string())], Some(client_a));
        tracker.on_search(&[(1, "CLIENT_B:PV".to_string())], Some(client_b));

        // Each client should resolve to its own PV name
        let resolved_a = tracker.resolve_search_cids(&[1], Some(client_a));
        assert_eq!(resolved_a.len(), 1);
        assert_eq!(resolved_a[0].1, "CLIENT_A:PV");

        let resolved_b = tracker.resolve_search_cids(&[1], Some(client_b));
        assert_eq!(resolved_b.len(), 1);
        assert_eq!(resolved_b[0].1, "CLIENT_B:PV");
    }

    #[test]
    fn test_search_cache_flat_fallback() {
        let mut tracker = PvaStateTracker::with_defaults();
        let client_ip: IpAddr = "192.168.1.10".parse().unwrap();

        // Cache with a known client IP
        tracker.on_search(&[(42, "SOME:PV:NAME".to_string())], Some(client_ip));

        // Resolve without knowing the client IP (flat fallback)
        let resolved = tracker.resolve_search_cids(&[42], None);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].1, "SOME:PV:NAME");
    }

    #[test]
    fn test_search_cache_used_by_create_channel_response_fallback() {
        // When capture misses CREATE_CHANNEL request but has SEARCH,
        // the search cache should resolve PV name in CREATE_CHANNEL response.
        let mut tracker = PvaStateTracker::with_defaults();
        let key = test_conn_key();
        let client_ip: IpAddr = "192.168.1.1".parse().unwrap();

        // Simulate SEARCH with CID=5 → "SEARCHED:PV"
        tracker.on_search(&[(5, "SEARCHED:PV".to_string())], Some(client_ip));

        // Simulate CREATE_CHANNEL response without having seen the request
        tracker.on_create_channel_response(&key, 5, 200);

        // The PV name should be resolved from search cache
        let pv = tracker.resolve_pv_name(&key, 200, 0);
        assert_eq!(pv, Some("SEARCHED:PV".to_string()));
    }

    #[test]
    fn test_search_responses_resolved_stat() {
        let mut tracker = PvaStateTracker::with_defaults();
        let client_ip: IpAddr = "192.168.1.10".parse().unwrap();

        tracker.on_search(
            &[(1, "PV:A".to_string()), (2, "PV:B".to_string())],
            Some(client_ip),
        );

        assert_eq!(tracker.stats.search_responses_resolved, 0);

        tracker.resolve_search_cids(&[1, 2], Some(client_ip));
        assert_eq!(tracker.stats.search_responses_resolved, 2);

        // Resolving again increments further
        tracker.resolve_search_cids(&[1], Some(client_ip));
        assert_eq!(tracker.stats.search_responses_resolved, 3);
    }

    #[test]
    fn test_retroactive_resolve_unknown_channels_from_search() {
        // Simulates the Java EPICS client scenario:
        // 1. Capture starts mid-stream, sees CREATE_CHANNEL responses (cid+sid)
        //    but missed the requests → channels are <unknown:cid=N>
        // 2. Later a SEARCH arrives with those CIDs → retroactively resolves PV names
        let mut tracker = PvaStateTracker::with_defaults();
        let key = test_conn_key();

        // Step 1: CREATE_CHANNEL responses without prior requests → unknown channels
        tracker.on_create_channel_response(&key, 100, 500);
        tracker.on_create_channel_response(&key, 101, 501);
        tracker.on_create_channel_response(&key, 102, 502);

        // Verify channels are unknown
        assert_eq!(
            tracker.resolve_pv_name(&key, 500, 0),
            Some("<unknown:cid=100>".to_string())
        );
        assert_eq!(
            tracker.resolve_pv_name(&key, 501, 0),
            Some("<unknown:cid=101>".to_string())
        );

        // Step 2: SEARCH arrives with CID→PV name mappings
        let client_ip: IpAddr = "192.168.1.1".parse().unwrap();
        tracker.on_search(
            &[
                (100, "MOTOR:X:POS".to_string()),
                (101, "MOTOR:Y:POS".to_string()),
                (102, "TEMP:SENSOR:1".to_string()),
            ],
            Some(client_ip),
        );

        // Verify channels are now resolved
        assert_eq!(
            tracker.resolve_pv_name(&key, 500, 0),
            Some("MOTOR:X:POS".to_string())
        );
        assert_eq!(
            tracker.resolve_pv_name(&key, 501, 0),
            Some("MOTOR:Y:POS".to_string())
        );
        assert_eq!(
            tracker.resolve_pv_name(&key, 502, 0),
            Some("TEMP:SENSOR:1".to_string())
        );

        // Verify retroactive resolution was counted
        assert_eq!(tracker.stats.search_retroactive_resolves, 3);
    }

    #[test]
    fn test_retroactive_resolve_also_updates_operations() {
        // When a placeholder operation has pv_name=None and its SID maps
        // to a channel that just got retroactively resolved, the operation's
        // pv_name should also be updated.
        let mut tracker = PvaStateTracker::with_defaults();
        let key = test_conn_key();

        // CREATE_CHANNEL response without request → <unknown:cid=100>
        tracker.on_create_channel_response(&key, 100, 500);

        // Op INIT on that channel → operation gets pv_name from channel
        // But the channel is unknown, so op gets "<unknown:cid=100>" as name
        tracker.on_op_init_request(&key, 500, 1, 13); // MONITOR

        // Verify op resolves to unknown
        let pv = tracker.resolve_pv_name(&key, 500, 1);
        assert!(pv.is_some());
        // The op should have inherited the unknown name since it looked up via SID

        // SEARCH arrives with the CID→PV mapping
        let client_ip: IpAddr = "192.168.1.1".parse().unwrap();
        tracker.on_search(&[(100, "RESOLVED:PV".to_string())], Some(client_ip));

        // Channel should now be resolved
        assert_eq!(
            tracker.resolve_pv_name(&key, 500, 0),
            Some("RESOLVED:PV".to_string())
        );
        // Operation should also resolve (via SID→CID→channel)
        let pv = tracker.resolve_pv_name(&key, 500, 1);
        assert_eq!(pv, Some("RESOLVED:PV".to_string()));
    }
}
