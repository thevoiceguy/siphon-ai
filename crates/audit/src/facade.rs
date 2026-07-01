//! Process-wide audit facade.
//!
//! Audit events fire from many layers — the admin HTTP handler
//! (`telemetry`), the SIP INVITE path (`sip-glue`), STIR/SHAKEN
//! rejection (`core`), and the `SIGHUP` reload handler (the daemon
//! binary). Threading an `Arc<dyn AuditSink>` through every one of
//! those constructors would touch a lot of signatures for a
//! best-effort, off-hot-path observability concern. Instead — like the
//! `metrics` and `tracing` facades already used across this codebase —
//! a single process-global handle is [`install`]ed once at startup and
//! every call site emits through [`emit`].
//!
//! Contract:
//! - Until [`install`] runs (audit disabled, or pre-startup), [`emit`]
//!   is a cheap no-op — no task is spawned.
//! - [`emit`] is fire-and-forget: it spawns the sink's async `emit` so
//!   the calling task (an admin request, a SIP transaction) never
//!   blocks on file or network I/O. It must be called from within the
//!   Tokio runtime, which every call site is.
//! - Hot reload is handled by installing a *swappable* sink (the daemon
//!   wraps the real sink so `SIGHUP` can replace it); the global handle
//!   itself is set exactly once.

use std::sync::OnceLock;

use tracing::debug;

use crate::event::AuditEvent;
use crate::sink::AuditSinkHandle;

static GLOBAL: OnceLock<AuditSinkHandle> = OnceLock::new();

/// Install the process-wide audit sink. Idempotent-ish: the first call
/// wins; a second call is ignored and logged (the daemon installs once
/// at startup). Returns `true` if this call installed the sink.
pub fn install(sink: AuditSinkHandle) -> bool {
    let mut installed = false;
    let _ = GLOBAL.get_or_init(|| {
        installed = true;
        sink
    });
    if !installed {
        debug!("audit sink already installed; ignoring second install");
    }
    installed
}

/// Emit an audit event through the installed sink. No-op (no spawn)
/// when audit is not configured. Never blocks the caller.
pub fn emit(event: AuditEvent) {
    if let Some(sink) = GLOBAL.get() {
        let sink = sink.clone();
        tokio::spawn(async move {
            sink.emit(event).await;
        });
    }
}

/// Whether an audit sink has been installed. Lets a call site skip
/// building an event payload entirely when audit is off.
pub fn is_enabled() -> bool {
    GLOBAL.get().is_some()
}
