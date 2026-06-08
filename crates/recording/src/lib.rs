//! Per-call audio recording for SiphonAI.
//!
//! Records a call's audio to a stereo WAV (caller = left, bot = right) by
//! forking the two legs off the media tap. The defining constraint is
//! CLAUDE.md §4.3: the audio hot path must never block on recording. So the
//! tap only ever does a non-blocking `try_send` of frame copies onto a
//! bounded channel; a per-call [`RecordingWriter`] task drains that channel,
//! mixes on a 20 ms clock, and does the (batched) file I/O off the audio
//! task.
//!
//! 0.5.0 chunk 1 ships the capture core: `[recording].mode = "always"` → a
//! stereo WAV per call. Control messages (start/stop/pause), per-route
//! overrides, the CDR pointer, and metrics land in later chunks.

mod config;
mod frame;
mod writer;

pub use config::{RecordingConfig, RecordingMode, RecordingSetup};
pub use frame::RecFrame;
pub use writer::{RecordingError, RecordingStats, RecordingWriter};
