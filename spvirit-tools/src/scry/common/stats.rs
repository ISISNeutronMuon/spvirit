use crate::scry::spvirit_state::PvaStateStats;
use crate::scry::common::fingerprint::detect::{DetectionEvent, DetectorCounts};
use crate::scry::common::fingerprint::types::{
    FingerprintCoverage, FingerprintStatus, SourceFingerprint,
};
use dns_lookup::lookup_addr;
use std::collections::HashMap;

use gethostname::gethostname;

//serde_json::json;

#[derive(Clone)]
pub struct PVEvent {
    pub system_time: std::time::SystemTime, // Real-world time
    pub timestamp: std::time::Instant,      // Monotonic clock
    pub source_ip: String,
    pub destination_ip: String,
    pub request_type: String,
    pub context: String,
    pub resolved_source_ip: String,
    pub resolved_destination_ip: String,
    pub host_name: String,
    // Structured fields for normalized Loki output
    pub pv_name: Option<String>,
    pub ioid: Option<u32>,
    pub sid: Option<u32>,
    pub cid: Option<u32>,
    pub value_text: Option<String>,
    pub mid_stream: bool,
}
impl PVEvent {
    pub fn new(source_ip: String, destination_ip: String, request_type: String) -> Self {
        Self {
            system_time: std::time::SystemTime::now(),
            timestamp: std::time::Instant::now(),
            source_ip,
            destination_ip,
            request_type,
            context: String::new(),
            resolved_source_ip: "Unknown".to_string(),
            resolved_destination_ip: "Unknown".to_string(),
            host_name: gethostname().to_string_lossy().to_string(),
            pv_name: None,
            ioid: None,
            sid: None,
            cid: None,
            value_text: None,
            mid_stream: false,
        }
    }

    pub fn new_resolved(
        source_ip: String,
        destination_ip: String,
        request_type: String,
        resolved_source_ip: String,
        resolved_destination_ip: String,
        context: String,
    ) -> Self {
        Self {
            system_time: std::time::SystemTime::now(),
            timestamp: std::time::Instant::now(),
            source_ip,
            destination_ip,
            request_type,
            context,
            resolved_source_ip,
            resolved_destination_ip,
            host_name: gethostname().to_string_lossy().to_string(),
            pv_name: None,
            ioid: None,
            sid: None,
            cid: None,
            value_text: None,
            mid_stream: false,
        }
    }

    pub fn print(&self) {
        println!(
            "Source IP: {}, Destination IP: {}, Request type: {}",
            self.source_ip, self.destination_ip, self.request_type
        );
    }

    pub fn to_string_with_host(&self) -> String {
        self.internal_to_string(true)
    }

    pub fn to_string(&self) -> String {
        self.internal_to_string(false)
    }
    fn internal_to_string(&self, show_host: bool) -> String {
        // let show_host = show_host;

        let req_type = self.request_type.clone();

        // we need to pad all ips to be 15 characters long
        //  right align
        let source_ip_fn = || {
            if self.resolved_source_ip == "Unknown" {
                self.source_ip.clone().to_string()
            } else {
                self.resolved_source_ip.clone().to_string()
            }
        };
        // left align
        // let destination_ip = format!("{: >15}", self.destination_ip);
        let destination_ip_fn = || {
            if self.resolved_destination_ip == "Unknown" {
                self.destination_ip.clone().to_string()
            } else {
                // println!("Resolved IP: {} -> {}", self.destination_ip, self.resolved_destination_ip);
                self.resolved_destination_ip.clone().to_string()
            }
        };
        // let source_ip = format!("{: <15}", source_ip_fn());
        // let destination_ip = format!("{: <15}", destination_ip_fn());

        // need limit the names to 15 characters
        let source_ip = || {
            let mut source_ip = source_ip_fn();
            if source_ip.len() > 15 {
                source_ip.truncate(15);
            }
            source_ip
        };
        let destination_ip = || {
            let mut destination_ip = destination_ip_fn();
            if destination_ip.len() > 15 {
                destination_ip.truncate(15);
            }
            destination_ip
        };

        let source_ip = format!("{: <15}", source_ip());
        let destination_ip = format!("{: <15}", destination_ip());

        let human_readable_time = || {
            // padded to 15 characters
            let duration = self.timestamp.elapsed();
            let secs = duration.as_secs();
            let millis = duration.subsec_millis();
            let micros = duration.subsec_micros();
            let nanos = duration.subsec_nanos();
            if secs > 60 {
                format!("{}m", secs / 60)
            } else if secs > 0 {
                format!("{}s", secs)
            } else if millis > 0 {
                format!("{}ms", millis)
            } else if micros > 0 {
                format!("{}μs", micros)
            } else {
                format!("{}ns", nanos)
            }
        };
        let human_readable_time = format!("{: <5}", human_readable_time());
        let mut hostname = "".to_string();
        if show_host {
            hostname = self.host_name.clone();
        }

        match req_type.as_str() {
            "SEARCH" => format!(
                "{}:{} -> {}:{} {} 🔍",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "GET" => format!(
                "{}:{} -> {}:{} {} 📥",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "POST" => format!(
                "{}:{} -> {}:{} {} 📤",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "PUT" => format!(
                "{}:{} -> {}:{} {} 📤",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "PUT_GET" => format!(
                "{}:{} -> {}:{} {} 📥📤",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "MONITOR" => format!(
                "{}:{} -> {}:{} {} 🖥️",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "ARRAY" => format!(
                "{}:{} -> {}:{} {} 📦",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "DESTROY_REQUEST" => format!(
                "{}:{} -> {}:{} {} 🗑️",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "PROCESS" => format!(
                "{}:{} -> {}:{} {} 🔄",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "GET_FIELD" => format!(
                "{}:{} -> {}:{} {} 📄",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "MESSAGE" => format!(
                "{}:{} -> {}:{} {} 📧",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "MULTIPLE_DATA" => format!(
                "{}:{} -> {}:{} {} 📦📦",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "RPC" => format!(
                "{}:{} -> {}:{} {} 📞",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "CANCEL_REQUEST" => format!(
                "{}:{} -> {}:{} {} 🚫",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "ORIGIN_TAG" => format!(
                "{}:{} -> {}:{} {} 🏷️",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "BEACON" => format!(
                "{}:{} -> {}:{} {} 🚨",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "CONNECTION_VALIDATION" => format!(
                "{}:{} -> {}:{} {} 🛡️",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "ECHO" => format!(
                "{}:{} -> {}:{} {} 🗣️",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "SEARCH_RESPONSE" => format!(
                "{}:{} -> {}:{} {} 🔍📤",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "AUTHNZ" => format!(
                "{}:{} -> {}:{} {} 🛡️",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "ACL_CHANGE" => format!(
                "{}:{} -> {}:{} {} 🛡️",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "CREATE_CHANNEL" => format!(
                "{}:{} -> {}:{} {} 📡",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "DESTROY_CHANNEL" => format!(
                "{}:{} -> {}:{} {} 💣📡",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "CONNECTION_VALIDATED" => format!(
                "{}:{} -> {}:{} {} ✔️",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            "UNKNOWN" => format!(
                "{}:{} -> {}:{} {} ❓",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
            _ => format!(
                "{}:{} -> {}:{} {}",
                human_readable_time, source_ip, destination_ip, req_type, hostname
            ),
        }
    }
}
#[derive(Clone)]
pub struct IfStats {
    pub data_rate: f64,
    pub packet_rate: f64,
    pub timestamp: std::time::Instant,
    pub source_ips: HashMap<String, u64>, // we will show ip and number of packets
    pub destination_ips: HashMap<String, u64>,
    pub request_types: HashMap<String, u64>,
    pub resolved_ips: HashMap<String, String>,
    pub events: Vec<PVEvent>,
    pub tracked_connections_summary: Vec<TrackedConnectionEntry>,
    pub tracked_connections_detail: Vec<TrackedConnectionEntry>,
    pub resolve: bool,
    pub buffer_size: usize,
    // Self-monitoring gauge fields
    pub pcap_received: u64,
    pub pcap_dropped: u64,
    pub pcap_if_dropped: u64,
    pub pva_stats: PvaStateStats,
    pub pva_stats_prev: PvaStateStats,
    pub pva_total_channels: usize,
    pub pva_total_connections: usize,
    pub pva_seg_started: u64,
    pub pva_seg_completed: u64,
    pub pva_seg_dropped: u64,
    pub pva_seg_drop_short: u64,
    pub pva_seg_drop_invalid: u64,
    pub pva_seg_drop_partial: u64,
    pub pva_seg_drop_missing_first: u64,
    pub pva_stream_dropped: u64,
    pub pva_seg_log: Vec<String>,
    /// Fingerprints harvested since the last export cycle (spec §7). Drained
    /// by `clear_fingerprint_exports` once written, so a closed window is
    /// exported exactly once.
    pub fingerprints: Vec<SourceFingerprint>,
    /// Every in-flight channel's status, including those with too few
    /// samples — spec §6.1 requires absence to be visible, so this is what
    /// the TUI renders rather than `fingerprints` alone.
    pub fingerprint_statuses: Vec<FingerprintStatus>,
    /// How much of the observable traffic was actually fingerprinted.
    pub fingerprint_coverage: FingerprintCoverage,
    /// Detection rule firings since the last export (spec §5). A backlog
    /// like `fingerprints`, drained by `clear_fingerprint_exports`.
    pub fingerprint_events: Vec<DetectionEvent>,
    /// Cumulative count of detection events discarded by the in-memory cap
    /// on `fingerprint_events` (the capture loop never drains that vector on
    /// its own, only the collector does, so it is bounded independently).
    /// Not reset by `clear_fingerprint_exports` — only by `clear()`, when the
    /// capture switches vantage point entirely. A non-zero value means the
    /// Events view is not the complete log: some earlier firings were
    /// evicted before anything ever read them, and that must never look
    /// identical to "nothing else fired" (spec 6.1). Exported on the
    /// `pv_fingerprint_detect` line as `event_buffer_dropped` so a
    /// collector-only deployment, which has no TUI to read, still learns the
    /// cap fired.
    pub fingerprint_events_dropped: u64,
    /// Cumulative tally of everything the detector withheld — suppressed,
    /// deduplicated or capped. Current state, replaced wholesale rather than
    /// accumulated.
    pub fingerprint_detect_counts: DetectorCounts,
    // pub client_ids: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct TrackedConnectionEntry {
    pub id: String,
    pub endpoint_a: String,
    pub endpoint_b: String,
    pub cid: Option<u32>,
    pub sid: Option<u32>,
    pub pv_name: Option<String>,
    pub channel_count: usize,
    pub operation_count: usize,
    pub updates_per_sec: f64,
    pub last_seen_secs: u64,
    pub mid_stream: bool,
    pub is_beacon: bool,
    pub is_broadcast: bool,
    pub recent_messages: Vec<String>,
}

impl Default for IfStats {
    fn default() -> Self {
        IfStats {
            data_rate: 0.0,
            packet_rate: 0.0,
            timestamp: std::time::Instant::now(),
            source_ips: HashMap::new(),
            destination_ips: HashMap::new(),
            request_types: HashMap::new(),
            events: Vec::new(),
            resolved_ips: HashMap::new(),
            tracked_connections_summary: Vec::new(),
            tracked_connections_detail: Vec::new(),
            resolve: false,
            buffer_size: 5000,
            pcap_received: 0,
            pcap_dropped: 0,
            pcap_if_dropped: 0,
            pva_stats: PvaStateStats::default(),
            pva_stats_prev: PvaStateStats::default(),
            pva_total_channels: 0,
            pva_total_connections: 0,
            pva_seg_started: 0,
            pva_seg_completed: 0,
            pva_seg_dropped: 0,
            pva_seg_drop_short: 0,
            pva_seg_drop_invalid: 0,
            pva_seg_drop_partial: 0,
            pva_seg_drop_missing_first: 0,
            pva_stream_dropped: 0,
            pva_seg_log: Vec::new(),
            fingerprints: Vec::new(),
            fingerprint_statuses: Vec::new(),
            fingerprint_coverage: FingerprintCoverage::default(),
            fingerprint_events: Vec::new(),
            fingerprint_events_dropped: 0,
            fingerprint_detect_counts: DetectorCounts::default(),
        }
    }
}

impl IfStats {
    pub fn new() -> Self {
        Self {
            data_rate: 0.0,
            packet_rate: 0.0,
            timestamp: std::time::Instant::now(),
            source_ips: HashMap::new(),
            destination_ips: HashMap::new(),
            request_types: HashMap::new(),
            events: Vec::new(),
            resolved_ips: HashMap::new(),
            tracked_connections_summary: Vec::new(),
            tracked_connections_detail: Vec::new(),
            resolve: false,
            buffer_size: 5000,
            pcap_received: 0,
            pcap_dropped: 0,
            pcap_if_dropped: 0,
            pva_stats: PvaStateStats::default(),
            pva_stats_prev: PvaStateStats::default(),
            pva_total_channels: 0,
            pva_total_connections: 0,
            pva_seg_started: 0,
            pva_seg_completed: 0,
            pva_seg_dropped: 0,
            pva_seg_drop_short: 0,
            pva_seg_drop_invalid: 0,
            pva_seg_drop_partial: 0,
            pva_seg_drop_missing_first: 0,
            pva_stream_dropped: 0,
            pva_seg_log: Vec::new(),
            fingerprints: Vec::new(),
            fingerprint_statuses: Vec::new(),
            fingerprint_coverage: FingerprintCoverage::default(),
            fingerprint_events: Vec::new(),
            fingerprint_events_dropped: 0,
            fingerprint_detect_counts: DetectorCounts::default(),
        }
    }

    pub fn print(&self) {
        println!(
            "Data rate: {:.2} Mbit/s, Packet rate: {:.2} pps",
            self.data_rate, self.packet_rate
        );
    }

    pub fn to_line_protocol(&self) -> Vec<String> {
        let mut data_line_protocol = vec![];
        let host_name = gethostname().to_string_lossy().to_string();
        data_line_protocol.push(format!(
            "rates,host={} pps={},data_rate={}\n",
            host_name, self.packet_rate, self.data_rate
        ));

        for (ip, count) in self.source_ips.iter() {
            data_line_protocol.push(format!(
                "source_ips,host={},ip={} count={}\n",
                host_name, ip, count
            ));
        }
        for (ip, count) in self.destination_ips.iter() {
            data_line_protocol.push(format!(
                "destination_ips,host={},ip={} count={}\n",
                host_name, ip, count
            ));
        }
        for (req, count) in self.request_types.iter() {
            data_line_protocol.push(format!(
                "request_types,host={},request_type={} count={}\n",
                host_name, req, count
            ));
        }

        for event in self.events.iter() {
            // println!("{}",event.timestamp.elapsed().as_secs());
            data_line_protocol.push(format!("events,host={},source_ip={},destination_ip={},request_type={} payload_size={},count=1\n",event.host_name,event.source_ip,event.destination_ip,event.request_type,event.context.len()));
        }

        // for node graphs on grafana we need a nodes table
        // id	title
        // lets merge sources and destinations
        let combined_set = self.source_ips.keys().chain(self.destination_ips.keys());
        // strip of repeated ips
        let combined_set: std::collections::HashSet<&String> = combined_set.collect();
        let mut combined_set_counts_per_ip = HashMap::new();
        for ip in combined_set.clone() {
            let count =
                self.source_ips.get(ip).unwrap_or(&0) + self.destination_ips.get(ip).unwrap_or(&0);
            combined_set_counts_per_ip.insert(ip.clone(), count);
        }

        for ip in combined_set {
            let resolved_ip = self.get_resolved_ip(ip.clone());
            if resolved_ip == "Unknown" {
                data_line_protocol.push(format!(
                    "node,host={},id={},title={} count={}\n",
                    host_name,
                    ip,
                    ip,
                    combined_set_counts_per_ip.get(ip).unwrap_or(&0)
                ));
            } else {
                data_line_protocol.push(format!(
                    "node,host={},id={},title={} count={}\n",
                    host_name,
                    ip,
                    resolved_ip,
                    combined_set_counts_per_ip.get(ip).unwrap_or(&0)
                ));
            }
        }
        // id	source	target	mainstat	detail

        //event dict that will summarise all unique events and count them
        let mut event_dict = HashMap::new();
        for event in self.events.iter() {
            let id = format!("{}-{}", event.source_ip, event.destination_ip);
            let count = event_dict.entry(id).or_insert(0);
            *count += 1;
        }

        for (id, count) in event_dict.iter() {
            let parts: Vec<&str> = id.split("-").collect();
            let source_ip = parts[0];
            let destination_ip = parts[1];
            data_line_protocol.push(format!(
                "edge,host={},id={},source={},target={} mainStat={}\n",
                host_name, id, source_ip, destination_ip, count
            ));
        }

        //format!("edges,host={},id={},source={},target={},mainstat={}, count={}\n",host_name,id,event.source_ip,event.destination_ip,event.request_type,event.context.len())

        // --- Self-monitoring gauge measurements ---

        // Capture engine gauges
        data_line_protocol.push(format!(
            "capture_gauge,host={} received={}i,dropped={}i,if_dropped={}i,data_rate_bps={},packet_rate_pps={},unique_sources={}i,unique_destinations={}i\n",
            host_name,
            self.pcap_received,
            self.pcap_dropped,
            self.pcap_if_dropped,
            self.data_rate,
            self.packet_rate,
            self.source_ips.len(),
            self.destination_ips.len(),
        ));

        // PVA state gauges (cumulative counters — use non_negative_derivative in Grafana)
        data_line_protocol.push(format!(
            "pva_gauge,host={} active_channels={}i,active_connections={}i,channels_created={}i,channels_destroyed={}i,channels_expired={}i,channels_evicted={}i,operations_created={}i,operations_completed={}i,operations_expired={}i,memory_bytes={}i,shed_placeholder_ops={}i,shed_message_rings={}i,shed_search_entries={}i,shed_channels={}i,create_channel_requests={}i,create_channel_responses={}i,seg_started={}i,seg_completed={}i,seg_dropped={}i,seg_drop_short={}i,seg_drop_invalid={}i,seg_drop_partial={}i,seg_drop_missing_first={}i,stream_dropped={}i\n",
            host_name,
            self.pva_total_channels,
            self.pva_total_connections,
            self.pva_stats.channels_created,
            self.pva_stats.channels_destroyed,
            self.pva_stats.channels_expired,
            self.pva_stats.channels_evicted,
            self.pva_stats.operations_created,
            self.pva_stats.operations_completed,
            self.pva_stats.operations_expired,
            self.pva_stats.memory_bytes,
            self.pva_stats.shed_placeholder_ops,
            self.pva_stats.shed_message_rings,
            self.pva_stats.shed_search_entries,
            self.pva_stats.shed_channels,
            self.pva_stats.create_channel_requests,
            self.pva_stats.create_channel_responses,
            self.pva_seg_started,
            self.pva_seg_completed,
            self.pva_seg_dropped,
            self.pva_seg_drop_short,
            self.pva_seg_drop_invalid,
            self.pva_seg_drop_partial,
            self.pva_seg_drop_missing_first,
            self.pva_stream_dropped,
        ));

        // PVA state deltas (per-interval rates — instant rate in Grafana)
        let delta_channels_created = self.pva_stats.channels_created.saturating_sub(self.pva_stats_prev.channels_created);
        let delta_channels_destroyed = self.pva_stats.channels_destroyed.saturating_sub(self.pva_stats_prev.channels_destroyed);
        let delta_channels_expired = self.pva_stats.channels_expired.saturating_sub(self.pva_stats_prev.channels_expired);
        let delta_channels_evicted = self.pva_stats.channels_evicted.saturating_sub(self.pva_stats_prev.channels_evicted);
        let delta_operations_created = self.pva_stats.operations_created.saturating_sub(self.pva_stats_prev.operations_created);
        let delta_operations_completed = self.pva_stats.operations_completed.saturating_sub(self.pva_stats_prev.operations_completed);
        data_line_protocol.push(format!(
            "pva_gauge_delta,host={} channels_created={}i,channels_destroyed={}i,channels_expired={}i,channels_evicted={}i,operations_created={}i,operations_completed={}i\n",
            host_name,
            delta_channels_created,
            delta_channels_destroyed,
            delta_channels_expired,
            delta_channels_evicted,
            delta_operations_created,
            delta_operations_completed,
        ));

        // Collector process gauges
        let rss_bytes = std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|pages| pages * 4096)
            .unwrap_or(0);
        let event_buffer_pct = if self.buffer_size > 0 {
            (self.events.len() as f64 / self.buffer_size as f64) * 100.0
        } else {
            0.0
        };
        data_line_protocol.push(format!(
            "collector_gauge,host={} rss_bytes={}i,event_buffer_pct={:.1},event_count={}i,buffer_size={}i\n",
            host_name,
            rss_bytes,
            event_buffer_pct,
            self.events.len(),
            self.buffer_size,
        ));

        // --- Source fingerprints (spec §7) ---
        //
        // Each fingerprint is stamped with the end of the window it
        // describes, not with the export time: the line is then a pure
        // function of the record, and a fingerprint delayed by a slow export
        // cycle still lands where it actually happened.
        for fp in self.fingerprints.iter() {
            let window_end_ns = fp
                .window_end
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            data_line_protocol.push(crate::scry::common::fingerprint::export::fingerprint_line_protocol(
                fp,
                &host_name,
                window_end_ns,
            ));
        }

        // Coverage is current state rather than a windowed record, so it is
        // stamped now. An all-zero coverage record means the fingerprinter
        // has not run at all; exporting it would look like measured zero
        // coverage, which is a different and much worse claim.
        let coverage_observed = self.fingerprint_coverage.tracked_channels > 0
            || self.fingerprint_coverage.deferred_total > 0
            || self.fingerprint_coverage.rotated_out_total > 0
            // Traffic the probe could not read is still traffic: if it is the
            // only thing that happened, the coverage line is the one place
            // that says so, and gating it out would hide the whole loss.
            || self.fingerprint_coverage.unprobeable_updates > 0;
        if coverage_observed {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            data_line_protocol.push(crate::scry::common::fingerprint::export::coverage_line_protocol(
                &self.fingerprint_coverage,
                &host_name,
                now_ns,
            ));
        }

        // The detector's withheld tally. Emitted whenever there is anything
        // to say, independently of the coverage gate: a suppressed or capped
        // firing is exactly the thing that must not be able to hide.
        if !self.fingerprint_detect_counts.is_empty()
            || self.fingerprint_events_dropped > 0
            || coverage_observed
        {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            data_line_protocol.push(
                crate::scry::common::fingerprint::export::detect_counts_line_protocol(
                    &self.fingerprint_detect_counts,
                    self.fingerprint_events_dropped,
                    &host_name,
                    now_ns,
                ),
            );
        }

        data_line_protocol
    }

    pub fn to_loki_stream(&self) -> Vec<(String, String, HashMap<String, String>)> {
        let database = "pv_events";
        let mut entries: Vec<(String, String, HashMap<String, String>)> = self
            .events
            .iter()
            .map(|event| {
                let timestamp_ns = match event
                    .system_time
                    .checked_add(event.timestamp.elapsed())
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                {
                    Some(duration) => duration.as_nanos().to_string(),
                    None => "0".to_string(),
                };

                // Per-category normalized log line
                let log_line = Self::format_loki_log_line(event);

                let mut metadata = HashMap::new();
                metadata.insert("source_ip".to_string(), event.source_ip.clone());
                metadata.insert("destination_ip".to_string(), event.destination_ip.clone());
                metadata.insert("request_type".to_string(), event.request_type.clone());
                metadata.insert("context".to_string(), event.context.clone());
                metadata.insert(
                    "resolved_source_ip".to_string(),
                    event.resolved_source_ip.clone(),
                );
                metadata.insert(
                    "resolved_destination_ip".to_string(),
                    event.resolved_destination_ip.clone(),
                );
                metadata.insert("host_name".to_string(), event.host_name.clone());
                metadata.insert("database".to_string(), database.to_string());

                // Add structured metadata for filtering
                if let Some(ref pv) = event.pv_name {
                    metadata.insert("pv_name".to_string(), pv.clone());
                }
                if let Some(ioid) = event.ioid {
                    metadata.insert("ioid".to_string(), ioid.to_string());
                }
                if let Some(sid) = event.sid {
                    metadata.insert("sid".to_string(), sid.to_string());
                }
                if let Some(cid) = event.cid {
                    metadata.insert("cid".to_string(), cid.to_string());
                }
                if event.mid_stream {
                    metadata.insert("mid_stream".to_string(), "true".to_string());
                }

                (timestamp_ns, log_line, metadata)
            })
            .collect();

        // Detection events (spec §7). `LokiLogger` promotes `request_type`
        // to a stream label, so putting the stream name there is what keeps
        // these out of `pv_events` rather than mixed into it. Stamped with
        // when the rule fired, not with export time.
        let host_name = gethostname().to_string_lossy().to_string();
        for ev in self.fingerprint_events.iter() {
            let (stream, line, labels) =
                crate::scry::common::fingerprint::detect::detection_loki_entry(ev);
            let timestamp_ns = ev
                .at
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos().to_string())
                .unwrap_or_else(|_| "0".to_string());
            let mut metadata = labels;
            metadata.insert("request_type".to_string(), stream);
            metadata.insert("source_ip".to_string(), ev.server_addr.ip().to_string());
            metadata.insert("destination_ip".to_string(), "observer".to_string());
            metadata.insert("host_name".to_string(), host_name.clone());
            metadata.insert("database".to_string(), "pv_fingerprint".to_string());
            entries.push((timestamp_ns, line, metadata));
        }

        entries
    }

    /// Format a normalized Loki log line based on request type category
    fn format_loki_log_line(event: &PVEvent) -> String {
        let header = format!(
            "{} from {} to {}:",
            event.request_type, event.source_ip, event.destination_ip
        );

        match event.request_type.as_str() {
            // Op commands: MONITOR, GET, PUT, PUT_GET, ARRAY, PROCESS, RPC
            "MONITOR" | "GET" | "PUT" | "PUT_GET" | "ARRAY" | "PROCESS" | "RPC" => {
                let pv_display = match &event.pv_name {
                    Some(name) => format!(" pv={}", name),
                    None => {
                        // Extract data=[...] hint from context to help identify unresolved PVs
                        let data_hint = event.context.find("data=[").and_then(|start| {
                            event.context[start..].find("])").map(|end| {
                                format!(" {}", &event.context[start..start + end + 1])
                            })
                        }).unwrap_or_default();
                        match event.ioid {
                            Some(id) => format!(" pv=<unresolved:ioid={}>{}", id, data_hint),
                            None => data_hint,
                        }
                    }
                };
                let value_display = event
                    .value_text
                    .as_ref()
                    .map(|v| format!(" value={}", v))
                    .unwrap_or_default();
                let ioid_display = event
                    .ioid
                    .map(|id| format!(" ioid={}", id))
                    .unwrap_or_default();
                format!("{}{}{}{}", header, pv_display, value_display, ioid_display)
            }

            // Channel lifecycle
            "CREATE_CHANNEL" => {
                let pv_display = event
                    .pv_name
                    .as_ref()
                    .map(|n| format!(" pv={}", n))
                    .unwrap_or_default();
                let cid_display = event
                    .cid
                    .map(|c| format!(" cid={}", c))
                    .unwrap_or_default();
                let sid_display = event
                    .sid
                    .map(|s| format!(" sid={}", s))
                    .unwrap_or_default();
                format!("{}{}{}{}", header, pv_display, cid_display, sid_display)
            }
            "DESTROY_CHANNEL" => {
                let cid_display = event
                    .cid
                    .map(|c| format!(" cid={}", c))
                    .unwrap_or_default();
                let sid_display = event
                    .sid
                    .map(|s| format!(" sid={}", s))
                    .unwrap_or_default();
                format!("{}{}{}", header, cid_display, sid_display)
            }

            // All other types: use the raw context but without redundant prefix
            _ => {
                format!("{} {}", header, event.context)
            }
        }
    }

    pub fn clear_events(&mut self) {
        self.events.clear();
    }

    /// Drop already-exported fingerprints so a closed window is written once.
    /// Statuses and coverage are left in place — they are current state, not
    /// a backlog, and the TUI reads them between export cycles.
    pub fn clear_fingerprint_exports(&mut self) {
        self.fingerprints.clear();
        self.fingerprint_events.clear();
    }

    /// Bound `fingerprint_events` at `max`, oldest first. The capture loop
    /// never drains this vector on its own (only the collector does), so it
    /// is bounded independently here. Every event the cap discards is
    /// counted in `fingerprint_events_dropped` — never dropped silently
    /// (spec 6.1): a capped log must never look identical to a complete one.
    pub fn cap_fingerprint_events(&mut self, max: usize) {
        let overflow = self.fingerprint_events.len().saturating_sub(max);
        if overflow > 0 {
            self.fingerprint_events_dropped += overflow as u64;
            self.fingerprint_events.drain(0..overflow);
        }
    }

    /// Rotate PVA stats snapshot for delta computation.
    /// Call after writing to InfluxDB each cycle.
    pub fn rotate_stats(&mut self) {
        self.pva_stats_prev = self.pva_stats.clone();
    }

    pub fn diable_resolve(&mut self) {
        self.resolve = false;
    }
    pub fn enable_resolve(&mut self) {
        self.resolve = true;
    }
    pub fn add_source_ip(&mut self, ip: String) {
        if !self.resolved_ips.contains_key(&ip) && self.resolve {
            let host = self.dns_addr_lookup(ip.clone());
            self.resolved_ips.insert(ip.clone(), host);
        }
        let count = self.source_ips.entry(ip).or_insert(0);
        *count += 1;
    }
    pub fn add_destination_ip(&mut self, ip: String) {
        // println!("{:?}", self.resolved_ips);
        if !self.resolved_ips.contains_key(&ip) && self.resolve {
            let host = self.dns_addr_lookup(ip.clone());
            self.resolved_ips.insert(ip.clone(), host);
        }
        let count = self.destination_ips.entry(ip).or_insert(0);
        *count += 1;
    }
    pub fn add_request_type(&mut self, request_type: String) {
        let count = self.request_types.entry(request_type).or_insert(0);
        *count += 1;
    }
    pub fn add_event(&mut self, event: PVEvent) {
        self.events.insert(0, event);
        if self.events.len() > self.buffer_size {
            self.events.pop();
        }
    }
    fn dns_addr_lookup(&mut self, ip: String) -> String {
        let ip_addr: std::net::IpAddr = ip.parse().unwrap();

        let host: String = match lookup_addr(&ip_addr) {
            Ok(host) => host,
            Err(_) => "Unknown".to_string(),
        };
        // println!("Resolved IP: {}:{} -> {}", ip, host);
        host
    }
    pub fn get_resolved_ip(&self, ip: String) -> String {
        match self.resolved_ips.get(&ip) {
            Some(host) => host.clone(),
            None => "Unknown".to_string(),
        }
    }
    pub fn set_buffer_size(&mut self, size: usize) {
        // 100k worked fine in testing but moving ifstats between threads caused a lot of CPU usage, realisticaly we should never need a buffer size of 100k events
        if size > 50000 {
            self.buffer_size = 50000;
        } else if size < 100 {
            self.buffer_size = 100;
        } else {
            self.buffer_size = size;
        }
    }
    fn get_memory_size(&self) -> usize {
        self.events.len() * std::mem::size_of::<PVEvent>()
    }
    pub fn get_memory_usage(&self) -> String {
        let mem_size = self.get_memory_size();
        if mem_size > 1_000_000 {
            format!("{:.2} MB", mem_size as f64 / 1_000_000.0)
        } else if mem_size > 1_000 {
            format!("{:.2} KB", mem_size as f64 / 1_000.0)
        } else {
            format!("{:.2} B", mem_size as f64)
        }
    }

    pub fn aggregate(&mut self, other: IfStats) {
        self.timestamp = std::time::Instant::now();
        for (ip, count) in other.source_ips.iter() {
            let entry = self.source_ips.entry(ip.clone()).or_insert(0);
            *entry += count;
        }
        for (ip, count) in other.destination_ips.iter() {
            let entry = self.destination_ips.entry(ip.clone()).or_insert(0);
            *entry += count;
        }
        for (req, count) in other.request_types.iter() {
            let entry = self.request_types.entry(req.clone()).or_insert(0);
            *entry += count;
        }
        for event in other.events.iter() {
            self.add_event(event.clone());
        }

        for (ip, host) in other.resolved_ips.iter() {
            self.resolved_ips.insert(ip.clone(), host.clone());
        }
        self.tracked_connections_summary = other.tracked_connections_summary.clone();
        self.tracked_connections_detail = other.tracked_connections_detail.clone();

        // Merge gauge fields (take latest values from other)
        self.pcap_received = other.pcap_received;
        self.pcap_dropped = other.pcap_dropped;
        self.pcap_if_dropped = other.pcap_if_dropped;
        self.pva_stats = other.pva_stats.clone();
        self.pva_stats_prev = other.pva_stats_prev.clone();
        self.pva_total_channels = other.pva_total_channels;
        self.pva_total_connections = other.pva_total_connections;
        // Fingerprints are a backlog and accumulate; statuses and coverage
        // are a snapshot of current state and are replaced wholesale.
        self.fingerprints.extend(other.fingerprints);
        self.fingerprint_events.extend(other.fingerprint_events);
        self.fingerprint_events_dropped += other.fingerprint_events_dropped;
        self.fingerprint_statuses = other.fingerprint_statuses;
        self.fingerprint_coverage = other.fingerprint_coverage;
        self.fingerprint_detect_counts = other.fingerprint_detect_counts;

        // iterate through events and count how many occured in the last second
        let mut last_second = 0;
        let agg_data_rate = 0.0;
        for event in self.events.iter() {
            if event.timestamp.elapsed().as_secs() < 1 {
                last_second += 1;
            }
        }
        self.packet_rate = last_second as f64;
        self.data_rate = agg_data_rate;
    }
    pub fn clear(&mut self) {
        self.data_rate = 0.0;
        self.packet_rate = 0.0;
        self.source_ips.clear();
        self.destination_ips.clear();
        self.request_types.clear();
        self.events.clear();
        self.resolved_ips.clear();
        self.tracked_connections_summary.clear();
        self.tracked_connections_detail.clear();
        self.pcap_received = 0;
        self.pcap_dropped = 0;
        self.pcap_if_dropped = 0;
        self.pva_stats = PvaStateStats::default();
        self.pva_stats_prev = PvaStateStats::default();
        self.pva_total_channels = 0;
        self.pva_total_connections = 0;
        self.fingerprints.clear();
        self.fingerprint_events.clear();
        self.fingerprint_events_dropped = 0;
        self.fingerprint_statuses.clear();
        self.fingerprint_coverage = FingerprintCoverage::default();
        self.fingerprint_detect_counts = DetectorCounts::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_fingerprint() -> crate::scry::common::fingerprint::types::SourceFingerprint {
        use crate::scry::common::fingerprint::types::{SamplingRegime, SourceFingerprint};
        use std::time::{Duration, UNIX_EPOCH};

        SourceFingerprint {
            channel: "TEST::PV".to_string(),
            server_addr: "10.0.0.11:5075".parse().unwrap(),
            phase_mean_ms: 262.016,
            phase_spread_us: 85.6,
            phase_circular_var: 0.0000003,
            value_lsb: Some(0.0175),
            lsb_confidence: 1.0,
            cadence_modal_s: 1.0,
            cadence_p50_s: 1.0,
            cadence_p95_s: 25.0,
            arrival_delta_mean_ms: 4.0,
            arrival_delta_var_ms: Some(0.1),
            sample_count: 40,
            window_start: UNIX_EPOCH + Duration::from_secs(1_755_756_000),
            window_end: UNIX_EPOCH + Duration::from_secs(1_755_756_900),
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            struct_id_changed: false,
            has_nan_limits: false,
            observed_count: 40,
            sampling: SamplingRegime::Full,
        }
    }

    fn sample_detection_event() -> crate::scry::common::fingerprint::detect::DetectionEvent {
        use crate::scry::common::fingerprint::detect::{DetectionEvent, DetectionRule};
        use std::time::{Duration, UNIX_EPOCH};

        let mut prior = sample_fingerprint();
        prior.phase_mean_ms = 695.973;
        DetectionEvent {
            rule: DetectionRule::D1SourceChange,
            channel: "TEST::PV".to_string(),
            server_addr: "10.0.0.11:5075".parse().unwrap(),
            detail: "phase shifted".to_string(),
            prior: Some(prior),
            current: sample_fingerprint(),
            at: UNIX_EPOCH + Duration::from_secs(1_755_756_900),
        }
    }

    /// Spec §7: detection events go to Loki, in their own stream rather than
    /// mixed into `pv_events`, and stamped with when the rule fired.
    #[test]
    fn if_stats_exports_detection_events_to_loki() {
        let mut stats = IfStats::new();
        stats.fingerprint_events.push(sample_detection_event());
        let entries = stats.to_loki_stream();
        let (ts, line, meta) = entries
            .iter()
            .find(|(_, _, m)| {
                m.get("request_type").map(String::as_str)
                    == Some(crate::scry::common::fingerprint::detect::DETECTION_STREAM)
            })
            .expect("detection entry");
        // The first tuple element is the Loki timestamp, not the stream name:
        // `LokiLogger` promotes `request_type` to the stream label.
        assert_eq!(ts, "1755756900000000000");
        assert_eq!(meta.get("rule").map(String::as_str), Some("D1"));
        assert!(line.contains("prior_phase_mean_ms=695.973"), "{}", line);
        assert!(line.contains("phase_mean_ms=262.016"), "{}", line);
    }

    /// Events are a backlog like fingerprints, not a snapshot: they must
    /// accumulate across aggregation and drain exactly once on export.
    #[test]
    fn if_stats_accumulates_and_drains_detection_events() {
        let mut acc = IfStats::new();
        acc.fingerprint_events.push(sample_detection_event());
        let mut other = IfStats::new();
        other.fingerprint_events.push(sample_detection_event());
        acc.aggregate(other);
        assert_eq!(acc.fingerprint_events.len(), 2);
        acc.clear_fingerprint_exports();
        assert!(acc.fingerprint_events.is_empty());
        assert!(
            acc.to_loki_stream().iter().all(|(_, _, m)| {
                m.get("request_type").map(String::as_str)
                    != Some(crate::scry::common::fingerprint::detect::DETECTION_STREAM)
            }),
            "a drained event was exported twice"
        );
    }

    /// Ruling: nothing the detector withheld may be invisible. The counters
    /// ride out on the same line-protocol path as coverage.
    #[test]
    fn if_stats_exports_detector_counts() {
        use crate::scry::common::fingerprint::detect::DetectorCounts;
        let mut stats = IfStats::new();
        stats.fingerprint_detect_counts =
            DetectorCounts {
                d1_suppressed: 7,
                events_dropped_by_cap: 3,
                d4_population_too_small: 9,
                d4_no_spread: 5,
                d2_pairs_evicted: 2,
                d2_channels_carried_over: 4,
                ..Default::default()
            };
        let lines = stats.to_line_protocol().concat();
        let line = lines
            .lines()
            .find(|l| l.starts_with("pv_fingerprint_detect,"))
            .expect("detector counts line");
        assert!(line.contains("d1_suppressed=7i"), "{}", line);
        assert!(line.contains("events_dropped_by_cap=3i"), "{}", line);
        // "D4 could not evaluate" is not the same fact as "D4 found
        // nothing", so both refusals have to reach the database too.
        assert!(line.contains("d4_population_too_small=9i"), "{}", line);
        assert!(line.contains("d4_no_spread=5i"), "{}", line);
        assert!(line.contains("d2_pairs_evicted=2i"), "{}", line);
        assert!(line.contains("d2_channels_carried_over=4i"), "{}", line);
    }

    /// The 500-event in-memory cap on `fingerprint_events` reaches the TUI,
    /// but a collector-only deployment has no screen: without an exported
    /// field it could never learn the cap fired, and a truncated event log
    /// would read as a complete one (spec 6.1). Distinct from the detector's
    /// own per-ingest `events_dropped_by_cap`, which counts something else.
    #[test]
    fn if_stats_exports_the_event_buffer_cap() {
        let mut stats = IfStats::new();
        stats.fingerprint_events_dropped = 11;
        let lines = stats.to_line_protocol().concat();
        let line = lines
            .lines()
            .find(|l| l.starts_with("pv_fingerprint_detect,"))
            .expect("the buffer cap alone must be enough to emit the line");
        assert!(line.contains("event_buffer_dropped=11i"), "{}", line);
        // The two counters must stay separately readable.
        assert!(line.contains("events_dropped_by_cap=0i"), "{}", line);
    }

    #[test]
    fn if_stats_exports_fingerprints_and_coverage() {
        use crate::scry::common::fingerprint::types::FingerprintCoverage;

        let mut stats = IfStats::new();
        stats.fingerprints.push(sample_fingerprint());
        stats.fingerprint_coverage = FingerprintCoverage {
            interface: "eth0".to_string(),
            tracked_channels: 10,
            admitted_channels: 9,
            deferred_channels: 1,
            deferred_total: 3,
            deferred_saturated: false,
            sampled_channels: 0,
            rotated_out_channels: 0,
            rotated_out_total: 0,
            rotated_out_saturated: false,
            unprobeable_updates: 42,
            min_samples: 30,
        };

        let lines = stats.to_line_protocol().concat();
        assert!(lines.contains("pv_fingerprint,"), "{}", lines);
        assert!(lines.contains("channel=TEST::PV"), "{}", lines);
        assert!(lines.contains("pv_fingerprint_coverage,"), "{}", lines);
        assert!(lines.contains("deferred_channels=1i"), "{}", lines);
        assert!(lines.contains("deferred_total=3i"), "{}", lines);
        assert!(lines.contains("rotated_out_total=0i"), "{}", lines);
        // Spec §6.3: updates the probe could not read never reach the engine,
        // so a collector-only deployment learns about them only here.
        assert!(lines.contains("unprobeable_updates=42i"), "{}", lines);

        // Exported fingerprints are one-shot: the collector clears them each
        // cycle so a window is never written twice.
        stats.clear_fingerprint_exports();
        let lines = stats.to_line_protocol().concat();
        assert!(!lines.contains("pv_fingerprint,"), "{}", lines);
        // Coverage is state, not a backlog, and must survive the clear.
        assert!(lines.contains("pv_fingerprint_coverage,"), "{}", lines);
    }

    /// Spec §6.1: a capped event log must never look like a complete one.
    /// Every event the cap discards must be counted, not silently dropped.
    #[test]
    fn cap_fingerprint_events_counts_every_discarded_event() {
        let mut stats = IfStats::new();
        for _ in 0..5 {
            stats.fingerprint_events.push(sample_detection_event());
        }
        assert_eq!(stats.fingerprint_events_dropped, 0);

        stats.cap_fingerprint_events(3);
        assert_eq!(stats.fingerprint_events.len(), 3);
        assert_eq!(stats.fingerprint_events_dropped, 2);

        // The counter is cumulative across multiple caps, not overwritten.
        for _ in 0..4 {
            stats.fingerprint_events.push(sample_detection_event());
        }
        stats.cap_fingerprint_events(3);
        assert_eq!(stats.fingerprint_events_dropped, 6);

        // clear_fingerprint_exports is an export-cycle drain, not a full
        // reset: the dropped tally must survive it.
        stats.clear_fingerprint_exports();
        assert_eq!(stats.fingerprint_events_dropped, 6);

        // Only clear() — a full vantage-point reset — zeroes it.
        stats.clear();
        assert_eq!(stats.fingerprint_events_dropped, 0);
    }

    /// The fingerprint line is stamped with the window it describes, so that a
    /// slow export cycle cannot shift the measurement in time.
    #[test]
    fn if_stats_stamps_a_fingerprint_with_its_window_end() {
        let mut stats = IfStats::new();
        stats.fingerprints.push(sample_fingerprint());
        let lines = stats.to_line_protocol().concat();
        let line = lines
            .lines()
            .find(|l| l.starts_with("pv_fingerprint,"))
            .expect("fingerprint line");
        assert!(line.ends_with(" 1755756900000000000"), "{}", line);
    }

    #[test]
    fn if_stats_omits_the_coverage_line_when_nothing_was_observed() {
        let stats = IfStats::new();
        let lines = stats.to_line_protocol().concat();
        assert!(!lines.contains("pv_fingerprint_coverage,"), "{}", lines);
    }

    /// Aggregation must not drop a harvested window on the floor: fingerprints
    /// accumulate, while coverage and statuses are replaced by the newest
    /// snapshot.
    #[test]
    fn if_stats_aggregate_accumulates_fingerprints() {
        let mut acc = IfStats::new();
        acc.fingerprints.push(sample_fingerprint());

        let mut other = IfStats::new();
        let mut second = sample_fingerprint();
        second.channel = "OTHER::PV".to_string();
        other.fingerprints.push(second);
        other.fingerprint_coverage.interface = "eth1".to_string();
        other.fingerprint_coverage.tracked_channels = 2;

        acc.aggregate(other);
        assert_eq!(acc.fingerprints.len(), 2);
        assert_eq!(acc.fingerprint_coverage.interface, "eth1");

        acc.clear();
        assert!(acc.fingerprints.is_empty());
        assert_eq!(acc.fingerprint_coverage.tracked_channels, 0);
    }

    #[test]
    fn test_pv_event_new() {
        let event = PVEvent::new(
            "192.168.1.1".to_string(),
            "192.168.1.2".to_string(),
            "GET".to_string(),
        );
        assert_eq!(event.source_ip, "192.168.1.1");
        assert_eq!(event.destination_ip, "192.168.1.2");
        assert_eq!(event.request_type, "GET");
        assert_eq!(event.resolved_source_ip, "Unknown");
        assert_eq!(event.resolved_destination_ip, "Unknown");
    }

    #[test]
    fn test_pv_event_to_string() {
        let event = PVEvent::new(
            "192.168.1.1".to_string(),
            "192.168.1.2".to_string(),
            "GET".to_string(),
        );
        let event_string = event.to_string();
        assert!(event_string.contains("192.168.1.1"));
        assert!(event_string.contains("192.168.1.2"));
        assert!(event_string.contains("GET"));
    }

    #[test]
    fn test_IfStats_new() {
        let stats = IfStats::new();
        assert_eq!(stats.data_rate, 0.0);
        assert_eq!(stats.packet_rate, 0.0);
        assert!(stats.source_ips.is_empty());
        assert!(stats.destination_ips.is_empty());
        assert!(stats.request_types.is_empty());
        assert!(stats.events.is_empty());
        assert!(stats.resolved_ips.is_empty());
        assert!(!stats.resolve);
        assert_eq!(stats.buffer_size, 5000);
    }

    #[test]
    fn test_IfStats_add_source_ip() {
        let mut stats = IfStats::new();
        stats.add_source_ip("192.168.1.1".to_string());
        assert_eq!(*stats.source_ips.get("192.168.1.1").unwrap(), 1);
    }

    #[test]
    fn test_IfStats_add_destination_ip() {
        let mut stats = IfStats::new();
        stats.add_destination_ip("192.168.1.2".to_string());
        assert_eq!(*stats.destination_ips.get("192.168.1.2").unwrap(), 1);
    }

    #[test]
    fn test_IfStats_add_request_type() {
        let mut stats = IfStats::new();
        stats.add_request_type("GET".to_string());
        assert_eq!(*stats.request_types.get("GET").unwrap(), 1);
    }

    #[test]
    fn test_IfStats_add_event() {
        let mut stats = IfStats::new();
        let event = PVEvent::new(
            "192.168.1.1".to_string(),
            "192.168.1.2".to_string(),
            "GET".to_string(),
        );
        stats.add_event(event.clone());
        assert_eq!(stats.events.len(), 1);
        assert_eq!(stats.events[0].source_ip, event.source_ip);
    }

    #[test]
    fn test_IfStats_memory_usage() {
        let mut stats = IfStats::new();
        let event = PVEvent::new(
            "192.168.1.1".to_string(),
            "192.168.1.2".to_string(),
            "GET".to_string(),
        );
        stats.add_event(event);
        let memory_usage = stats.get_memory_usage();
        assert!(memory_usage.ends_with("B"));
    }

    #[test]
    fn test_IfStats_set_buffer_size() {
        let mut stats = IfStats::new();
        stats.set_buffer_size(100000);
        assert_eq!(stats.buffer_size, 50000);
        stats.set_buffer_size(50);
        assert_eq!(stats.buffer_size, 100);
        stats.set_buffer_size(1000);
        assert_eq!(stats.buffer_size, 1000);
    }

    #[test]
    fn test_IfStats_resolve_ip() {
        let mut stats = IfStats::new();
        stats.enable_resolve();
        stats.add_source_ip("8.8.8.8".to_string());
        let resolved_ip = stats.get_resolved_ip("8.8.8.8".to_string());
        assert!(stats.resolved_ips.contains_key("8.8.8.8"));
        assert!(!resolved_ip.is_empty());
    }
}
