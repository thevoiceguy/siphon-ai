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
