mod protocol;

use protocol::coverage_matrix::command_coverage;
use spvirit_codec::epics_decode::{PvaPacket, PvaPacketCommand};
use spvirit_codec::spvd_decode::PvdDecoder;
use spvirit_codec::spvirit_encode::encode_header;

fn build_frame(
    command: u8,
    payload: &[u8],
    is_server: bool,
    is_be: bool,
    is_control: bool,
) -> Vec<u8> {
    let mut out = encode_header(
        is_server,
        is_be,
        is_control,
        2,
        command,
        payload.len() as u32,
    );
    out.extend_from_slice(payload);
    out
}

fn minimal_payload_for_command(command: u8) -> Vec<u8> {
    match command {
        0 => {
            let mut payload = vec![0u8; 34];
            payload.push(0); // protocol=""
            payload.push(0); // status_if=""
            payload
        }
        1 => vec![0u8; 8],
        2 => vec![],
        3 => {
            let mut payload = vec![0u8; 26];
            payload.push(0); // protocol count
            payload.extend_from_slice(&0u16.to_le_bytes()); // pv count
            payload
        }
        4 => {
            let mut payload = vec![0u8; 34];
            payload.push(0); // protocol=""
            payload
        }
        5 => vec![0],
        6 => vec![0xFF],
        7 => 0u16.to_le_bytes().to_vec(),
        8 => vec![0u8; 8],
        9 => vec![0xFF],
        10 | 11 | 12 | 13 | 14 | 16 | 20 => {
            let mut payload = Vec::new();
            payload.extend_from_slice(&1u32.to_le_bytes()); // sid
            payload.extend_from_slice(&7u32.to_le_bytes()); // ioid
            payload.push(0x00); // subcmd
            payload
        }
        15 => 7u32.to_le_bytes().to_vec(),
        17 => 1u32.to_le_bytes().to_vec(),
        18 => vec![0xFF],
        19 => vec![0], // zero entries
        21 => 7u32.to_le_bytes().to_vec(),
        22 => vec![0u8; 16],
        _ => vec![],
    }
}

#[test]
fn decode_surface_covers_all_protocol_commands() {
    for entry in command_coverage() {
        let payload = minimal_payload_for_command(entry.command);
        let frame = build_frame(entry.command, &payload, false, false, false);
        let mut packet = PvaPacket::new(&frame);
        let decoded = packet.decode_payload();
        assert!(
            decoded.is_some(),
            "decode failed for command {}",
            entry.command
        );

        let decoded = decoded.expect("decoded");
        match (entry.command, decoded) {
            (0, PvaPacketCommand::Beacon(_))
            | (1, PvaPacketCommand::ConnectionValidation(_))
            | (3, PvaPacketCommand::Search(_))
            | (4, PvaPacketCommand::SearchResponse(_))
            | (5, PvaPacketCommand::AuthNZ(_))
            | (6, PvaPacketCommand::AclChange(_))
            | (7, PvaPacketCommand::CreateChannel(_))
            | (8, PvaPacketCommand::DestroyChannel(_))
            | (9, PvaPacketCommand::ConnectionValidated(_))
            | (10, PvaPacketCommand::Op(_))
            | (11, PvaPacketCommand::Op(_))
            | (12, PvaPacketCommand::Op(_))
            | (13, PvaPacketCommand::Op(_))
            | (14, PvaPacketCommand::Op(_))
            | (15, PvaPacketCommand::DestroyRequest(_))
            | (16, PvaPacketCommand::Op(_))
            | (17, PvaPacketCommand::GetField(_))
            | (18, PvaPacketCommand::Message(_))
            | (19, PvaPacketCommand::MultipleData(_))
            | (20, PvaPacketCommand::Op(_))
            | (21, PvaPacketCommand::CancelRequest(_))
            | (22, PvaPacketCommand::OriginTag(_)) => {}
            (2, PvaPacketCommand::Echo(_)) => {}
            (id, other) => panic!("unexpected decoded variant for cmd {}: {:?}", id, other),
        }
    }
}

#[test]
fn malformed_packet_length_is_rejected() {
    let mut frame = encode_header(false, false, false, 2, 7, 16);
    frame.extend_from_slice(&[0u8; 2]);
    let mut packet = PvaPacket::new(&frame);
    assert!(packet.decode_payload().is_none());
}

#[test]
fn malformed_reserved_flag_bits_mark_header_invalid() {
    let mut raw = vec![0xCA, 0x02, 0x0E, 0x07];
    raw.extend_from_slice(&0u32.to_le_bytes());
    let packet = PvaPacket::new(&raw);
    assert!(!packet.is_valid());
}

#[test]
fn malformed_magic_mark_header_invalid() {
    let mut raw = vec![0xCB, 0x02, 0x00, 0x07];
    raw.extend_from_slice(&0u32.to_le_bytes());
    let packet = PvaPacket::new(&raw);
    assert!(!packet.is_valid());
}

#[test]
fn malformed_unknown_pvd_type_decodes_to_none() {
    let decoder = PvdDecoder::new(false);
    let field_desc = [1u8, b'v', 0x06];
    assert!(decoder.parse_field_desc(&field_desc).is_err());
}

#[test]
fn golden_spec_vectors_decode() {
    let vectors: &[(&str, &[u8], u8)] = &[
        (
            "cmd01_connection_validation_le.bin",
            include_bytes!("protocol/spec_vectors/cmd01_connection_validation_le.bin"),
            1,
        ),
        (
            "cmd07_create_channel_req_le.bin",
            include_bytes!("protocol/spec_vectors/cmd07_create_channel_req_le.bin"),
            7,
        ),
        (
            "cmd10_get_init_req_le.bin",
            include_bytes!("protocol/spec_vectors/cmd10_get_init_req_le.bin"),
            10,
        ),
        (
            "cmd11_put_init_req_le.bin",
            include_bytes!("protocol/spec_vectors/cmd11_put_init_req_le.bin"),
            11,
        ),
        (
            "cmd13_monitor_init_req_le.bin",
            include_bytes!("protocol/spec_vectors/cmd13_monitor_init_req_le.bin"),
            13,
        ),
        (
            "cmd18_status_error_message_le.bin",
            include_bytes!("protocol/spec_vectors/cmd18_status_error_message_le.bin"),
            18,
        ),
    ];

    for (name, bytes, expected_cmd) in vectors {
        let mut packet = PvaPacket::new(bytes);
        assert_eq!(packet.header.command, *expected_cmd, "vector {}", name);
        let decoded = packet.decode_payload();
        assert!(decoded.is_some(), "vector {} failed to decode", name);
    }
}

// ─── Wire format locks: nested pvRequest + filtered/delta MONITOR frames ─────

#[test]
fn pv_request_nested_roundtrip_locks_wire_format() {
    use spvirit_codec::spvd_encode::{decode_pv_request_fields, encode_pv_request};

    // Empty request → decodes to None (all fields).
    let empty = encode_pv_request(&[], false);
    assert!(decode_pv_request_fields(&empty, false).is_none());

    // Flat top-level field.
    let flat = encode_pv_request(&["value"], false);
    let paths = decode_pv_request_fields(&flat, false).expect("flat paths");
    assert_eq!(paths, vec!["value".to_string()]);

    // Nested dotted path → round-trips to the same dotted path.
    let nested = encode_pv_request(&["alarm.severity"], false);
    let paths = decode_pv_request_fields(&nested, false).expect("nested paths");
    assert_eq!(paths, vec!["alarm.severity".to_string()]);

    // Multiple, mixed nesting.
    let mixed = encode_pv_request(&["value", "alarm.severity", "timeStamp"], false);
    let paths = decode_pv_request_fields(&mixed, false).expect("mixed paths");
    assert_eq!(
        paths,
        vec![
            "value".to_string(),
            "alarm.severity".to_string(),
            "timeStamp".to_string(),
        ]
    );
}

#[test]
fn encode_nt_payload_filtered_locks_bitset_and_projection() {
    use spvirit_codec::spvd_decode::{DecodedValue, PvdDecoder};
    use spvirit_codec::spvd_encode::{
        encode_nt_payload_filtered, encode_structure_desc, filter_structure_desc, nt_payload_desc,
    };
    use spvirit_types::{NtPayload, NtScalar, ScalarValue};

    let mut nt = NtScalar::from_value(ScalarValue::F64(1.25));
    nt.alarm_severity = 2;
    let payload = NtPayload::Scalar(nt);

    let full_desc = nt_payload_desc(&payload);
    let desc = filter_structure_desc(&full_desc, &["alarm.severity".to_string()]);

    // Top level has exactly one field: `alarm`.
    assert_eq!(desc.fields.len(), 1);
    assert_eq!(desc.fields[0].name, "alarm");

    let (bitset, values) = encode_nt_payload_filtered(&payload, &desc, false);

    // Bitset non-empty; bit 0 (whole-struct) set for initial full-projection
    // frame. Lock that bit 0 of the first byte is 1.
    assert!(!bitset.is_empty(), "bitset must be present");
    // Bitset format is size-prefixed; locate the first data byte.
    // size-prefix is 1 byte (PVA size<255); data follows.
    assert!(bitset[0] > 0, "bitset size byte non-zero");
    let data_byte = bitset[1];
    assert_eq!(data_byte & 0x01, 0x01, "bit 0 (whole-struct) must be set");

    // Values decode cleanly against the filtered descriptor and expose
    // exactly `{alarm: {severity}}`.
    let desc_bytes = encode_structure_desc(&desc, false);
    let mut pvd = Vec::with_capacity(1 + desc_bytes.len() + values.len());
    pvd.push(0x80);
    pvd.extend_from_slice(&desc_bytes);
    pvd.extend_from_slice(&values);
    let decoder = PvdDecoder::new(false);
    let parsed = decoder.parse_introspection(&pvd).expect("desc parse");
    let (decoded, _consumed) = decoder
        .decode_structure(&pvd[1 + desc_bytes.len()..], &parsed)
        .expect("value decode");

    let DecodedValue::Structure(top) = decoded else {
        panic!("expected top-level struct");
    };
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].0, "alarm");
    let DecodedValue::Structure(ref alarm) = top[0].1 else {
        panic!("alarm must be a struct");
    };
    assert_eq!(alarm.len(), 1);
    assert_eq!(alarm[0].0, "severity");
}

#[test]
fn encode_nt_payload_delta_is_none_when_filtered_view_unchanged() {
    use spvirit_codec::spvd_encode::{
        encode_nt_payload_delta, filter_structure_desc, nt_payload_desc,
    };
    use spvirit_types::{NtPayload, NtScalar, ScalarValue};

    let mut a = NtScalar::from_value(ScalarValue::F64(1.0));
    a.alarm_severity = 0;
    let mut b = NtScalar::from_value(ScalarValue::F64(2.0)); // value changed
    b.alarm_severity = 0; // but severity unchanged
    let pa = NtPayload::Scalar(a);
    let pb = NtPayload::Scalar(b);

    let full_desc = nt_payload_desc(&pa);
    let desc = filter_structure_desc(&full_desc, &["alarm.severity".to_string()]);

    let delta = encode_nt_payload_delta(&pa, &pb, &desc, false);
    assert!(
        delta.is_none(),
        "delta must be None when selected fields unchanged"
    );
}

#[test]
fn encode_nt_payload_delta_emits_bytes_when_selected_field_changes() {
    use spvirit_codec::spvd_encode::{
        encode_nt_payload_delta, filter_structure_desc, nt_payload_desc,
    };
    use spvirit_types::{NtPayload, NtScalar, ScalarValue};

    let mut a = NtScalar::from_value(ScalarValue::F64(1.0));
    a.alarm_severity = 0;
    let mut b = NtScalar::from_value(ScalarValue::F64(1.0));
    b.alarm_severity = 2;
    let pa = NtPayload::Scalar(a);
    let pb = NtPayload::Scalar(b);

    let full_desc = nt_payload_desc(&pa);
    let desc = filter_structure_desc(&full_desc, &["alarm.severity".to_string()]);

    let (bitset, values) =
        encode_nt_payload_delta(&pa, &pb, &desc, false).expect("delta expected on change");
    assert!(!bitset.is_empty());
    assert!(!values.is_empty(), "changed severity must emit bytes");
}
