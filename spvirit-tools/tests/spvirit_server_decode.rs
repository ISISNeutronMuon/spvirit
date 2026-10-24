//! Relocated from `spvirit-tools/src/bin/spvirit_server.rs`'s `#[cfg(test)]`
//! module. `spvirit-server/src/decode.rs` has no test module of its own, so
//! these three cases are the only coverage the PUT-body decoder has anywhere
//! in the workspace. They moved here (rather than being deleted alongside the
//! bin's duplicate of the decoder) so that coverage survives.

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_codec::spvd_encode::{encode_nt_scalar_bitset_parts, nt_scalar_desc};
use spvirit_codec::spvirit_encode::encode_size_pva;
use spvirit_server::decode::decode_put_body;
use spvirit_server::types::{NtScalar, ScalarValue};

#[test]
fn decode_put_body_accepts_status_prefix() {
    let nt = NtScalar::from_value(ScalarValue::F64(2.5));
    let desc = nt_scalar_desc(&nt.value);
    let (bitset, values) = encode_nt_scalar_bitset_parts(&nt, false);
    let mut body = Vec::new();
    body.push(0xFF);
    body.extend_from_slice(&bitset);
    body.extend_from_slice(&values);

    let decoded = decode_put_body(&body, &desc, false);
    assert!(decoded.is_some());
}

#[test]
fn decode_put_body_accepts_bitset_without_struct_bit() {
    let nt = NtScalar::from_value(ScalarValue::F64(0.0));
    let desc = nt_scalar_desc(&nt.value);
    let mut body = Vec::new();
    body.extend_from_slice(&encode_size_pva(1, false));
    body.push(0x01); // bit0 set (value) with no struct bit
    body.extend_from_slice(&30.0f64.to_le_bytes());

    let decoded = decode_put_body(&body, &desc, false).expect("decoded");
    if let DecodedValue::Structure(fields) = decoded {
        let value = fields
            .iter()
            .find(|(name, _)| name == "value")
            .expect("value field");
        assert!(matches!(value.1, DecodedValue::Float64(v) if (v - 30.0).abs() < 1e-6));
    } else {
        panic!("expected structure");
    }
}

#[test]
fn decode_put_body_accepts_value_only_payload() {
    let nt = NtScalar::from_value(ScalarValue::F64(0.0));
    let desc = nt_scalar_desc(&nt.value);
    let body = 12.5f64.to_le_bytes();

    let decoded = decode_put_body(&body, &desc, false).expect("decoded");
    if let DecodedValue::Structure(fields) = decoded {
        let value = fields
            .iter()
            .find(|(name, _)| name == "value")
            .expect("value field");
        assert!(matches!(value.1, DecodedValue::Float64(v) if (v - 12.5).abs() < 1e-6));
    } else {
        panic!("expected structure");
    }
}
