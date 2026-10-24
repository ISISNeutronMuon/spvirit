use std::collections::HashMap;

use spvirit_codec::spvd_decode::{DecodedValue, PvdDecoder};
use spvirit_codec::spvd_encode::{encode_nt_payload_full, encode_structure_desc, nt_payload_desc};
use spvirit_tools::spvirit_server::types::{
    NdCodec, NdDimension, NtAttribute, NtNdArray, NtPayload, NtScalarArray, NtTable, NtTableColumn,
    ScalarArrayValue, ScalarValue,
};

fn roundtrip_payload(payload: &NtPayload) -> DecodedValue {
    let desc = nt_payload_desc(payload);
    let desc_bytes = encode_structure_desc(&desc, false);

    let mut bytes = Vec::new();
    bytes.push(0x80);
    bytes.extend_from_slice(&desc_bytes);
    bytes.extend_from_slice(&encode_nt_payload_full(payload, false));

    let decoder = PvdDecoder::new(false);
    let parsed_desc = decoder
        .parse_introspection(&bytes)
        .expect("parse introspection");
    let start = 1 + desc_bytes.len();
    let (decoded, consumed) = decoder
        .decode_structure(&bytes[start..], &parsed_desc)
        .expect("decode structure");
    assert!(consumed > 0);
    decoded
}

fn payload_has_value_field(decoded: &DecodedValue) {
    let DecodedValue::Structure(fields) = decoded else {
        panic!("expected structure");
    };
    assert!(fields.iter().any(|(name, _)| name == "value"));
}

#[test]
fn nt_scalar_array_roundtrip_all_scalar_types() {
    let cases = vec![
        ScalarArrayValue::Bool(vec![true, false]),
        ScalarArrayValue::I8(vec![1, -2, 3]),
        ScalarArrayValue::I16(vec![10, -20]),
        ScalarArrayValue::I32(vec![100, -200]),
        ScalarArrayValue::I64(vec![1000, -2000]),
        ScalarArrayValue::U8(vec![1, 2, 3]),
        ScalarArrayValue::U16(vec![10, 20]),
        ScalarArrayValue::U32(vec![100, 200]),
        ScalarArrayValue::U64(vec![1000, 2000]),
        ScalarArrayValue::F32(vec![1.25, 2.5]),
        // Not PI/E approximations: clippy::approx_constant is deny-by-default
        // and would fail the whole workspace clippy run.
        ScalarArrayValue::F64(vec![3.25, -2.75]),
        ScalarArrayValue::Str(vec!["a".to_string(), "b".to_string()]),
    ];

    for value in cases {
        let payload = NtPayload::ScalarArray(NtScalarArray::from_value(value));
        let decoded = roundtrip_payload(&payload);
        payload_has_value_field(&decoded);
    }
}

#[test]
fn nt_table_column_length_validation() {
    let table = NtTable {
        labels: vec!["x".to_string(), "y".to_string()],
        columns: vec![
            NtTableColumn {
                name: "x".to_string(),
                values: ScalarArrayValue::F64(vec![1.0, 2.0, 3.0]),
            },
            NtTableColumn {
                name: "y".to_string(),
                values: ScalarArrayValue::I32(vec![10, 20]),
            },
        ],
        descriptor: Some("bad table".to_string()),
        alarm: None,
        time_stamp: None,
    };

    let err = table.validate().expect_err("validation should fail");
    assert!(err.contains("length"));
}

#[test]
fn nt_ndarray_roundtrip_required_fields() {
    let mut codec_params = HashMap::new();
    codec_params.insert("quality".to_string(), "high".to_string());

    let ndarray = NtNdArray {
        value: ScalarArrayValue::U8(vec![1, 2, 3, 4]),
        codec: NdCodec {
            name: "blosc".to_string(),
            parameters: codec_params,
        },
        compressed_size: 4,
        uncompressed_size: 4,
        dimension: vec![NdDimension {
            size: 4,
            offset: 0,
            full_size: 4,
            binning: 1,
            reverse: false,
        }],
        unique_id: 42,
        data_time_stamp: Default::default(),
        attribute: vec![NtAttribute {
            name: "ColorMode".to_string(),
            value: ScalarValue::I32(1),
            descriptor: "Color mode attribute".to_string(),
            source_type: 0,
            source: "cam".to_string(),
        }],
        descriptor: Some("camera frame".to_string()),
        alarm: None,
        time_stamp: None,
        display: None,
    };
    ndarray.validate().expect("ndarray validates");

    let payload = NtPayload::NdArray(ndarray);
    let decoded = roundtrip_payload(&payload);
    payload_has_value_field(&decoded);

    let DecodedValue::Structure(fields) = decoded else {
        panic!("expected structure");
    };
    assert!(fields.iter().any(|(name, _)| name == "codec"));
    assert!(fields.iter().any(|(name, _)| name == "compressedSize"));
    assert!(fields.iter().any(|(name, _)| name == "uncompressedSize"));
    assert!(fields.iter().any(|(name, _)| name == "dimension"));
    assert!(fields.iter().any(|(name, _)| name == "attribute"));
}

#[test]
fn nt_fixture_vectors_are_present() {
    assert!(!include_bytes!("protocol/spec_vectors/nt_scalar_array_f64_le.bin").is_empty());
    assert!(!include_bytes!("protocol/spec_vectors/nt_table_le.bin").is_empty());
    assert!(!include_bytes!("protocol/spec_vectors/nt_ndarray_le.bin").is_empty());
}
