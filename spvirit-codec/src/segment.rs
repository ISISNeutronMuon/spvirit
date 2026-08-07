//! Reassembly of segmented PVA messages.

use crate::error::{DecodeError, DecodeResult};

/// Default ceiling on a reassembled message, in bytes. Chosen to admit large
/// NTNDArray frames while keeping the rewritten `payload_length` inside `u32`.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 268_435_456; // 256 MiB

/// What [`SegmentReassembler::push`] did with the frame it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentOutcome {
    /// A complete message: the first segment's header with the segment bits
    /// cleared and `payload_length` rewritten to the concatenated total,
    /// followed by the concatenated payloads.
    Complete(Vec<u8>),
    /// A first or middle segment was absorbed. Push more frames.
    Pending,
    /// A control frame, returned verbatim. Reassembly state is untouched —
    /// control frames are legal between the segments of one message.
    Control(Vec<u8>),
}

struct Pending {
    header: [u8; 8],
    payloads: Vec<Vec<u8>>,
    total: usize,
}

/// Reassembles segmented PVA messages.
///
/// Sans-io: the caller reads a header and payload off the wire and pushes
/// them in. One instance per connection, because a message's segments may be
/// separated by control frames that the caller handles in between.
pub struct SegmentReassembler {
    max_bytes: usize,
    pending: Option<Pending>,
}

impl SegmentReassembler {
    /// A reassembler with the [`DEFAULT_MAX_MESSAGE_BYTES`] cap.
    pub fn new() -> Self {
        Self::with_max_bytes(DEFAULT_MAX_MESSAGE_BYTES)
    }

    /// A reassembler with a custom cap on the reassembled message size.
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self { max_bytes, pending: None }
    }

    /// Bytes of payload currently held for an in-progress message.
    pub fn pending_bytes(&self) -> usize {
        self.pending.as_ref().map_or(0, |p| p.total)
    }

    /// Discard any in-progress message. Call this when the connection resets.
    pub fn reset(&mut self) {
        self.pending = None;
    }

    /// Feed one frame. See [`SegmentOutcome`].
    ///
    /// On any `Err` the in-progress message is discarded and the reassembler
    /// is ready for the next one; the caller decides whether to keep the
    /// connection.
    pub fn push(&mut self, header: [u8; 8], payload: Vec<u8>) -> DecodeResult<SegmentOutcome> {
        let flags = header[2];

        if (flags & 0x01) != 0 {
            let mut out = Vec::with_capacity(8 + payload.len());
            out.extend_from_slice(&header);
            out.extend_from_slice(&payload);
            return Ok(SegmentOutcome::Control(out));
        }

        let command = header[3];
        match (flags & 0x30) >> 4 {
            // Unsegmented.
            0 => {
                if let Some(p) = self.pending.take() {
                    return Err(DecodeError::SegmentInterrupted {
                        expected: p.header[3],
                        got: command,
                    });
                }
                let mut out = Vec::with_capacity(8 + payload.len());
                out.extend_from_slice(&header);
                out.extend_from_slice(&payload);
                Ok(SegmentOutcome::Complete(out))
            }
            // First segment.
            1 => {
                if self.pending.take().is_some() {
                    return Err(DecodeError::UnexpectedSegment { flags });
                }
                if payload.len() > self.max_bytes {
                    return Err(DecodeError::MessageTooLarge {
                        total: payload.len(),
                        limit: self.max_bytes,
                    });
                }
                self.pending = Some(Pending {
                    header,
                    total: payload.len(),
                    payloads: vec![payload],
                });
                Ok(SegmentOutcome::Pending)
            }
            // Last (2) or middle (3).
            code => {
                let mut p = match self.pending.take() {
                    Some(p) => p,
                    None => return Err(DecodeError::UnexpectedSegment { flags }),
                };
                if p.header[3] != command || (p.header[2] & 0x40) != (flags & 0x40) {
                    return Err(DecodeError::SegmentCommandMismatch {
                        expected: p.header[3],
                        got: command,
                    });
                }
                let total = p.total + payload.len();
                if total > self.max_bytes {
                    return Err(DecodeError::MessageTooLarge { total, limit: self.max_bytes });
                }
                p.total = total;
                p.payloads.push(payload);
                if code == 2 {
                    Ok(SegmentOutcome::Complete(finish(p)))
                } else {
                    self.pending = Some(p);
                    Ok(SegmentOutcome::Pending)
                }
            }
        }
    }
}

impl Default for SegmentReassembler {
    fn default() -> Self {
        Self::new()
    }
}

/// Rebuild a standalone message from the first segment's header.
fn finish(p: Pending) -> Vec<u8> {
    let mut header = p.header;
    let is_be = (header[2] & 0x80) != 0;
    header[2] &= !0x30;
    let total = p.total as u32;
    let len = if is_be { total.to_be_bytes() } else { total.to_le_bytes() };
    header[4..8].copy_from_slice(&len);

    let mut out = Vec::with_capacity(8 + p.total);
    out.extend_from_slice(&header);
    for payload in p.payloads {
        out.extend_from_slice(&payload);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DecodeError;

    /// Build an 8-byte PVA header. `seg` is the raw 2-bit segment code:
    /// 0 unsegmented, 1 first, 2 last, 3 middle.
    fn hdr(command: u8, seg: u8, payload_len: u32, is_be: bool) -> [u8; 8] {
        let mut flags = 0x40u8; // server direction
        if is_be {
            flags |= 0x80;
        }
        flags |= seg << 4;
        let mut h = [0u8; 8];
        h[0] = 0xCA;
        h[1] = 2;
        h[2] = flags;
        h[3] = command;
        let l = if is_be { payload_len.to_be_bytes() } else { payload_len.to_le_bytes() };
        h[4..8].copy_from_slice(&l);
        h
    }

    fn control_hdr() -> [u8; 8] {
        let mut h = hdr(3, 0, 0, false);
        h[2] |= 0x01;
        h
    }

    #[test]
    fn three_segments_reassemble_into_one_message() {
        let mut r = SegmentReassembler::new();
        assert_eq!(r.push(hdr(13, 1, 2, false), vec![1, 2]).unwrap(), SegmentOutcome::Pending);
        assert_eq!(r.push(hdr(13, 3, 2, false), vec![3, 4]).unwrap(), SegmentOutcome::Pending);
        let out = match r.push(hdr(13, 2, 2, false), vec![5, 6]).unwrap() {
            SegmentOutcome::Complete(b) => b,
            other => panic!("expected Complete, got {other:?}"),
        };
        assert_eq!(&out[8..], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(out[2] & 0x30, 0, "segment bits must be cleared");
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 6);
        assert_eq!(out[3], 13, "command byte preserved");
        assert_eq!(r.pending_bytes(), 0);
    }

    #[test]
    fn big_endian_length_is_rewritten_big_endian() {
        let mut r = SegmentReassembler::new();
        r.push(hdr(13, 1, 2, true), vec![1, 2]).unwrap();
        let out = match r.push(hdr(13, 2, 1, true), vec![3]).unwrap() {
            SegmentOutcome::Complete(b) => b,
            other => panic!("expected Complete, got {other:?}"),
        };
        assert_eq!(u32::from_be_bytes(out[4..8].try_into().unwrap()), 3);
    }

    #[test]
    fn unsegmented_message_passes_straight_through() {
        let mut r = SegmentReassembler::new();
        let out = match r.push(hdr(11, 0, 3, false), vec![7, 8, 9]).unwrap() {
            SegmentOutcome::Complete(b) => b,
            other => panic!("expected Complete, got {other:?}"),
        };
        assert_eq!(&out[8..], &[7, 8, 9]);
    }

    #[test]
    fn control_frame_between_segments_does_not_disturb_reassembly() {
        let mut r = SegmentReassembler::new();
        r.push(hdr(13, 1, 2, false), vec![1, 2]).unwrap();
        match r.push(control_hdr(), vec![]).unwrap() {
            SegmentOutcome::Control(b) => assert_eq!(b.len(), 8),
            other => panic!("expected Control, got {other:?}"),
        }
        assert_eq!(r.pending_bytes(), 2, "pending state survives the control frame");
        let out = match r.push(hdr(13, 2, 2, false), vec![3, 4]).unwrap() {
            SegmentOutcome::Complete(b) => b,
            other => panic!("expected Complete, got {other:?}"),
        };
        assert_eq!(&out[8..], &[1, 2, 3, 4]);
    }

    #[test]
    fn unsegmented_message_mid_reassembly_is_an_error() {
        let mut r = SegmentReassembler::new();
        r.push(hdr(13, 1, 2, false), vec![1, 2]).unwrap();
        assert_eq!(
            r.push(hdr(11, 0, 1, false), vec![9]).unwrap_err(),
            DecodeError::SegmentInterrupted { expected: 13, got: 11 }
        );
        assert_eq!(r.pending_bytes(), 0, "state is reset after the error");
    }

    #[test]
    fn orphan_middle_and_last_segments_are_errors() {
        let mut r = SegmentReassembler::new();
        assert!(matches!(
            r.push(hdr(13, 3, 1, false), vec![1]).unwrap_err(),
            DecodeError::UnexpectedSegment { .. }
        ));
        assert!(matches!(
            r.push(hdr(13, 2, 1, false), vec![1]).unwrap_err(),
            DecodeError::UnexpectedSegment { .. }
        ));
    }

    #[test]
    fn second_first_segment_while_pending_is_an_error() {
        let mut r = SegmentReassembler::new();
        r.push(hdr(13, 1, 1, false), vec![1]).unwrap();
        assert!(matches!(
            r.push(hdr(13, 1, 1, false), vec![2]).unwrap_err(),
            DecodeError::UnexpectedSegment { .. }
        ));
        assert_eq!(r.pending_bytes(), 0);
    }

    #[test]
    fn command_mismatch_across_segments_is_an_error() {
        let mut r = SegmentReassembler::new();
        r.push(hdr(13, 1, 1, false), vec![1]).unwrap();
        assert_eq!(
            r.push(hdr(11, 3, 1, false), vec![2]).unwrap_err(),
            DecodeError::SegmentCommandMismatch { expected: 13, got: 11 }
        );
        assert_eq!(r.pending_bytes(), 0);
    }

    #[test]
    fn exceeding_the_cap_errors_and_leaves_the_reassembler_reusable() {
        let mut r = SegmentReassembler::with_max_bytes(4);
        r.push(hdr(13, 1, 3, false), vec![1, 2, 3]).unwrap();
        assert_eq!(
            r.push(hdr(13, 3, 3, false), vec![4, 5, 6]).unwrap_err(),
            DecodeError::MessageTooLarge { total: 6, limit: 4 }
        );
        assert_eq!(r.pending_bytes(), 0);
        // Still usable for the next message.
        let out = match r.push(hdr(11, 0, 1, false), vec![9]).unwrap() {
            SegmentOutcome::Complete(b) => b,
            other => panic!("expected Complete, got {other:?}"),
        };
        assert_eq!(&out[8..], &[9]);
    }

    #[test]
    fn reset_discards_pending_state() {
        let mut r = SegmentReassembler::new();
        r.push(hdr(13, 1, 2, false), vec![1, 2]).unwrap();
        assert_eq!(r.pending_bytes(), 2);
        r.reset();
        assert_eq!(r.pending_bytes(), 0);
    }
}
