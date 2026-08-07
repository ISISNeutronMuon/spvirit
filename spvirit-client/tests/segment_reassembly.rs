//! Client-side reassembly of segmented PVA messages.

use std::time::Duration;

use spvirit_client::transport::read_frame;
use spvirit_codec::SegmentReassembler;
use tokio::io::AsyncWriteExt;

/// Build an 8-byte PVA header. `seg`: 0 unsegmented, 1 first, 2 last, 3 middle.
fn hdr(command: u8, seg: u8, payload_len: u32) -> [u8; 8] {
    let mut h = [0u8; 8];
    h[0] = 0xCA;
    h[1] = 2;
    h[2] = 0x40 | (seg << 4); // server direction, little-endian
    h[3] = command;
    h[4..8].copy_from_slice(&payload_len.to_le_bytes());
    h
}

#[tokio::test]
async fn read_frame_reassembles_a_segmented_message() {
    let (mut client, mut server) = tokio::io::duplex(4096);

    server.write_all(&hdr(13, 1, 3)).await.unwrap();
    server.write_all(&[1, 2, 3]).await.unwrap();
    server.write_all(&hdr(13, 3, 2)).await.unwrap();
    server.write_all(&[4, 5]).await.unwrap();
    server.write_all(&hdr(13, 2, 1)).await.unwrap();
    server.write_all(&[6]).await.unwrap();

    let mut reassembler = SegmentReassembler::new();
    let msg = read_frame(&mut client, Duration::from_secs(2), &mut reassembler)
        .await
        .expect("reassembled message");

    assert_eq!(&msg[8..], &[1, 2, 3, 4, 5, 6]);
    assert_eq!(msg[2] & 0x30, 0, "segment bits cleared");
    assert_eq!(u32::from_le_bytes(msg[4..8].try_into().unwrap()), 6);
}

#[tokio::test]
async fn read_frame_returns_unsegmented_messages_unchanged() {
    let (mut client, mut server) = tokio::io::duplex(4096);
    server.write_all(&hdr(11, 0, 2)).await.unwrap();
    server.write_all(&[9, 9]).await.unwrap();

    let mut reassembler = SegmentReassembler::new();
    let msg = read_frame(&mut client, Duration::from_secs(2), &mut reassembler)
        .await
        .unwrap();
    assert_eq!(&msg[8..], &[9, 9]);
}

#[tokio::test]
async fn control_frame_between_segments_is_returned_then_reassembly_continues() {
    let (mut client, mut server) = tokio::io::duplex(4096);

    server.write_all(&hdr(13, 1, 2)).await.unwrap();
    server.write_all(&[1, 2]).await.unwrap();
    let mut ctrl = hdr(3, 0, 0);
    ctrl[2] |= 0x01;
    server.write_all(&ctrl).await.unwrap();
    server.write_all(&hdr(13, 2, 2)).await.unwrap();
    server.write_all(&[3, 4]).await.unwrap();

    let mut reassembler = SegmentReassembler::new();
    let timeout = Duration::from_secs(2);

    // The control frame surfaces first; the caller handles it and reads on.
    let first = read_frame(&mut client, timeout, &mut reassembler)
        .await
        .unwrap();
    assert_eq!(first[2] & 0x01, 1, "control frame");
    assert_eq!(reassembler.pending_bytes(), 2, "reassembly survived it");

    let second = read_frame(&mut client, timeout, &mut reassembler)
        .await
        .unwrap();
    assert_eq!(&second[8..], &[1, 2, 3, 4]);
}

#[tokio::test]
async fn a_protocol_violation_surfaces_as_a_codec_error() {
    let (mut client, mut server) = tokio::io::duplex(4096);
    // A last segment with nothing in progress.
    server.write_all(&hdr(13, 2, 1)).await.unwrap();
    server.write_all(&[1]).await.unwrap();

    let mut reassembler = SegmentReassembler::new();
    let err = read_frame(&mut client, Duration::from_secs(2), &mut reassembler)
        .await
        .expect_err("orphan last segment must be rejected");
    assert!(
        matches!(err, spvirit_client::types::PvGetError::Codec(_)),
        "expected a Codec error, got {err:?}"
    );
}
