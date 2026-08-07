use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::types::PvGetError;
use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::{SegmentOutcome, SegmentReassembler};

/// Read one complete PVA message, reassembling segments as needed.
///
/// Control frames are returned to the caller as they arrive; a message split
/// across segments may have control frames interleaved, which is why the
/// reassembler is a parameter rather than a local. One per connection.
pub async fn read_frame<R>(
    reader: &mut R,
    timeout_dur: Duration,
    reassembler: &mut SegmentReassembler,
) -> Result<Vec<u8>, PvGetError>
where
    R: AsyncRead + Unpin,
{
    let deadline = tokio::time::Instant::now() + timeout_dur;
    loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .ok_or(PvGetError::Timeout("read header"))?;

        let mut header = [0u8; 8];
        timeout(remaining, reader.read_exact(&mut header))
            .await
            .map_err(|_| PvGetError::Timeout("read header"))??;

        let parsed = PvaHeader::new(&header);
        let payload_len = if parsed.flags.is_control {
            0usize
        } else {
            parsed.payload_length as usize
        };

        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .ok_or(PvGetError::Timeout("read payload"))?;
            timeout(remaining, reader.read_exact(&mut payload))
                .await
                .map_err(|_| PvGetError::Timeout("read payload"))??;
        }

        match reassembler.push(header, payload)? {
            SegmentOutcome::Complete(msg) | SegmentOutcome::Control(msg) => return Ok(msg),
            SegmentOutcome::Pending => continue,
        }
    }
}

/// Read one complete PVA message from a TCP stream.
///
/// `reassembler` must be the one owned by this connection — see
/// [`read_frame`].
pub async fn read_packet(
    stream: &mut TcpStream,
    timeout_dur: Duration,
    reassembler: &mut SegmentReassembler,
) -> Result<Vec<u8>, PvGetError> {
    read_frame(stream, timeout_dur, reassembler).await
}

/// Read complete PVA messages until `predicate` accepts one.
pub async fn read_until<F>(
    stream: &mut TcpStream,
    timeout_dur: Duration,
    reassembler: &mut SegmentReassembler,
    mut predicate: F,
) -> Result<Vec<u8>, PvGetError>
where
    F: FnMut(&PvaPacketCommand) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout_dur;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(PvGetError::Timeout("read_until"));
        }
        let remaining = deadline - now;
        let bytes = read_packet(stream, remaining, reassembler).await?;
        let mut pkt = PvaPacket::new(&bytes);
        if let Some(cmd) = pkt.decode_payload() {
            if predicate(&cmd) {
                return Ok(bytes);
            }
        }
    }
}
