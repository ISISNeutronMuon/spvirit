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
}
