//! Music-on-hold source for parked calls (DEV_PLAN_0.7.0.md §2.4).
//!
//! Built per-park at the call's negotiated rate. Produces an *infinite*
//! 20 ms PCM16 stream so the tap's park loop never has to handle an
//! end-of-stream:
//!
//! - `[park].moh_file` set **and** its native rate matches the call →
//!   a looping [`forge_injection::FileSource`] (on end-of-file it
//!   `reset()`s to the top and keeps going).
//! - file unset / rate mismatch (forge has no resampler) / open error →
//!   [`forge_injection::ToneGenerator::comfort_noise`] at the call's
//!   rate. A rate mismatch logs once at `info` and falls back; it is
//!   **not** a park failure (§4 of the design note).
//!
//! `next_frame` never errors and never panics — the looping + fallback
//! guarantee a frame every time, so the parked caller always hears
//! *something* (hold music, comfort noise, or — only on a truly broken
//! decoder mid-loop — silence).

use std::path::Path;

use forge_injection::{AudioSource, FileSource, ToneGenerator};
use tracing::{debug, info};

/// Per-call hold-music generator. See the module docs.
pub struct MohSource {
    inner: Inner,
    /// Samples per 20 ms frame at the call's rate (160 @ 8 k, 320 @ 16 k).
    frame_samples: usize,
    /// The call's rate — used to synthesize a silence frame on the
    /// (should-never-happen) double-decode-failure path.
    sample_rate: u32,
}

enum Inner {
    /// Looping file playback.
    File(FileSource),
    /// Infinite comfort noise (or silence) — the fallback.
    Tone(ToneGenerator),
}

impl MohSource {
    /// Build the MOH source for a call at `sample_rate`. `moh_file`
    /// `None` (or any load/rate problem) yields comfort noise.
    pub fn new(moh_file: Option<&Path>, sample_rate: u32) -> Self {
        let frame_samples = (sample_rate / 1000) as usize * 20;
        let inner = match moh_file {
            Some(path) => Self::open_file(path, sample_rate)
                .unwrap_or_else(|| Inner::Tone(ToneGenerator::comfort_noise(sample_rate))),
            None => Inner::Tone(ToneGenerator::comfort_noise(sample_rate)),
        };
        Self {
            inner,
            frame_samples,
            sample_rate,
        }
    }

    /// Try to open `path` as a looping source at `sample_rate`. `None`
    /// on any failure (caller falls back to comfort noise) — forge has
    /// no resampler, so a rate mismatch is a `None` here, by design.
    fn open_file(path: &Path, sample_rate: u32) -> Option<Inner> {
        match FileSource::open_trusted(path).and_then(|f| f.with_sample_rate(sample_rate)) {
            Ok(f) => {
                debug!(path = %path.display(), sample_rate, "MOH file opened");
                Some(Inner::File(f))
            }
            Err(e) => {
                info!(
                    path = %path.display(),
                    sample_rate,
                    error = %e,
                    "MOH file unusable at call rate; falling back to comfort noise"
                );
                None
            }
        }
    }

    /// One 20 ms PCM16 frame. Never errors: a file source loops at EOF,
    /// the tone source is infinite, and the worst case is a silence
    /// frame (a decoder that fails even after a reset).
    pub fn next_frame(&mut self) -> Vec<i16> {
        let n = self.frame_samples;
        match &mut self.inner {
            Inner::Tone(t) => t.read_frame(n).unwrap_or_else(|_| self.silence()),
            Inner::File(f) => match f.read_frame(n) {
                // A full frame mid-file.
                Ok(frame) if frame.len() == n => frame,
                // Short read or EOF → loop: reset and read a fresh
                // frame from the top. (The brief boundary glitch is
                // inaudible in hold music.)
                _ => {
                    if f.reset().is_err() {
                        return self.silence();
                    }
                    f.read_frame(n).unwrap_or_else(|_| vec![0i16; n])
                }
            },
        }
    }

    fn silence(&self) -> Vec<i16> {
        vec![0i16; self.frame_samples]
    }

    /// The call rate this source produces at — used by the tap to
    /// sanity-check it matches the forge session.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

impl std::fmt::Debug for MohSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MohSource")
            .field(
                "kind",
                &match self.inner {
                    Inner::File(_) => "file",
                    Inner::Tone(_) => "tone",
                },
            )
            .field("sample_rate", &self.sample_rate)
            .finish()
    }
}

/// Play-once announcement source (0.26.0) — the "this call may be
/// recorded" prompt played to the caller before capture starts. Unlike
/// [`MohSource`] it does **not** loop: EOF means the announcement
/// finished. And unlike MOH it is **fail-loud**: a compliance prompt
/// that can't play must surface as an error (the caller of `new`
/// fail-closes recording), never degrade to comfort noise.
pub struct AnnounceSource {
    file: FileSource,
    frame_samples: usize,
}

impl std::fmt::Debug for AnnounceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnnounceSource")
            .field("frame_samples", &self.frame_samples)
            .finish_non_exhaustive()
    }
}

impl AnnounceSource {
    /// Open `path` at the call's `sample_rate`. Errors on any load or
    /// rate problem (forge has no resampler — provide the file at the
    /// bridge rate, 8 or 16 kHz).
    pub fn new(path: &Path, sample_rate: u32) -> Result<Self, String> {
        let frame_samples = (sample_rate / 1000) as usize * 20;
        let file = FileSource::open_trusted(path)
            .and_then(|f| f.with_sample_rate(sample_rate))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self {
            file,
            frame_samples,
        })
    }

    /// One 20 ms PCM16 frame, or `None` when the announcement has
    /// finished. A short tail read is zero-padded so the last frame
    /// plays whole; a decode error ends the announcement (fail-closed
    /// is the caller's job).
    pub fn next_frame(&mut self) -> Option<Vec<i16>> {
        match self.file.read_frame(self.frame_samples) {
            Ok(frame) if frame.is_empty() => None,
            Ok(mut frame) => {
                frame.resize(self.frame_samples, 0);
                Some(frame)
            }
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comfort_noise_when_no_file() {
        let mut moh = MohSource::new(None, 8000);
        let frame = moh.next_frame();
        assert_eq!(frame.len(), 160);
        // Comfort noise is non-silent.
        assert!(frame.iter().any(|&s| s != 0));
    }

    #[test]
    fn frame_size_tracks_rate() {
        let mut moh16 = MohSource::new(None, 16000);
        assert_eq!(moh16.next_frame().len(), 320);
    }

    /// Minimal mono PCM16 WAV: `n_samples` at `rate`.
    fn write_wav(path: &Path, rate: u32, n_samples: usize) {
        let data_len = (n_samples * 2) as u32;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // PCM
        b.extend_from_slice(&1u16.to_le_bytes()); // mono
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..n_samples {
            b.extend_from_slice(&(((i % 100) as i16) * 50).to_le_bytes());
        }
        std::fs::write(path, b).unwrap();
    }

    #[test]
    fn announce_plays_once_then_eof() {
        let dir = std::env::temp_dir().join(format!("siphon_ann_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("prompt.wav");
        // 2.5 frames at 8 kHz: 400 samples → 2 full frames + 1 padded tail.
        write_wav(&wav, 8000, 400);

        let mut src = AnnounceSource::new(&wav, 8000).expect("opens at matching rate");
        let mut frames = 0;
        while let Some(frame) = src.next_frame() {
            assert_eq!(frame.len(), 160, "frames are whole (tail zero-padded)");
            frames += 1;
            assert!(frames < 10, "must not loop like MOH");
        }
        assert_eq!(frames, 3, "2 full + 1 padded tail, then EOF");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn announce_fails_loud_on_missing_or_mismatched_file() {
        assert!(AnnounceSource::new(Path::new("/nonexistent/x.wav"), 8000).is_err());
        let dir = std::env::temp_dir().join(format!("siphon_ann_mm_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("wrong-rate.wav");
        write_wav(&wav, 44_100, 1000);
        // 44.1 kHz file at an 8 kHz call: forge has no resampler → error,
        // never comfort-noise (this is a compliance prompt).
        assert!(AnnounceSource::new(&wav, 8000).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_falls_back_to_comfort_noise() {
        let mut moh = MohSource::new(Some(Path::new("/nonexistent/x.wav")), 8000);
        let frame = moh.next_frame();
        assert_eq!(frame.len(), 160);
        assert!(matches!(moh.inner, Inner::Tone(_)));
    }

    #[test]
    fn produces_frames_indefinitely() {
        let mut moh = MohSource::new(None, 8000);
        for _ in 0..1000 {
            assert_eq!(moh.next_frame().len(), 160);
        }
    }
}
