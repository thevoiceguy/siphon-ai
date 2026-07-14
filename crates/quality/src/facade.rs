//! Process-wide quality-record facade.
//!
//! Records are emitted by a per-call worker task deep inside the
//! `CallController`; threading a sink handle plus the record cadence
//! through the acceptor / outbound-service constructor chains would
//! touch a lot of signatures for a best-effort, off-hot-path
//! observability concern. Like the audit facade (0.20.0) — and the
//! `metrics` / `tracing` facades before it — a single process-global
//! handle is [`install`]ed once at startup.
//!
//! Contract:
//! - Until [`install`] runs (`[quality]` disabled, or pre-startup),
//!   [`emit`] is a cheap no-op and [`record_interval`] returns `None`
//!   (the controller then spawns no record task at all).
//! - [`emit`] is fire-and-forget: it spawns the sink's async `emit` so
//!   the calling task never blocks on file or network I/O. It counts
//!   `siphon_ai_quality_records_total{kind}` on the way through.
//! - `[quality]` is restart-required (no SIGHUP swap in this release);
//!   the global handle is set exactly once.

use std::sync::OnceLock;
use std::time::Duration;

use tracing::debug;

use crate::record::QualityRecord;
use crate::sink::QualitySinkHandle;

struct Installed {
    sink: QualitySinkHandle,
    interval: Duration,
}

static GLOBAL: OnceLock<Installed> = OnceLock::new();

/// Install the process-wide quality sink and the per-call record
/// cadence. First call wins; a second call is ignored and logged (the
/// daemon installs once at startup). Returns `true` if this call
/// installed the sink.
pub fn install(sink: QualitySinkHandle, interval: Duration) -> bool {
    let mut installed = false;
    let _ = GLOBAL.get_or_init(|| {
        installed = true;
        Installed { sink, interval }
    });
    if !installed {
        debug!("quality sink already installed; ignoring second install");
    }
    installed
}

/// Emit a quality record through the installed sink. No-op (no spawn)
/// when `[quality]` is not configured. Never blocks the caller.
pub fn emit(record: QualityRecord) {
    if let Some(g) = GLOBAL.get() {
        metrics::counter!(
            "siphon_ai_quality_records_total",
            "kind" => record.kind.as_str()
        )
        .increment(1);
        let sink = g.sink.clone();
        tokio::spawn(async move {
            sink.emit(record).await;
        });
    }
}

/// The configured per-call record cadence, or `None` when `[quality]`
/// is off — the controller uses this to decide whether to spawn a
/// record task for the call at all.
pub fn record_interval() -> Option<Duration> {
    GLOBAL.get().map(|g| g.interval)
}

/// Whether a quality sink has been installed.
pub fn is_enabled() -> bool {
    GLOBAL.get().is_some()
}
