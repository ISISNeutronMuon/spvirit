use crate::scry::common::fingerprint::engine::FingerprintEngine;
use crate::scry::common::fingerprint::nt_probe;
use crate::scry::common::fingerprint::options::FingerprintOptions;
use crate::scry::common::fingerprint::types::{FingerprintKey, FingerprintStatus, Observation};
use crate::scry::common::interfaces::InterfaceSelection;
use crate::scry::common::stats::{IfStats, PVEvent, TrackedConnectionEntry};
use crate::scry::utils::packet_processor_common::{
    create_capture, find_pva_message_ranges, parse_packet, try_create_capture,
};
use spvirit_codec::epics_decode::{
    DecodeMode, PvaHeader, PvaPacket, PvaPacketCommand, command_name,
};
use crate::scry::spvirit_state::{ConnectionKey, PvaStateConfig, PvaStateTracker};

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct StreamKey {
    source: String,
    destination: String,
    src_port: u16,
    dst_port: u16,
}

impl StreamKey {
    fn new(source: &str, src_port: u16, destination: &str, dst_port: u16) -> Self {
        Self {
            source: source.to_string(),
            destination: destination.to_string(),
            src_port,
            dst_port,
        }
    }
}

struct SegmentedBuffer {
    header_bytes: [u8; 8],
    payload: Vec<u8>,
    is_msb: bool,
}

struct SegDropCounters {
    total: u64,
    short: u64,
    invalid: u64,
    partial: u64,
    missing_first: u64,
}

impl SegDropCounters {
    fn new() -> Self {
        Self {
            total: 0,
            short: 0,
            invalid: 0,
            partial: 0,
            missing_first: 0,
        }
    }
}

fn push_seg_log(log: &mut VecDeque<String>, entry: String) {
    log.push_back(entry);
    while log.len() > 40 {
        log.pop_front();
    }
}

fn record_seg_drop(
    counters: &mut SegDropCounters,
    log: &mut VecDeque<String>,
    reason: &str,
    stream_key: &StreamKey,
) {
    counters.total = counters.total.saturating_add(1);
    match reason {
        "short" => counters.short = counters.short.saturating_add(1),
        "invalid" => counters.invalid = counters.invalid.saturating_add(1),
        "partial" => counters.partial = counters.partial.saturating_add(1),
        "missing_first" => counters.missing_first = counters.missing_first.saturating_add(1),
        _ => {}
    }
    push_seg_log(
        log,
        format!(
            "seg_drop {} {}:{} -> {}:{}",
            reason,
            stream_key.source,
            stream_key.src_port,
            stream_key.destination,
            stream_key.dst_port
        ),
    );
}

impl SegmentedBuffer {
    fn new(header_bytes: [u8; 8], is_msb: bool) -> Self {
        Self {
            header_bytes,
            payload: Vec::new(),
            is_msb,
        }
    }

    fn append(&mut self, payload: &[u8]) {
        self.payload.extend_from_slice(payload);
    }

    fn assemble(self) -> Vec<u8> {
        let mut header = self.header_bytes;
        header[2] &= !0x30;
        let payload_len = self.payload.len() as u32;
        let len_bytes = if self.is_msb {
            payload_len.to_be_bytes()
        } else {
            payload_len.to_le_bytes()
        };
        header[4..8].copy_from_slice(&len_bytes);
        let mut out = Vec::with_capacity(8 + self.payload.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&self.payload);
        out
    }
}

enum PvaMessage<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8>),
}

impl<'a> PvaMessage<'a> {
    fn as_slice(&self) -> &[u8] {
        match self {
            PvaMessage::Borrowed(slice) => slice,
            PvaMessage::Owned(data) => data.as_slice(),
        }
    }
}

fn assemble_pva_message<'a>(
    slice: &'a [u8],
    stream_key: &StreamKey,
    buffers: &mut HashMap<StreamKey, SegmentedBuffer>,
    seg_started: &mut u64,
    seg_completed: &mut u64,
    seg_counters: &mut SegDropCounters,
    seg_log: &mut VecDeque<String>,
    saw_pva: &mut bool,
) -> Option<PvaMessage<'a>> {
    if slice.len() < 8 {
        record_seg_drop(seg_counters, seg_log, "short", stream_key);
        return None;
    }
    let header = PvaHeader::new(slice);
    if header.version != 2 || !header.flags.is_valid {
        record_seg_drop(seg_counters, seg_log, "invalid", stream_key);
        return None;
    }
    if !header.flags.is_control && header.command > 22 {
        record_seg_drop(seg_counters, seg_log, "invalid", stream_key);
        return None;
    }
    *saw_pva = true;
    if header.flags.is_segmented == 0 {
        return Some(PvaMessage::Borrowed(slice));
    }
    let payload_len = header.payload_length as usize;
    if slice.len() < 8 + payload_len {
        record_seg_drop(seg_counters, seg_log, "partial", stream_key);
        return None;
    }
    let payload = &slice[8..8 + payload_len];

    if header.flags.is_first_segment {
        let header_bytes: [u8; 8] = slice[0..8].try_into().ok()?;
        let mut buffer = SegmentedBuffer::new(header_bytes, header.flags.is_msb);
        buffer.append(payload);
        buffers.insert(stream_key.clone(), buffer);
        *seg_started = seg_started.saturating_add(1);
        return None;
    }

    if header.flags.is_last_segment {
        let mut buffer = match buffers.remove(stream_key) {
            Some(buffer) => buffer,
            None => {
                record_seg_drop(seg_counters, seg_log, "missing_first", stream_key);
                return None;
            }
        };
        buffer.append(payload);
        let completed = buffer.assemble();
        *seg_completed = seg_completed.saturating_add(1);
        return Some(PvaMessage::Owned(completed));
    }

    let buffer = match buffers.get_mut(stream_key) {
        Some(buffer) => buffer,
        None => {
            record_seg_drop(seg_counters, seg_log, "missing_first", stream_key);
            return None;
        }
    };
    buffer.append(payload);

    None
}

pub fn process_packets(
    _verbose: bool,
    show_payload: bool,
    device: &str,
    integration_period: u64,
    interface_statistics: &mut Arc<Mutex<IfStats>>,
    exclude_cmds: Vec<String>,
    pva_ttl_secs: u64,
    pva_max_channels: usize,
    pva_max_memory_mb: usize,
    tracked_only: bool,
    pva_max_update_rate: usize,
    interface_selection: Option<Arc<InterfaceSelection>>,
    fingerprint: FingerprintOptions,
) {
    debug!("Starting packet processing");

    // Initialize PVA state tracker for channel/operation tracking
    let pva_config = PvaStateConfig::new(pva_max_channels, pva_ttl_secs)
        .with_max_update_rate(pva_max_update_rate)
        .with_max_memory_bytes(pva_max_memory_mb * 1024 * 1024);
    let mut pva_state = PvaStateTracker::new(pva_config.clone());
    let mut fp_engine = FingerprintEngine::new(fingerprint.config.clone());
    let mut fp_detector =
        crate::scry::common::fingerprint::detect::Detector::new(fingerprint.detector.clone());
    let fp_enabled = fingerprint.enabled;
    debug!(
        "PVA state tracker initialized: max_channels={}, ttl={}s, max_memory={}MiB, tracked_only={}, max_update_rate={}",
        pva_max_channels, pva_ttl_secs, pva_max_memory_mb, tracked_only, pva_max_update_rate
    );

    let mut current_device = interface_selection
        .as_ref()
        .and_then(|selection| selection.current_name())
        .unwrap_or_else(|| device.to_string());
    let mut cap = create_capture(&current_device);

    let mut stream_buffers: HashMap<StreamKey, Vec<u8>> = HashMap::new();
    let mut segmented_buffers: HashMap<StreamKey, SegmentedBuffer> = HashMap::new();
    let mut pva_confirmed: HashSet<StreamKey> = HashSet::new();
    let mut pva_rejected: HashSet<StreamKey> = HashSet::new();
    let mut seg_started: u64 = 0;
    let mut seg_completed: u64 = 0;
    let mut seg_counters = SegDropCounters::new();
    let mut stream_dropped: u64 = 0;
    let mut seg_log: VecDeque<String> = VecDeque::new();
    const MAX_STREAM_BUFFER: usize = 4 * 1024 * 1024;
    const STREAM_TRIM_TARGET: usize = 2 * 1024 * 1024;

    let mut accumulator = 0u64;
    let mut packet_count = 0u64;
    let mut time = std::time::Instant::now();
    let mut last_snapshot = std::time::Instant::now();

    debug!("Starting packet processing loop");
    loop {
        if let Some(selection) = interface_selection.as_ref()
            && let Some(next_device) = selection.current_name()
                && next_device != current_device {
                    match try_create_capture(&next_device) {
                        Ok(new_cap) => {
                            debug!("Switching capture interface to {}", next_device);
                            current_device = next_device;
                            cap = new_cap;
                            stream_buffers.clear();
                            segmented_buffers.clear();
                            pva_confirmed.clear();
                            pva_rejected.clear();
                            seg_started = 0;
                            seg_completed = 0;
                            seg_counters = SegDropCounters::new();
                            stream_dropped = 0;
                            seg_log.clear();
                            accumulator = 0;
                            packet_count = 0;
                            time = std::time::Instant::now();
                            last_snapshot = std::time::Instant::now();
                            pva_state = PvaStateTracker::new(pva_config.clone());
                            // A fingerprint describes a source as seen from one
                            // vantage point (spec 6.3). Carrying half-filled
                            // windows across an interface switch would blend two
                            // vantage points under one interface label, so the
                            // engine is rebuilt alongside the state tracker.
                            fp_engine = FingerprintEngine::new(fingerprint.config.clone());
                            // The detector's priors describe sources as seen
                            // from the old vantage point, so comparing the
                            // next window against them would report the
                            // observer's move as a source change.
                            fp_detector = crate::scry::common::fingerprint::detect::Detector::new(
                                fingerprint.detector.clone(),
                            );
                            if let Ok(mut stats) = interface_statistics.lock() {
                                stats.clear();
                                stats.pva_seg_log.clear();
                                stats.timestamp = std::time::Instant::now();
                            }
                        }
                        Err(err) => {
                            // Triggered by the 'i'/'I' keys from inside the
                            // TUI, so this must never reach stdout.
                            error!(
                                "Failed to switch capture to '{}': {}",
                                next_device, err
                            );
                            if let Some(idx) = selection
                                .interfaces
                                .iter()
                                .position(|iface| iface.name == current_device)
                            {
                                selection.set_index(idx);
                            }
                        }
                    }
                }
        match cap.next_packet() {
            Ok(packet) => {
                // Capture timestamp for observable O4. Read before the packet
                // data is borrowed. tv_sec/tv_usec differ in width across
                // platforms, so cast both to i128.
                let observed_unix_nanos: i128 = (packet.header.ts.tv_sec as i128)
                    * 1_000_000_000
                    + (packet.header.ts.tv_usec as i128) * 1_000;
                let parsed = match parse_packet(packet.data) {
                    Some(parsed) => parsed,
                    None => continue,
                };

                if parsed.payload_length == 0 {
                    continue;
                }

                let payload = &packet.data[parsed.payload_offset
                    ..parsed.payload_offset + parsed.payload_length];
                let stream_key = StreamKey::new(
                    &parsed.source,
                    parsed.src_port,
                    &parsed.destination,
                    parsed.dst_port,
                );

                let mut handled_any = false;
                let mut counted = false;

                let mut ranges: Vec<(usize, usize)> = Vec::new();
                let mut is_tcp = false;
                if parsed.protocol == 6 {
                    is_tcp = true;

                    // Permanently skip streams already identified as non-PVA
                    if pva_rejected.contains(&stream_key) {
                        continue;
                    }

                    // For brand-new streams, check if the first byte is PVA magic
                    let is_new = !stream_buffers.contains_key(&stream_key);
                    if is_new && (payload.is_empty() || payload[0] != 0xCA) {
                        pva_rejected.insert(stream_key.clone());
                        continue;
                    }

                    let buffer = stream_buffers.entry(stream_key.clone()).or_default();
                    buffer.extend_from_slice(payload);
                    if buffer.len() > MAX_STREAM_BUFFER {
                        let trim = buffer.len().saturating_sub(STREAM_TRIM_TARGET);
                        if trim > 0 {
                            buffer.drain(0..trim);
                        }
                        stream_dropped = stream_dropped.saturating_add(1);
                    }
                    ranges = find_pva_message_ranges(buffer);
                    if let Some((first_start, _)) = ranges.first()
                        && *first_start > 0 {
                            buffer.drain(0..*first_start);
                            ranges = find_pva_message_ranges(buffer);
                        }
                } else if parsed.protocol == 17 {
                    ranges = find_pva_message_ranges(payload);
                }

                if ranges.is_empty() {
                    continue;
                }

                if is_tcp && !pva_confirmed.contains(&stream_key) {
                    let buffer = stream_buffers.get(&stream_key).unwrap();
                    let (start, end) = ranges[0];
                    if start == 0 && end <= buffer.len() && buffer.len() >= 8 {
                        let header = PvaHeader::new(&buffer[0..8]);
                        if header.version == 2
                            && header.flags.is_valid
                            && (header.flags.is_control || header.command <= 22)
                        {
                            pva_confirmed.insert(stream_key.clone());
                        } else {
                            // Failed confirmation — permanently reject this stream
                            pva_rejected.insert(stream_key.clone());
                            stream_buffers.remove(&stream_key);
                            continue;
                        }
                    } else {
                        continue;
                    }
                }

                let mut last_tcp_end = 0usize;

                for (start, end) in ranges {
                    let pva_message_slice = if is_tcp {
                        let buffer = stream_buffers.get(&stream_key).unwrap();
                        if end > buffer.len() {
                            continue;
                        }
                        last_tcp_end = end;
                        &buffer[start..end]
                    } else if end <= payload.len() {
                        &payload[start..end]
                    } else {
                        continue;
                    };

                    let pva_message = match assemble_pva_message(
                        pva_message_slice,
                        &stream_key,
                        &mut segmented_buffers,
                        &mut seg_started,
                        &mut seg_completed,
                        &mut seg_counters,
                        &mut seg_log,
                        &mut handled_any,
                    ) {
                        Some(message) => message,
                        None => continue,
                    };

                    let mut packet_pva = PvaPacket::new(pva_message.as_slice());
                    let is_server = packet_pva.header.flags.is_server;
                    let message_command = command_name(packet_pva.header.command);

                    if message_command == "Unknown" {
                        continue;
                    }
                    let cmd_upper = message_command.to_uppercase();
                    if exclude_cmds.contains(&cmd_upper) {
                        continue;
                    }

                    handled_any = true;
                    let version = packet_pva.header.version.to_string();
                    let flags = packet_pva.header.flags.raw.to_string();
                    let payload_size = packet_pva.header.payload_length;

                    let mut decoded_packet = packet_pva.decode_payload();

                    // Build canonical connection key (order-independent)
                    let conn_key = ConnectionKey::from_parts(
                        &parsed.source,
                        parsed.src_port,
                        &parsed.destination,
                        parsed.dst_port,
                    );

                        // Update PVA state based on decoded packet
                        if let Some(ref cmd) = decoded_packet {
                            // Count direction for all decoded PVA messages
                            pva_state.count_direction(is_server);

                            // SEARCH and SEARCH_RESPONSE don't require a connection key
                            // (they're UDP and only interact with the global search cache)
                            match cmd {
                                PvaPacketCommand::Search(search_payload) => {
                                    // Cache CID → PV name from SEARCH messages
                                    // as fallback for resolving PV names when
                                    // the client CREATE_CHANNEL request is missed.
                                    // Also retroactively resolves existing unknown channels.
                                    if !search_payload.pv_requests.is_empty() {
                                        let source_ip: Option<std::net::IpAddr> =
                                            parsed.source.parse().ok();
                                        pva_state.on_search(
                                            &search_payload.pv_requests,
                                            source_ip,
                                        );
                                    }
                                }
                                PvaPacketCommand::SearchResponse(sr_payload) => {
                                    // Resolve CIDs in the response back to PV names
                                    // using the search cache populated from SEARCH requests.
                                    // The destination of the response is the original client.
                                    if !sr_payload.cids.is_empty() {
                                        let client_ip: Option<std::net::IpAddr> =
                                            parsed.destination.parse().ok();
                                        let resolved =
                                            pva_state.resolve_search_cids(
                                                &sr_payload.cids,
                                                client_ip,
                                            );
                                        if !resolved.is_empty() {
                                            debug!(
                                                "SEARCH_RESPONSE resolved {} of {} CIDs: {}",
                                                resolved.len(),
                                                sr_payload.cids.len(),
                                                resolved
                                                    .iter()
                                                    .map(|(cid, pv)| format!("{}={}", cid, pv))
                                                    .collect::<Vec<_>>()
                                                    .join(", ")
                                            );
                                        }
                                    }
                                }
                                _ => {}
                            }

                            // Connection-scoped state updates (require conn_key)
                            if let Some(ref conn_key) = conn_key {
                                match cmd {
                                    PvaPacketCommand::CreateChannel(payload) => {
                                        if is_server {
                                            // Server response: register SID mapping
                                            pva_state.on_create_channel_response(
                                                conn_key,
                                                payload.cid,
                                                payload.sid,
                                            );
                                            debug!(
                                                "CREATE_CHANNEL response: cid={}, sid={}",
                                                payload.cid, payload.sid
                                            );
                                        } else {
                                            // Client request: register CID -> PV name for each channel
                                            for (cid, pv_name) in &payload.channels {
                                                pva_state.on_create_channel_request(
                                                    conn_key,
                                                    *cid,
                                                    pv_name.clone(),
                                                );
                                            }
                                            debug!(
                                                "CREATE_CHANNEL request: {} channel(s)",
                                                payload.channels.len()
                                            );
                                        }
                                    }
                                    PvaPacketCommand::DestroyChannel(payload) => {
                                        pva_state.on_destroy_channel(
                                            conn_key,
                                            payload.cid,
                                            payload.sid,
                                        );
                                        debug!(
                                            "DESTROY_CHANNEL: cid={}, sid={}",
                                            payload.cid, payload.sid
                                        );
                                    }
                                    // Track MONITOR/GET/PUT operations
                                    PvaPacketCommand::Op(op_payload) => {
                                        if !is_server && (op_payload.subcmd & 0x08) != 0 {
                                            // Client INIT request - register IOID -> SID mapping
                                            pva_state.on_op_init_request(
                                                conn_key,
                                                op_payload.sid_or_cid,
                                                op_payload.ioid,
                                                op_payload.command,
                                            );
                                            debug!(
                                                "Op INIT request: sid={}, ioid={}",
                                                op_payload.sid_or_cid, op_payload.ioid
                                            );
                                        } else if is_server && (op_payload.subcmd & 0x08) != 0 {
                                            // Server INIT response - store introspection
                                            if let Some(ref intro) = op_payload.introspection {
                                                pva_state.on_op_init_response(
                                                    conn_key,
                                                    op_payload.ioid,
                                                    Some(intro.clone()),
                                                );
                                                debug!(
                                                    "Op INIT response: ioid={}, fields={}",
                                                    op_payload.ioid,
                                                    intro.fields.len()
                                                );
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }

                        let mut if_stats = interface_statistics.lock().unwrap();
                        if_stats.add_source_ip(parsed.source.clone());
                        if_stats.add_destination_ip(parsed.destination.clone());
                        if_stats.add_request_type(message_command.to_string());

                        // let context = hash_string(
                        //     packet.data[offset..]
                        //         .to_vec()
                        //         .iter()
                        //         .map(|&i| i.to_string())
                        //         .collect::<String>(),
                        // );

                        let context_string: String;
                        let mut is_tracked = false;

                        // For Op packets, try to decode values using stored field description
                        if let Some(ref mut command) = decoded_packet
                            && let PvaPacketCommand::Op(op_payload) = command
                                && let Some(ref ck) = conn_key {
                                    // Touch connection/op/channel activity on any op message
                                    // Pass command so mid-stream joins auto-create placeholder ops
                                    pva_state.on_op_activity(
                                        ck,
                                        op_payload.sid_or_cid,
                                        op_payload.ioid,
                                        op_payload.command,
                                    );

                                    // Try to decode value using stored field description
                                    if op_payload.decoded_value.is_none()
                                        && !op_payload.body.is_empty()
                                    {
                                        // Server data updates (MONITOR/GET response with data)
                                        // OR client PUT requests (with data to write)
                                        // subcmd meanings:
                                        //   0x00 = data update (MONITOR/GET response or PUT request)
                                        //   0x08 = INIT (setup, has introspection)
                                        //   0x10 = DESTROY
                                        //   0x40 = pipeline data
                                        let should_decode = if is_server {
                                            // Server: decode data responses (subcmd 0x00 or 0x40 for pipeline)
                                            op_payload.subcmd == 0x00
                                                || (op_payload.subcmd & 0x40) != 0
                                        } else {
                                            // Client: decode PUT data (command=11 or 12, subcmd=0x00)
                                            (op_payload.command == 11 || op_payload.command == 12)
                                                && (op_payload.subcmd == 0x00
                                                    || (op_payload.subcmd & 0x40) != 0)
                                        };

                                        debug!("Op decode check: cmd={}, subcmd=0x{:02x}, is_server={}, should_decode={}, body_len={}",
                                               op_payload.command, op_payload.subcmd, is_server, should_decode, op_payload.body.len());

                                        if should_decode {
                                            if let Some(op_state) =
                                                pva_state.get_operation(ck, op_payload.ioid)
                                            {
                                                if let Some(ref field_desc) = op_state.field_desc {
                                                    debug!(
                                                        "Decoding with field_desc: {} fields",
                                                        field_desc.fields.len()
                                                    );
                                                    // Mid-stream capture: the peer's bitset layout is
                                                    // unknown, so try every layout (Lenient). A failed
                                                    // decode leaves decoded_value as None, as before.
                                                    let _ = op_payload.decode_with_field_desc(
                                                        field_desc,
                                                        false, // Little endian
                                                        DecodeMode::Lenient,
                                                    );
                                                    debug!(
                                                        "After decode: decoded_value={:?}",
                                                        op_payload.decoded_value.is_some()
                                                    );
                                                } else {
                                                    debug!(
                                                        "No field_desc available for ioid={}",
                                                        op_payload.ioid
                                                    );
                                                }
                                            } else {
                                                debug!("No op_state for ioid={}", op_payload.ioid);
                                            }
                                        }
                                    }
                                }

                        // Extract structured fields for normalized Loki output
                        let mut event_pv_name: Option<String> = None;
                        let mut event_ioid: Option<u32> = None;
                        let mut event_sid: Option<u32> = None;
                        let mut event_cid: Option<u32> = None;
                        let mut event_value_text: Option<String> = None;
                        let mut event_mid_stream = false;

                        if let Some(ref command) = decoded_packet {
                            // Try to resolve PV name for Op commands (GET/PUT/MONITOR etc.)
                            let pv_context = match command {
                                PvaPacketCommand::Op(op_payload) => {
                                    event_ioid = Some(op_payload.ioid);
                                    event_sid = Some(op_payload.sid_or_cid);

                                    // Extract decoded value text
                                    if let Some(ref decoded) = op_payload.decoded_value {
                                        let formatted = format!("{}", decoded);
                                        if !formatted.is_empty() && formatted != "{}" {
                                            event_value_text = Some(formatted);
                                        }
                                    }

                                    // Use the already-built canonical connection key
                                    if let Some(ref ck) = conn_key {
                                        // Check mid-stream status
                                        event_mid_stream = pva_state.is_connection_mid_stream(ck);

                                        // resolve_pv_name tries IOID first (for server responses),
                                        // then falls back to SID (for client requests)
                                        let pv_name = pva_state.resolve_pv_name(
                                            ck,
                                            op_payload.sid_or_cid,
                                            op_payload.ioid,
                                        );
                                        if let Some(pv) = pv_name {
                                            is_tracked = true;
                                            event_pv_name = Some(pv.clone());
                                            format!(" pv={}", pv)
                                        } else {
                                            String::new()
                                        }
                                    } else {
                                        String::new()
                                    }
                                }
                                // CREATE_CHANNEL and DESTROY_CHANNEL are always "tracked" since they manage state
                                PvaPacketCommand::CreateChannel(payload) => {
                                    is_tracked = true;
                                    event_cid = Some(payload.cid);
                                    event_sid = Some(payload.sid);
                                    // For client requests, extract PV names
                                    if !payload.channels.is_empty() {
                                        let pv_names: Vec<&str> = payload.channels.iter().map(|(_, n)| n.as_str()).collect();
                                        event_pv_name = Some(pv_names.join(","));
                                        // Use first channel's CID for the event
                                        event_cid = Some(payload.channels[0].0);
                                    }
                                    String::new()
                                }
                                PvaPacketCommand::DestroyChannel(payload) => {
                                    is_tracked = true;
                                    event_cid = Some(payload.cid);
                                    event_sid = Some(payload.sid);
                                    String::new()
                                }
                                PvaPacketCommand::SearchResponse(sr_payload) => {
                                    // Resolve CIDs to PV names for display
                                    if !sr_payload.cids.is_empty() {
                                        let client_ip: Option<std::net::IpAddr> =
                                            parsed.destination.parse().ok();
                                        let resolved =
                                            pva_state.resolve_search_cids(
                                                &sr_payload.cids,
                                                client_ip,
                                            );
                                        if !resolved.is_empty() {
                                            is_tracked = true;
                                            let pv_names: Vec<&str> =
                                                resolved.iter().map(|(_, n)| n.as_str()).collect();
                                            event_pv_name = Some(pv_names.join(","));
                                            event_cid = Some(sr_payload.cids[0]);
                                            format!(
                                                " pvs=[{}]",
                                                pv_names.join(",")
                                            )
                                        } else {
                                            String::new()
                                        }
                                    } else {
                                        String::new()
                                    }
                                }
                                PvaPacketCommand::Search(search_payload) => {
                                    // SEARCH requests are tracked when they contain PV names
                                    if !search_payload.pv_names.is_empty() {
                                        is_tracked = true;
                                        event_pv_name =
                                            Some(search_payload.pv_names.join(","));
                                    }
                                    String::new()
                                }
                                _ => String::new(),
                            };
                            context_string = format!("{}{}", command, pv_context);
                        } else {
                            context_string = "Unknown".to_string();
                        }

                        if let Some(ref ck) = conn_key {
                            let (sid, ioid) = match decoded_packet {
                                Some(PvaPacketCommand::Op(ref op_payload)) => {
                                    (op_payload.sid_or_cid, op_payload.ioid)
                                }
                                _ => (0, 0),
                            };
                            pva_state.on_message(
                                ck,
                                sid,
                                ioid,
                                message_command,
                                context_string.clone(),
                                is_server,
                            );
                        }

                        // Skip this packet if tracked_only is enabled and we don't have PV info
                        if tracked_only && !is_tracked {
                            // Still count the packet for stats but don't display or store it
                            if !counted {
                                accumulator += packet.data.len() as u64;
                                packet_count += 1;
                                counted = true;
                            }
                            continue;
                        }

                        let source_ip = parsed.source.clone();
                        let destination_ip = parsed.destination.clone();
                        let request_type = message_command.to_string();
                        let resolved_source_ip = if if_stats.resolved_ips.contains_key(&parsed.source) {
                            if_stats.resolved_ips.get(&parsed.source).unwrap().clone()
                        } else {
                            "Unknown".to_string()
                        };
                        let resolved_destination_ip =
                            if if_stats.resolved_ips.contains_key(&parsed.destination) {
                                if_stats
                                    .resolved_ips
                                    .get(&parsed.destination)
                                    .unwrap()
                                    .clone()
                            } else {
                                "Unknown".to_string()
                            };

                        if show_payload {
                            if let Some(ref _command) = decoded_packet {
                                info!(
                                    "Packet: {} \t -> \t {} Protocol: {} Version: {} Flags: {} Command: {} Payload size: {}\nDecoded: {}",
                                    parsed.source,
                                    parsed.destination,
                                    parsed.protocol,
                                    version,
                                    flags,
                                    message_command,
                                    payload_size,
                                    context_string
                                );
                            } else {
                                info!(
                                    "Packet: {} \t -> \t {} Protocol: {} Version: {} Flags: {} Command: {} Payload size: {} (no decoded payload)",
                                    parsed.source,
                                    parsed.destination,
                                    parsed.protocol,
                                    version,
                                    flags,
                                    message_command,
                                    payload_size
                                );
                            }
                        }

                        let mut event = PVEvent::new_resolved(
                            source_ip,
                            destination_ip,
                            request_type,
                            resolved_source_ip,
                            resolved_destination_ip,
                            context_string,
                        );
                        event.pv_name = event_pv_name;
                        event.ioid = event_ioid;
                        event.sid = event_sid;
                        event.cid = event_cid;
                        event.value_text = event_value_text;
                        event.mid_stream = event_mid_stream;

                        // Observable feed: only server-sourced MONITOR data
                        // updates. GET responses are excluded on purpose --
                        // a GET is client-driven, so its cadence measures the
                        // client's polling rather than the source's scan rate
                        // and would corrupt O3. Do not "fix" this by widening
                        // the command match.
                        if fp_enabled
                            && let Some(PvaPacketCommand::Op(ref op_payload)) = decoded_packet {
                                // subcmd 0x00 = data update, 0x40 = pipeline
                                // data. INIT (0x08) and DESTROY (0x10) carry
                                // no sample and are never admitted.
                                let is_monitor_data = is_server
                                    && op_payload.command == 13
                                    && (op_payload.subcmd == 0x00
                                        || (op_payload.subcmd & 0x40) != 0);
                                if is_monitor_data {
                                    if let (Some(pv), Some(value)) = (
                                        event.pv_name.as_ref(),
                                        op_payload.decoded_value.as_ref(),
                                    ) {
                                        if let Some(obs) = nt_probe::probe(value) {
                                            // The struct id comes from the
                                            // introspection stored at INIT, not
                                            // from this message: the codec fills
                                            // `op_payload.introspection` only on
                                            // an INIT response (subcmd 0x08),
                                            // which this predicate excludes, so
                                            // reading it here would leave the
                                            // field systemically absent.
                                            let struct_id = conn_key
                                                .as_ref()
                                                .and_then(|ck| {
                                                    pva_state.get_operation(ck, op_payload.ioid)
                                                })
                                                .and_then(|op| op.field_desc.as_ref())
                                                .and_then(|desc| desc.struct_id.clone());
                                            match format!(
                                                "{}:{}",
                                                parsed.source, parsed.src_port
                                            )
                                            .parse()
                                            {
                                                Ok(server_addr) => fp_engine.observe(
                                                    FingerprintKey::new(pv.clone(), server_addr),
                                                    Observation {
                                                        stamp_seconds: obs.stamp_seconds,
                                                        stamp_nanos: obs.stamp_nanos,
                                                        observed_unix_nanos,
                                                        value: obs.scalar_value,
                                                        has_nan_limits: obs.has_nan_limits,
                                                        struct_id,
                                                    },
                                                ),
                                                Err(err) => {
                                                    // Unreachable while
                                                    // `parse_packet` accepts only
                                                    // ethertype 0x0800, but an
                                                    // IPv6 literal would need
                                                    // bracketing; surface the loss
                                                    // rather than dropping the
                                                    // update in silence.
                                                    fp_engine.note_unprobeable_update();
                                                    debug!(
                                                        "Fingerprint: unparseable server address {}:{} ({}), update skipped",
                                                        parsed.source, parsed.src_port, err
                                                    );
                                                }
                                            }
                                        } else {
                                            // Spec 6.3: not every monitor
                                            // update carries a timestamped
                                            // scalar -- NTNDArray, NTTable and
                                            // plain structures never do. The
                                            // engine cannot use them, but they
                                            // are still traffic it did not see,
                                            // so the discard is counted rather
                                            // than left out of coverage
                                            // entirely.
                                            fp_engine.note_unprobeable_update();
                                        }
                                    } else {
                                        // decoded_value is None when no
                                        // field_desc exists for this
                                        // operation -- the observer joined
                                        // mid-stream and missed the MONITOR
                                        // INIT that carried the
                                        // introspection. That is traffic the
                                        // fingerprinter is structurally
                                        // unable to decode, so it is counted
                                        // here rather than silently dropped
                                        // (spec 6.3).
                                        fp_engine.note_unprobeable_update();
                                    }
                                }
                            }
                        if_stats.add_event(event);

                        if last_snapshot.elapsed().as_secs() >= 1 {
                            pva_state.cleanup_expired();

                            let mut summaries: Vec<TrackedConnectionEntry> = Vec::new();
                            for snap in pva_state.connection_snapshots() {
                                let endpoint_a = snap.addr_a.to_string();
                                let endpoint_b = snap.addr_b.to_string();
                                let id = format!("conn:{}<->{}", endpoint_a, endpoint_b);
                                let pv_summary = if snap.pv_names.is_empty() {
                                    None
                                } else {
                                    Some(snap.pv_names.join(","))
                                };
                                summaries.push(TrackedConnectionEntry {
                                    id,
                                    endpoint_a,
                                    endpoint_b,
                                    cid: None,
                                    sid: None,
                                    pv_name: pv_summary,
                                    channel_count: snap.channel_count,
                                    operation_count: snap.operation_count,
                                    updates_per_sec: snap.updates_per_sec,
                                    last_seen_secs: snap.last_seen.as_secs(),
                                    mid_stream: snap.mid_stream,
                                    is_beacon: snap.is_beacon,
                                    is_broadcast: snap.is_broadcast,
                                    recent_messages: snap.recent_messages,
                                });
                            }
                            summaries.sort_by(|a, b| a.id.cmp(&b.id));

                            let mut details: Vec<TrackedConnectionEntry> = Vec::new();
                            for snap in pva_state.channel_snapshots() {
                                let endpoint_a = snap.addr_a.to_string();
                                let endpoint_b = snap.addr_b.to_string();
                                let sid_text = snap
                                    .sid
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| "-".to_string());
                                let id = format!(
                                    "chan:{}<->{}|cid:{}|sid:{}|pv:{}",
                                    endpoint_a, endpoint_b, snap.cid, sid_text, snap.pv_name
                                );
                                details.push(TrackedConnectionEntry {
                                    id,
                                    endpoint_a,
                                    endpoint_b,
                                    cid: Some(snap.cid),
                                    sid: snap.sid,
                                    pv_name: Some(snap.pv_name),
                                    channel_count: 1,
                                    operation_count: 0,
                                    updates_per_sec: snap.updates_per_sec,
                                    last_seen_secs: snap.last_seen.as_secs(),
                                    mid_stream: snap.mid_stream,
                                    is_beacon: snap.is_beacon,
                                    is_broadcast: snap.is_broadcast,
                                    recent_messages: snap.recent_messages,
                                });
                            }
                            details.sort_by(|a, b| a.id.cmp(&b.id));

                            if_stats.tracked_connections_summary = summaries;
                            if_stats.tracked_connections_detail = details;

                            // Harvest on the capture clock rather than the wall
                            // clock, so a replayed or delayed capture windows
                            // correctly.
                            if fp_enabled {
                                // Sampled before the harvest: closing a window
                                // clears the per-window deferred/rotated-out
                                // sets, so reading coverage afterwards would
                                // report zero loss on exactly the tick whose
                                // coverage describes that window (spec 6.3).
                                // `current_device`, not `device`: the TUI can
                                // switch interfaces mid-run and the label must
                                // name the one actually being watched.
                                let coverage = fp_engine.coverage(&current_device);

                                // Windows that closed below `min_samples` are
                                // kept, not discarded: `harvest` has already
                                // removed the channel, so a later `snapshot`
                                // cannot show it and the loss would be
                                // indistinguishable from "never observed"
                                // (spec 6.1). Rotation is not advanced here --
                                // `harvest` advances the slot itself when it
                                // closes a window.
                                let mut statuses: Vec<FingerprintStatus> = Vec::new();
                                let mut harvested = Vec::new();
                                for status in fp_engine.harvest(observed_unix_nanos) {
                                    if let Some(fp) = status.ready().cloned() {
                                        harvested.push(fp);
                                    } else {
                                        statuses.push(status);
                                    }
                                }
                                if !harvested.is_empty() {
                                    // D2 and D4 are batch rules: they need the
                                    // whole harvest at once, so the detector is
                                    // fed the closed windows together rather
                                    // than one at a time.
                                    let now = std::time::UNIX_EPOCH
                                        + std::time::Duration::from_nanos(
                                            observed_unix_nanos.max(0) as u64,
                                        );
                                    if_stats
                                        .fingerprint_events
                                        .extend(fp_detector.ingest(&harvested, now));
                                    // Keep only the newest fingerprint per
                                    // channel: the collector drains this each
                                    // export, but the TUI never does, so
                                    // unbounded growth is a real leak here.
                                    for fp in harvested {
                                        if_stats
                                            .fingerprints
                                            .retain(|e| e.key() != fp.key());
                                        if_stats.fingerprints.push(fp);
                                    }
                                    // Detection events are a log, so they are
                                    // capped by age rather than deduplicated.
                                    // The cap must never look like a complete
                                    // log: every event it discards is counted
                                    // in `fingerprint_events_dropped` so the
                                    // Events view can say so (spec 6.1).
                                    const MAX_FP_EVENTS: usize = 500;
                                    if_stats.cap_fingerprint_events(MAX_FP_EVENTS);
                                }
                                if_stats.fingerprint_detect_counts = *fp_detector.counts();
                                statuses.extend(fp_engine.snapshot(observed_unix_nanos));
                                // Each half is key-sorted, but their
                                // concatenation is not, so the combined view
                                // would jump between ticks without this.
                                statuses.sort_by_key(|a| a.key());
                                if_stats.fingerprint_statuses = statuses;
                                if_stats.fingerprint_coverage = coverage;
                            }
                            last_snapshot = std::time::Instant::now();
                        }
                    }

                if is_tcp && last_tcp_end > 0
                    && let Some(buffer) = stream_buffers.get_mut(&stream_key) {
                        buffer.drain(0..last_tcp_end);
                    }

                if handled_any && !counted {
                    accumulator += packet.data.len() as u64;
                    packet_count += 1;
                }

                if time.elapsed().as_secs() > integration_period {
                    let rate = accumulator as f64 / time.elapsed().as_secs() as f64;
                    let packet_rate = packet_count as f64 / time.elapsed().as_secs() as f64;

                    let mut if_stats = interface_statistics.lock().unwrap();
                    if_stats.data_rate = rate;
                    if_stats.packet_rate = packet_rate;

                    // Update pcap capture stats
                    if let Ok(pcap_stats) = cap.stats() {
                        if_stats.pcap_received = pcap_stats.received as u64;
                        if_stats.pcap_dropped = pcap_stats.dropped as u64;
                        if_stats.pcap_if_dropped = pcap_stats.if_dropped as u64;
                    }

                    // Update PVA state tracker stats
                    if_stats.pva_stats = pva_state.stats.clone();
                    if_stats.pva_total_channels = pva_state.active_channel_count();
                    if_stats.pva_total_connections = pva_state.active_connection_count();
                    if_stats.pva_seg_started = seg_started;
                    if_stats.pva_seg_completed = seg_completed;
                    if_stats.pva_seg_dropped = seg_counters.total;
                    if_stats.pva_seg_drop_short = seg_counters.short;
                    if_stats.pva_seg_drop_invalid = seg_counters.invalid;
                    if_stats.pva_seg_drop_partial = seg_counters.partial;
                    if_stats.pva_seg_drop_missing_first = seg_counters.missing_first;
                    if_stats.pva_stream_dropped = stream_dropped;
                    if_stats.pva_seg_log = seg_log.iter().cloned().collect();

                    accumulator = 0;
                    packet_count = 0;
                    time = std::time::Instant::now();
                }
            }
            Err(pcap::Error::TimeoutExpired) => {}
            Err(_e) => {
                // eprintln!("Error capturing packet: {}", e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}
