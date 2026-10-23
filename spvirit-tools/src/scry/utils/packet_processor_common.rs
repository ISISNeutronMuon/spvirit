use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};

use spvirit_codec::epics_decode::PvaHeader;

#[derive(Debug, Clone)]
pub struct ParsedPacket {
    pub source: String,
    pub destination: String,
    pub protocol: u8,
    pub src_port: u16,
    pub dst_port: u16,
    pub payload_offset: usize,
    /// Actual application payload length (excludes Ethernet padding)
    pub payload_length: usize,
}

pub fn hash_string(s: String) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}
pub fn try_create_capture(device: &str) -> Result<pcap::Capture<pcap::Active>, pcap::Error> {
    let cap = pcap::Capture::from_device(device)?
        .immediate_mode(true)
        .timeout(1000)
        .promisc(true)
        .open()?;

    // Explicitly request both inbound and outbound traffic.
    // On some Windows/Npcap configurations, outgoing packets may not
    // be captured unless direction is explicitly set to InOut.
    if let Err(e) = cap.direction(pcap::Direction::InOut) {
        eprintln!(
            "Warning: could not set capture direction to InOut on '{}': {} \
             (outgoing packets may not be captured)",
            device, e
        );
    }

    Ok(cap)
}

pub fn create_capture(device: &str) -> pcap::Capture<pcap::Active> {
    match try_create_capture(device) {
        Ok(capture) => capture,
        Err(e) => {
            eprintln!("Failed to open capture for '{}': {}", device, e);
            std::process::exit(1);
        }
    }
}

pub fn find_pva_message_ranges(payload: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut idx = 0usize;
    while idx + 8 <= payload.len() {
        if payload[idx] != 0xCA {
            idx += 1;
            continue;
        }
        let header = PvaHeader::new(&payload[idx..idx + 8]);
        if header.version != 2 || !header.flags.is_valid {
            idx += 1;
            continue;
        }
        let total_len = if header.flags.is_control {
            8usize
        } else {
            8usize + header.payload_length as usize
        };
        if idx + total_len > payload.len() {
            break;
        }
        ranges.push((idx, idx + total_len));
        idx += total_len;
    }
    ranges
}

fn parse_ipv4_header(
    data: &[u8],
    offset: usize,
) -> Option<(usize, u8, u16, [u8; 4], [u8; 4])> {
    if data.len() < offset + 20 {
        return None;
    }
    let version = data[offset] >> 4;
    if version != 4 {
        return None;
    }
    let ihl = (data[offset] & 0x0F) as usize * 4;
    if ihl < 20 || data.len() < offset + ihl {
        return None;
    }
    let total_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]);
    if total_len < ihl as u16 {
        return None;
    }
    let protocol = data[offset + 9];
    let source: [u8; 4] = data[offset + 12..offset + 16].try_into().ok()?;
    let destination: [u8; 4] = data[offset + 16..offset + 20].try_into().ok()?;
    Some((ihl, protocol, total_len, source, destination))
}

fn push_offset(candidates: &mut Vec<usize>, offset: usize) {
    if !candidates.contains(&offset) {
        candidates.push(offset);
    }
}

pub fn parse_packet(data: &[u8]) -> Option<ParsedPacket> {
    if data.len() < 20 {
        return None;
    }

    let mut candidates: Vec<usize> = Vec::new();
    if data.len() >= 14 {
        let ethertype = u16::from_be_bytes([data[12], data[13]]);
        match ethertype {
            0x0800 => push_offset(&mut candidates, 14), // IPv4
            0x8100 | 0x88A8 => {
                if data.len() >= 18 {
                    let inner = u16::from_be_bytes([data[16], data[17]]);
                    if inner == 0x0800 {
                        push_offset(&mut candidates, 18);
                    } else if inner == 0x8100 && data.len() >= 22 {
                        let inner2 = u16::from_be_bytes([data[20], data[21]]);
                        if inner2 == 0x0800 {
                            push_offset(&mut candidates, 22);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    push_offset(&mut candidates, 0);
    push_offset(&mut candidates, 4);
    push_offset(&mut candidates, 16);

    for ip_offset in candidates {
        let (ihl, protocol, total_len, src, dst) = match parse_ipv4_header(data, ip_offset) {
            Some(values) => values,
            None => continue,
        };

        if (ip_offset + total_len as usize) > data.len() {
            continue;
        }

        let l4_offset = ip_offset + ihl;
        if data.len() < l4_offset + 4 {
            continue;
        }

        match protocol {
            6 => {
                if data.len() < l4_offset + 20 {
                    continue;
                }
                let data_offset = ((data[l4_offset + 12] >> 4) as usize) * 4;
                if data_offset < 20 || data.len() < l4_offset + data_offset {
                    continue;
                }
                let src_port = u16::from_be_bytes([data[l4_offset], data[l4_offset + 1]]);
                let dst_port = u16::from_be_bytes([data[l4_offset + 2], data[l4_offset + 3]]);
                let payload_offset = l4_offset + data_offset;
                let ip_end = ip_offset + total_len as usize;
                let payload_length = if ip_end > payload_offset {
                    ip_end - payload_offset
                } else {
                    0
                };
                return Some(ParsedPacket {
                    source: format!("{}.{}.{}.{}", src[0], src[1], src[2], src[3]),
                    destination: format!("{}.{}.{}.{}", dst[0], dst[1], dst[2], dst[3]),
                    protocol,
                    src_port,
                    dst_port,
                    payload_offset,
                    payload_length,
                });
            }
            17 => {
                if data.len() < l4_offset + 8 {
                    continue;
                }
                let src_port = u16::from_be_bytes([data[l4_offset], data[l4_offset + 1]]);
                let dst_port = u16::from_be_bytes([data[l4_offset + 2], data[l4_offset + 3]]);
                let payload_offset = l4_offset + 8;
                let ip_end = ip_offset + total_len as usize;
                let payload_length = if ip_end > payload_offset {
                    ip_end - payload_offset
                } else {
                    0
                };
                return Some(ParsedPacket {
                    source: format!("{}.{}.{}.{}", src[0], src[1], src[2], src[3]),
                    destination: format!("{}.{}.{}.{}", dst[0], dst[1], dst[2], dst[3]),
                    protocol,
                    src_port,
                    dst_port,
                    payload_offset,
                    payload_length,
                });
            }
            _ => continue,
        }
    }

    None
}

// ---------------------------------------------------------------------------
// PVA Reassembler — TCP stream + segmented-message reassembly
// ---------------------------------------------------------------------------

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

impl SegmentedBuffer {
    fn new(header_bytes: [u8; 8], is_msb: bool) -> Self {
        Self {
            header_bytes,
            payload: Vec::new(),
            is_msb,
        }
    }

    fn append(&mut self, data: &[u8]) {
        self.payload.extend_from_slice(data);
    }

    fn assemble(self) -> Vec<u8> {
        let mut header = self.header_bytes;
        header[2] &= !0x30; // clear segmentation flags
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

/// Counters for dropped PVA segments during reassembly.
pub struct ReassemblyDropCounters {
    pub total: u64,
    pub short: u64,
    pub invalid: u64,
    pub partial: u64,
    pub missing_first: u64,
}

impl ReassemblyDropCounters {
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

fn push_reasm_log(log: &mut VecDeque<String>, entry: String) {
    log.push_back(entry);
    while log.len() > 40 {
        log.pop_front();
    }
}

fn record_reasm_drop(
    counters: &mut ReassemblyDropCounters,
    log: &mut VecDeque<String>,
    reason: &str,
    key: &StreamKey,
) {
    counters.total = counters.total.saturating_add(1);
    match reason {
        "short" => counters.short = counters.short.saturating_add(1),
        "invalid" => counters.invalid = counters.invalid.saturating_add(1),
        "partial" => counters.partial = counters.partial.saturating_add(1),
        "missing_first" => counters.missing_first = counters.missing_first.saturating_add(1),
        _ => {}
    }
    push_reasm_log(
        log,
        format!(
            "seg_drop {} {}:{} -> {}:{}",
            reason, key.source, key.src_port, key.destination, key.dst_port
        ),
    );
}

/// TCP stream + PVA segmented-message reassembler.
///
/// Accumulates TCP payloads per-stream, locates PVA message boundaries,
/// and reassembles multi-segment PVA messages.  Returns fully assembled
/// PVA messages ready for decoding.
pub struct PvaReassembler {
    stream_buffers: HashMap<StreamKey, Vec<u8>>,
    segmented_buffers: HashMap<StreamKey, SegmentedBuffer>,
    pva_confirmed: HashSet<StreamKey>,
    pva_rejected: HashSet<StreamKey>,
    pub seg_started: u64,
    pub seg_completed: u64,
    pub seg_counters: ReassemblyDropCounters,
    pub stream_dropped: u64,
    pub seg_log: VecDeque<String>,
    max_stream_buffer: usize,
    stream_trim_target: usize,
}

impl PvaReassembler {
    pub fn new() -> Self {
        Self {
            stream_buffers: HashMap::new(),
            segmented_buffers: HashMap::new(),
            pva_confirmed: HashSet::new(),
            pva_rejected: HashSet::new(),
            seg_started: 0,
            seg_completed: 0,
            seg_counters: ReassemblyDropCounters::new(),
            stream_dropped: 0,
            seg_log: VecDeque::new(),
            max_stream_buffer: 4 * 1024 * 1024,
            stream_trim_target: 2 * 1024 * 1024,
        }
    }

    /// Feed a parsed packet and its L4 payload through the reassembler.
    ///
    /// Returns a vec of fully-assembled PVA message byte buffers.
    /// TCP traffic is stream-reassembled; UDP messages are returned directly.
    pub fn process(&mut self, parsed: &ParsedPacket, payload: &[u8]) -> Vec<Vec<u8>> {
        let stream_key = StreamKey::new(
            &parsed.source,
            parsed.src_port,
            &parsed.destination,
            parsed.dst_port,
        );

        let mut ranges: Vec<(usize, usize)>;
        let is_tcp = parsed.protocol == 6;

        if is_tcp {
            // Skip streams already identified as non-PVA
            if self.pva_rejected.contains(&stream_key) {
                return vec![];
            }
            // For new streams, first byte must be PVA magic
            let is_new = !self.stream_buffers.contains_key(&stream_key);
            if is_new && (payload.is_empty() || payload[0] != 0xCA) {
                self.pva_rejected.insert(stream_key);
                return vec![];
            }
            // Accumulate payload into per-stream buffer
            let buffer = self.stream_buffers.entry(stream_key.clone()).or_default();
            buffer.extend_from_slice(payload);
            if buffer.len() > self.max_stream_buffer {
                let trim = buffer.len().saturating_sub(self.stream_trim_target);
                if trim > 0 {
                    buffer.drain(0..trim);
                }
                self.stream_dropped = self.stream_dropped.saturating_add(1);
            }
            ranges = find_pva_message_ranges(buffer);
            if let Some((first_start, _)) = ranges.first() {
                if *first_start > 0 {
                    buffer.drain(0..*first_start);
                    ranges = find_pva_message_ranges(buffer);
                }
            }
        } else if parsed.protocol == 17 {
            ranges = find_pva_message_ranges(payload);
        } else {
            return vec![];
        }

        if ranges.is_empty() {
            return vec![];
        }

        // TCP PVA confirmation on first valid message
        if is_tcp && !self.pva_confirmed.contains(&stream_key) {
            let buffer = self.stream_buffers.get(&stream_key).unwrap();
            let (start, end) = ranges[0];
            if start == 0 && end <= buffer.len() && buffer.len() >= 8 {
                let header = PvaHeader::new(&buffer[0..8]);
                if header.version == 2
                    && header.flags.is_valid
                    && (header.flags.is_control || header.command <= 22)
                {
                    self.pva_confirmed.insert(stream_key.clone());
                } else {
                    self.pva_rejected.insert(stream_key.clone());
                    self.stream_buffers.remove(&stream_key);
                    return vec![];
                }
            } else {
                return vec![];
            }
        }

        let mut messages = Vec::new();
        let mut last_tcp_end = 0usize;

        for (start, end) in &ranges {
            // Copy to owned data to avoid borrow conflicts with assemble_segment
            let slice = if is_tcp {
                let buffer = self.stream_buffers.get(&stream_key).unwrap();
                if *end > buffer.len() {
                    continue;
                }
                last_tcp_end = *end;
                buffer[*start..*end].to_vec()
            } else if *end <= payload.len() {
                payload[*start..*end].to_vec()
            } else {
                continue;
            };

            if let Some(assembled) = self.assemble_segment(&slice, &stream_key) {
                messages.push(assembled);
            }
        }

        // Drain consumed bytes from TCP stream buffer
        if is_tcp && last_tcp_end > 0 {
            if let Some(buffer) = self.stream_buffers.get_mut(&stream_key) {
                buffer.drain(0..last_tcp_end);
            }
        }

        messages
    }

    /// Reassemble PVA segmented messages (first/middle/last).
    /// Non-segmented messages are returned immediately.
    fn assemble_segment(&mut self, slice: &[u8], stream_key: &StreamKey) -> Option<Vec<u8>> {
        if slice.len() < 8 {
            record_reasm_drop(&mut self.seg_counters, &mut self.seg_log, "short", stream_key);
            return None;
        }
        let header = PvaHeader::new(slice);
        if header.version != 2 || !header.flags.is_valid {
            record_reasm_drop(
                &mut self.seg_counters,
                &mut self.seg_log,
                "invalid",
                stream_key,
            );
            return None;
        }
        if !header.flags.is_control && header.command > 22 {
            record_reasm_drop(
                &mut self.seg_counters,
                &mut self.seg_log,
                "invalid",
                stream_key,
            );
            return None;
        }

        // Non-segmented — return immediately
        if header.flags.is_segmented == 0 {
            return Some(slice.to_vec());
        }

        let payload_len = header.payload_length as usize;
        if slice.len() < 8 + payload_len {
            record_reasm_drop(
                &mut self.seg_counters,
                &mut self.seg_log,
                "partial",
                stream_key,
            );
            return None;
        }
        let seg_payload = &slice[8..8 + payload_len];

        if header.flags.is_first_segment {
            let header_bytes: [u8; 8] = slice[0..8].try_into().ok()?;
            let mut buffer = SegmentedBuffer::new(header_bytes, header.flags.is_msb);
            buffer.append(seg_payload);
            self.segmented_buffers.insert(stream_key.clone(), buffer);
            self.seg_started = self.seg_started.saturating_add(1);
            return None;
        }

        if header.flags.is_last_segment {
            let mut buffer = match self.segmented_buffers.remove(stream_key) {
                Some(b) => b,
                None => {
                    record_reasm_drop(
                        &mut self.seg_counters,
                        &mut self.seg_log,
                        "missing_first",
                        stream_key,
                    );
                    return None;
                }
            };
            buffer.append(seg_payload);
            let completed = buffer.assemble();
            self.seg_completed = self.seg_completed.saturating_add(1);
            return Some(completed);
        }

        // Middle segment
        match self.segmented_buffers.get_mut(stream_key) {
            Some(buffer) => {
                buffer.append(seg_payload);
            }
            None => {
                record_reasm_drop(
                    &mut self.seg_counters,
                    &mut self.seg_log,
                    "missing_first",
                    stream_key,
                );
            }
        }
        None
    }
}
