// Refer to https://github.com/mdavidsaver/cashark/blob/master/pva.lua

// Lookup table for PVA commands
// -- application messages

use hex;
use std::fmt;
use tracing::debug;

use crate::error::DecodeResult;
use crate::spvd_decode::{DecodedValue, PvdDecoder, StructureDesc, format_compact_value};
use crate::spvirit_encode::format_pva_address;

/// Single source of truth for PVA application command codes.
///
/// Index == command code.  Any code beyond the table returns `"Unknown"`.
const PVA_COMMAND_NAMES: &[&str] = &[
    "BEACON",                // 0
    "CONNECTION_VALIDATION", // 1
    "ECHO",                  // 2
    "SEARCH",                // 3
    "SEARCH_RESPONSE",       // 4
    "AUTHNZ",                // 5
    "ACL_CHANGE",            // 6
    "CREATE_CHANNEL",        // 7
    "DESTROY_CHANNEL",       // 8
    "CONNECTION_VALIDATED",  // 9
    "GET",                   // 10
    "PUT",                   // 11
    "PUT_GET",               // 12
    "MONITOR",               // 13
    "ARRAY",                 // 14
    "DESTROY_REQUEST",       // 15
    "PROCESS",               // 16
    "GET_FIELD",             // 17
    "MESSAGE",               // 18
    "MULTIPLE_DATA",         // 19
    "RPC",                   // 20
    "CANCEL_REQUEST",        // 21
    "ORIGIN_TAG",            // 22
];

/// Look up a PVA command name by its numeric code.
pub fn command_name(code: u8) -> &'static str {
    PVA_COMMAND_NAMES
        .get(code as usize)
        .copied()
        .unwrap_or("Unknown")
}

/// Look up a PVA command code by its name.  Returns 255 for unknown names.
pub fn command_to_integer(command: &str) -> u8 {
    PVA_COMMAND_NAMES
        .iter()
        .position(|&name| name == command)
        .map(|i| i as u8)
        .unwrap_or(255)
}

/// Convenience wrapper that matches the pre-existing `PvaCommands` API.
/// Prefer calling [`command_name`] directly for new code.
#[derive(Debug)]
pub struct PvaCommands;

impl PvaCommands {
    pub fn new() -> Self {
        Self
    }

    pub fn get_command(&self, code: u8) -> &'static str {
        command_name(code)
    }
}
#[derive(Debug)]
pub struct PvaControlFlags {
    pub raw: u8,
    // bits 0 is specifies application or control message (0 or 1 resprectively)
    // bits 1,2,3, must always be zero
    // bits 5 and 4 specify if the message is segmented 00 = not segmented, 01 = first segment, 10 = last segment, 11 = in-the-middle segment
    // bit 6 specifies the direction of the message (0 = client, 1 = server)
    // bit 7 specifies the byte order (0 = LSB, 1 = MSB)
    pub is_application: bool,
    pub is_control: bool,
    pub is_segmented: u8,
    pub is_first_segment: bool,
    pub is_last_segment: bool,
    pub is_middle_segment: bool,
    pub is_client: bool,
    pub is_server: bool,
    pub is_lsb: bool,
    pub is_msb: bool,
    pub is_valid: bool,
}

impl PvaControlFlags {
    pub fn new(raw: u8) -> Self {
        let is_application = (raw & 0x01) == 0; // Bit 0: 0 for application, 1 for control
        let is_control = (raw & 0x01) != 0; // Bit 0: 1 for control
        let is_segmented = (raw & 0x30) >> 4; // Bits 5 and 4
        let is_first_segment = is_segmented == 0x01; // 01
        let is_last_segment = is_segmented == 0x02; // 10
        let is_middle_segment = is_segmented == 0x03; // 11
        let is_client = (raw & 0x40) == 0; // Bit 6: 0 for client, 1 for server
        let is_server = (raw & 0x40) != 0; // Bit 6: 1 for server
        let is_lsb = (raw & 0x80) == 0; // Bit 7: 0 for LSB, 1 for MSB
        let is_msb = (raw & 0x80) != 0; // Bit 7: 1 for MSB
        let is_valid = (raw & 0x0E) == 0; // Bits 1,2,3 must be zero

        Self {
            raw,
            is_application,
            is_control,
            is_segmented,
            is_first_segment,
            is_last_segment,
            is_middle_segment,
            is_client,
            is_server,
            is_lsb,
            is_msb,
            is_valid,
        }
    }
    fn is_valid(&self) -> bool {
        self.is_valid
    }
}
#[derive(Debug)]
pub struct PvaHeader {
    pub magic: u8,
    pub version: u8,
    pub flags: PvaControlFlags,
    pub command: u8,
    pub payload_length: u32,
}

impl PvaHeader {
    pub fn new(raw: &[u8]) -> Self {
        Self::try_new(raw).expect("PVA header requires at least 8 bytes")
    }

    pub fn try_new(raw: &[u8]) -> Option<Self> {
        if raw.len() < 8 {
            return None;
        }
        let magic = raw[0];
        let version = raw[1];
        let flags = PvaControlFlags::new(raw[2]);
        let command: u8 = raw[3];
        let payload_length_bytes: [u8; 4] = raw[4..8]
            .try_into()
            .expect("Slice for payload_length has incorrect length");
        let payload_length = if flags.is_msb {
            u32::from_be_bytes(payload_length_bytes)
        } else {
            u32::from_le_bytes(payload_length_bytes)
        };

        Some(Self {
            magic,
            version,
            flags,
            command,
            payload_length,
        })
    }
    pub fn is_valid(&self) -> bool {
        self.magic == 0xCA && self.flags.is_valid()
    }
}

#[derive(Debug)]
pub enum PvaPacketCommand {
    Control(PvaControlPayload),
    Search(PvaSearchPayload),
    SearchResponse(PvaSearchResponsePayload),
    Beacon(PvaBeaconPayload),
    ConnectionValidation(PvaConnectionValidationPayload),
    ConnectionValidated(PvaConnectionValidatedPayload),
    AuthNZ(PvaAuthNzPayload),
    AclChange(PvaAclChangePayload),
    Op(PvaOpPayload),
    CreateChannel(PvaCreateChannelPayload),
    DestroyChannel(PvaDestroyChannelPayload),
    GetField(PvaGetFieldPayload),
    Message(PvaMessagePayload),
    MultipleData(PvaMultipleDataPayload),
    CancelRequest(PvaCancelRequestPayload),
    DestroyRequest(PvaDestroyRequestPayload),
    OriginTag(PvaOriginTagPayload),
    Echo(Vec<u8>),
    Unknown(PvaUnknownPayload),
}
#[derive(Debug)]
pub struct PvaPacket {
    pub header: PvaHeader,
    pub payload: Vec<u8>,
}

impl PvaPacket {
    pub fn new(raw: &[u8]) -> Self {
        let header = PvaHeader::new(raw);
        let payload = raw.to_vec();
        Self { header, payload }
    }
    /// Fallible constructor for untrusted input (e.g. raw UDP datagrams).
    ///
    /// Returns `None` when `raw` is shorter than the 8-byte PVA header,
    /// instead of panicking like [`PvaPacket::new`]. Use this on any
    /// network-ingress path where the length is attacker-controlled.
    pub fn try_new(raw: &[u8]) -> Option<PvaPacket> {
        let header = PvaHeader::try_new(raw)?;
        let payload = raw.to_vec();
        Some(Self { header, payload })
    }
    pub fn decode_payload(&mut self) -> Option<PvaPacketCommand> {
        let pva_header_size = 8;
        if self.payload.len() < pva_header_size {
            debug!("Packet too short to contain a PVA payload beyond the header.");
            return None;
        }

        let expected_total_len = if self.header.flags.is_control {
            pva_header_size
        } else {
            pva_header_size + self.header.payload_length as usize
        };
        if self.payload.len() < expected_total_len {
            debug!(
                "Packet data length {} is less than expected total length {} (header {} + payload_length {})",
                self.payload.len(),
                expected_total_len,
                pva_header_size,
                self.header.payload_length
            );
            return None;
        }

        let command_payload_slice = &self.payload[pva_header_size..expected_total_len];

        if self.header.flags.is_control {
            return Some(PvaPacketCommand::Control(PvaControlPayload::new(
                self.header.command,
                self.header.payload_length,
            )));
        }

        let decoded = match self.header.command {
            0 => PvaBeaconPayload::new(command_payload_slice, self.header.flags.is_msb)
                .map(PvaPacketCommand::Beacon),
            2 => Some(PvaPacketCommand::Echo(command_payload_slice.to_vec())),
            1 => PvaConnectionValidationPayload::new(
                command_payload_slice,
                self.header.flags.is_msb,
                self.header.flags.is_server,
            )
            .map(PvaPacketCommand::ConnectionValidation),
            3 => PvaSearchPayload::new(command_payload_slice, self.header.flags.is_msb)
                .map(PvaPacketCommand::Search),
            4 => PvaSearchResponsePayload::new(command_payload_slice, self.header.flags.is_msb)
                .map(PvaPacketCommand::SearchResponse),
            5 => PvaAuthNzPayload::new(command_payload_slice, self.header.flags.is_msb)
                .map(PvaPacketCommand::AuthNZ),
            6 => PvaAclChangePayload::new(command_payload_slice, self.header.flags.is_msb)
                .map(PvaPacketCommand::AclChange),
            7 => PvaCreateChannelPayload::new(
                command_payload_slice,
                self.header.flags.is_msb,
                self.header.flags.is_server,
            )
            .map(PvaPacketCommand::CreateChannel),
            8 => PvaDestroyChannelPayload::new(command_payload_slice, self.header.flags.is_msb)
                .map(PvaPacketCommand::DestroyChannel),
            9 => {
                PvaConnectionValidatedPayload::new(command_payload_slice, self.header.flags.is_msb)
                    .map(PvaPacketCommand::ConnectionValidated)
            }
            10 | 11 | 12 | 13 | 14 | 16 | 20 => PvaOpPayload::new(
                command_payload_slice,
                self.header.flags.is_msb,
                self.header.flags.is_server,
                self.header.command,
            )
            .map(PvaPacketCommand::Op),
            15 => PvaDestroyRequestPayload::new(command_payload_slice, self.header.flags.is_msb)
                .map(PvaPacketCommand::DestroyRequest),
            17 => PvaGetFieldPayload::new(
                command_payload_slice,
                self.header.flags.is_msb,
                self.header.flags.is_server,
            )
            .map(PvaPacketCommand::GetField),
            18 => PvaMessagePayload::new(command_payload_slice, self.header.flags.is_msb)
                .map(PvaPacketCommand::Message),
            19 => PvaMultipleDataPayload::new(command_payload_slice, self.header.flags.is_msb)
                .map(PvaPacketCommand::MultipleData),
            21 => PvaCancelRequestPayload::new(command_payload_slice, self.header.flags.is_msb)
                .map(PvaPacketCommand::CancelRequest),
            22 => PvaOriginTagPayload::new(command_payload_slice).map(PvaPacketCommand::OriginTag),
            _ => None,
        };

        if let Some(cmd) = decoded {
            Some(cmd)
        } else {
            debug!(
                "Decoding not implemented or unknown command: {}",
                self.header.command
            );
            Some(PvaPacketCommand::Unknown(PvaUnknownPayload::new(
                self.header.command,
                false,
                command_payload_slice.len(),
            )))
        }
    }

    pub fn is_valid(&self) -> bool {
        self.header.is_valid()
    }
}

/// helpers
pub fn decode_size(raw: &[u8], is_be: bool) -> Option<(usize, usize)> {
    if raw.is_empty() {
        return None;
    }

    match raw[0] {
        255 => Some((0, 1)),
        254 => {
            if raw.len() < 5 {
                return None;
            }
            let size_bytes = &raw[1..5];
            let size = if is_be {
                u32::from_be_bytes(size_bytes.try_into().unwrap())
            } else {
                u32::from_le_bytes(size_bytes.try_into().unwrap())
            };
            Some((size as usize, 5))
        }
        short_len => Some((short_len as usize, 1)),
    }
}

// decoding string using the above helper
pub fn decode_string(raw: &[u8], is_be: bool) -> Option<(String, usize)> {
    let (size, offset) = decode_size(raw, is_be)?;
    let total_len = offset + size;
    if raw.len() < total_len {
        return None;
    }

    let string_bytes = &raw[offset..total_len];
    let s = String::from_utf8_lossy(string_bytes).to_string();
    Some((s, total_len))
}

pub fn decode_status(raw: &[u8], is_be: bool) -> (Option<PvaStatus>, usize) {
    if raw.is_empty() {
        return (None, 0);
    }
    let code = raw[0];
    if code == 0xff {
        return (None, 1);
    }
    let mut idx = 1usize;
    let mut message: Option<String> = None;
    let mut stack: Option<String> = None;
    if let Some((msg, consumed)) = decode_string(&raw[idx..], is_be) {
        message = Some(msg);
        idx += consumed;
        if let Some((st, consumed2)) = decode_string(&raw[idx..], is_be) {
            stack = Some(st);
            idx += consumed2;
        }
    }
    (
        Some(PvaStatus {
            code,
            message,
            stack,
        }),
        idx,
    )
}

pub fn decode_op_response_status(raw: &[u8], is_be: bool) -> Result<Option<PvaStatus>, String> {
    let pkt = PvaPacket::new(raw);
    let payload_len = pkt.header.payload_length as usize;
    if raw.len() < 8 + payload_len {
        return Err("op response truncated".to_string());
    }
    let payload = &raw[8..8 + payload_len];
    if payload.len() < 5 {
        return Err("op response payload too short".to_string());
    }
    Ok(decode_status(&payload[5..], is_be).0)
}

#[derive(Debug)]
pub struct PvaControlPayload {
    pub command: u8,
    pub data: u32,
}

impl PvaControlPayload {
    pub fn new(command: u8, data: u32) -> Self {
        Self { command, data }
    }
}

#[derive(Debug)]
pub struct PvaSearchResponsePayload {
    pub guid: [u8; 12],
    pub seq: u32,
    pub addr: [u8; 16],
    pub port: u16,
    pub protocol: String,
    pub found: bool,
    pub cids: Vec<u32>,
}

impl PvaSearchResponsePayload {
    pub fn new(raw: &[u8], is_be: bool) -> Option<Self> {
        if raw.len() < 34 {
            debug!("PvaSearchResponsePayload::new: raw too short {}", raw.len());
            return None;
        }
        let guid: [u8; 12] = raw[0..12].try_into().ok()?;
        let seq = if is_be {
            u32::from_be_bytes(raw[12..16].try_into().ok()?)
        } else {
            u32::from_le_bytes(raw[12..16].try_into().ok()?)
        };
        let addr: [u8; 16] = raw[16..32].try_into().ok()?;
        let port = if is_be {
            u16::from_be_bytes(raw[32..34].try_into().ok()?)
        } else {
            u16::from_le_bytes(raw[32..34].try_into().ok()?)
        };

        let mut offset = 34;
        let (protocol, consumed) = decode_string(&raw[offset..], is_be)?;
        offset += consumed;

        if raw.len() <= offset {
            return Some(Self {
                guid,
                seq,
                addr,
                port,
                protocol,
                found: false,
                cids: vec![],
            });
        }

        let found = raw[offset] != 0;
        offset += 1;
        let mut cids: Vec<u32> = vec![];
        if raw.len() >= offset + 2 {
            let count = if is_be {
                u16::from_be_bytes(raw[offset..offset + 2].try_into().ok()?)
            } else {
                u16::from_le_bytes(raw[offset..offset + 2].try_into().ok()?)
            };
            offset += 2;
            for _ in 0..count {
                if raw.len() < offset + 4 {
                    break;
                }
                let cid = if is_be {
                    u32::from_be_bytes(raw[offset..offset + 4].try_into().ok()?)
                } else {
                    u32::from_le_bytes(raw[offset..offset + 4].try_into().ok()?)
                };
                cids.push(cid);
                offset += 4;
            }
        }

        Some(Self {
            guid,
            seq,
            addr,
            port,
            protocol,
            found,
            cids,
        })
    }
}

#[derive(Debug)]
pub struct PvaConnectionValidationPayload {
    pub is_server: bool,
    pub buffer_size: u32,
    pub introspection_registry_size: u16,
    pub qos: u16,
    pub authz: Option<String>,
    pub user: Option<String>,
    pub host: Option<String>,
}

impl PvaConnectionValidationPayload {
    pub fn new(raw: &[u8], is_be: bool, is_server: bool) -> Option<Self> {
        if raw.len() < 6 {
            debug!(
                "PvaConnectionValidationPayload::new: raw too short {}",
                raw.len()
            );
            return None;
        }
        let buffer_size = if is_be {
            u32::from_be_bytes(raw[0..4].try_into().ok()?)
        } else {
            u32::from_le_bytes(raw[0..4].try_into().ok()?)
        };
        let introspection_registry_size = if is_be {
            u16::from_be_bytes(raw[4..6].try_into().ok()?)
        } else {
            u16::from_le_bytes(raw[4..6].try_into().ok()?)
        };

        if is_server {
            // Server→client: buffer_size(u32) + isize(u16) + Size(nauth) + nauth × string
            // No QoS field.
            let mut offset = 6;
            let authz = if offset < raw.len() {
                if let Some((count, consumed)) = decode_size(&raw[offset..], is_be) {
                    offset += consumed;
                    let mut first_method = None;
                    for _ in 0..count {
                        if let Some((s, c)) = decode_string(&raw[offset..], is_be) {
                            if first_method.is_none() && !s.is_empty() {
                                first_method = Some(s);
                            }
                            offset += c;
                        }
                    }
                    first_method
                } else {
                    // Fallback: try single string (legacy spvirit servers).
                    decode_string(&raw[offset..], is_be).map(|(s, _)| s)
                }
            } else {
                None
            };

            Some(Self {
                is_server,
                buffer_size,
                introspection_registry_size,
                qos: 0,
                authz,
                user: None,
                host: None,
            })
        } else {
            // Client→server: buffer_size(u32) + isize(u16) + qos(u16) + auth_method(string) [+ FieldDesc cred]
            if raw.len() < 8 {
                return None;
            }
            let qos = if is_be {
                u16::from_be_bytes(raw[6..8].try_into().ok()?)
            } else {
                u16::from_le_bytes(raw[6..8].try_into().ok()?)
            };
            let (authz, user, host) = if raw.len() > 8 {
                if let Some((s, consumed)) = decode_string(&raw[8..], is_be) {
                    let off = 8 + consumed;
                    let (user, host) = if off < raw.len() {
                        decode_ca_credentials(&raw[off..], is_be)
                    } else {
                        (None, None)
                    };
                    (Some(s), user, host)
                } else {
                    (None, None, None)
                }
            } else {
                (None, None, None)
            };

            Some(Self {
                is_server,
                buffer_size,
                introspection_registry_size,
                qos,
                authz,
                user,
                host,
            })
        }
    }
}

/// Decode the trailing credentials PVStructure of a client ConnectionValidation.
/// Returns (user, host); either may be None if absent or the struct is unreadable.
/// Unreadable trailing bytes are tolerated (returns (None, None)) — the auth
/// method string is authoritative; a malformed credential blob must not fail the
/// whole ConnectionValidation decode.
fn decode_ca_credentials(bytes: &[u8], is_be: bool) -> (Option<String>, Option<String>) {
    let dec = PvdDecoder::new(is_be);
    // The blob is a FieldDesc followed by the packed value for that structure.
    let (desc, consumed) = match dec.parse_introspection_with_len(bytes) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let value_bytes = &bytes[consumed..];
    match dec.decode_structure(value_bytes, &desc) {
        Ok((decoded, _)) => (
            decoded_string_field(&decoded, "user"),
            decoded_string_field(&decoded, "host"),
        ),
        Err(_) => (None, None),
    }
}

/// Extract a named string field from a decoded PVStructure value, mirroring the
/// field-lookup pattern used by `extract_nt_scalar_value` in `spvd_decode.rs`.
fn decoded_string_field(decoded: &DecodedValue, name: &str) -> Option<String> {
    if let DecodedValue::Structure(fields) = decoded {
        for (field_name, value) in fields {
            if field_name == name
                && let DecodedValue::String(s) = value
            {
                return Some(s.clone());
            }
        }
    }
    None
}

#[derive(Debug)]
pub struct PvaConnectionValidatedPayload {
    pub status: Option<PvaStatus>,
}

impl PvaConnectionValidatedPayload {
    pub fn new(raw: &[u8], is_be: bool) -> Option<Self> {
        let (status, _consumed) = decode_status(raw, is_be);
        Some(Self { status })
    }
}

#[derive(Debug)]
pub struct PvaAuthNzPayload {
    pub raw: Vec<u8>,
    pub strings: Vec<String>,
}

impl PvaAuthNzPayload {
    pub fn new(raw: &[u8], is_be: bool) -> Option<Self> {
        let mut strings = vec![];
        if let Some((count, consumed)) = decode_size(raw, is_be) {
            let mut offset = consumed;
            for _ in 0..count {
                if let Some((s, len)) = decode_string(&raw[offset..], is_be) {
                    strings.push(s);
                    offset += len;
                } else {
                    break;
                }
            }
        }
        Some(Self {
            raw: raw.to_vec(),
            strings,
        })
    }
}

#[derive(Debug)]
pub struct PvaAclChangePayload {
    pub status: Option<PvaStatus>,
    pub raw: Vec<u8>,
}

impl PvaAclChangePayload {
    pub fn new(raw: &[u8], is_be: bool) -> Option<Self> {
        let (status, consumed) = decode_status(raw, is_be);
        let raw_rem = if raw.len() > consumed {
            raw[consumed..].to_vec()
        } else {
            vec![]
        };
        Some(Self {
            status,
            raw: raw_rem,
        })
    }
}

#[derive(Debug)]
pub struct PvaGetFieldPayload {
    pub is_server: bool,
    pub cid: u32,
    pub sid: Option<u32>,
    pub ioid: Option<u32>,
    pub field_name: Option<String>,
    pub status: Option<PvaStatus>,
    pub introspection: Option<StructureDesc>,
    pub raw: Vec<u8>,
}

impl PvaGetFieldPayload {
    pub fn new(raw: &[u8], is_be: bool, is_server: bool) -> Option<Self> {
        if !is_server {
            if raw.len() < 4 {
                debug!(
                    "PvaGetFieldPayload::new (client): raw too short {}",
                    raw.len()
                );
                return None;
            }
            let cid = if is_be {
                u32::from_be_bytes(raw[0..4].try_into().ok()?)
            } else {
                u32::from_le_bytes(raw[0..4].try_into().ok()?)
            };

            // Two client-side wire variants are observed for GET_FIELD:
            // 1) legacy: [cid][field_name]
            // 2) EPICS pvAccess: [sid][ioid][field_name]
            let legacy_field = if raw.len() > 4 {
                decode_string(&raw[4..], is_be)
                    .and_then(|(s, consumed)| (4 + consumed == raw.len()).then_some(s))
            } else {
                None
            };

            let epics_variant = if raw.len() >= 9 {
                let ioid = if is_be {
                    u32::from_be_bytes(raw[4..8].try_into().ok()?)
                } else {
                    u32::from_le_bytes(raw[4..8].try_into().ok()?)
                };
                decode_string(&raw[8..], is_be)
                    .and_then(|(s, consumed)| (8 + consumed == raw.len()).then_some((ioid, s)))
            } else {
                None
            };

            let (sid, ioid, field_name) = if let Some((ioid, field)) = epics_variant {
                (Some(cid), Some(ioid), Some(field))
            } else {
                (None, None, legacy_field)
            };

            return Some(Self {
                is_server,
                cid,
                sid,
                ioid,
                field_name,
                status: None,
                introspection: None,
                raw: vec![],
            });
        }

        let parse_status_then_intro = |bytes: &[u8]| {
            let (status, consumed) = decode_status(bytes, is_be);
            let pvd_raw = if bytes.len() > consumed {
                bytes[consumed..].to_vec()
            } else {
                vec![]
            };
            let introspection = if !pvd_raw.is_empty() {
                let decoder = PvdDecoder::new(is_be);
                decoder.parse_introspection(&pvd_raw).ok()
            } else {
                None
            };
            (status, pvd_raw, introspection)
        };

        // Server GET_FIELD responses are encoded as:
        // [request_id/cid][status][optional introspection]
        // Keep cid present for both success and error responses.
        let (cid, status, pvd_raw, introspection) = if raw.len() >= 4 {
            let parsed_cid = if is_be {
                u32::from_be_bytes(raw[0..4].try_into().ok()?)
            } else {
                u32::from_le_bytes(raw[0..4].try_into().ok()?)
            };
            let (status, pvd_raw, introspection) = parse_status_then_intro(&raw[4..]);
            (parsed_cid, status, pvd_raw, introspection)
        } else {
            let (status, pvd_raw, introspection) = parse_status_then_intro(raw);
            (0, status, pvd_raw, introspection)
        };

        Some(Self {
            is_server,
            cid,
            sid: None,
            ioid: None,
            field_name: None,
            status,
            introspection,
            raw: pvd_raw,
        })
    }
}

#[derive(Debug)]
pub struct PvaMessagePayload {
    pub ioid: u32,
    pub message_type: u8,
    pub message: Option<String>,
    /// Legacy compat: if the payload looks like old Status format, decode that.
    pub status: Option<PvaStatus>,
    pub raw: Vec<u8>,
}

impl PvaMessagePayload {
    pub fn new(raw: &[u8], is_be: bool) -> Option<Self> {
        // PVA spec MESSAGE format: ioid(u32) + message_type(u8) + message(string)
        if raw.len() >= 5 {
            let ioid = if is_be {
                u32::from_be_bytes(raw[0..4].try_into().ok()?)
            } else {
                u32::from_le_bytes(raw[0..4].try_into().ok()?)
            };
            let message_type = raw[4];
            let message = if raw.len() > 5 {
                decode_string(&raw[5..], is_be).map(|(s, _)| s)
            } else {
                None
            };
            // Build a synthetic PvaStatus so existing tests/code that inspect .status still work.
            let code = match message_type {
                0 => 0xFF, // info → OK
                1 => 0x01, // warning
                2 => 0x02, // error
                _ => 0x03, // fatal
            };
            let status = Some(PvaStatus {
                code,
                message: message.clone(),
                stack: None,
            });
            Some(Self {
                ioid,
                message_type,
                message,
                status,
                raw: raw.to_vec(),
            })
        } else {
            // Fallback for very short payloads
            Some(Self {
                ioid: 0,
                message_type: 0,
                message: None,
                status: None,
                raw: raw.to_vec(),
            })
        }
    }
}

#[derive(Debug)]
pub struct PvaMultipleDataEntry {
    pub ioid: u32,
    pub subcmd: u8,
}

#[derive(Debug)]
pub struct PvaMultipleDataPayload {
    pub entries: Vec<PvaMultipleDataEntry>,
    pub raw: Vec<u8>,
}

impl PvaMultipleDataPayload {
    pub fn new(raw: &[u8], is_be: bool) -> Option<Self> {
        let mut entries: Vec<PvaMultipleDataEntry> = vec![];
        if let Some((count, consumed)) = decode_size(raw, is_be) {
            let mut offset = consumed;
            for _ in 0..count {
                if raw.len() < offset + 5 {
                    break;
                }
                let ioid = if is_be {
                    u32::from_be_bytes(raw[offset..offset + 4].try_into().ok()?)
                } else {
                    u32::from_le_bytes(raw[offset..offset + 4].try_into().ok()?)
                };
                let subcmd = raw[offset + 4];
                entries.push(PvaMultipleDataEntry { ioid, subcmd });
                offset += 5;
            }
        }
        Some(Self {
            entries,
            raw: raw.to_vec(),
        })
    }
}

#[derive(Debug)]
pub struct PvaCancelRequestPayload {
    pub request_id: u32,
    pub status: Option<PvaStatus>,
}

impl PvaCancelRequestPayload {
    pub fn new(raw: &[u8], is_be: bool) -> Option<Self> {
        if raw.len() < 4 {
            debug!("PvaCancelRequestPayload::new: raw too short {}", raw.len());
            return None;
        }
        let request_id = if is_be {
            u32::from_be_bytes(raw[0..4].try_into().ok()?)
        } else {
            u32::from_le_bytes(raw[0..4].try_into().ok()?)
        };
        let (status, _) = if raw.len() > 4 {
            decode_status(&raw[4..], is_be)
        } else {
            (None, 0)
        };
        Some(Self { request_id, status })
    }
}

#[derive(Debug)]
pub struct PvaDestroyRequestPayload {
    pub sid: u32,
    pub request_id: u32,
}

impl PvaDestroyRequestPayload {
    /// Decode a `destroyRequest` (0x0F) payload.
    ///
    /// The PVA spec payload is `serverChannelID (i32)` followed by
    /// `requestID (i32)`. Older spvirit clients sent only the 4-byte
    /// requestID; that legacy form is still accepted (with `sid` = 0).
    pub fn new(raw: &[u8], is_be: bool) -> Option<Self> {
        let word = |range: std::ops::Range<usize>| -> Option<u32> {
            let bytes = raw.get(range)?.try_into().ok()?;
            Some(if is_be {
                u32::from_be_bytes(bytes)
            } else {
                u32::from_le_bytes(bytes)
            })
        };
        if raw.len() >= 8 {
            Some(Self {
                sid: word(0..4)?,
                request_id: word(4..8)?,
            })
        } else if raw.len() >= 4 {
            Some(Self {
                sid: 0,
                request_id: word(0..4)?,
            })
        } else {
            debug!("PvaDestroyRequestPayload::new: raw too short {}", raw.len());
            None
        }
    }
}

#[derive(Debug)]
pub struct PvaOriginTagPayload {
    pub address: [u8; 16],
}

impl PvaOriginTagPayload {
    pub fn new(raw: &[u8]) -> Option<Self> {
        if raw.len() < 16 {
            debug!("PvaOriginTagPayload::new: raw too short {}", raw.len());
            return None;
        }
        let address: [u8; 16] = raw[0..16].try_into().ok()?;
        Some(Self { address })
    }
}

#[derive(Debug)]
pub struct PvaUnknownPayload {
    pub command: u8,
    pub is_control: bool,
    pub raw_len: usize,
}

impl PvaUnknownPayload {
    pub fn new(command: u8, is_control: bool, raw_len: usize) -> Self {
        Self {
            command,
            is_control,
            raw_len,
        }
    }
}

/// payload decoder
/// SEARCH
#[derive(Debug)]
pub struct PvaSearchPayload {
    pub seq: u32,
    pub mask: u8,
    pub addr: [u8; 16],
    pub port: u16,
    pub protocols: Vec<String>,
    pub pv_requests: Vec<(u32, String)>,
    pub pv_names: Vec<String>,
}

impl PvaSearchPayload {
    pub fn new(raw: &[u8], is_be: bool) -> Option<Self> {
        if raw.is_empty() {
            debug!("PvaSearchPayload::new received an empty raw slice.");
            return None;
        }
        const MIN_FIXED_SEARCH_PAYLOAD_SIZE: usize = 26;
        if raw.len() < MIN_FIXED_SEARCH_PAYLOAD_SIZE {
            debug!(
                "PvaSearchPayload::new: raw slice length {} is less than min fixed size {}.",
                raw.len(),
                MIN_FIXED_SEARCH_PAYLOAD_SIZE
            );
            return None;
        }

        let seq = if is_be {
            u32::from_be_bytes(raw[0..4].try_into().unwrap())
        } else {
            u32::from_le_bytes(raw[0..4].try_into().unwrap())
        };

        let mask = raw[4];
        let addr: [u8; 16] = raw[8..24].try_into().unwrap();
        let port = if is_be {
            u16::from_be_bytes(raw[24..26].try_into().unwrap())
        } else {
            u16::from_le_bytes(raw[24..26].try_into().unwrap())
        };

        let mut offset = 26;

        let (protocol_count, consumed) = decode_size(&raw[offset..], is_be)?;
        offset += consumed;

        let mut protocols = vec![];
        for _ in 0..protocol_count {
            let (protocol, len) = decode_string(&raw[offset..], is_be)?;
            protocols.push(protocol);
            offset += len;
        }

        // PV names here
        if raw.len() < offset + 2 {
            return None;
        }
        let pv_count = if is_be {
            u16::from_be_bytes(raw[offset..offset + 2].try_into().unwrap())
        } else {
            u16::from_le_bytes(raw[offset..offset + 2].try_into().unwrap())
        };
        offset += 2;

        let mut pv_names = vec![];
        let mut pv_requests = vec![];
        for _ in 0..pv_count {
            if raw.len() < offset + 4 {
                debug!(
                    "PvaSearchPayload::new: not enough data for PV CID at offset {}. Raw len: {}",
                    offset,
                    raw.len()
                );
                return None;
            }
            let cid = if is_be {
                u32::from_be_bytes(raw[offset..offset + 4].try_into().unwrap())
            } else {
                u32::from_le_bytes(raw[offset..offset + 4].try_into().unwrap())
            };
            offset += 4;
            let (pv_name, len) = decode_string(&raw[offset..], is_be)?;
            pv_names.push(pv_name.clone());
            pv_requests.push((cid, pv_name));
            offset += len;
        }

        Some(Self {
            seq,
            mask,
            addr,
            port,
            protocols,
            pv_requests,
            pv_names,
        })
    }
}

/// struct beaconMessage {
#[derive(Debug)]
pub struct PvaBeaconPayload {
    pub guid: [u8; 12],
    pub flags: u8,
    pub beacon_sequence_id: u8,
    pub change_count: u16,
    pub server_address: [u8; 16],
    pub server_port: u16,
    pub protocol: String,
    pub server_status_if: String,
}

impl PvaBeaconPayload {
    pub fn new(raw: &[u8], is_be: bool) -> Option<Self> {
        // guid(12) + flags(1) + beacon_sequence_id(1) + change_count(2) + server_address(16) + server_port(2)
        const MIN_FIXED_BEACON_PAYLOAD_SIZE: usize = 12 + 1 + 1 + 2 + 16 + 2;

        if raw.len() < MIN_FIXED_BEACON_PAYLOAD_SIZE {
            debug!(
                "PvaBeaconPayload::new: raw slice length {} is less than min fixed size {}.",
                raw.len(),
                MIN_FIXED_BEACON_PAYLOAD_SIZE
            );
            return None;
        }

        let guid: [u8; 12] = raw[0..12].try_into().unwrap();
        let flags = raw[12];
        let beacon_sequence_id = raw[13];
        let change_count = if is_be {
            u16::from_be_bytes(raw[14..16].try_into().unwrap())
        } else {
            u16::from_le_bytes(raw[14..16].try_into().unwrap())
        };
        let server_address: [u8; 16] = raw[16..32].try_into().unwrap();
        let server_port = if is_be {
            u16::from_be_bytes(raw[32..34].try_into().unwrap())
        } else {
            u16::from_le_bytes(raw[32..34].try_into().unwrap())
        };
        let (protocol, len) = decode_string(&raw[34..], is_be)?;
        let protocol = protocol;
        let server_status_if = if len > 0 {
            let (server_status_if, _server_status_len) = decode_string(&raw[34 + len..], is_be)?;
            server_status_if
        } else {
            String::new()
        };

        Some(Self {
            guid,
            flags,
            beacon_sequence_id,
            change_count,
            server_address,
            server_port,
            protocol,
            server_status_if,
        })
    }
}

/// CREATE_CHANNEL payload (cmd=7)
/// Client: count(2), then for each: cid(4), pv_name(string)
/// Server: cid(4), sid(4), status
#[derive(Debug)]
pub struct PvaCreateChannelPayload {
    /// Is this from server (response) or client (request)?
    pub is_server: bool,
    /// For client requests: list of (cid, pv_name) tuples
    pub channels: Vec<(u32, String)>,
    /// For server response: client channel ID
    pub cid: u32,
    /// For server response: server channel ID
    pub sid: u32,
    /// For server response: status
    pub status: Option<PvaStatus>,
}

impl PvaCreateChannelPayload {
    pub fn new(raw: &[u8], is_be: bool, is_server: bool) -> Option<Self> {
        if raw.is_empty() {
            debug!("PvaCreateChannelPayload::new received an empty raw slice.");
            return None;
        }

        if is_server {
            // Server response: cid(4), sid(4), status
            if raw.len() < 8 {
                debug!("CREATE_CHANNEL server response too short: {}", raw.len());
                return None;
            }

            let cid = if is_be {
                u32::from_be_bytes(raw[0..4].try_into().unwrap())
            } else {
                u32::from_le_bytes(raw[0..4].try_into().unwrap())
            };

            let sid = if is_be {
                u32::from_be_bytes(raw[4..8].try_into().unwrap())
            } else {
                u32::from_le_bytes(raw[4..8].try_into().unwrap())
            };

            // Decode status if present
            let status = if raw.len() > 8 {
                let code = raw[8];
                if code == 0xff {
                    None // OK, no status message
                } else {
                    let mut idx = 9;
                    let message = if idx < raw.len() {
                        decode_string(&raw[idx..], is_be).map(|(msg, consumed)| {
                            idx += consumed;
                            msg
                        })
                    } else {
                        None
                    };
                    let stack = if idx < raw.len() {
                        decode_string(&raw[idx..], is_be).map(|(s, _)| s)
                    } else {
                        None
                    };
                    Some(PvaStatus {
                        code,
                        message,
                        stack,
                    })
                }
            } else {
                None
            };

            Some(Self {
                is_server: true,
                channels: vec![],
                cid,
                sid,
                status,
            })
        } else {
            // Client request: count(2), then for each: cid(4), pv_name(string)
            if raw.len() < 2 {
                debug!("CREATE_CHANNEL client request too short: {}", raw.len());
                return None;
            }

            let count = if is_be {
                u16::from_be_bytes(raw[0..2].try_into().unwrap())
            } else {
                u16::from_le_bytes(raw[0..2].try_into().unwrap())
            };

            let mut offset = 2;
            let mut channels = Vec::with_capacity(count as usize);

            for _ in 0..count {
                if raw.len() < offset + 4 {
                    debug!(
                        "CREATE_CHANNEL: not enough data for CID at offset {}",
                        offset
                    );
                    break;
                }

                let cid = if is_be {
                    u32::from_be_bytes(raw[offset..offset + 4].try_into().unwrap())
                } else {
                    u32::from_le_bytes(raw[offset..offset + 4].try_into().unwrap())
                };
                offset += 4;

                if let Some((pv_name, consumed)) = decode_string(&raw[offset..], is_be) {
                    offset += consumed;
                    channels.push((cid, pv_name));
                } else {
                    debug!(
                        "CREATE_CHANNEL: failed to decode PV name at offset {}",
                        offset
                    );
                    break;
                }
            }

            Some(Self {
                is_server: false,
                channels,
                cid: 0,
                sid: 0,
                status: None,
            })
        }
    }
}

impl fmt::Display for PvaCreateChannelPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_server {
            let status_text = if let Some(s) = &self.status {
                format!(" status={}", s.code)
            } else {
                String::new()
            };
            write!(
                f,
                "CREATE_CHANNEL(cid={}, sid={}{})",
                self.cid, self.sid, status_text
            )
        } else {
            let pv_list: Vec<String> = self
                .channels
                .iter()
                .map(|(cid, name)| format!("{}:'{}'", cid, name))
                .collect();
            write!(f, "CREATE_CHANNEL({})", pv_list.join(", "))
        }
    }
}

/// DESTROY_CHANNEL payload (cmd=8)
/// Format: sid(4), cid(4)
#[derive(Debug)]
pub struct PvaDestroyChannelPayload {
    /// Server channel ID
    pub sid: u32,
    /// Client channel ID
    pub cid: u32,
}

impl PvaDestroyChannelPayload {
    pub fn new(raw: &[u8], is_be: bool) -> Option<Self> {
        if raw.len() < 8 {
            debug!("DESTROY_CHANNEL payload too short: {}", raw.len());
            return None;
        }

        let sid = if is_be {
            u32::from_be_bytes(raw[0..4].try_into().unwrap())
        } else {
            u32::from_le_bytes(raw[0..4].try_into().unwrap())
        };

        let cid = if is_be {
            u32::from_be_bytes(raw[4..8].try_into().unwrap())
        } else {
            u32::from_le_bytes(raw[4..8].try_into().unwrap())
        };

        Some(Self { sid, cid })
    }
}

impl fmt::Display for PvaDestroyChannelPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DESTROY_CHANNEL(sid={}, cid={})", self.sid, self.cid)
    }
}

/// Generic operation payload (GET/PUT/PUT_GET/MONITOR/ARRAY/RPC)
#[derive(Debug)]
pub struct PvaOpPayload {
    pub sid_or_cid: u32,
    pub ioid: u32,
    pub subcmd: u8,
    pub body: Vec<u8>,
    pub command: u8,
    pub is_server: bool,
    pub status: Option<PvaStatus>,
    pub pv_names: Vec<String>,
    /// Parsed introspection data (for INIT responses)
    pub introspection: Option<StructureDesc>,
    /// Decoded value (when field_desc is available)
    pub decoded_value: Option<DecodedValue>,
}

// Heuristic extraction of PV-like names from a PVD body.
fn extract_pv_names(raw: &[u8]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < raw.len() {
        // start with an alphanumeric character
        if raw[i].is_ascii_alphanumeric() {
            let start = i;
            i += 1;
            while i < raw.len() {
                let b = raw[i];
                if b.is_ascii_alphanumeric()
                    || b == b':'
                    || b == b'.'
                    || b == b'_'
                    || b == b'-'
                    || b == b'/'
                {
                    i += 1;
                } else {
                    break;
                }
            }
            let len = i - start;
            if len >= 3 && len <= 128 {
                if let Ok(s) = std::str::from_utf8(&raw[start..start + len]) {
                    // validate candidate contains at least one alphabetic char
                    if s.chars().any(|c| c.is_ascii_alphabetic()) {
                        if !names.contains(&s.to_string()) {
                            names.push(s.to_string());
                            if names.len() >= 8 {
                                break;
                            }
                        }
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    names
}

impl PvaOpPayload {
    pub fn new(raw: &[u8], is_be: bool, is_server: bool, command: u8) -> Option<Self> {
        // operation payloads have slightly different fixed offsets depending on client/server
        if raw.len() < 5 {
            debug!("PvaOpPayload::new: raw too short {}", raw.len());
            return None;
        }

        let (sid_or_cid, ioid, subcmd, offset) = if is_server {
            // server op: ioid(4), subcmd(1)
            if raw.len() < 5 {
                return None;
            }
            let ioid = if is_be {
                u32::from_be_bytes(raw[0..4].try_into().unwrap())
            } else {
                u32::from_le_bytes(raw[0..4].try_into().unwrap())
            };
            let subcmd = raw[4];
            (0, ioid, subcmd, 5)
        } else {
            // client op: sid(4), ioid(4), subcmd(1)
            if raw.len() < 9 {
                return None;
            }
            let sid = if is_be {
                u32::from_be_bytes(raw[0..4].try_into().unwrap())
            } else {
                u32::from_le_bytes(raw[0..4].try_into().unwrap())
            };
            let ioid = if is_be {
                u32::from_be_bytes(raw[4..8].try_into().unwrap())
            } else {
                u32::from_le_bytes(raw[4..8].try_into().unwrap())
            };
            let subcmd = raw[8];
            (sid, ioid, subcmd, 9)
        };

        let body = if raw.len() > offset {
            raw[offset..].to_vec()
        } else {
            vec![]
        };

        // Status is only present in certain subcmd types:
        // Status format (per Lua dissector): first byte = code. If code==0xff (255) -> OK
        // shorthand (1 byte only). Otherwise follow with two length-prefixed strings:
        // message, stack.
        // Server responses carry a status prefix for INIT responses (subcmd & 0x08),
        // and for non-INIT responses on GET (10), PUT (11), PUT_GET (12).
        // Monitor (13) data updates (non-INIT) do NOT have a status prefix.
        let mut status: Option<PvaStatus> = None;
        let mut pvd_raw: Vec<u8> = vec![];

        let has_status = is_server && ((subcmd & 0x08) != 0 || (command != 13 && command != 14));

        if !body.is_empty() {
            if has_status {
                let (parsed, consumed) = decode_status(&body, is_be);
                status = parsed;
                pvd_raw = if body.len() > consumed {
                    body[consumed..].to_vec()
                } else {
                    vec![]
                };
            } else {
                pvd_raw = body.clone();
            }
        }

        let pv_names = extract_pv_names(&pvd_raw);

        // Try to parse introspection from INIT response (subcmd & 0x08 and is_server)
        let introspection = if is_server && (subcmd & 0x08) != 0 && !pvd_raw.is_empty() {
            let decoder = PvdDecoder::new(is_be);
            decoder.parse_introspection(&pvd_raw).ok()
        } else {
            None
        };

        let result = Some(Self {
            sid_or_cid,
            ioid,
            subcmd,
            body: pvd_raw,
            command,
            is_server,
            status: status.clone(),
            pv_names,
            introspection,
            decoded_value: None, // Will be set by packet processor with field_desc
        });

        result
    }

    /// Decode the body using provided field description.
    ///
    /// See [`DecodeMode`] for the MONITOR bitset-layout policy. Live
    /// connections, where the introspection is known, want
    /// [`DecodeMode::Strict`].
    pub fn decode_with_field_desc(
        &mut self,
        field_desc: &StructureDesc,
        is_be: bool,
        mode: DecodeMode,
    ) -> DecodeResult<()> {
        if self.body.is_empty() {
            return Ok(());
        }

        let decoder = PvdDecoder::new(is_be);

        // For data updates (subcmd == 0x00 or subcmd & 0x40), use bitset decoding
        if self.subcmd == 0x00 || (self.subcmd & 0x40) != 0 {
            if self.command == 13 {
                let update = match mode {
                    DecodeMode::Strict => decoder.decode_monitor_update(&self.body, field_desc)?,
                    DecodeMode::Lenient => {
                        decoder
                            .decode_monitor_update_lenient(&self.body, field_desc)?
                            .0
                    }
                };
                self.decoded_value = Some(update.value);
            } else {
                let (value, _) = decoder.decode_structure_with_bitset(&self.body, field_desc)?;
                self.decoded_value = Some(value);
            }
        } else {
            // Full structure decode
            let (value, _) = decoder.decode_structure(&self.body, field_desc)?;
            self.decoded_value = Some(value);
        }
        Ok(())
    }
}

/// How strictly to interpret a MONITOR body's bitset layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeMode {
    /// Specification order only: changed bitset, data, overrun bitset. Use
    /// this on live connections, where the introspection is known.
    Strict,
    /// Try every known layout and pick the most plausible. For mid-stream
    /// packet captures where the peer's layout is unknown.
    ///
    /// Nothing in this workspace uses it; it is public API for out-of-tree
    /// consumers talking to implementations that disagree about where the
    /// overrun bitset goes.
    Lenient,
}

#[derive(Debug, Clone)]
pub struct PvaStatus {
    pub code: u8,
    pub message: Option<String>,
    pub stack: Option<String>,
}

impl PvaStatus {
    pub fn is_error(&self) -> bool {
        self.code != 0
    }
}

/// Display implementations
// beacon payload display
impl fmt::Display for PvaBeaconPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Beacon:GUID=[{}],Flags=[{}],SeqId=[{}],ChangeCount=[{}],ServerAddress=[{}],ServerPort=[{}],Protocol=[{}]",
            hex::encode(self.guid),
            self.flags,
            self.beacon_sequence_id,
            self.change_count,
            format_pva_address(&self.server_address),
            self.server_port,
            self.protocol
        )
    }
}

// search payload display
impl fmt::Display for PvaSearchPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Search:PVs=[{}]", self.pv_names.join(","))
    }
}

impl fmt::Display for PvaControlPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.command {
            0 => "MARK_TOTAL_BYTES_SENT",
            1 => "ACK_TOTAL_BYTES_RECEIVED",
            2 => "SET_BYTE_ORDER",
            3 => "ECHO_REQUEST",
            4 => "ECHO_RESPONSE",
            _ => "CONTROL",
        };
        write!(f, "{}(data={})", name, self.data)
    }
}

impl fmt::Display for PvaSearchResponsePayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let found_text = if self.found { "true" } else { "false" };
        if self.cids.is_empty() {
            write!(
                f,
                "SearchResponse(found={}, proto={})",
                found_text, self.protocol
            )
        } else {
            write!(
                f,
                "SearchResponse(found={}, proto={}, cids=[{}])",
                found_text,
                self.protocol,
                self.cids
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<String>>()
                    .join(",")
            )
        }
    }
}

impl fmt::Display for PvaConnectionValidationPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dir = if self.is_server { "server" } else { "client" };
        let authz = self.authz.as_deref().unwrap_or("");
        if authz.is_empty() {
            write!(
                f,
                "ConnectionValidation(dir={}, qsize={}, isize={}, qos=0x{:04x})",
                dir, self.buffer_size, self.introspection_registry_size, self.qos
            )
        } else {
            write!(
                f,
                "ConnectionValidation(dir={}, qsize={}, isize={}, qos=0x{:04x}, authz={})",
                dir, self.buffer_size, self.introspection_registry_size, self.qos, authz
            )
        }
    }
}

impl fmt::Display for PvaStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "code={} message={} stack={}",
            self.code,
            self.message.as_deref().unwrap_or(""),
            self.stack.as_deref().unwrap_or("")
        )
    }
}

impl fmt::Display for PvaConnectionValidatedPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.status {
            Some(s) => write!(f, "ConnectionValidated(status={})", s.code),
            None => write!(f, "ConnectionValidated(status=OK)"),
        }
    }
}

impl fmt::Display for PvaAuthNzPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.strings.is_empty() {
            write!(f, "AuthNZ(strings=[{}])", self.strings.join(","))
        } else {
            write!(f, "AuthNZ(raw_len={})", self.raw.len())
        }
    }
}

impl fmt::Display for PvaAclChangePayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.status {
            Some(s) => write!(f, "ACL_CHANGE(status={})", s.code),
            None => write!(f, "ACL_CHANGE(status=OK)"),
        }
    }
}

impl fmt::Display for PvaGetFieldPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_server {
            let status = self.status.as_ref().map(|s| s.code).unwrap_or(0xff);
            write!(f, "GET_FIELD(status={})", status)
        } else {
            let field = self.field_name.as_deref().unwrap_or("");
            if field.is_empty() {
                write!(f, "GET_FIELD(cid={})", self.cid)
            } else {
                write!(f, "GET_FIELD(cid={}, field={})", self.cid, field)
            }
        }
    }
}

impl fmt::Display for PvaMessagePayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.status {
            Some(s) => {
                if let Some(msg) = &s.message {
                    write!(f, "MESSAGE(status={}, msg='{}')", s.code, msg)
                } else {
                    write!(f, "MESSAGE(status={})", s.code)
                }
            }
            None => write!(f, "MESSAGE(status=OK)"),
        }
    }
}

impl fmt::Display for PvaMultipleDataPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.entries.is_empty() {
            write!(f, "MULTIPLE_DATA(raw_len={})", self.raw.len())
        } else {
            write!(f, "MULTIPLE_DATA(entries={})", self.entries.len())
        }
    }
}

impl fmt::Display for PvaCancelRequestPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = self.status.as_ref().map(|s| s.code);
        match status {
            Some(code) => write!(f, "CANCEL_REQUEST(id={}, status={})", self.request_id, code),
            None => write!(f, "CANCEL_REQUEST(id={})", self.request_id),
        }
    }
}

impl fmt::Display for PvaDestroyRequestPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DESTROY_REQUEST(sid={}, id={})",
            self.sid, self.request_id
        )
    }
}

impl fmt::Display for PvaOriginTagPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ORIGIN_TAG(addr={})", format_pva_address(&self.address))
    }
}

impl fmt::Display for PvaUnknownPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.is_control {
            "CONTROL"
        } else {
            "APPLICATION"
        };
        write!(
            f,
            "UNKNOWN(cmd={}, type={}, raw_len={})",
            self.command, kind, self.raw_len
        )
    }
}

// generic display for all payloads
impl fmt::Display for PvaPacketCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PvaPacketCommand::Control(payload) => write!(f, "{}", payload),
            PvaPacketCommand::Search(payload) => write!(f, "{}", payload),
            PvaPacketCommand::SearchResponse(payload) => write!(f, "{}", payload),
            PvaPacketCommand::Beacon(payload) => write!(f, "{}", payload),
            PvaPacketCommand::ConnectionValidation(payload) => write!(f, "{}", payload),
            PvaPacketCommand::ConnectionValidated(payload) => write!(f, "{}", payload),
            PvaPacketCommand::AuthNZ(payload) => write!(f, "{}", payload),
            PvaPacketCommand::AclChange(payload) => write!(f, "{}", payload),
            PvaPacketCommand::Op(payload) => write!(f, "{}", payload),
            PvaPacketCommand::CreateChannel(payload) => write!(f, "{}", payload),
            PvaPacketCommand::DestroyChannel(payload) => write!(f, "{}", payload),
            PvaPacketCommand::GetField(payload) => write!(f, "{}", payload),
            PvaPacketCommand::Message(payload) => write!(f, "{}", payload),
            PvaPacketCommand::MultipleData(payload) => write!(f, "{}", payload),
            PvaPacketCommand::CancelRequest(payload) => write!(f, "{}", payload),
            PvaPacketCommand::DestroyRequest(payload) => write!(f, "{}", payload),
            PvaPacketCommand::OriginTag(payload) => write!(f, "{}", payload),
            PvaPacketCommand::Echo(bytes) => write!(f, "ECHO ({} bytes)", bytes.len()),
            PvaPacketCommand::Unknown(payload) => write!(f, "{}", payload),
        }
    }
}

impl fmt::Display for PvaOpPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cmd_name = match self.command {
            10 => "GET",
            11 => "PUT",
            12 => "PUT_GET",
            13 => "MONITOR",
            14 => "ARRAY",
            16 => "PROCESS",
            20 => "RPC",
            _ => "OP",
        };

        let status_text = if let Some(s) = &self.status {
            match &s.message {
                Some(m) if !m.is_empty() => format!(" status={} msg='{}'", s.code, m),
                _ => format!(" status={}", s.code),
            }
        } else {
            String::new()
        };

        // Show decoded value if available, otherwise fall back to heuristic strings
        let value_text = if let Some(ref decoded) = self.decoded_value {
            let formatted = format_compact_value(decoded);
            if formatted.is_empty() || formatted == "{}" {
                String::new()
            } else {
                format!(" [{}]", formatted)
            }
        } else if !self.pv_names.is_empty() {
            format!(" data=[{}]", self.pv_names.join(","))
        } else {
            String::new()
        };

        if self.is_server {
            write!(
                f,
                "{}(ioid={}, sub=0x{:02x}{}{})",
                cmd_name, self.ioid, self.subcmd, status_text, value_text
            )
        } else {
            write!(
                f,
                "{}(sid={}, ioid={}, sub=0x{:02x}{}{})",
                cmd_name, self.sid_or_cid, self.ioid, self.subcmd, status_text, value_text
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spvd_decode::extract_nt_scalar_value;

    #[test]
    fn try_new_rejects_short_datagram_without_panic() {
        // A malicious/truncated UDP datagram shorter than the 8-byte header
        // must not panic; try_new returns None and the caller skips it.
        for len in 0..8usize {
            let short = vec![0xCAu8; len];
            assert!(
                PvaHeader::try_new(&short).is_none(),
                "PvaHeader::try_new should reject {len} bytes"
            );
            assert!(
                PvaPacket::try_new(&short).is_none(),
                "PvaPacket::try_new should reject {len} bytes"
            );
        }
        // Exactly 8 bytes is the minimum valid header and must succeed.
        let ok = vec![0u8; 8];
        assert!(PvaPacket::try_new(&ok).is_some());
    }

    #[test]
    fn destroy_request_decodes_spec_and_legacy_forms() {
        // Spec form (pvxs/pvAccessCPP): serverChannelID then requestID.
        let mut spec = Vec::new();
        spec.extend_from_slice(&7u32.to_le_bytes());
        spec.extend_from_slice(&42u32.to_le_bytes());
        let p = PvaDestroyRequestPayload::new(&spec, false).unwrap();
        assert_eq!((p.sid, p.request_id), (7, 42));

        // Legacy 4-byte spvirit form: requestID only.
        let legacy = 42u32.to_le_bytes();
        let p = PvaDestroyRequestPayload::new(&legacy, false).unwrap();
        assert_eq!((p.sid, p.request_id), (0, 42));

        assert!(PvaDestroyRequestPayload::new(&[0u8; 3], false).is_none());
    }
    use crate::spvd_encode::{
        encode_nt_payload_bitset_parts, encode_nt_scalar_bitset_parts, encode_size_pvd,
        nt_payload_desc, nt_scalar_desc,
    };
    use crate::spvirit_encode::encode_header;
    use spvirit_types::{NtPayload, NtScalar, NtScalarArray, ScalarArrayValue, ScalarValue};

    #[test]
    fn test_decode_status_ok() {
        let raw = [0xff];
        let (status, consumed) = decode_status(&raw, false);
        assert!(status.is_none());
        assert_eq!(consumed, 1);
    }

    #[test]
    fn test_decode_status_message() {
        let raw = [1u8, 2, b'h', b'i', 2, b's', b't'];
        let (status, consumed) = decode_status(&raw, false);
        assert_eq!(consumed, 7);
        let status = status.unwrap();
        assert_eq!(status.code, 1);
        assert_eq!(status.message.as_deref(), Some("hi"));
        assert_eq!(status.stack.as_deref(), Some("st"));
    }

    #[test]
    fn test_search_response_decode() {
        let mut raw: Vec<u8> = vec![];
        raw.extend_from_slice(&[0u8; 12]); // guid
        raw.extend_from_slice(&1u32.to_le_bytes()); // seq
        raw.extend_from_slice(&[0u8; 16]); // addr
        raw.extend_from_slice(&5076u16.to_le_bytes()); // port
        raw.push(3); // protocol size
        raw.extend_from_slice(b"tcp");
        raw.push(1); // found
        raw.extend_from_slice(&1u16.to_le_bytes()); // count
        raw.extend_from_slice(&42u32.to_le_bytes()); // cid

        let decoded = PvaSearchResponsePayload::new(&raw, false).unwrap();
        assert!(decoded.found);
        assert_eq!(decoded.protocol, "tcp");
        assert_eq!(decoded.cids, vec![42u32]);
    }

    fn build_monitor_packet(ioid: u32, subcmd: u8, body: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&ioid.to_le_bytes());
        payload.push(subcmd);
        payload.extend_from_slice(body);
        let mut out = encode_header(true, false, false, 2, 13, payload.len() as u32);
        out.extend_from_slice(&payload);
        out
    }

    /// Strict mode decodes the specification layout: changed bitset, data,
    /// overrun bitset.
    #[test]
    fn test_monitor_decode_spec_order_strict() {
        let nt = NtScalar::from_value(ScalarValue::F64(3.5));
        let desc = nt_scalar_desc(&nt.value);
        let (changed_bitset, values) = encode_nt_scalar_bitset_parts(&nt, false);

        let mut body_spec = Vec::new();
        body_spec.extend_from_slice(&changed_bitset);
        body_spec.extend_from_slice(&values);
        body_spec.extend_from_slice(&encode_size_pvd(0, false));

        let pkt = build_monitor_packet(1, 0x00, &body_spec);
        let mut pva = PvaPacket::new(&pkt);
        let mut cmd = pva.decode_payload().expect("decoded");
        if let PvaPacketCommand::Op(ref mut op) = cmd {
            op.decode_with_field_desc(&desc, false, DecodeMode::Strict)
                .expect("spec-order body decodes");
            let decoded = op.decoded_value.as_ref().expect("decoded");
            let value = extract_nt_scalar_value(decoded).expect("value");
            match value {
                DecodedValue::Float64(v) => assert!((*v - 3.5).abs() < 1e-6),
                other => panic!("unexpected value {:?}", other),
            }
        } else {
            panic!("unexpected cmd");
        }
    }

    /// The two non-specification layouts this codec used to guess at are now
    /// only reachable through the lenient decoder. Ported from the old
    /// `test_monitor_decode_overrun_and_legacy`, which relied on
    /// `decode_with_field_desc` scoring all three variants.
    #[test]
    fn test_monitor_decode_non_spec_layouts_via_lenient() {
        use crate::monitor::MonitorLayout;
        use crate::spvd_decode::PvdDecoder;

        let nt = NtScalar::from_value(ScalarValue::F64(3.5));
        let desc = nt_scalar_desc(&nt.value);
        let (changed_bitset, values) = encode_nt_scalar_bitset_parts(&nt, false);
        let decoder = PvdDecoder::new(false);

        // changed bitset, overrun bitset, data.
        let mut body_overrun = Vec::new();
        body_overrun.extend_from_slice(&changed_bitset);
        body_overrun.extend_from_slice(&encode_size_pvd(0, false));
        body_overrun.extend_from_slice(&values);

        let (update, layout) = decoder
            .decode_monitor_update_lenient(&body_overrun, &desc)
            .expect("overrun-before-data body decodes");
        assert_eq!(layout, MonitorLayout::OverrunBeforeData);
        match extract_nt_scalar_value(&update.value).expect("value") {
            DecodedValue::Float64(v) => assert!((*v - 3.5).abs() < 1e-6),
            other => panic!("unexpected value {:?}", other),
        }

        // changed bitset, data, no overrun bitset.
        let mut body_legacy = Vec::new();
        body_legacy.extend_from_slice(&changed_bitset);
        body_legacy.extend_from_slice(&values);

        let (update, layout) = decoder
            .decode_monitor_update_lenient(&body_legacy, &desc)
            .expect("changed-only body decodes");
        assert_eq!(layout, MonitorLayout::ChangedOnly);
        match extract_nt_scalar_value(&update.value).expect("value") {
            DecodedValue::Float64(v) => assert!((*v - 3.5).abs() < 1e-6),
            other => panic!("unexpected value {:?}", other),
        }
    }

    #[test]
    fn test_monitor_decode_prefers_spec_order_for_array_payload() {
        let payload_value =
            NtPayload::ScalarArray(NtScalarArray::from_value(ScalarArrayValue::F64(vec![
                1.0, 2.0, 3.0, 4.0,
            ])));
        let desc = nt_payload_desc(&payload_value);
        let (changed_bitset, values) = encode_nt_payload_bitset_parts(&payload_value, false);

        let mut body_spec = Vec::new();
        body_spec.extend_from_slice(&changed_bitset);
        body_spec.extend_from_slice(&values);
        body_spec.extend_from_slice(&encode_size_pvd(0, false));

        let pkt = build_monitor_packet(11, 0x00, &body_spec);
        let mut pva = PvaPacket::new(&pkt);
        let mut cmd = pva.decode_payload().expect("decoded");
        if let PvaPacketCommand::Op(ref mut op) = cmd {
            op.decode_with_field_desc(&desc, false, DecodeMode::Strict)
                .expect("spec-order body decodes");
            let decoded = op.decoded_value.as_ref().expect("decoded");
            let value = extract_nt_scalar_value(decoded).expect("value");
            match value {
                DecodedValue::Array(items) => {
                    assert_eq!(items.len(), 4);
                    assert!(matches!(items[0], DecodedValue::Float64(v) if (v - 1.0).abs() < 1e-6));
                    assert!(matches!(items[3], DecodedValue::Float64(v) if (v - 4.0).abs() < 1e-6));
                }
                other => panic!("unexpected value {:?}", other),
            }
        } else {
            panic!("unexpected cmd");
        }
    }

    #[test]
    fn pva_status_reports_error_state() {
        let ok = PvaStatus {
            code: 0,
            message: None,
            stack: None,
        };
        let err = PvaStatus {
            code: 2,
            message: Some("bad".to_string()),
            stack: None,
        };
        assert!(!ok.is_error());
        assert!(err.is_error());
    }

    #[test]
    fn pva_status_display_includes_message_and_stack() {
        let status = PvaStatus {
            code: 2,
            message: Some("bad".to_string()),
            stack: Some("trace".to_string()),
        };
        assert_eq!(status.to_string(), "code=2 message=bad stack=trace");
    }

    #[test]
    fn decode_op_response_status_reads_status_from_packet() {
        let raw = vec![
            0xCA, 0x02, 0x40, 0x0B, 0x0A, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x00, 0x02,
            0x03, b'b', b'a', b'd', 0x00,
        ];
        let status = decode_op_response_status(&raw, false)
            .expect("status parse")
            .expect("status");
        assert!(status.is_error());
        assert_eq!(status.message.as_deref(), Some("bad"));
    }

    #[test]
    fn connection_validation_decodes_ca_user_and_host() {
        // Client->server ConnectionValidation with method "ca" and a trailing
        // credentials structure { string user; string host }.
        // Build via the crate encoder to avoid hand-rolling PVD bytes.
        let raw = crate::spvirit_encode::encode_connection_validation_client_ca(
            0x10000, // buffer_size
            1,       // introspection size
            0,       // qos
            "ca",    // method
            "operator1",
            "ws-42.lab",
            /* is_be = */ true,
        );
        let p = PvaConnectionValidationPayload::new(&raw, true, false).expect("decode");
        assert_eq!(p.authz.as_deref(), Some("ca"));
        assert_eq!(p.user.as_deref(), Some("operator1"));
        assert_eq!(p.host.as_deref(), Some("ws-42.lab"));
    }

    #[test]
    fn connection_validation_anonymous_has_no_user() {
        let raw = crate::spvirit_encode::encode_connection_validation_client_anon(
            0x10000, 1, 0, "anonymous", true,
        );
        let p = PvaConnectionValidationPayload::new(&raw, true, false).expect("decode");
        assert_eq!(p.authz.as_deref(), Some("anonymous"));
        assert_eq!(p.user, None);
        assert_eq!(p.host, None);
    }
}
