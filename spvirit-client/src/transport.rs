use std::io::{self, ErrorKind};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout};

use crate::types::PvGetError;
use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::{SegmentOutcome, SegmentReassembler};

/// Persistent partial-read state for one connection, used by
/// [`read_frame_resumable`].
///
/// The `select!` in the monitor loop drops the read future whenever its
/// echo-keepalive branch fires (~every 10 s). If that happens mid-frame the
/// bytes already pulled off the socket would be lost with a plain per-call
/// buffer, desyncing the TCP framing. Keeping the in-progress header/payload
/// cursor here — a value the caller owns, alongside the per-connection
/// [`SegmentReassembler`] — lets the next call resume exactly where the
/// dropped one left off. `read()` itself is cancellation-safe (a dropped read
/// consumes nothing), so the only state that has to survive a drop is this
/// cursor.
///
/// The `payload` allocation is also reused as the fill buffer across resumed
/// reads of a single frame.
#[derive(Default)]
pub struct FrameBuf {
    /// The 8-byte header being filled.
    header: [u8; 8],
    /// Bytes of `header` read so far.
    header_filled: usize,
    /// Set once all 8 header bytes are read and parsed.
    header_done: bool,
    /// Payload fill buffer (sized to the parsed `payload_length`).
    payload: Vec<u8>,
    /// Bytes of `payload` read so far.
    payload_filled: usize,
    /// Expected payload length for the current frame (0 for control frames).
    payload_need: usize,
}

impl FrameBuf {
    /// A fresh buffer with no partial read in progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset to await a new frame (keeps the `payload` allocation for reuse).
    fn reset(&mut self) {
        self.header_filled = 0;
        self.header_done = false;
        self.payload_filled = 0;
        self.payload_need = 0;
    }
}

/// Outcome of a single [`fill`] call.
enum Fill {
    /// The buffer was filled completely.
    Done,
    /// The deadline elapsed with no byte yet read into the buffer.
    TimedOut,
}

/// Fill `buf[*filled..]` using cancellation-safe `read()`.
///
/// `read()` (unlike `read_exact`) consumes nothing when its future is dropped,
/// so `*filled` — updated only after a read returns — always reflects exactly
/// the bytes taken off the socket. Dropping this future mid-fill therefore
/// loses no data.
///
/// The deadline can only produce `TimedOut` when `allow_timeout` is set *and*
/// no byte has been read into `buf` yet (`*filled == 0`): that is the only
/// clean boundary. Once any byte has been read — or when `allow_timeout` is
/// false (the payload phase, whose header is already spent) — we are committed
/// to finishing the buffer and block on cancellation-safe `read()`, never
/// abandoning a partially-read buffer. This is what keeps the two
/// `Err(Timeout) => continue` call sites from ever seeing a mid-frame timeout,
/// and prevents a busy-loop when the deadline has already elapsed mid-frame.
async fn fill<R>(
    reader: &mut R,
    buf: &mut [u8],
    filled: &mut usize,
    deadline: Instant,
    allow_timeout: bool,
) -> Result<Fill, PvGetError>
where
    R: AsyncRead + Unpin,
{
    while *filled < buf.len() {
        if allow_timeout && *filled == 0 {
            // No bytes yet at a clean boundary: bound the wait by the deadline.
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(d) if !d.is_zero() => d,
                _ => return Ok(Fill::TimedOut),
            };
            match timeout(remaining, reader.read(&mut buf[*filled..])).await {
                Ok(Ok(0)) => {
                    return Err(PvGetError::Io(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "eof before frame",
                    )));
                }
                Ok(Ok(n)) => *filled += n,
                Ok(Err(e)) => return Err(PvGetError::Io(e)),
                Err(_elapsed) => return Ok(Fill::TimedOut),
            }
        } else {
            // Committed to this buffer: read to completion (no deadline).
            // Still cancellation-safe, so a dropped future loses nothing.
            match reader.read(&mut buf[*filled..]).await {
                Ok(0) => {
                    return Err(PvGetError::Io(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "eof mid-frame",
                    )));
                }
                Ok(n) => *filled += n,
                Err(e) => return Err(PvGetError::Io(e)),
            }
        }
    }
    Ok(Fill::Done)
}

/// Core frame reader shared by [`read_frame`] and [`read_frame_resumable`].
///
/// All partial-read state lives in `fb`, so whether it survives a dropped
/// future is entirely the caller's choice of whether `fb` outlives the call.
async fn read_frame_into<R>(
    reader: &mut R,
    timeout_dur: Duration,
    reassembler: &mut SegmentReassembler,
    fb: &mut FrameBuf,
) -> Result<Vec<u8>, PvGetError>
where
    R: AsyncRead + Unpin,
{
    let deadline = Instant::now() + timeout_dur;
    loop {
        // Phase 1: header (8 bytes). A timeout with no header byte yet read is
        // a clean frame boundary and is surfaced to the caller.
        if !fb.header_done {
            match fill(reader, &mut fb.header, &mut fb.header_filled, deadline, true).await? {
                Fill::TimedOut => return Err(PvGetError::Timeout("read header")),
                Fill::Done => {}
            }
            let parsed = PvaHeader::new(&fb.header);
            fb.payload_need = if parsed.flags.is_control {
                0
            } else {
                parsed.payload_length as usize
            };
            fb.payload.clear();
            fb.payload.resize(fb.payload_need, 0);
            fb.payload_filled = 0;
            fb.header_done = true;
        }

        // Phase 2: payload. The header is already consumed, so this is never a
        // clean boundary: `fill` (allow_timeout = false) blocks through to
        // completion and never returns `TimedOut`.
        if fb.payload_filled < fb.payload_need {
            match fill(
                reader,
                &mut fb.payload[..fb.payload_need],
                &mut fb.payload_filled,
                deadline,
                false,
            )
            .await?
            {
                Fill::TimedOut => unreachable!("payload fill never times out"),
                Fill::Done => {}
            }
        }

        let header = fb.header;
        let payload = std::mem::take(&mut fb.payload);
        fb.reset();

        match reassembler.push(header, payload)? {
            SegmentOutcome::Complete(msg) | SegmentOutcome::Control(msg) => return Ok(msg),
            SegmentOutcome::Pending => continue,
        }
    }
}

/// Read one complete PVA message, reassembling segments as needed.
///
/// Control frames are returned to the caller as they arrive; a message split
/// across segments may have control frames interleaved, which is why the
/// reassembler is a parameter rather than a local. One per connection.
///
/// The per-call buffer is fresh, so this returns [`PvGetError::Timeout`] only
/// at a clean frame boundary (before any byte of the next frame is consumed):
/// a mid-frame stall is read to completion rather than abandoned. Callers that
/// drop this future mid-frame (e.g. from a `select!` branch) must instead use
/// [`read_frame_resumable`], which persists the partial read.
pub async fn read_frame<R>(
    reader: &mut R,
    timeout_dur: Duration,
    reassembler: &mut SegmentReassembler,
) -> Result<Vec<u8>, PvGetError>
where
    R: AsyncRead + Unpin,
{
    let mut fb = FrameBuf::new();
    read_frame_into(reader, timeout_dur, reassembler, &mut fb).await
}

/// Like [`read_frame`], but resumes from partial state held in `fb`.
///
/// Use this when the read future may be dropped mid-frame — as the monitor
/// loop's `select!` does when its echo-keepalive branch fires. Because `fb`
/// outlives any single call, the bytes already pulled off the socket for an
/// in-progress frame survive the drop and the next call continues from there,
/// keeping the TCP framing in sync.
pub async fn read_frame_resumable<R>(
    reader: &mut R,
    timeout_dur: Duration,
    reassembler: &mut SegmentReassembler,
    fb: &mut FrameBuf,
) -> Result<Vec<u8>, PvGetError>
where
    R: AsyncRead + Unpin,
{
    read_frame_into(reader, timeout_dur, reassembler, fb).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::time::sleep;

    /// A complete, unsegmented, little-endian PVA app frame: 8-byte header
    /// (magic, version, flags=0x40 server-dir, command=13) followed by a
    /// 5-byte payload `HELLO`.
    fn sample_frame() -> Vec<u8> {
        let mut f = vec![0xCA, 0x02, 0x40, 0x0D];
        f.extend_from_slice(&5u32.to_le_bytes()); // payload_length = 5, LE
        f.extend_from_slice(b"HELLO");
        f
    }

    /// C1 (item 1): a partial header that arrives, stalls past the read
    /// timeout, then completes must NOT desync the stream. The consumer loop
    /// mirrors the real `Err(Timeout) => continue` sites. Against the old
    /// `timeout(remaining, read_exact(..))` implementation the mid-frame
    /// timeout cancels `read_exact` after it has consumed the first 3 header
    /// bytes, losing them, so the next read starts from the middle of the
    /// frame and every subsequent frame is mis-parsed.
    #[tokio::test]
    async fn partial_header_then_stall_does_not_desync() {
        let (mut client, mut server) = tokio::io::duplex(256);
        let frame = sample_frame();
        let expected = frame.clone();

        // Writer: first 3 header bytes now, the remainder after a stall that
        // outlasts the per-read timeout used by the reader below.
        let writer = tokio::spawn(async move {
            server.write_all(&frame[..3]).await.unwrap();
            server.flush().await.unwrap();
            sleep(Duration::from_millis(120)).await;
            server.write_all(&frame[3..]).await.unwrap();
            server.flush().await.unwrap();
            // Keep the write half open briefly so a desynced reader hits a
            // deterministic EOF rather than hanging.
            sleep(Duration::from_millis(50)).await;
            // dropping `server` here closes the stream -> EOF for the reader
        });

        let mut reass = SegmentReassembler::new();
        let got = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match read_frame(&mut client, Duration::from_millis(30), &mut reass).await {
                    Ok(msg) => break msg,
                    Err(PvGetError::Timeout(_)) => continue,
                    Err(e) => panic!("unexpected read error (desync): {e}"),
                }
            }
        })
        .await
        .expect("reader timed out (desync/hang)");

        writer.await.unwrap();
        assert_eq!(got, expected, "frame must be reassembled intact");
    }

    /// C1 (item 2, option a): the monitor loop's echo-keepalive `select!`
    /// branch drops the in-flight read future mid-frame. With
    /// [`read_frame_resumable`] the partial read is held in the caller-owned
    /// [`FrameBuf`], so the dropped read loses nothing and the next call
    /// resumes and returns the intact frame. This models the ~10 s echo tick
    /// firing while a frame is partway across the socket.
    #[tokio::test]
    async fn echo_tick_drops_read_future_mid_frame_without_losing_bytes() {
        let (mut client, mut server) = tokio::io::duplex(256);
        let frame = sample_frame();
        let expected = frame.clone();

        let mut reass = SegmentReassembler::new();
        let mut fb = FrameBuf::new();

        // Deliver the first 5 header bytes only, then let the reader block.
        server.write_all(&frame[..5]).await.unwrap();
        server.flush().await.unwrap();

        // Model the echo branch winning the `select!`: the read future is
        // polled (consuming the 5 available header bytes into `fb`) and then
        // dropped when the timer branch fires.
        tokio::select! {
            _ = read_frame_resumable(&mut client, Duration::from_secs(30), &mut reass, &mut fb) => {
                panic!("frame is incomplete; read must not resolve yet");
            }
            _ = sleep(Duration::from_millis(50)) => {
                // echo tick: drop the read future mid-frame
            }
        }

        // The dropped future must have preserved the partial header.
        assert_eq!(fb.header_filled, 5, "partial header must survive the drop");

        // Deliver the rest of the frame and read again — it must resume.
        server.write_all(&frame[5..]).await.unwrap();
        server.flush().await.unwrap();

        let got = tokio::time::timeout(
            Duration::from_secs(5),
            read_frame_resumable(&mut client, Duration::from_secs(30), &mut reass, &mut fb),
        )
        .await
        .expect("resumed read timed out")
        .expect("resumed read failed");

        assert_eq!(got, expected, "resumed frame must be reassembled intact");
    }

    /// A control frame (segment/control bit set, zero payload) round-trips
    /// through the resumable reader with no payload phase.
    #[tokio::test]
    async fn resumable_reader_handles_control_frame() {
        let (mut client, mut server) = tokio::io::duplex(64);
        // Control frame: flags 0x41 (control bit set), command 2 (echo), len 0.
        let mut frame = vec![0xCA, 0x02, 0x41, 0x02];
        frame.extend_from_slice(&0u32.to_le_bytes());
        let expected = frame.clone();

        server.write_all(&frame).await.unwrap();
        server.flush().await.unwrap();
        drop(server);

        let mut reass = SegmentReassembler::new();
        let mut fb = FrameBuf::new();
        let got = read_frame_resumable(&mut client, Duration::from_secs(5), &mut reass, &mut fb)
            .await
            .expect("control frame read");
        assert_eq!(got, expected);
    }
}
