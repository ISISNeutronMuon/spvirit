//! Typed decode errors for the PVA and PVD codecs.

use std::fmt;

/// Everything that can go wrong decoding a PVA frame or a PVD value.
///
/// Replaces the bare `None` the decoders used to return, which could not
/// distinguish "buffer ran out" from "this array is implausibly large".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The buffer ended before the field did.
    Truncated { needed: usize, available: usize },
    /// An array's element count exceeds the configured `DecodeLimits` entry.
    ArrayTooLarge { kind: &'static str, count: usize, limit: usize },
    /// An array's element count cannot fit in the bytes that remain, so the
    /// count itself is corrupt. Caught before allocating.
    CountExceedsBuffer { count: usize, min_bytes: usize, available: usize },
    /// Reassembly exceeded `SegmentReassembler`'s byte cap.
    MessageTooLarge { total: usize, limit: usize },
    /// A middle or last segment arrived with no message in progress, or a
    /// first segment arrived while one was already in progress.
    UnexpectedSegment { flags: u8 },
    /// An unsegmented application message arrived mid-reassembly.
    SegmentInterrupted { expected: u8, got: u8 },
    /// Segments of one message disagree on command byte or direction.
    SegmentCommandMismatch { expected: u8, got: u8 },
    /// An introspection tag byte we do not recognise.
    UnknownTypeTag(u8),
    /// A `0xFE` "only id" reference with no matching cached type. Usually
    /// means the `PvdDecoder` was not reused across the connection.
    UnresolvedTypeId(u16),
    /// A union selector outside the union's field list.
    UnknownUnionSelector { selector: usize, len: usize },
    /// Structurally invalid input that needs no parameters to explain.
    Malformed(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { needed, available } => {
                write!(f, "truncated: need {needed} bytes, {available} available")
            }
            Self::ArrayTooLarge { kind, count, limit } => {
                write!(f, "{kind} of {count} elements exceeds the limit of {limit}")
            }
            Self::CountExceedsBuffer { count, min_bytes, available } => write!(
                f,
                "element count {count} needs at least {min_bytes} bytes, {available} available"
            ),
            Self::MessageTooLarge { total, limit } => {
                write!(f, "reassembled message of {total} bytes exceeds the limit of {limit}")
            }
            Self::UnexpectedSegment { flags } => {
                write!(f, "unexpected segment, flags 0x{flags:02x}")
            }
            Self::SegmentInterrupted { expected, got } => write!(
                f,
                "reassembly of command {expected} interrupted by unsegmented command {got}"
            ),
            Self::SegmentCommandMismatch { expected, got } => {
                write!(f, "segment command mismatch: expected {expected}, got {got}")
            }
            Self::UnknownTypeTag(tag) => write!(f, "unknown type tag 0x{tag:02x}"),
            Self::UnresolvedTypeId(id) => {
                write!(f, "unresolved introspection id {id}: decoder not reused across connection?")
            }
            Self::UnknownUnionSelector { selector, len } => {
                write!(f, "union selector {selector} out of range for {len} fields")
            }
            Self::Malformed(what) => write!(f, "malformed: {what}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Result alias for the decode paths.
pub type DecodeResult<T> = Result<T, DecodeError>;

/// UTF-8 decoding policy for size-prefixed strings.
///
/// The codec deliberately runs two policies at two different layers, and the
/// split is intentional (see the boundary note on [`decode_string_prefixed`]):
///
/// - [`Utf8Policy::Strict`] — used for structured pvData values. A value that
///   is not valid UTF-8 is a genuine protocol error and must be surfaced.
/// - [`Utf8Policy::Lossy`] — used at the outer epics-protocol framing boundary,
///   where this codec acts as a passive observer of peer traffic and must stay
///   robust to malformed/foreign bytes rather than abort framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Utf8Policy {
    /// Reject non-UTF-8 bytes with [`DecodeError::Malformed`].
    Strict,
    /// Replace non-UTF-8 bytes with U+FFFD (`String::from_utf8_lossy`).
    Lossy,
}

/// Decode a PVA variable-length size prefix.
///
/// Single source of truth for the size-prefix encoding shared by the outer
/// epics-protocol framing layer ([`crate::epics_decode`]) and the structured
/// pvData value layer ([`crate::spvd_decode::PvdDecoder`]). Returns
/// `(size, bytes_consumed)`.
///
/// Encoding: `0xFF` is the null marker (decoded as size 0, 1 byte); `0xFE`
/// introduces a 4-byte little/big-endian length; any other first byte is the
/// length itself (1 byte).
pub fn decode_size_prefixed(data: &[u8], is_be: bool) -> DecodeResult<(usize, usize)> {
    if data.is_empty() {
        return Err(DecodeError::Truncated {
            needed: 1,
            available: 0,
        });
    }
    let first = data[0];
    if first == 0xFF {
        // Null marker; treated as size 0.
        return Ok((0, 1));
    }
    if first < 254 {
        return Ok((first as usize, 1));
    }
    // first == 254: a 4-byte size follows. (0xFF handled above.)
    if data.len() < 5 {
        return Err(DecodeError::Truncated {
            needed: 5,
            available: data.len(),
        });
    }
    let size = if is_be {
        u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize
    } else {
        u32::from_le_bytes([data[1], data[2], data[3], data[4]]) as usize
    };
    Ok((size, 5))
}

/// Decode a size-prefixed string under the given UTF-8 [`Utf8Policy`].
///
/// BOUNDARY: the choice of policy is the whole point of this being one helper.
/// Structured pvData values decode STRICT (a non-UTF-8 value is a real error);
/// the outer epics-protocol framing boundary decodes LOSSY (passive-observer
/// robustness). Do not collapse the two onto a single policy — the split is
/// deliberate. Returns `(string, total_bytes_consumed)` (prefix + body).
pub fn decode_string_prefixed(
    data: &[u8],
    is_be: bool,
    policy: Utf8Policy,
) -> DecodeResult<(String, usize)> {
    let (size, size_bytes) = decode_size_prefixed(data, is_be)?;
    if size == 0 {
        return Ok((String::new(), size_bytes));
    }
    let end = size_bytes + size;
    if data.len() < end {
        return Err(DecodeError::Truncated {
            needed: end,
            available: data.len(),
        });
    }
    let bytes = &data[size_bytes..end];
    let s = match policy {
        Utf8Policy::Strict => std::str::from_utf8(bytes)
            .map_err(|_| DecodeError::Malformed("string is not valid UTF-8"))?
            .to_string(),
        Utf8Policy::Lossy => String::from_utf8_lossy(bytes).into_owned(),
    };
    Ok((s, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_the_offending_values() {
        let e = DecodeError::ArrayTooLarge { kind: "string array", count: 900_000, limit: 65_536 };
        assert_eq!(
            e.to_string(),
            "string array of 900000 elements exceeds the limit of 65536"
        );
    }

    #[test]
    fn errors_compare_by_value() {
        let a = DecodeError::Truncated { needed: 12, available: 4 };
        let b = DecodeError::Truncated { needed: 12, available: 4 };
        let c = DecodeError::Truncated { needed: 12, available: 5 };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn implements_std_error() {
        fn assert_error<E: std::error::Error>(_: &E) {}
        assert_error(&DecodeError::Malformed("bad tag"));
    }

    #[test]
    fn size_prefix_short_medium_and_null() {
        // Short form: first byte is the length.
        assert_eq!(decode_size_prefixed(&[3, b'a', b'b', b'c'], false).unwrap(), (3, 1));
        // Null marker 0xFF -> size 0, 1 byte consumed.
        assert_eq!(decode_size_prefixed(&[0xFF], false).unwrap(), (0, 1));
        // 0xFE introduces a 4-byte length; endianness respected.
        assert_eq!(
            decode_size_prefixed(&[0xFE, 0x01, 0x00, 0x00, 0x00], false).unwrap(),
            (1, 5)
        );
        assert_eq!(
            decode_size_prefixed(&[0xFE, 0x00, 0x00, 0x00, 0x01], true).unwrap(),
            (1, 5)
        );
        // Empty buffer and a truncated 4-byte length both report Truncated.
        assert!(matches!(
            decode_size_prefixed(&[], false),
            Err(DecodeError::Truncated { .. })
        ));
        assert!(matches!(
            decode_size_prefixed(&[0xFE, 0x00], false),
            Err(DecodeError::Truncated { .. })
        ));
    }

    #[test]
    fn utf8_policy_split_is_intentional() {
        // A size-prefixed body containing an invalid UTF-8 byte (0xFF mid-string).
        let data = [2u8, b'a', 0xFF];
        // STRICT (structured pvData values): rejects the invalid value.
        assert!(matches!(
            decode_string_prefixed(&data, false, Utf8Policy::Strict),
            Err(DecodeError::Malformed(_))
        ));
        // LOSSY (outer framing boundary, passive observer): replaces with U+FFFD.
        let (s, consumed) = decode_string_prefixed(&data, false, Utf8Policy::Lossy).unwrap();
        assert_eq!(consumed, 3);
        assert_eq!(s, "a\u{FFFD}");
    }

    #[test]
    fn empty_string_consumes_only_the_prefix() {
        for policy in [Utf8Policy::Strict, Utf8Policy::Lossy] {
            assert_eq!(
                decode_string_prefixed(&[0u8], false, policy).unwrap(),
                (String::new(), 1)
            );
        }
    }
}
