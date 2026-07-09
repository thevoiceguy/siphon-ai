//! Compiled `[recording]` configuration.

use std::path::PathBuf;

use crate::kek::Kek;

/// On-disk recording format (0.25.0). WAV is the default and the v0.5.0
/// behaviour; Opus (in Ogg) is ~10× smaller for voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordingFormat {
    #[default]
    Wav,
    Opus,
}

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
    /// `[recording.encryption]` (0.24.0): seal recordings into `.wava`
    /// envelopes under this KEK. `None` = plaintext WAV.
    pub encryption: Option<Kek>,
    /// `[recording].format` (0.25.0): `wav` (default) or `opus`.
    pub format: RecordingFormat,
    /// `[recording.announcement].file` (0.26.0): a WAV played to the
    /// caller before capture starts. `None` = no announcement.
    pub announcement: Option<PathBuf>,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            mode: RecordingMode::Off,
            dir: PathBuf::new(),
            encryption: None,
            format: RecordingFormat::default(),
            announcement: None,
        }
    }
}

impl RecordingConfig {
    /// The output path for `call_id` under this config's directory.
    /// Extension encodes format + encryption: `wav`/`opus` plaintext,
    /// `wava`/`opusa` sealed. (Local-path templating is out of scope —
    /// the object-storage `key_template` covers naming.)
    pub fn path_for(&self, call_id: &str) -> PathBuf {
        let ext = match (self.format, self.encryption.is_some()) {
            (RecordingFormat::Wav, false) => "wav",
            (RecordingFormat::Wav, true) => "wava",
            (RecordingFormat::Opus, false) => "opus",
            (RecordingFormat::Opus, true) => "opusa",
        };
        self.dir.join(format!("{call_id}.{ext}"))
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
    /// Encrypt at rest under this KEK (0.24.0); `None` = plaintext.
    pub encryption: Option<Kek>,
    /// Output format (0.25.0).
    pub format: RecordingFormat,
    /// Announcement to play before capture starts (0.26.0).
    pub announcement: Option<PathBuf>,
}
