//! The audio frames forked off the media tap for recording.

/// One 20 ms PCM16-LE mono frame, tagged with the leg it came from. The
/// media tap `try_send`s these (best-effort, never blocking the audio path)
/// to the per-call [`RecordingWriter`](crate::RecordingWriter), which mixes
/// the two legs into a stereo WAV.
#[derive(Debug, Clone)]
pub enum RecFrame {
    /// Audio from the caller (inbound RTP, decoded) — the **left** channel.
    Caller(Vec<u8>),
    /// Audio played toward the caller (from the WS server) — the **right**
    /// channel. Muted playout is intentionally *not* forwarded, so a muted
    /// span records as silence on the right (what the caller actually heard).
    Bot(Vec<u8>),
}
