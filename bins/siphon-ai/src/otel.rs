//! Deferred activation of the OTLP tracing layer (0.22.0).
//!
//! `main::init_tracing` installs a reloadable OTLP layer **inactive** (a
//! no-op `None`) before config is loaded, so config-load warnings still
//! print. The [`Runtime`](crate::Runtime), which has the config and installs
//! the process-global OTLP provider, then calls [`OtelActivation::activate`]
//! to swap in a live layer bound to `opentelemetry::global::tracer`. When
//! `[observability.otlp]` is disabled, `activate` is simply never called and
//! the layer stays a zero-cost no-op.
//!
//! The activation is a boxed closure so the concrete `reload::Handle<...>`
//! type (which names the whole subscriber-layer stack) stays inside
//! `init_tracing` and never has to be spelled out here or on `Runtime`.

use tracing::warn;

/// A one-shot handle that turns the inactive OTLP tracing layer live. Built by
/// `init_tracing`, consumed by the runtime after the OTLP provider is set.
pub struct OtelActivation {
    reload: Box<dyn FnOnce() -> Result<(), tracing_subscriber::reload::Error> + Send>,
}

impl OtelActivation {
    /// Wrap the layer-reload closure. The closure must build the live OTLP
    /// layer from the *current* global tracer and swap it in — so it has to
    /// run after the global OTLP provider is installed.
    pub fn new(
        reload: Box<dyn FnOnce() -> Result<(), tracing_subscriber::reload::Error> + Send>,
    ) -> Self {
        Self { reload }
    }

    /// Swap the inactive OTLP layer for a live one bound to the current global
    /// tracer. Best-effort — a reload error is logged, never fatal.
    pub fn activate(self) {
        if let Err(e) = (self.reload)() {
            warn!(error = %e, "failed to activate OTLP tracing layer; spans will not export");
        }
    }
}
