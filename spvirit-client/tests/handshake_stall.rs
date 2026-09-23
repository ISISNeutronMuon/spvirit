//! Site 4: a peer that stalls *mid-header* must not hang the handshake.
//!
//! Note the shape of the hostile peer. A peer that accepts the connection and
//! writes nothing at all already times out today -- `transport::fill` bounds
//! the wait whenever no byte of the current frame has been read yet. The hole
//! is the *committed* read: once one byte is in, the unbounded branch blocks
//! with no deadline. So this peer writes exactly one byte and then holds the
//! socket open.

use std::time::Duration;

use spvirit_client::client::pva_handshake;
use spvirit_client::types::PvOptions;
use spvirit_codec::SegmentReassembler;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn a_peer_that_stalls_after_one_header_byte_does_not_hang_the_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    // The hostile peer: one byte of an 8-byte header, then silence. The
    // socket is deliberately never dropped -- an EOF would let the reader
    // fail for the wrong reason.
    let peer = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        sock.write_all(&[0xCA]).await.expect("write one byte");
        sock.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(sock);
    });

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut opts = PvOptions::new("STALL:PV".to_string());
    opts.timeout = Duration::from_millis(300);
    let mut reassembler = SegmentReassembler::new();

    // Generous outer bound: five seconds is ~16x the handshake deadline, so
    // tripping it means "hung", not "slow CI".
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        pva_handshake(&mut stream, &opts, &mut reassembler),
    )
    .await
    .expect("pva_handshake hung on a peer that stalled mid-header");

    assert!(
        outcome.is_err(),
        "the peer never completed a frame, so the handshake must fail"
    );

    peer.abort();
}
