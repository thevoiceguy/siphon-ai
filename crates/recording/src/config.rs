//! Compiled `[recording]` configuration.

use std::path::PathBuf;

/// When to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordingMode {
    /// No recording (default) — zero behaviour change.
    #[default]
    Off,
    /// Record every call that reaches a controller.
    Always,
    /// Wire the per-call writer but leave it idle — the WS server drives it
    /// with `StartRecording` / `StopRecording` (and `Pause` / `Resume`).
    OnDemand,
}

/// Compiled `[recording]` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingConfig {
    pub mode: RecordingMode,
    /// Directory recordings are written to (required when `mode != Off`).
    pub dir: PathBuf,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            mode: RecordingMode::Off,
            dir: PathBuf::new(),
        }
    }
}

impl RecordingConfig {
    /// The output path for `call_id` under this config's directory.
    /// (Templating beyond `<dir>/<call_id>.wav` is a later chunk.)
    pub fn path_for(&self, call_id: &str) -> PathBuf {
        self.dir.join(format!("{call_id}.wav"))
    }
}

/// Per-call recording instructions handed to the `CallController` when a
/// call may be recorded. Carries the resolved output path; the sample rate
/// is taken from the call's media tap.
#[derive(Debug, Clone)]
pub struct RecordingSetup {
    pub path: PathBuf,
    /// `true` (mode `always`) → start recording immediately. `false` (mode
    /// `on_demand`) → wait for a `StartRecording` from the WS server.
    pub auto_start: bool,
}
