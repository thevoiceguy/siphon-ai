//! Audio plumbing between forge's `i16` samples and PROTOCOL.md's
//! PCM16-LE wire bytes.
//!
//! Two orthogonal concerns live here:
//!
//! 1. **Pack / unpack** — pure functions converting `&[i16]` ↔
//!    little-endian PCM16 bytes. Used at the WS frame boundary.
//! 2. **Reframer** — a stateful chunker. Accepts arbitrary-sized
//!    `i16` chunks from upstream (forge) and emits exact 20 ms
//!    frames downstream (the WS).
//!
//! ## Why a reframer
//!
//! PROTOCOL.md §2.2 mandates exactly 20 ms per binary WS frame —
//! 160 samples at 8 kHz, 320 at 16 kHz. forge typically delivers
//! frames at the negotiated SIP `ptime` (also usually 20 ms), but
//! that is not guaranteed: a peer may negotiate `ptime=10` or
//! `ptime=30`, codec re-packetisation can produce different sizes,
//! and end-of-talk-spurt frames can be short. The reframer makes
//! the WS-side contract robust regardless.
//!
//! ## Hot path
//!
//! Per CLAUDE.md §4.3, the steady-state path:
//! - Pushes one `i16` slice per inbound RTP packet (50 fps).
//! - Pops one `Vec<i16>` (160 / 320 samples) per outbound WS frame.
//! - Allocates one `Vec<i16>` per pop (the popped frame).
//! - Allocates one `Vec<u8>` per pack (the wire bytes).
//! - Holds no locks, never blocks.
//!
//! Two allocations per frame (~640 B at 8 kHz / 20 ms) at 50 fps is
//! ~32 kB/s/call of allocator churn — measurable but not hot. If a
//! profiler shows it bottlenecking, we can pool the buffers.
//!
//! ## Inbound (WS → forge) direction
//!
//! The server sends PCM16-LE bytes of arbitrary size; this module's
//! [`unpack_pcm16_le`] decodes them to `Vec<i16>` and forge takes care
//! of re-packetising for whichever codec the SIP leg negotiated. No
//! reframing is needed in that direction.

use std::collections::VecDeque;

use thiserror::Error;

/// Frame duration on the wire, in milliseconds. Locked at 20 ms by
/// PROTOCOL.md §2.2.
pub const FRAME_DURATION_MS: u32 = 20;

/// Bytes per PCM16 sample (16-bit, mono).
pub const BYTES_PER_SAMPLE: usize = 2;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AudioError {
    /// PROTOCOL.md §3.1 only allows `8000` or `16000` Hz at v1.
    #[error("unsupported sample rate {0} Hz; v1 protocol allows only 8000 or 16000")]
    UnsupportedSampleRate(u32),

    /// A PCM16 byte buffer must have an even length.
    #[error("PCM16 byte buffer length {0} is not divisible by 2")]
    OddByteCount(usize),
}

/// Pack `i16` samples to PCM16 little-endian bytes.
///
/// Allocates exactly `samples.len() * 2` bytes. No-op-cheap for an
/// empty input.
#[inline]
pub fn pack_pcm16_le(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * BYTES_PER_SAMPLE);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Unpack PCM16 little-endian bytes to `i16` samples.
///
/// Returns [`AudioError::OddByteCount`] if `bytes.len()` is not even —
/// PROTOCOL.md §2.2 guarantees an even count, so an odd one means the
/// counterparty is buggy.
#[inline]
pub fn unpack_pcm16_le(bytes: &[u8]) -> Result<Vec<i16>, AudioError> {
    if bytes.len() % BYTES_PER_SAMPLE != 0 {
        return Err(AudioError::OddByteCount(bytes.len()));
    }
    let mut out = Vec::with_capacity(bytes.len() / BYTES_PER_SAMPLE);
    for chunk in bytes.chunks_exact(BYTES_PER_SAMPLE) {
        out.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    Ok(out)
}

/// How many samples make up one 20 ms frame at the given rate.
///
/// Returns [`AudioError::UnsupportedSampleRate`] for any rate other
/// than 8000 or 16000 Hz.
#[inline]
pub fn samples_per_frame(sample_rate: u32) -> Result<usize, AudioError> {
    match sample_rate {
        8000 => Ok(160),
        16000 => Ok(320),
        other => Err(AudioError::UnsupportedSampleRate(other)),
    }
}

/// Stateful re-chunker: accept arbitrary-sized `i16` slices, emit exact
/// 20 ms frames sized to the configured sample rate.
///
/// ```ignore
/// let mut buf = Reframer::new(8000)?;
/// buf.push(&samples_from_forge);
/// while let Some(frame) = buf.pop_frame() {
///     let bytes = pack_pcm16_le(&frame);
///     ws_audio_tx.send(bytes).await?;
/// }
/// ```
///
/// At end-of-call, any partial buffer is dropped; the lost audio is
/// always less than one 20 ms frame and indistinguishable from natural
/// hangup latency.
#[derive(Debug)]
pub struct Reframer {
    sample_rate: u32,
    samples_per_frame: usize,
    pending: VecDeque<i16>,
}

impl Reframer {
    pub fn new(sample_rate: u32) -> Result<Self, AudioError> {
        let samples_per_frame = samples_per_frame(sample_rate)?;
        Ok(Self {
            sample_rate,
            samples_per_frame,
            pending: VecDeque::with_capacity(samples_per_frame * 2),
        })
    }

    /// Append samples to the pending buffer. Does not allocate beyond
    /// the initial capacity unless the buffer grows past the reserved
    /// 2-frame headroom.
    pub fn push(&mut self, samples: &[i16]) {
        self.pending.extend(samples.iter().copied());
    }

    /// Pop one complete 20 ms frame, if available.
    ///
    /// Returns `None` when fewer than [`Self::samples_per_frame`]
    /// samples are buffered.
    pub fn pop_frame(&mut self) -> Option<Vec<i16>> {
        if self.pending.len() < self.samples_per_frame {
            return None;
        }
        let frame: Vec<i16> = self.pending.drain(..self.samples_per_frame).collect();
        Some(frame)
    }

    /// Buffered sample count not yet popped.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn samples_per_frame(&self) -> usize {
        self.samples_per_frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── pack / unpack ──────────────────────────────────────────────

    #[test]
    fn pack_endianness_lo_byte_first() {
        // 0x1234 in little-endian is [0x34, 0x12].
        let bytes = pack_pcm16_le(&[0x1234]);
        assert_eq!(bytes, vec![0x34, 0x12]);
    }

    #[test]
    fn pack_negative_sample_two_complement() {
        // -1 == 0xFFFF, both bytes 0xFF.
        let bytes = pack_pcm16_le(&[-1]);
        assert_eq!(bytes, vec![0xFF, 0xFF]);
    }

    #[test]
    fn pack_empty_returns_empty() {
        assert!(pack_pcm16_le(&[]).is_empty());
    }

    #[test]
    fn unpack_lo_byte_first() {
        let samples = unpack_pcm16_le(&[0x34, 0x12]).unwrap();
        assert_eq!(samples, vec![0x1234]);
    }

    #[test]
    fn unpack_odd_count_rejected() {
        let err = unpack_pcm16_le(&[0x01, 0x02, 0x03]).unwrap_err();
        assert_eq!(err, AudioError::OddByteCount(3));
    }

    #[test]
    fn unpack_empty_returns_empty() {
        assert_eq!(unpack_pcm16_le(&[]).unwrap(), Vec::<i16>::new());
    }

    #[test]
    fn pack_then_unpack_round_trips() {
        let samples = vec![0i16, 1, -1, i16::MAX, i16::MIN, 0x55AA, -0x55AA, 12345];
        let bytes = pack_pcm16_le(&samples);
        assert_eq!(bytes.len(), samples.len() * 2);
        let recovered = unpack_pcm16_le(&bytes).unwrap();
        assert_eq!(recovered, samples);
    }

    // ─── samples_per_frame ──────────────────────────────────────────

    #[test]
    fn samples_per_frame_matches_protocol_table() {
        // Spec §2.2.
        assert_eq!(samples_per_frame(8000).unwrap(), 160);
        assert_eq!(samples_per_frame(16000).unwrap(), 320);
    }

    #[test]
    fn samples_per_frame_rejects_unsupported_rates() {
        for rate in [0, 7999, 8001, 11025, 22050, 44100, 48000] {
            assert_eq!(
                samples_per_frame(rate),
                Err(AudioError::UnsupportedSampleRate(rate)),
            );
        }
    }

    // ─── Reframer ───────────────────────────────────────────────────

    #[test]
    fn reframer_rejects_unsupported_sample_rate() {
        assert!(Reframer::new(48000).is_err());
    }

    #[test]
    fn reframer_8khz_emits_one_frame_per_exact_input() {
        let mut buf = Reframer::new(8000).unwrap();
        let input = vec![0i16; 160];
        buf.push(&input);
        assert_eq!(buf.pop_frame().unwrap().len(), 160);
        assert!(buf.pop_frame().is_none());
        assert_eq!(buf.pending(), 0);
    }

    #[test]
    fn reframer_16khz_emits_320_sample_frames() {
        let mut buf = Reframer::new(16000).unwrap();
        buf.push(&[1i16; 320]);
        let frame = buf.pop_frame().unwrap();
        assert_eq!(frame.len(), 320);
        assert!(frame.iter().all(|&s| s == 1));
    }

    #[test]
    fn reframer_holds_partial_input() {
        // 100 samples at 8 kHz = 12.5 ms, less than one frame.
        let mut buf = Reframer::new(8000).unwrap();
        buf.push(&[0i16; 100]);
        assert!(buf.pop_frame().is_none());
        assert_eq!(buf.pending(), 100);
    }

    #[test]
    fn reframer_emits_one_frame_and_holds_remainder() {
        // 30 ms input → one 20 ms frame + 10 ms held.
        let mut buf = Reframer::new(8000).unwrap();
        buf.push(&[7i16; 240]); // 240 samples = 30 ms @ 8 kHz
        let frame = buf.pop_frame().unwrap();
        assert_eq!(frame.len(), 160);
        assert!(frame.iter().all(|&s| s == 7));
        assert!(buf.pop_frame().is_none());
        assert_eq!(buf.pending(), 80);
    }

    #[test]
    fn reframer_accumulates_across_pushes() {
        // 100 + 100 = 200 samples → one frame, 40 left over.
        let mut buf = Reframer::new(8000).unwrap();
        buf.push(&[1i16; 100]);
        assert!(buf.pop_frame().is_none());
        buf.push(&[2i16; 100]);
        let frame = buf.pop_frame().unwrap();
        assert_eq!(frame.len(), 160);
        // Boundary: first 100 samples are 1's, next 60 are 2's.
        assert!(frame[..100].iter().all(|&s| s == 1));
        assert!(frame[100..].iter().all(|&s| s == 2));
        assert_eq!(buf.pending(), 40);
        assert!(buf.pop_frame().is_none());
    }

    #[test]
    fn reframer_emits_multiple_frames_from_large_push() {
        // 5 frames worth of input.
        let mut buf = Reframer::new(8000).unwrap();
        buf.push(&[9i16; 160 * 5]);
        for _ in 0..5 {
            assert_eq!(buf.pop_frame().unwrap().len(), 160);
        }
        assert!(buf.pop_frame().is_none());
        assert_eq!(buf.pending(), 0);
    }

    #[test]
    fn reframer_empty_push_is_a_noop() {
        let mut buf = Reframer::new(8000).unwrap();
        buf.push(&[]);
        assert_eq!(buf.pending(), 0);
        assert!(buf.pop_frame().is_none());
    }

    // ─── Cross-check: byte sizes match PROTOCOL.md §2.2 table ───────

    #[test]
    fn protocol_byte_sizes_per_frame_are_320_and_640() {
        let frame_8k = vec![0i16; samples_per_frame(8000).unwrap()];
        assert_eq!(pack_pcm16_le(&frame_8k).len(), 320);
        let frame_16k = vec![0i16; samples_per_frame(16000).unwrap()];
        assert_eq!(pack_pcm16_le(&frame_16k).len(), 640);
    }
}
