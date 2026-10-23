/// Encode a variable-length integer (PVA size encoding).
///
/// Values 0..253 are stored as a single byte. Values >= 254 are stored as
/// 0xFE followed by a 4-byte u32.
pub fn encode_size(size: usize, is_be: bool) -> Vec<u8> {
    if size == 0 {
        return vec![0x00];
    }
    if size < 254 {
        return vec![size as u8];
    }
    let mut out = vec![0xFE];
    let bytes = if is_be {
        (size as u32).to_be_bytes()
    } else {
        (size as u32).to_le_bytes()
    };
    out.extend_from_slice(&bytes);
    out
}

/// Encode a string as a PVA size-prefixed byte sequence.
pub fn encode_string(value: &str, is_be: bool) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut out = encode_size(bytes.len(), is_be);
    out.extend_from_slice(bytes);
    out
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
