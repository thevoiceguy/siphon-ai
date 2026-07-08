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
//! Modes: `always` records the whole call; `on_demand` wires the writer
//! idle and the WS server drives it with start/stop/pause/resume (a pause
//! omits the span). Per-route overrides, the CDR pointer, and metrics land
//! in later chunks.

mod config;
mod control;
mod envelope;
mod frame;
mod kek;
mod writer;

pub use config::{RecordingConfig, RecordingMode, RecordingSetup};
pub use control::{RecControl, RecEvent};
pub use envelope::{decrypt, peek_key_id, EnvelopeError};
pub use frame::RecFrame;
pub use kek::{Kek, KekError};
pub use writer::{RecordingError, RecordingStats, RecordingWriter};
