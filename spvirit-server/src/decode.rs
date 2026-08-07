//! PUT body decoding utilities.
//!
//! The PVA PUT command carries a variable-format payload that encodes field
//! updates.  Different clients encode it in slightly different ways, so the
//! decoder tries several strategies in order.

use spvirit_codec::spvd_decode::{DecodedValue, PvdDecoder, StructureDesc};
use spvirit_codec::spvd_encode::encode_size_pvd;

/// Decode a PUT body payload using the known structure descriptor.
///
/// Tries multiple strategies to handle the various PUT encodings produced by
/// different PVA clients (standard bitset, status-prefixed, shifted bitset,
/// value-only).
pub fn decode_put_body(body: &[u8], desc: &StructureDesc, is_be: bool) -> Option<DecodedValue> {
    let decoder = PvdDecoder::new(is_be);
    if let Ok((value, _)) = decoder.decode_structure_with_bitset(body, desc) {
        if !decoded_is_empty(&value) {
            return Some(value);
        }
    }
    if !body.is_empty() && body[0] == 0xFF {
        if let Ok((value, _)) = decoder.decode_structure_with_bitset(&body[1..], desc) {
            if !decoded_is_empty(&value) {
                return Some(value);
            }
        }
    }
    if let Some(value) = decode_put_body_shifted_bitset(body, desc, is_be) {
        return Some(value);
    }
    if let Some(value) = decode_put_body_value_only(body, desc, is_be) {
        return Some(value);
    }
    None
}

fn decoded_is_empty(value: &DecodedValue) -> bool {
    matches!(value, DecodedValue::Structure(fields) if fields.is_empty())
}

fn decode_put_body_shifted_bitset(
    body: &[u8],
    desc: &StructureDesc,
    is_be: bool,
) -> Option<DecodedValue> {
    let decoder = PvdDecoder::new(is_be);
    let (size, consumed) = decoder.decode_size(body).ok()?;
    if size == 0 || body.len() < consumed + size {
        return None;
    }
    let bitset = &body[consumed..consumed + size];
    let data = &body[consumed + size..];
    let shifted = shift_bitset_left(bitset, 1);
    let mut shifted_body = Vec::new();
    shifted_body.extend_from_slice(&encode_size_pvd(shifted.len(), is_be));
    shifted_body.extend_from_slice(&shifted);
    shifted_body.extend_from_slice(data);
    decoder
        .decode_structure_with_bitset(&shifted_body, desc)
        .ok()
        .map(|(value, _)| value)
        .filter(|value| !decoded_is_empty(value))
}

fn decode_put_body_value_only(
    body: &[u8],
    desc: &StructureDesc,
    is_be: bool,
) -> Option<DecodedValue> {
    let decoder = PvdDecoder::new(is_be);
    if let Ok((size, consumed)) = decoder.decode_size(body) {
        if consumed + size <= body.len() {
            let data = &body[consumed + size..];
            if let Some(value) = decode_value_only_from_data(data, desc, &decoder) {
                return Some(value);
            }
        }
    }
    decode_value_only_from_data(body, desc, &decoder)
}

fn decode_value_only_from_data(
    data: &[u8],
    desc: &StructureDesc,
    decoder: &PvdDecoder,
) -> Option<DecodedValue> {
    let value_field = desc.fields.iter().find(|f| f.name == "value")?;
    decoder
        .decode_value(data, &value_field.field_type)
        .ok()
        .map(|(value, _)| DecodedValue::Structure(vec![("value".to_string(), value)]))
}

/// Shift a bitset left by `shift` bit positions.
pub fn shift_bitset_left(bitset: &[u8], shift: usize) -> Vec<u8> {
    if shift == 0 {
        return bitset.to_vec();
    }
    let total_bits = bitset.len() * 8;
    let new_bits = total_bits + shift;
    let mut out = vec![0u8; (new_bits + 7) / 8];
    for bit in 0..total_bits {
        if (bitset[bit / 8] & (1 << (bit % 8))) != 0 {
            let new_bit = bit + shift;
            out[new_bit / 8] |= 1 << (new_bit % 8);
        }
    }
    out
}

/// Reassemble a segmented PVA message from the first header and accumulated
/// payload fragments.
pub fn assemble_segmented_message(first_header: [u8; 8], payloads: Vec<Vec<u8>>) -> Vec<u8> {
    let mut header = first_header;
    let is_be = (header[2] & 0x80) != 0;
    header[2] &= !0x30;
    let total_len: usize = payloads.iter().map(|p| p.len()).sum();
    let len_bytes = if is_be {
        (total_len as u32).to_be_bytes()
    } else {
        (total_len as u32).to_le_bytes()
    };
    header[4..8].copy_from_slice(&len_bytes);
    let mut out = header.to_vec();
    for payload in payloads {
        out.extend_from_slice(&payload);
    }
    out
}
