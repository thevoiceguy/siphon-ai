//! Control messages to, and events from, the recording writer.

/// A control the `CallController` routes to the writer (from a WS
/// `BridgeIn` recording message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecControl {
    /// Begin recording (on-demand). No-op if already recording.
    Start,
    /// Suspend recording — the paused span is **omitted** from the WAV
    /// (PCI "stop while the caller reads a card number"), not silenced.
    Pause,
    /// Resume after a [`RecControl::Pause`].
    Resume,
    /// Finalize the recording now (close the file early). The writer goes
    /// terminal for this call; further controls are ignored.
    Stop,
}

/// An event the writer reports back to the `CallController`, which maps it
/// to a WS `BridgeOut` recording event. The controller tags it with the
/// recording id.
#[derive(Debug, Clone)]
pub enum RecEvent {
    /// Recording began (file open). `at` is the monotonic instant the
    /// file opened — the writer's first 20 ms frame is written on the
    /// immediately-following tick, so this is the timeline position of
    /// the file's first sample (`recording_started.offset_ms`,
    /// PROTOCOL.md §3.11).
    Started { at: std::time::Instant },
    /// Recording finalized cleanly (file written + header patched).
    Stopped {
        data_bytes: u64,
        frames: u64,
        at: std::time::Instant,
    },
    /// Recording could not start or write.
    Failed {
        reason: String,
        at: std::time::Instant,
    },
}
