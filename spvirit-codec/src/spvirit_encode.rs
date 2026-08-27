//! PVA message encoding helpers.

use crate::spvd_decode::StructureDesc;
use crate::spvd_encode::{
    encode_nt_payload_bitset, encode_nt_payload_bitset_parts, encode_nt_payload_delta,
    encode_nt_payload_filtered, encode_nt_payload_full, encode_nt_scalar_bitset,
    encode_nt_scalar_full, encode_structure_desc, nt_payload_desc,
};
use spvirit_types::{NtPayload, NtScalar};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub fn encode_size_pva(size: usize, is_be: bool) -> Vec<u8> {
    crate::encode_common::encode_size(size, is_be)
}

pub fn encode_string_pva(value: &str, is_be: bool) -> Vec<u8> {
    crate::encode_common::encode_string(value, is_be)
}

fn encode_status_ok() -> Vec<u8> {
    vec![0xFF]
}

fn encode_status_error(message: &str, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x02);
    out.extend_from_slice(&encode_string_pva(message, is_be));
    out.extend_from_slice(&encode_string_pva("", is_be));
    out
}

/// Encode a WARNING status (type byte 0x01) with a message.
pub fn encode_status_warning(message: &str, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x01);
    out.extend_from_slice(&encode_string_pva(message, is_be));
    out.extend_from_slice(&encode_string_pva("", is_be));
    out
}

/// Encode a FATAL status (type byte 0x03) with a message.
pub fn encode_status_fatal(message: &str, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x03);
    out.extend_from_slice(&encode_string_pva(message, is_be));
    out.extend_from_slice(&encode_string_pva("", is_be));
    out
}

pub fn encode_message_error(message: &str, version: u8, is_be: bool) -> Vec<u8> {
    // MESSAGE (cmd=18) payload: ioid(u32) + message_type(u8) + message(string)
    // Use ioid=0 and message_type=2 (error).
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        0u32.to_be_bytes()
    } else {
        0u32.to_le_bytes()
    });
    payload.push(2); // error
    payload.extend_from_slice(&encode_string_pva(message, is_be));
    let mut out = encode_header(true, is_be, false, version, 18, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_header(
    is_server: bool,
    is_be: bool,
    is_control: bool,
    version: u8,
    command: u8,
    payload_length: u32,
) -> Vec<u8> {
    let magic = 0xCA;
    let mut flags = 0u8;
    if is_control {
        flags |= 0x01;
    }
    if is_server {
        flags |= 0x40;
    }
    if is_be {
        flags |= 0x80;
    }
    let mut out = vec![magic, version, flags, command];
    let len_bytes = if is_be {
        payload_length.to_be_bytes()
    } else {
        payload_length.to_le_bytes()
    };
    out.extend_from_slice(&len_bytes);
    out
}

pub fn encode_search_response(
    guid: [u8; 12],
    seq: u32,
    addr: [u8; 16],
    port: u16,
    protocol: &str,
    found: bool,
    cids: &[u32],
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&guid);
    payload.extend_from_slice(&if is_be {
        seq.to_be_bytes()
    } else {
        seq.to_le_bytes()
    });
    payload.extend_from_slice(&addr);
    payload.extend_from_slice(&if is_be {
        port.to_be_bytes()
    } else {
        port.to_le_bytes()
    });
    payload.extend_from_slice(&encode_string_pva(protocol, is_be));
    payload.push(if found { 1 } else { 0 });
    let count = cids.len() as u16;
    payload.extend_from_slice(&if is_be {
        count.to_be_bytes()
    } else {
        count.to_le_bytes()
    });
    for cid in cids {
        payload.extend_from_slice(&if is_be {
            cid.to_be_bytes()
        } else {
            cid.to_le_bytes()
        });
    }

    let mut out = encode_header(true, is_be, false, version, 4, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_connection_validated(is_server: bool, version: u8, is_be: bool) -> Vec<u8> {
    let payload = encode_status_ok();
    let mut out = encode_header(is_server, is_be, false, version, 9, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_control_message(
    is_server: bool,
    is_be: bool,
    version: u8,
    command: u8,
    data: u32,
) -> Vec<u8> {
    // Control messages: header only; size field carries data.
    encode_header(is_server, is_be, true, version, command, data)
}

pub fn encode_connection_validation(
    buffer_size: u32,
    introspection_registry_size: u16,
    auth_methods: &[&str],
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    // Server→client CONNECTION_VALIDATION (cmd=1):
    //   buffer_size(u32) + introspection_registry_size(u16)
    //   + Size(count) + count × string   (auth method names)
    // NOTE: No QoS field in the server→client direction.
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        buffer_size.to_be_bytes()
    } else {
        buffer_size.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        introspection_registry_size.to_be_bytes()
    } else {
        introspection_registry_size.to_le_bytes()
    });
    payload.extend_from_slice(&encode_size_pva(auth_methods.len(), is_be));
    for method in auth_methods {
        payload.extend_from_slice(&encode_string_pva(method, is_be));
    }
    let mut out = encode_header(true, is_be, false, version, 1, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_authnz_user_host(user: &str, host: &str, is_be: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0xFD]);
    if is_be {
        out.extend_from_slice(&1u16.to_be_bytes());
    } else {
        out.extend_from_slice(&1u16.to_le_bytes());
    }
    out.extend_from_slice(&[0x80, 0x00]);
    out.push(0x02);
    out.push(0x04);
    out.extend_from_slice(b"user");
    out.push(0x60);
    out.push(0x04);
    out.extend_from_slice(b"host");
    out.push(0x60);
    out.extend_from_slice(&encode_string_pva(user, is_be));
    out.extend_from_slice(&encode_string_pva(host, is_be));
    out
}

pub fn encode_client_connection_validation(
    buffer_size: u32,
    introspection_registry_size: u16,
    qos: u16,
    authz: &str,
    user: &str,
    host: &str,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        buffer_size.to_be_bytes()
    } else {
        buffer_size.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        introspection_registry_size.to_be_bytes()
    } else {
        introspection_registry_size.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        qos.to_be_bytes()
    } else {
        qos.to_le_bytes()
    });
    payload.extend_from_slice(&encode_string_pva(authz, is_be));
    payload.extend_from_slice(&encode_authnz_user_host(user, host, is_be));
    let mut out = encode_header(false, is_be, false, version, 1, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

/// Encode just the payload of a client->server CONNECTION_VALIDATION message
/// carrying a "ca"-style auth method plus its trailing (user, host) credentials
/// PVStructure. Returns payload bytes only (no PVA header) — suitable for
/// feeding directly to `PvaConnectionValidationPayload::new`.
pub fn encode_connection_validation_client_ca(
    buffer_size: u32,
    introspection_registry_size: u16,
    qos: u16,
    method: &str,
    user: &str,
    host: &str,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        buffer_size.to_be_bytes()
    } else {
        buffer_size.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        introspection_registry_size.to_be_bytes()
    } else {
        introspection_registry_size.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        qos.to_be_bytes()
    } else {
        qos.to_le_bytes()
    });
    payload.extend_from_slice(&encode_string_pva(method, is_be));
    payload.extend_from_slice(&encode_authnz_user_host(user, host, is_be));
    payload
}

/// Encode just the payload of a client->server CONNECTION_VALIDATION message
/// with no trailing credentials structure (e.g. "anonymous" auth). Returns
/// payload bytes only (no PVA header).
pub fn encode_connection_validation_client_anon(
    buffer_size: u32,
    introspection_registry_size: u16,
    qos: u16,
    method: &str,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        buffer_size.to_be_bytes()
    } else {
        buffer_size.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        introspection_registry_size.to_be_bytes()
    } else {
        introspection_registry_size.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        qos.to_be_bytes()
    } else {
        qos.to_le_bytes()
    });
    payload.extend_from_slice(&encode_string_pva(method, is_be));
    payload
}

pub fn encode_create_channel_request(cid: u32, pv_name: &str, version: u8, is_be: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        1u16.to_be_bytes()
    } else {
        1u16.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        cid.to_be_bytes()
    } else {
        cid.to_le_bytes()
    });
    payload.extend_from_slice(&encode_string_pva(pv_name, is_be));
    let mut out = encode_header(false, is_be, false, version, 7, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_get_field_request(
    sid: u32,
    ioid: u32,
    sub_field: Option<&str>,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        sid.to_be_bytes()
    } else {
        sid.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.extend_from_slice(&encode_string_pva(sub_field.unwrap_or(""), is_be));
    let mut out = encode_header(false, is_be, false, version, 17, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_request(
    command: u8,
    sid: u32,
    ioid: u32,
    subcmd: u8,
    extra: &[u8],
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        sid.to_be_bytes()
    } else {
        sid.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(subcmd);
    payload.extend_from_slice(extra);
    let mut out = encode_header(false, is_be, false, version, command, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_get_request(
    sid: u32,
    ioid: u32,
    subcmd: u8,
    extra: &[u8],
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    encode_op_request(10, sid, ioid, subcmd, extra, version, is_be)
}

pub fn encode_put_request(
    sid: u32,
    ioid: u32,
    subcmd: u8,
    extra: &[u8],
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    encode_op_request(11, sid, ioid, subcmd, extra, version, is_be)
}

pub fn encode_monitor_request(
    sid: u32,
    ioid: u32,
    subcmd: u8,
    extra: &[u8],
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    encode_op_request(13, sid, ioid, subcmd, extra, version, is_be)
}

pub fn encode_rpc_request(
    sid: u32,
    ioid: u32,
    subcmd: u8,
    extra: &[u8],
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    encode_op_request(20, sid, ioid, subcmd, extra, version, is_be)
}

pub fn encode_search_request(
    seq: u32,
    flags: u8,
    port: u16,
    reply_addr: [u8; 16],
    pv_requests: &[(u32, &str)],
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        seq.to_be_bytes()
    } else {
        seq.to_le_bytes()
    });
    payload.push(flags);
    payload.extend_from_slice(&[0u8; 3]);
    payload.extend_from_slice(&reply_addr);
    payload.extend_from_slice(&if is_be {
        port.to_be_bytes()
    } else {
        port.to_le_bytes()
    });
    payload.extend_from_slice(&encode_size_pva(1, is_be));
    payload.extend_from_slice(&encode_string_pva("tcp", is_be));
    payload.extend_from_slice(&if is_be {
        (pv_requests.len() as u16).to_be_bytes()
    } else {
        (pv_requests.len() as u16).to_le_bytes()
    });
    for (cid, pv_name) in pv_requests {
        payload.extend_from_slice(&if is_be {
            cid.to_be_bytes()
        } else {
            cid.to_le_bytes()
        });
        payload.extend_from_slice(&encode_string_pva(pv_name, is_be));
    }

    let mut out = encode_header(false, is_be, false, version, 3, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_create_channel_response(cid: u32, sid: u32, version: u8, is_be: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        cid.to_be_bytes()
    } else {
        cid.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        sid.to_be_bytes()
    } else {
        sid.to_le_bytes()
    });
    payload.extend_from_slice(&encode_status_ok());
    let mut out = encode_header(true, is_be, false, version, 7, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_create_channel_error(cid: u32, message: &str, version: u8, is_be: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        cid.to_be_bytes()
    } else {
        cid.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        0u32.to_be_bytes()
    } else {
        0u32.to_le_bytes()
    });
    payload.push(0x01);
    payload.extend_from_slice(&encode_string_pva(message, is_be));
    payload.extend_from_slice(&encode_string_pva("", is_be));
    let mut out = encode_header(true, is_be, false, version, 7, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_get_field_response(
    request_id: u32,
    desc: &StructureDesc,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        request_id.to_be_bytes()
    } else {
        request_id.to_le_bytes()
    });
    payload.extend_from_slice(&encode_status_ok());
    payload.push(0x80);
    payload.extend_from_slice(&encode_structure_desc(desc, is_be));
    let mut out = encode_header(true, is_be, false, version, 17, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_get_field_error(request_id: u32, message: &str, version: u8, is_be: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        request_id.to_be_bytes()
    } else {
        request_id.to_le_bytes()
    });
    payload.extend_from_slice(&encode_status_error(message, is_be));
    let mut out = encode_header(true, is_be, false, version, 17, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_init_response(
    command: u8,
    ioid: u32,
    subcmd: u8,
    desc: &StructureDesc,
    nt: &NtScalar,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(subcmd);
    payload.extend_from_slice(&encode_status_ok());
    payload.push(0x80); // structure type for introspection
    payload.extend_from_slice(&encode_structure_desc(desc, is_be));
    payload.extend_from_slice(&encode_nt_scalar_full(nt, is_be));

    let mut out = encode_header(true, is_be, false, version, command, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_init_response_desc(
    command: u8,
    ioid: u32,
    subcmd: u8,
    desc: &StructureDesc,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(subcmd);
    payload.extend_from_slice(&encode_status_ok());
    payload.push(0x80); // structure type for introspection
    payload.extend_from_slice(&encode_structure_desc(desc, is_be));

    let mut out = encode_header(true, is_be, false, version, command, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_data_response(
    command: u8,
    ioid: u32,
    nt: &NtScalar,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(0x00);
    payload.extend_from_slice(&encode_nt_scalar_bitset(nt, is_be));
    let mut out = encode_header(true, is_be, false, version, command, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_get_data_response_payload(
    ioid: u32,
    payload_value: &NtPayload,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    encode_op_data_response_payload(10, ioid, payload_value, version, is_be)
}

pub fn encode_op_data_response_payload(
    command: u8,
    ioid: u32,
    payload_value: &NtPayload,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(0x00);
    payload.extend_from_slice(&encode_status_ok());
    payload.extend_from_slice(&encode_nt_payload_bitset(payload_value, is_be));
    let mut out = encode_header(true, is_be, false, version, command, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

/// Like [`encode_op_data_response_payload`] but encodes only the fields
/// present in `filtered_desc` (as negotiated in the INIT response).
pub fn encode_op_data_response_filtered(
    command: u8,
    ioid: u32,
    payload_value: &NtPayload,
    filtered_desc: &StructureDesc,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let (bitset, values) = encode_nt_payload_filtered(payload_value, filtered_desc, is_be);
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(0x00);
    payload.extend_from_slice(&encode_status_ok());
    payload.extend_from_slice(&bitset);
    payload.extend_from_slice(&values);
    let mut out = encode_header(true, is_be, false, version, command, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_status_response(
    command: u8,
    ioid: u32,
    subcmd: u8,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(subcmd);
    payload.extend_from_slice(&encode_status_ok());
    let mut out = encode_header(true, is_be, false, version, command, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_status_error_response(
    command: u8,
    ioid: u32,
    subcmd: u8,
    message: &str,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(subcmd);
    payload.extend_from_slice(&encode_status_error(message, is_be));
    let mut out = encode_header(true, is_be, false, version, command, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_rpc_data_response_payload(
    ioid: u32,
    subcmd: u8,
    payload_value: &NtPayload,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let desc = nt_payload_desc(payload_value);
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(subcmd);
    payload.extend_from_slice(&encode_status_ok());
    payload.push(0x80);
    payload.extend_from_slice(&encode_structure_desc(&desc, is_be));
    payload.extend_from_slice(&encode_nt_payload_full(payload_value, is_be));
    let mut out = encode_header(true, is_be, false, version, 20, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_put_get_init_response(
    ioid: u32,
    put_desc: &StructureDesc,
    get_desc: &StructureDesc,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(0x08);
    payload.extend_from_slice(&encode_status_ok());
    payload.push(0x80);
    payload.extend_from_slice(&encode_structure_desc(put_desc, is_be));
    payload.push(0x80);
    payload.extend_from_slice(&encode_structure_desc(get_desc, is_be));
    let mut out = encode_header(true, is_be, false, version, 12, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_put_get_data_response(
    ioid: u32,
    nt: &NtScalar,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    encode_op_put_get_data_response_payload(ioid, &NtPayload::Scalar(nt.clone()), version, is_be)
}

pub fn encode_op_put_get_data_response_payload(
    ioid: u32,
    payload_value: &NtPayload,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(0x00);
    payload.extend_from_slice(&encode_status_ok());
    payload.extend_from_slice(&encode_nt_payload_bitset(payload_value, is_be));
    let mut out = encode_header(true, is_be, false, version, 12, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_put_response(ioid: u32, subcmd: u8, version: u8, is_be: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(subcmd);
    payload.extend_from_slice(&encode_status_ok());
    let mut out = encode_header(true, is_be, false, version, 11, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_put_status_response(
    ioid: u32,
    subcmd: u8,
    message: &str,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(subcmd);
    payload.extend_from_slice(&encode_status_error(message, is_be));
    let mut out = encode_header(true, is_be, false, version, 11, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_put_getput_response(
    ioid: u32,
    nt: &NtScalar,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    encode_op_put_getput_response_payload(ioid, &NtPayload::Scalar(nt.clone()), version, is_be)
}

pub fn encode_op_put_getput_response_payload(
    ioid: u32,
    payload_value: &NtPayload,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(0x40);
    payload.extend_from_slice(&encode_status_ok());
    payload.extend_from_slice(&encode_nt_payload_bitset(payload_value, is_be));
    let mut out = encode_header(true, is_be, false, version, 11, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_put_get_init_error_response(
    ioid: u32,
    message: &str,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(0x08);
    payload.extend_from_slice(&encode_status_error(message, is_be));
    let mut out = encode_header(true, is_be, false, version, 12, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_put_get_data_error_response(
    ioid: u32,
    message: &str,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(0x00);
    payload.extend_from_slice(&encode_status_error(message, is_be));
    let mut out = encode_header(true, is_be, false, version, 12, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_monitor_data_response(
    ioid: u32,
    subcmd: u8,
    nt: &NtScalar,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    encode_monitor_data_response_payload(
        ioid,
        subcmd,
        &NtPayload::Scalar(nt.clone()),
        version,
        is_be,
    )
}

pub fn encode_monitor_data_response_payload(
    ioid: u32,
    subcmd: u8,
    payload_value: &NtPayload,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let (changed_bitset, values) = encode_nt_payload_bitset_parts(payload_value, is_be);
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(subcmd);
    if (subcmd & 0x10) != 0 {
        payload.extend_from_slice(&encode_status_ok());
    }
    payload.extend_from_slice(&changed_bitset);
    payload.extend_from_slice(&values);
    // overrun bitset: empty (after data per spec)
    payload.extend_from_slice(&encode_size_pva(0, is_be));
    let mut out = encode_header(true, is_be, false, version, 13, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

/// Like [`encode_monitor_data_response_payload`] but encodes only the fields
/// present in `filtered_desc`.
pub fn encode_monitor_data_response_filtered(
    ioid: u32,
    subcmd: u8,
    payload_value: &NtPayload,
    filtered_desc: &StructureDesc,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let (bitset, values) = encode_nt_payload_filtered(payload_value, filtered_desc, is_be);
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(subcmd);
    if (subcmd & 0x10) != 0 {
        payload.extend_from_slice(&encode_status_ok());
    }
    payload.extend_from_slice(&bitset);
    payload.extend_from_slice(&values);
    payload.extend_from_slice(&encode_size_pva(0, is_be));
    let mut out = encode_header(true, is_be, false, version, 13, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

/// Encode a sparse monitor-data response containing only fields that changed
/// between `prev_value` and `next_value` (projected onto `filtered_desc`).
/// Returns `None` when nothing changed — the caller should suppress the send
/// entirely and NOT decrement any pipeline credit.
pub fn encode_monitor_data_response_delta(
    ioid: u32,
    subcmd: u8,
    prev_value: &NtPayload,
    next_value: &NtPayload,
    filtered_desc: &StructureDesc,
    version: u8,
    is_be: bool,
) -> Option<Vec<u8>> {
    let (bitset, values) = encode_nt_payload_delta(prev_value, next_value, filtered_desc, is_be)?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(subcmd);
    if (subcmd & 0x10) != 0 {
        payload.extend_from_slice(&encode_status_ok());
    }
    payload.extend_from_slice(&bitset);
    payload.extend_from_slice(&values);
    payload.extend_from_slice(&encode_size_pva(0, is_be));
    let mut out = encode_header(true, is_be, false, version, 13, payload.len() as u32);
    out.extend_from_slice(&payload);
    Some(out)
}

pub fn encode_destroy_channel_response(sid: u32, cid: u32, version: u8, is_be: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        sid.to_be_bytes()
    } else {
        sid.to_le_bytes()
    });
    payload.extend_from_slice(&if is_be {
        cid.to_be_bytes()
    } else {
        cid.to_le_bytes()
    });
    let mut out = encode_header(true, is_be, false, version, 8, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_op_error(
    command: u8,
    subcmd: u8,
    ioid: u32,
    message: &str,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&if is_be {
        ioid.to_be_bytes()
    } else {
        ioid.to_le_bytes()
    });
    payload.push(subcmd);
    payload.push(0x02); // ERROR
    payload.extend_from_slice(&encode_string_pva(message, is_be));
    payload.extend_from_slice(&encode_string_pva("", is_be));
    let mut out = encode_header(true, is_be, false, version, command, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_beacon(
    guid: [u8; 12],
    seq: u8,
    change_count: u16,
    addr: [u8; 16],
    port: u16,
    protocol: &str,
    version: u8,
    is_be: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&guid);
    payload.push(0x00); // flags
    payload.push(seq);
    payload.extend_from_slice(&if is_be {
        change_count.to_be_bytes()
    } else {
        change_count.to_le_bytes()
    });
    payload.extend_from_slice(&addr);
    payload.extend_from_slice(&if is_be {
        port.to_be_bytes()
    } else {
        port.to_le_bytes()
    });
    payload.extend_from_slice(&encode_string_pva(protocol, is_be));
    // serverStatus: NULL FieldDesc (0xFF) means "no server status".
    // Writing a PVA string here instead would be misinterpreted as a TypeCode
    // by compliant clients (e.g. Phoebus), causing a BufferUnderflowException.
    payload.push(0xFF);
    let mut out = encode_header(true, is_be, false, version, 0, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

// ---------------------------------------------------------------------------
// IP address ↔ 16-byte PVA wire-format conversion helpers
// ---------------------------------------------------------------------------

/// Convert an [`IpAddr`] to the 16-byte PVA wire representation.
///
/// IPv4 addresses are stored as IPv4-mapped IPv6 (`::ffff:a.b.c.d`).
/// Native IPv6 addresses are stored as-is.
pub fn ip_to_bytes(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(v4) => {
            let mut out = [0u8; 16];
            out[10] = 0xFF;
            out[11] = 0xFF;
            out[12..16].copy_from_slice(&v4.octets());
            out
        }
        IpAddr::V6(v6) => v6.octets(),
    }
}

/// Decode a 16-byte PVA address field to an [`IpAddr`].
///
/// Returns `None` for all-zeros (unspecified).
/// IPv4-mapped addresses (`::ffff:a.b.c.d`) are returned as [`IpAddr::V4`].
pub fn ip_from_bytes(addr: &[u8; 16]) -> Option<IpAddr> {
    if addr.iter().all(|&b| b == 0) {
        return None;
    }
    // IPv4-mapped IPv6 address ::ffff:a.b.c.d
    if addr[0..10].iter().all(|&b| b == 0) && addr[10] == 0xFF && addr[11] == 0xFF {
        return Some(IpAddr::V4(Ipv4Addr::new(
            addr[12], addr[13], addr[14], addr[15],
        )));
    }
    Some(IpAddr::V6(Ipv6Addr::from(*addr)))
}

pub fn socket_addr_from_pva_bytes(addr: [u8; 16], port: u16) -> Option<SocketAddr> {
    ip_from_bytes(&addr).map(|ip| SocketAddr::new(ip, port))
}

/// Format a 16-byte PVA address field as a human-readable IP string.
///
/// All-zeros → `"0.0.0.0"`, IPv4-mapped → dotted-quad, otherwise IPv6 notation.
pub fn format_pva_address(addr: &[u8; 16]) -> String {
    match ip_from_bytes(addr) {
        Some(ip) => ip.to_string(),
        None => "0.0.0.0".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epics_decode::{PvaPacket, PvaPacketCommand};

    #[test]
    fn encode_decode_connection_validation_roundtrip() {
        let msg = encode_connection_validation(4096, 2, &["anonymous", "ca"], 2, true);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::ConnectionValidation(payload) => {
                assert!(payload.is_server);
                assert_eq!(payload.buffer_size, 4096);
                assert_eq!(payload.introspection_registry_size, 2);
                assert_eq!(payload.qos, 0); // server→client has no qos
                assert_eq!(payload.authz.as_deref(), Some("anonymous"));
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_client_connection_validation_roundtrip() {
        let msg = encode_client_connection_validation(
            87_040, 32_767, 0, "ca", "alice", "host1", 2, false,
        );
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::ConnectionValidation(payload) => {
                assert!(!payload.is_server);
                assert_eq!(payload.buffer_size, 87_040);
                assert_eq!(payload.introspection_registry_size, 32_767);
                assert_eq!(payload.qos, 0);
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_search_response_roundtrip() {
        let guid = [1u8; 12];
        let seq = 42;
        let addr = [0u8; 16];
        let port = 5075;
        let cids = vec![100u32, 101u32];
        let msg = encode_search_response(guid, seq, addr, port, "tcp", true, &cids, 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::SearchResponse(payload) => {
                assert_eq!(payload.guid, guid);
                assert_eq!(payload.seq, seq);
                assert_eq!(payload.port, port);
                assert!(payload.found);
                assert_eq!(payload.cids, cids);
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_connection_validated_roundtrip() {
        let msg = encode_connection_validated(true, 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::ConnectionValidated(payload) => {
                // 0xFF means "OK" which decodes to None in our decoder.
                assert!(payload.status.is_none());
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn get_data_response_includes_status() {
        let nt = NtScalar::from_value(spvirit_types::ScalarValue::F64(1.0));
        let msg = encode_op_get_data_response_payload(0x11223344, &NtPayload::Scalar(nt), 2, false);
        assert!(msg.len() > 13);
        let status_offset = 8 + 4 + 1;
        assert_eq!(msg[status_offset], 0xFF);

        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::Op(op) => {
                assert_eq!(op.command, 10);
                assert_eq!(op.subcmd, 0x00);
                assert!(!op.body.is_empty());
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn put_get_init_includes_two_descriptors() {
        let nt = NtScalar::from_value(spvirit_types::ScalarValue::F64(1.0));
        let desc = crate::spvd_encode::nt_scalar_desc(&nt.value);
        let msg = encode_op_put_get_init_response(0x01020304, &desc, &desc, 2, false);

        let payload = &msg[8..];
        assert!(payload.len() > 6);
        // ioid(4) + subcmd(1) + status(1)
        assert_eq!(payload[5], 0xFF);
        let rest = &payload[6..];
        let first = rest.first().copied().unwrap_or(0);
        assert_eq!(first, 0x80);
        let second_pos = rest.iter().skip(1).position(|b| *b == 0x80);
        assert!(second_pos.is_some(), "expected second descriptor marker");
    }

    #[test]
    fn put_get_data_includes_status() {
        let nt = NtScalar::from_value(spvirit_types::ScalarValue::F64(2.0));
        let msg = encode_op_put_get_data_response(0x55667788, &nt, 2, false);
        assert!(msg.len() > 13);
        let status_offset = 8 + 4 + 1;
        assert_eq!(msg[status_offset], 0xFF);
    }

    #[test]
    fn put_getput_response_encodes_subcmd_0x40() {
        let nt = NtScalar::from_value(spvirit_types::ScalarValue::F64(2.0));
        let msg = encode_op_put_getput_response(0x01020304, &nt, 2, false);
        assert!(msg.len() > 13);
        let status_offset = 8 + 4 + 1;
        assert_eq!(msg[status_offset], 0xFF);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::Op(op) => {
                assert_eq!(op.command, 11);
                assert_eq!(op.subcmd, 0x40);
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_get_field_response_roundtrip() {
        let desc = StructureDesc {
            struct_id: Some("epics:nt/NTScalar:1.0".to_string()),
            fields: vec![crate::spvd_decode::FieldDesc {
                name: "value".to_string(),
                field_type: crate::spvd_decode::FieldType::Scalar(
                    crate::spvd_decode::TypeCode::String,
                ),
            }],
        };
        let msg = encode_get_field_response(11, &desc, 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::GetField(payload) => {
                assert!(payload.is_server);
                assert_eq!(payload.cid, 11);
                assert!(payload.status.is_none());
                let intro = payload.introspection.expect("introspection");
                assert_eq!(intro.fields.len(), 1);
                assert_eq!(intro.fields[0].name, "value");
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_get_field_error_roundtrip() {
        let msg = encode_get_field_error(7, "listing disabled", 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::GetField(payload) => {
                assert!(payload.is_server);
                assert_eq!(payload.cid, 7);
                let status = payload.status.expect("status");
                assert_eq!(status.code, 0x02);
                assert_eq!(status.message.as_deref(), Some("listing disabled"));
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_create_channel_request_roundtrip() {
        let msg = encode_create_channel_request(7, "TEST:PV", 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::CreateChannel(payload) => {
                assert!(!payload.is_server);
                assert_eq!(payload.channels, vec![(7, "TEST:PV".to_string())]);
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_get_field_request_roundtrip() {
        let msg = encode_get_field_request(9, 1, Some("*"), 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::GetField(payload) => {
                assert!(!payload.is_server);
                assert_eq!(payload.sid, Some(9));
                assert_eq!(payload.ioid, Some(1));
                assert_eq!(payload.field_name.as_deref(), Some("*"));
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_get_request_roundtrip() {
        let msg = encode_get_request(1, 2, 0x08, &[0xfd, 0x02, 0x00], 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::Op(op) => {
                assert_eq!(op.command, 10);
                assert_eq!(op.sid_or_cid, 1);
                assert_eq!(op.ioid, 2);
                assert_eq!(op.subcmd, 0x08);
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_put_request_roundtrip() {
        let msg = encode_put_request(3, 4, 0x40, &[0xAA], 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::Op(op) => {
                assert_eq!(op.command, 11);
                assert_eq!(op.sid_or_cid, 3);
                assert_eq!(op.ioid, 4);
                assert_eq!(op.subcmd, 0x40);
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_monitor_request_roundtrip() {
        let msg = encode_monitor_request(5, 6, 0x44, &[], 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::Op(op) => {
                assert_eq!(op.command, 13);
                assert_eq!(op.sid_or_cid, 5);
                assert_eq!(op.ioid, 6);
                assert_eq!(op.subcmd, 0x44);
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_rpc_request_roundtrip() {
        let msg = encode_rpc_request(7, 8, 0x00, &[0x80, 0x00], 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::Op(op) => {
                assert_eq!(op.command, 20);
                assert_eq!(op.sid_or_cid, 7);
                assert_eq!(op.ioid, 8);
                assert_eq!(op.subcmd, 0x00);
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn encode_decode_search_request_roundtrip() {
        let seq = 1234;
        let cid = 42;
        let port = 5076;
        let reply_addr = ip_to_bytes(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)));
        let requests = [(cid, "TEST:PV")];
        let msg = encode_search_request(seq, 0x81, port, reply_addr, &requests, 2, false);
        let mut pkt = PvaPacket::new(&msg);
        let cmd = pkt.decode_payload().expect("decoded");
        match cmd {
            PvaPacketCommand::Search(payload) => {
                assert_eq!(payload.seq, seq);
                assert_eq!(payload.mask, 0x81);
                assert_eq!(payload.addr, reply_addr);
                assert_eq!(payload.port, port);
                assert_eq!(payload.protocols, vec!["tcp".to_string()]);
                assert_eq!(payload.pv_requests.len(), 1);
                assert_eq!(payload.pv_requests[0].0, cid);
                assert_eq!(payload.pv_requests[0].1, "TEST:PV");
            }
            other => panic!("unexpected decode: {:?}", other),
        }
    }

    #[test]
    fn socket_addr_from_pva_bytes_decodes_ipv4_mapped() {
        let addr = ip_to_bytes(IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40)));
        assert_eq!(
            socket_addr_from_pva_bytes(addr, 5075),
            Some("10.20.30.40:5075".parse().unwrap())
        );
    }

    #[test]
    fn socket_addr_from_pva_bytes_decodes_ipv6() {
        let addr = ip_to_bytes(IpAddr::V6("2001:db8::1".parse().unwrap()));
        assert_eq!(
            socket_addr_from_pva_bytes(addr, 5075),
            Some("[2001:db8::1]:5075".parse().unwrap())
        );
    }

    #[test]
    fn socket_addr_from_pva_bytes_returns_none_for_unspecified() {
        assert_eq!(socket_addr_from_pva_bytes([0u8; 16], 5075), None);
    }
}
