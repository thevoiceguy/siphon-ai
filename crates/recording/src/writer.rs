//! The per-call recording writer task.
//!
//! Mixes the two tapped legs into a stereo WAV on a 20 ms monotonic clock
//! and writes it out. It runs as its own per-call task — **never on the
//! audio hot path** (CLAUDE.md §4.3): the tap `try_send`s frame copies to
//! the bounded channel this task drains, and the (batched) file I/O happens
//! here, not on the media task.
//!
//! Layout: dual-channel stereo PCM16-LE — caller on the **left**, bot on the
//! **right**. Each 20 ms tick emits exactly one stereo frame using the most
//! recent frame seen for each leg, or silence for a leg that produced
//! nothing — so the recording stays time-aligned to the call's wall clock
//! and its duration tracks the call. (Two frames for the same leg between
//! ticks: the later wins — a rare 20 ms drop under jitter, not a desync.)

use std::io::SeekFrom;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::frame::RecFrame;

/// Recording cadence — one stereo frame per 20 ms, matching the bridge.
const FRAME_MS: u64 = 20;
/// Flush the in-memory buffer to disk once it reaches this size (~1 s of
/// stereo 16 kHz audio). Batches syscalls so the writer task rarely blocks.
const FLUSH_BYTES: usize = 64 * 1024;
/// WAV header length (canonical 44-byte PCM header).
const WAV_HEADER_LEN: usize = 44;

/// Outcome of a finished recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingStats {
    pub path: PathBuf,
    pub frames: u64,
    pub data_bytes: u64,
}

#[derive(Debug, Error)]
pub enum RecordingError {
    #[error("recording I/O failed for {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsupported sample rate {0} (8000 or 16000 only)")]
    UnsupportedSampleRate(u32),
}

/// Per-call recording writer. Build with [`RecordingWriter::new`], then
/// drive with [`RecordingWriter::run`], feeding it the tap's `RecFrame`s.
pub struct RecordingWriter {
    path: PathBuf,
    sample_rate: u32,
}

impl RecordingWriter {
    pub fn new(path: PathBuf, sample_rate: u32) -> Self {
        Self { path, sample_rate }
    }

    /// Run until `rx` closes (the call ended and the tap dropped its
    /// sender), then flush and finalize the WAV header. Returns the
    /// recording stats, or an error if the file couldn't be written.
    pub async fn run(
        self,
        mut rx: mpsc::Receiver<RecFrame>,
    ) -> Result<RecordingStats, RecordingError> {
        let mono_samples = match self.sample_rate {
            8000 => 160usize,
            16000 => 320usize,
            other => return Err(RecordingError::UnsupportedSampleRate(other)),
        };
        let mono_bytes = mono_samples * 2; // PCM16
        let io_err = |source| RecordingError::Io {
            path: self.path.clone(),
            source,
        };

        let file = File::create(&self.path).await.map_err(io_err)?;
        let mut out = BufWriter::new(file);
        // Placeholder header; patched with the real sizes at finalize.
        out.write_all(&wav_header(self.sample_rate, 0))
            .await
            .map_err(io_err)?;

        let mut latest_caller: Option<Vec<u8>> = None;
        let mut latest_bot: Option<Vec<u8>> = None;
        let mut buf: Vec<u8> = Vec::with_capacity(FLUSH_BYTES + mono_bytes * 2);
        let mut frames: u64 = 0;
        let mut data_bytes: u64 = 0;

        let mut tick = tokio::time::interval(Duration::from_millis(FRAME_MS));
        // Missed ticks are skipped (not bunched) so the recording stays
        // wall-clock aligned rather than writing a catch-up burst of silence.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                maybe = rx.recv() => match maybe {
                    Some(RecFrame::Caller(b)) => latest_caller = Some(b),
                    Some(RecFrame::Bot(b)) => latest_bot = Some(b),
                    None => break, // tap dropped its sender → call over
                },
                _ = tick.tick() => {
                    interleave_into(
                        &mut buf,
                        latest_caller.take().as_deref(),
                        latest_bot.take().as_deref(),
                        mono_bytes,
                    );
                    frames += 1;
                    data_bytes += (mono_bytes * 2) as u64;
                    if buf.len() >= FLUSH_BYTES {
                        out.write_all(&buf).await.map_err(io_err)?;
                        buf.clear();
                    }
                }
            }
        }

        // Flush the tail, then patch the RIFF/data sizes into the header.
        if !buf.is_empty() {
            out.write_all(&buf).await.map_err(io_err)?;
        }
        out.flush().await.map_err(io_err)?;
        let data_u32 = u32::try_from(data_bytes).unwrap_or(u32::MAX);
        out.seek(SeekFrom::Start(4)).await.map_err(io_err)?;
        out.write_all(&(36u32.wrapping_add(data_u32)).to_le_bytes())
            .await
            .map_err(io_err)?;
        out.seek(SeekFrom::Start(40)).await.map_err(io_err)?;
        out.write_all(&data_u32.to_le_bytes())
            .await
            .map_err(io_err)?;
        out.flush().await.map_err(io_err)?;

        debug!(path = %self.path.display(), frames, data_bytes, "recording finalized");
        if data_bytes > u32::MAX as u64 {
            warn!(path = %self.path.display(), "recording exceeded 4 GiB; WAV header sizes saturated");
        }
        Ok(RecordingStats {
            path: self.path,
            frames,
            data_bytes,
        })
    }
}

/// Append one interleaved stereo frame (L = caller, R = bot) to `buf`.
/// Each leg is taken as exactly `mono_bytes` (truncated or zero-padded);
/// a `None` leg is silence.
fn interleave_into(
    buf: &mut Vec<u8>,
    caller: Option<&[u8]>,
    bot: Option<&[u8]>,
    mono_bytes: usize,
) {
    let sample = |src: Option<&[u8]>, i: usize| -> [u8; 2] {
        match src {
            Some(s) if i + 1 < s.len() => [s[i], s[i + 1]],
            _ => [0, 0],
        }
    };
    let mut i = 0;
    while i < mono_bytes {
        buf.extend_from_slice(&sample(caller, i)); // left
        buf.extend_from_slice(&sample(bot, i)); // right
        i += 2;
    }
}

/// Canonical 44-byte PCM WAV header for stereo 16-bit at `sample_rate`,
/// with `data_len` bytes of sample data (0 for the streaming placeholder).
fn wav_header(sample_rate: u32, data_len: u32) -> [u8; WAV_HEADER_LEN] {
    const CHANNELS: u16 = 2;
    const BITS: u16 = 16;
    let block_align: u16 = CHANNELS * (BITS / 8);
    let byte_rate: u32 = sample_rate * block_align as u32;
    let mut h = [0u8; WAV_HEADER_LEN];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36u32.wrapping_add(data_len)).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&CHANNELS.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&BITS.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_well_formed() {
        let h = wav_header(8000, 320);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[36..40], b"data");
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 36 + 320);
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 320);
        assert_eq!(u16::from_le_bytes(h[22..24].try_into().unwrap()), 2); // stereo
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 8000);
        // byte_rate = 8000 * 2ch * 2bytes = 32000
        assert_eq!(u32::from_le_bytes(h[28..32].try_into().unwrap()), 32000);
    }

    #[test]
    fn interleave_pairs_legs_left_right() {
        let mut buf = Vec::new();
        // 2 samples (4 bytes) per leg.
        let caller = [0x11, 0x11, 0x22, 0x22];
        let bot = [0x33, 0x33, 0x44, 0x44];
        interleave_into(&mut buf, Some(&caller), Some(&bot), 4);
        assert_eq!(buf, vec![0x11, 0x11, 0x33, 0x33, 0x22, 0x22, 0x44, 0x44]);
    }

    #[test]
    fn interleave_silences_missing_leg() {
        let mut buf = Vec::new();
        let caller = [0xab, 0xcd];
        interleave_into(&mut buf, Some(&caller), None, 2);
        assert_eq!(buf, vec![0xab, 0xcd, 0x00, 0x00]); // bot silent
    }

    #[tokio::test]
    async fn writes_a_valid_stereo_wav() {
        let dir = std::env::temp_dir().join(format!("siphon_rec_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.wav");
        let (tx, rx) = mpsc::channel(64);
        let writer = RecordingWriter::new(path.clone(), 8000);
        let h = tokio::spawn(writer.run(rx));
        // Feed a few frames of each leg, then close.
        for _ in 0..5 {
            tx.send(RecFrame::Caller(vec![1u8; 320])).await.unwrap();
            tx.send(RecFrame::Bot(vec![2u8; 320])).await.unwrap();
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        drop(tx);
        let stats = h.await.unwrap().unwrap();
        assert!(
            stats.frames >= 4,
            "expected several frames, got {}",
            stats.frames
        );

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
        let data_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        // Header sizes are consistent with the file length.
        assert_eq!(bytes.len(), WAV_HEADER_LEN + data_len);
        assert_eq!(data_len as u64, stats.data_bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
