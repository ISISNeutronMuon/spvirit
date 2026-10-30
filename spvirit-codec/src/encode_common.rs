/// Encode a variable-length integer (PVA size encoding).
///
/// Values 0..253 are stored as a single byte. Values >= 254 are stored as
/// 0xFE followed by a 4-byte u32.
pub fn encode_size(size: usize, is_be: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    push_size(&mut out, size, is_be);
    out
}

/// Append the PVA size encoding of `size` to `out` (sink form of [`encode_size`]).
pub(crate) fn push_size(out: &mut Vec<u8>, size: usize, is_be: bool) {
    if size < 254 {
        out.push(size as u8);
        return;
    }
    out.push(0xFE);
    out.extend_from_slice(&if is_be {
        (size as u32).to_be_bytes()
    } else {
        (size as u32).to_le_bytes()
    });
}

/// Encode a string as a PVA size-prefixed byte sequence.
pub fn encode_string(value: &str, is_be: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 5);
    push_string(&mut out, value, is_be);
    out
}

/// Append a size-prefixed string to `out` (sink form of [`encode_string`]).
pub(crate) fn push_string(out: &mut Vec<u8>, value: &str, is_be: bool) {
    push_size(out, value.len(), is_be);
    out.extend_from_slice(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spvirit_encode::{encode_size_pva, encode_string_pva};

    /// The `_pva` and `_pvd` names always produced identical bytes; this pins
    /// that the surviving name is the same function, across every branch of
    /// the size prefix.
    #[test]
    fn the_surviving_encoders_cover_all_three_size_forms() {
        for (n, expected) in [
            (0usize, vec![0x00u8]),
            (1, vec![0x01]),
            (253, vec![253]),
            (254, {
                let mut v = vec![0xFE];
                v.extend_from_slice(&254u32.to_le_bytes());
                v
            }),
        ] {
            assert_eq!(encode_size(n, false), expected, "size {n}");
            assert_eq!(encode_size_pva(n, false), expected, "pva size {n}");
        }
        assert_eq!(encode_size(254, true), {
            let mut v = vec![0xFE];
            v.extend_from_slice(&254u32.to_be_bytes());
            v
        });

        let s = "pva";
        let mut expected = encode_size(3, false);
        expected.extend_from_slice(b"pva");
        assert_eq!(encode_string(s, false), expected);
        assert_eq!(encode_string_pva(s, false), expected);
    }
}
