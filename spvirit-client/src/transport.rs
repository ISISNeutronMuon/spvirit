use std::io::{self, ErrorKind};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout};

use crate::types::PvGetError;
use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::{SegmentOutcome, SegmentReassembler};

/// The largest payload this reader will allocate for one inbound frame.
///
/// The PVA protocol puts no ceiling on `payload_length` beyond the u32 field
/// itself, so there is no protocol number to adopt. Reuse the workspace
/// constant that already bounds *reassembled* messages, so one number governs
/// the whole inbound byte budget instead of two that can drift apart.
pub const MAX_INBOUND_PAYLOAD_BYTES: usize = spvirit_codec::DEFAULT_MAX_MESSAGE_BYTES;

/// The largest buffer capacity [`FrameBuf::recycle`] will keep across frames.
///
/// Recycling exists to save the per-frame allocation for the common case,
/// and the common monitor update is small: an NTScalar is a few hundred
/// bytes, and 64 KiB still covers an 8 K-element `double` waveform. Anything
/// bigger is a one-off worth freeing — a gateway holds one `FrameBuf` per
/// upstream monitor, so retaining a single 200 MiB frame per connection
/// (anything up to `max_payload` would otherwise be kept) multiplies by the
/// monitor count and can be forced by one hostile frame. At 64 KiB the
/// worst-case pinned memory is 64 KiB per monitor connection.
const RECYCLE_RETAIN_MAX: usize = 64 * 1024;

/// Read-ahead buffer for a long-lived frame-reading loop (see [`buffered`]).
///
/// Unbuffered, every frame costs at least two `recv` calls — 8 header bytes,
/// then the payload. With a 64 KiB read-ahead a burst of small monitor
/// updates is pulled off the socket in one call and parsed from memory.
pub const READ_BUF_BYTES: usize = 64 * 1024;

/// Wrap a reader in a [`READ_BUF_BYTES`] read-ahead buffer.
///
/// Only for a reader that a single loop owns until the connection ends: the
/// buffer holds bytes already taken off the socket, so dropping it mid-stream
/// loses them. Inside that loop the read helpers stay cancellation-safe — a
/// dropped `read()` on a [`BufReader`] consumes nothing, and anything it
/// already buffered stays in the `BufReader`, which outlives the dropped
/// future just as [`FrameBuf`] does. Writes pass straight through to the inner
/// stream.
pub fn buffered<R: AsyncRead>(reader: R) -> BufReader<R> {
    BufReader::with_capacity(READ_BUF_BYTES, reader)
}

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
/// The `payload` allocation is reused as the fill buffer across resumed reads
/// of a single frame, and across frames when the caller donates the finished
/// message back with [`FrameBuf::recycle`].
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
    /// Largest `payload_length` this buffer will honour.
    max_payload: usize,
}

impl Default for FrameBuf {
    fn default() -> Self {
        Self {
            header: [0u8; 8],
            header_filled: 0,
            header_done: false,
            payload: Vec::new(),
            payload_filled: 0,
            payload_need: 0,
            max_payload: MAX_INBOUND_PAYLOAD_BYTES,
        }
    }
}

impl FrameBuf {
    /// A fresh buffer with no partial read in progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// A buffer with a non-default payload ceiling.
    ///
    /// Exists so the cap can be exercised without a large allocation, and so
    /// an embedder with a tighter byte budget can impose one.
    pub fn with_max_payload_bytes(max: usize) -> Self {
        Self {
            max_payload: max,
            ..Self::default()
        }
    }

    /// Reset to await a new frame. Note that `read_frame_into` takes the
    /// payload buffer out before calling this, so there is nothing left to
    /// keep unless the caller donates one back via [`FrameBuf::recycle`].
    fn reset(&mut self) {
        self.header_filled = 0;
        self.header_done = false;
        self.payload_filled = 0;
        self.payload_need = 0;
    }

    /// Donate a finished message buffer back for the next frame's payload.
    ///
    /// `read_frame_into` moves the payload allocation out into the message it
    /// returns, so without this the next frame allocates from scratch. A
    /// caller that is done with a message (the monitor loop, at the end of an
    /// iteration) hands it back here.
    ///
    /// Ignored when a frame is partially read — replacing the buffer then
    /// would discard bytes already taken off the socket, which would break
    /// the cancel-safety invariant (prior audit item C1) — ignored when the
    /// donation is no larger than what is already held, and dropped (freed)
    /// when its capacity exceeds [`RECYCLE_RETAIN_MAX`] or this buffer's own
    /// `max_payload` (plus the 8-byte header), so one large frame cannot pin
    /// its allocation for the connection's lifetime.
    pub fn recycle(&mut self, buf: Vec<u8>) {
        if self.header_filled != 0 || self.header_done || self.payload_filled != 0 {
            return;
        }
        // A donated message is header + payload, hence the `+ 8`.
        if buf.capacity() > RECYCLE_RETAIN_MAX.min(self.max_payload.saturating_add(8)) {
            return;
        }
        if buf.capacity() <= self.payload.capacity() {
            return;
        }
        let mut buf = buf;
        buf.clear();
        self.payload = buf;
    }

    #[cfg(test)]
    fn payload_capacity(&self) -> usize {
        self.payload.capacity()
    }

    #[cfg(test)]
    fn payload_ptr(&self) -> *const u8 {
        self.payload.as_ptr()
    }

    /// Test hook: put the buffer into the "header parsed, payload partly
    /// read" state that `recycle` must refuse to disturb.
    #[cfg(test)]
    fn begin_for_test(&mut self, need: usize, filled: usize) {
        self.header_done = true;
        self.payload_need = need;
        self.payload.clear();
        self.payload.resize(need, 0);
        self.payload_filled = filled;
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
/// A clean-boundary `TimedOut` (no byte read yet, `*filled == 0`) is produced
/// only when `allow_timeout` is set — that is the frame boundary the idle-poll
/// call sites expect.
///
/// The committed portion (once any byte has been read, or the payload phase
/// whose header is already spent) is governed by `bound_committed`:
/// - `false` — block on cancellation-safe `read()` with no deadline, never
///   abandoning a partially-read buffer. A fresh-buffer caller that loops on
///   `Err(Timeout) => continue` (e.g. the PUT reader) relies on this to emulate
///   an indefinitely-blocking read without ever desyncing on a mid-frame stall.
/// - `true` — bound every committed read by the deadline too and surface
///   `TimedOut`. Callers that either abort the connection (`read_until`) or hold
///   a persistent `fb` that resumes (`read_frame_resumable`) opt in, so a peer
///   that stalls mid-frame can no longer hang them forever (review R2-M1).
///
/// In all cases `read()` (unlike `read_exact`) consumes nothing when its future
/// is dropped, so `*filled` always reflects exactly the bytes taken off the
/// socket and dropping this future mid-fill loses no data.
async fn fill<R>(
    reader: &mut R,
    buf: &mut [u8],
    filled: &mut usize,
    deadline: Instant,
    allow_timeout: bool,
    bound_committed: bool,
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
        } else if bound_committed {
            // Committed but deadline-bounded: surface TimedOut so the caller can
            // abort (read_until) or resume from a persistent buffer
            // (read_frame_resumable) instead of hanging on a peer that stops
            // mid-frame. Still cancellation-safe.
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(d) if !d.is_zero() => d,
                _ => return Ok(Fill::TimedOut),
            };
            match timeout(remaining, reader.read(&mut buf[*filled..])).await {
                Ok(Ok(0)) => {
                    return Err(PvGetError::Io(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "eof mid-frame",
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
    bound_committed: bool,
) -> Result<Vec<u8>, PvGetError>
where
    R: AsyncRead + Unpin,
{
    let deadline = Instant::now() + timeout_dur;
    loop {
        // Phase 1: header (8 bytes). A timeout with no header byte yet read is
        // a clean frame boundary and is surfaced to the caller. When
        // `bound_committed` is set, a stall after a *partial* header also times
        // out rather than hanging.
        if !fb.header_done {
            match fill(
                reader,
                &mut fb.header,
                &mut fb.header_filled,
                deadline,
                true,
                bound_committed,
            )
            .await?
            {
                Fill::TimedOut => return Err(PvGetError::Timeout("read header")),
                Fill::Done => {}
            }
            let parsed = PvaHeader::new(&fb.header);
            let declared = if parsed.flags.is_control {
                0
            } else {
                parsed.payload_length as usize
            };
            // `declared` is a raw u32 off the wire. Refuse before the resize:
            // eight bytes of header must not be able to make this process ask
            // the allocator for gigabytes.
            if declared > fb.max_payload {
                return Err(PvGetError::Protocol(format!(
                    "inbound frame declares {declared} payload bytes, over the \
                     {} byte limit",
                    fb.max_payload
                )));
            }
            fb.payload_need = declared;
            fb.payload.clear();
            fb.payload.resize(fb.payload_need, 0);
            fb.payload_filled = 0;
            fb.header_done = true;
        }

        // Phase 2: payload. The header is already consumed, so this is never a
        // clean boundary (`allow_timeout = false`). With `bound_committed` unset
        // `fill` blocks through to completion and never returns `TimedOut`; with
        // it set a mid-payload stall times out and is surfaced as a payload
        // timeout (the caller either aborts or resumes from its persistent fb).
        if fb.payload_filled < fb.payload_need {
            match fill(
                reader,
                &mut fb.payload[..fb.payload_need],
                &mut fb.payload_filled,
                deadline,
                false,
                bound_committed,
            )
            .await?
            {
                Fill::TimedOut => return Err(PvGetError::Timeout("read payload")),
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
/// a mid-frame stall is read to completion rather than abandoned (block-forever
/// committed reads). This keeps a fresh-buffer caller that loops on
/// `Err(Timeout) => continue` from ever desyncing on a partially-read frame.
/// Callers that must bound a mid-frame stall need a persistent buffer — use
/// [`read_frame_resumable`], which both persists the partial read and bounds it.
pub async fn read_frame<R>(
    reader: &mut R,
    timeout_dur: Duration,
    reassembler: &mut SegmentReassembler,
) -> Result<Vec<u8>, PvGetError>
where
    R: AsyncRead + Unpin,
{
    let mut fb = FrameBuf::new();
    read_frame_into(reader, timeout_dur, reassembler, &mut fb, false).await
}

/// Like [`read_frame`], but resumes from partial state held in `fb`.
///
/// Use this when the read future may be dropped mid-frame — as the monitor
/// loop's `select!` does when its echo-keepalive branch fires. Because `fb`
/// outlives any single call, the bytes already pulled off the socket for an
/// in-progress frame survive the drop and the next call continues from there,
/// keeping the TCP framing in sync.
///
/// Committed reads are deadline-bounded (`bound_committed = true`): a peer that
/// stalls mid-frame yields `Err(Timeout)` instead of hanging the reader, and
/// because `fb` persists, the next call resumes the same frame with no desync
/// (review R2-M1). The monitor loop already treats `Err(Timeout) => continue`.
pub async fn read_frame_resumable<R>(
    reader: &mut R,
    timeout_dur: Duration,
    reassembler: &mut SegmentReassembler,
    fb: &mut FrameBuf,
) -> Result<Vec<u8>, PvGetError>
where
    R: AsyncRead + Unpin,
{
    read_frame_into(reader, timeout_dur, reassembler, fb, true).await
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
///
/// Committed reads are deadline-bounded, so a peer that sends a partial frame
/// and then stalls yields `Err(Timeout)` rather than hanging the resolve /
/// handshake / RPC paths forever (review R2-M1). This is desync-safe because a
/// mid-frame timeout aborts the whole call (`?`) and every caller then abandons
/// the connection — the partial bytes in the local `fb` are never reused. The
/// `fb` is persisted across the loop only so successive complete frames share
/// one buffer.
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
    let mut fb = FrameBuf::new();
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(PvGetError::Timeout("read_until"));
        }
        let remaining = deadline - now;
        let bytes = read_frame_into(stream, remaining, reassembler, &mut fb, true).await?;
        let mut pkt = PvaPacket::new(&bytes);
        if let Some(cmd) = pkt.decode_payload()
            && predicate(&cmd)
        {
            return Ok(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::sleep;

    #[test]
    fn recycle_reuses_the_donated_allocation_for_the_next_frame() {
        let mut fb = FrameBuf::new();
        let mut donated = Vec::with_capacity(4096);
        donated.extend_from_slice(&[0u8; 16]);
        let want = donated.as_ptr();

        fb.recycle(donated);
        assert_eq!(fb.payload_capacity(), 4096);
        assert_eq!(fb.payload_ptr(), want, "the same allocation must be kept");
    }

    #[test]
    fn recycle_is_ignored_while_a_partial_read_is_in_progress() {
        let mut fb = FrameBuf::new();
        // Simulate a frame whose header is parsed and whose payload is
        // half-filled: donating here would drop bytes already read.
        fb.begin_for_test(64, 10);
        let before = fb.payload_capacity();

        fb.recycle(Vec::with_capacity(4096));
        assert_eq!(
            fb.payload_capacity(),
            before,
            "an in-progress frame must not have its buffer replaced"
        );
    }

    #[test]
    fn recycle_keeps_the_larger_of_the_two_buffers() {
        let mut fb = FrameBuf::new();
        fb.recycle(Vec::with_capacity(8192));
        fb.recycle(Vec::with_capacity(64));
        assert_eq!(
            fb.payload_capacity(),
            8192,
            "a smaller donation must not shrink the buffer"
        );
    }

    /// One large frame must not pin its allocation for the connection's
    /// lifetime: a donation above the retention ceiling is dropped.
    #[test]
    fn recycle_drops_a_donation_above_the_retention_ceiling() {
        let mut fb = FrameBuf::new();
        fb.recycle(Vec::with_capacity(RECYCLE_RETAIN_MAX + 1));
        assert_eq!(
            fb.payload_capacity(),
            0,
            "an oversized donation must be freed, not retained"
        );

        // A donation at the ceiling is still kept.
        fb.recycle(Vec::with_capacity(RECYCLE_RETAIN_MAX));
        assert_eq!(fb.payload_capacity(), RECYCLE_RETAIN_MAX);
    }

    /// The ceiling is also bounded by the buffer's own payload cap: nothing
    /// larger than the largest frame it will honour is worth keeping.
    #[test]
    fn recycle_drops_a_donation_above_the_payload_cap() {
        let mut fb = FrameBuf::with_max_payload_bytes(1024);
        fb.recycle(Vec::with_capacity(4096));
        assert_eq!(
            fb.payload_capacity(),
            0,
            "a donation above max_payload must be freed, not retained"
        );
    }

    /// An application frame (server, LE, MONITOR) carrying `payload`.
    fn frame_with_payload(payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0xCA, 0x02, 0x40, 0x0D];
        f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        f.extend_from_slice(payload);
        f
    }

    /// Recycling on the real read path: read frame A, donate it back, then a
    /// smaller and a larger frame. Each must come back byte-exact at its own
    /// length — no stale tail from the recycled allocation, no truncation.
    #[tokio::test]
    async fn recycled_buffer_yields_exact_frames_on_the_real_read_path() {
        let a = frame_with_payload(&[0xAA; 32]);
        let smaller = frame_with_payload(b"HELLO");
        let larger = frame_with_payload(&(0..100u8).collect::<Vec<_>>());
        let mut wire = a.clone();
        wire.extend_from_slice(&smaller);
        wire.extend_from_slice(&larger);

        let mut reader = &wire[..];
        let mut reassembler = SegmentReassembler::new();
        let mut fb = FrameBuf::new();
        let t = Duration::from_millis(200);

        let msg_a = read_frame_resumable(&mut reader, t, &mut reassembler, &mut fb)
            .await
            .expect("frame A");
        assert_eq!(msg_a, a);
        fb.recycle(msg_a);
        assert!(fb.payload_capacity() >= a.len(), "frame A's buffer is kept");

        let msg_s = read_frame_resumable(&mut reader, t, &mut reassembler, &mut fb)
            .await
            .expect("smaller frame");
        assert_eq!(msg_s.len(), smaller.len());
        assert_eq!(msg_s, smaller);
        fb.recycle(msg_s);

        let msg_l = read_frame_resumable(&mut reader, t, &mut reassembler, &mut fb)
            .await
            .expect("larger frame");
        assert_eq!(msg_l.len(), larger.len());
        assert_eq!(msg_l, larger);
    }

    /// The retention ceiling on the real read path: a frame larger than
    /// `RECYCLE_RETAIN_MAX`, read through `read_frame_resumable` and then
    /// donated, is freed rather than kept — and the next frame still reads
    /// byte-exact into a fresh allocation.
    #[tokio::test]
    async fn a_large_frame_read_and_donated_is_not_retained() {
        let large = frame_with_payload(&vec![0x5A; RECYCLE_RETAIN_MAX + 1]);
        let small = frame_with_payload(b"HELLO");
        let mut wire = large.clone();
        wire.extend_from_slice(&small);

        let mut reader = &wire[..];
        let mut reassembler = SegmentReassembler::new();
        let mut fb = FrameBuf::new();
        let t = Duration::from_millis(200);

        let msg_large = read_frame_resumable(&mut reader, t, &mut reassembler, &mut fb)
            .await
            .expect("large frame");
        assert_eq!(msg_large, large);
        assert!(msg_large.capacity() > RECYCLE_RETAIN_MAX);
        fb.recycle(msg_large);
        assert_eq!(
            fb.payload_capacity(),
            0,
            "a frame above the retention ceiling must be freed, not retained"
        );

        let msg_small = read_frame_resumable(&mut reader, t, &mut reassembler, &mut fb)
            .await
            .expect("small frame");
        assert_eq!(msg_small, small);
    }

    /// The partial-state guard also covers a half-read header
    /// (`header_filled != 0`, `header_done` still false), produced here by
    /// genuinely dropping a read future mid-header.
    #[tokio::test]
    async fn recycle_is_ignored_while_a_header_is_half_read() {
        let (mut client, mut server) = tokio::io::duplex(256);
        let frame = sample_frame();
        let mut reass = SegmentReassembler::new();
        let mut fb = FrameBuf::new();

        server.write_all(&frame[..3]).await.unwrap();
        server.flush().await.unwrap();
        tokio::select! {
            _ = read_frame_resumable(&mut client, Duration::from_secs(30), &mut reass, &mut fb) => {
                panic!("frame is incomplete; read must not resolve yet");
            }
            _ = sleep(Duration::from_millis(50)) => {}
        }
        assert_eq!(fb.header_filled, 3);
        assert!(!fb.header_done);
        let before = fb.payload_capacity();

        fb.recycle(Vec::with_capacity(4096));
        assert_eq!(
            fb.payload_capacity(),
            before,
            "a half-read header must not have its buffer replaced"
        );

        server.write_all(&frame[3..]).await.unwrap();
        server.flush().await.unwrap();
        let got = read_frame_resumable(&mut client, Duration::from_secs(5), &mut reass, &mut fb)
            .await
            .expect("the frame resumes intact");
        assert_eq!(got, frame);
    }

    /// Site 2. `read_frame_into` resized straight to the peer-declared u32.
    /// Eight bytes of header from an upstream server made the gateway ask for
    /// up to 4 GiB. The cap is checked against a *configured* small ceiling
    /// here so the test itself never allocates anything large.
    #[tokio::test]
    async fn a_frame_declaring_more_than_the_cap_is_rejected() {
        // magic, version 2, flags 0 (client, LE, application), command 0,
        // payload_length = 1024. No payload bytes follow at all.
        let mut frame = vec![0xCA, 0x02, 0x00, 0x00];
        frame.extend_from_slice(&1024u32.to_le_bytes());

        let mut reader = &frame[..];
        let mut reassembler = SegmentReassembler::new();
        let mut fb = FrameBuf::with_max_payload_bytes(16);

        let err = read_frame_resumable(
            &mut reader,
            Duration::from_millis(200),
            &mut reassembler,
            &mut fb,
        )
        .await
        .expect_err("a frame over the cap must be rejected");

        match err {
            PvGetError::Protocol(msg) => assert!(
                msg.contains("1024") && msg.contains("16"),
                "the error should name both the declared length and the cap: {msg}"
            ),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    /// The default ceiling is the workspace's single inbound byte budget, the
    /// same constant that already bounds segmented reassembly. One number,
    /// not two.
    #[test]
    fn the_default_payload_cap_is_the_workspace_message_limit() {
        assert_eq!(
            MAX_INBOUND_PAYLOAD_BYTES,
            spvirit_codec::DEFAULT_MAX_MESSAGE_BYTES
        );
        assert_eq!(MAX_INBOUND_PAYLOAD_BYTES, 268_435_456);
    }

    /// An ordinary frame is unaffected.
    #[tokio::test]
    async fn a_frame_within_the_cap_still_reads() {
        let mut frame = vec![0xCA, 0x02, 0x00, 0x00];
        frame.extend_from_slice(&4u32.to_le_bytes());
        frame.extend_from_slice(&[1, 2, 3, 4]);

        let mut reader = &frame[..];
        let mut reassembler = SegmentReassembler::new();
        let mut fb = FrameBuf::new();

        let msg = read_frame_resumable(
            &mut reader,
            Duration::from_millis(200),
            &mut reassembler,
            &mut fb,
        )
        .await
        .expect("a 4-byte payload is well inside the cap");
        assert_eq!(msg.len(), 12);
    }

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

    /// The buffered monitor path: one `recv` delivers frame A, all of B and
    /// half of C. The read-ahead holds B and C's head; a read future for C
    /// is then dropped mid-frame (the echo tick winning the `select!`).
    /// Nothing may be lost — B comes back from the buffer, and C resumes from
    /// `fb` once its tail arrives.
    #[tokio::test]
    async fn buffered_reader_keeps_read_ahead_and_partial_frames_across_a_drop() {
        let (client, mut server) = tokio::io::duplex(1024);
        let a = frame_with_payload(b"AAAA");
        let b = frame_with_payload(&[0xBB; 40]);
        let c = frame_with_payload(&[0xCC; 30]);
        let mut first = a.clone();
        first.extend_from_slice(&b);
        first.extend_from_slice(&c[..13]);
        server.write_all(&first).await.unwrap();
        server.flush().await.unwrap();

        let mut reader = buffered(client);
        let mut reass = SegmentReassembler::new();
        let mut fb = FrameBuf::new();
        let t = Duration::from_secs(5);

        let got_a = read_frame_resumable(&mut reader, t, &mut reass, &mut fb)
            .await
            .unwrap();
        assert_eq!(got_a, a);
        assert!(
            !reader.buffer().is_empty(),
            "B and the head of C should be sitting in the read-ahead"
        );
        let got_b = read_frame_resumable(&mut reader, t, &mut reass, &mut fb)
            .await
            .unwrap();
        assert_eq!(got_b, b);

        tokio::select! {
            _ = read_frame_resumable(&mut reader, t, &mut reass, &mut fb) => {
                panic!("C is incomplete; read must not resolve yet");
            }
            _ = sleep(Duration::from_millis(50)) => {}
        }
        assert!(fb.header_done, "C's header must survive the drop");
        assert_eq!(fb.payload_filled, 5, "C's partial payload must survive");

        server.write_all(&c[13..]).await.unwrap();
        server.flush().await.unwrap();
        let got_c = read_frame_resumable(&mut reader, t, &mut reass, &mut fb)
            .await
            .unwrap();
        assert_eq!(got_c, c);
    }

    /// Writes on the buffered wrapper go straight to the inner stream, so the
    /// monitor loop can keep writing echoes/ACKs through the same handle.
    #[tokio::test]
    async fn buffered_reader_passes_writes_through() {
        let (client, mut server) = tokio::io::duplex(64);
        let mut wrapped = buffered(client);
        wrapped.write_all(b"ping").await.unwrap();
        wrapped.flush().await.unwrap();
        let mut got = [0u8; 4];
        server.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");
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

    /// R2-M1: a peer that delivers a partial frame and then stalls mid-payload
    /// must make a `bound_committed` reader (`read_frame_resumable`, and by the
    /// same path `read_until`) time out instead of hanging forever. Because the
    /// caller-owned `fb` persists, the timed-out partial read is resumed on the
    /// next call and the frame is reassembled intact — no desync.
    #[tokio::test]
    async fn mid_payload_stall_times_out_then_resumes_without_desync() {
        let (mut client, mut server) = tokio::io::duplex(256);
        let frame = sample_frame(); // 8-byte header + 5-byte payload
        let expected = frame.clone();

        // Deliver the full header plus the first 2 payload bytes, then stall.
        server.write_all(&frame[..10]).await.unwrap();
        server.flush().await.unwrap();

        let mut reass = SegmentReassembler::new();
        let mut fb = FrameBuf::new();

        // The committed (payload) read must surface a timeout rather than block.
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            read_frame_resumable(&mut client, Duration::from_millis(30), &mut reass, &mut fb),
        )
        .await
        .expect("committed read must time out, not hang");
        assert!(
            matches!(err, Err(PvGetError::Timeout(_))),
            "mid-payload stall must yield Timeout, got {err:?}"
        );
        // Partial state survives for the resume.
        assert!(fb.header_done, "header must be retained across the timeout");
        assert_eq!(fb.payload_filled, 2, "partial payload must be retained");

        // Deliver the rest; the resumed read returns the intact frame.
        server.write_all(&frame[10..]).await.unwrap();
        server.flush().await.unwrap();
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            read_frame_resumable(&mut client, Duration::from_secs(5), &mut reass, &mut fb),
        )
        .await
        .expect("resumed read timed out")
        .expect("resumed read failed");
        assert_eq!(got, expected, "resumed frame must be intact (no desync)");
    }

    /// The block-forever committed read that [`read_frame`] uses (the PUT reader
    /// loop relies on it to emulate an indefinitely-blocking read with a fresh
    /// buffer) must NOT time out mid-frame: a partial frame leaves the read
    /// pending regardless of `timeout_dur`, so a `select!` timer wins instead.
    #[tokio::test]
    async fn read_frame_does_not_time_out_mid_frame() {
        let (mut client, mut server) = tokio::io::duplex(256);
        let frame = sample_frame();
        server.write_all(&frame[..10]).await.unwrap();
        server.flush().await.unwrap();

        let mut reass = SegmentReassembler::new();
        let timed_out = tokio::select! {
            r = read_frame(&mut client, Duration::from_millis(30), &mut reass) => {
                panic!("read_frame must not resolve on a mid-frame stall, got {r:?}");
            }
            _ = sleep(Duration::from_millis(150)) => true,
        };
        assert!(timed_out, "committed read stayed pending as expected");
        // Keep `server` alive until here so the stall is a stall, not an EOF.
        drop(server);
    }
}
