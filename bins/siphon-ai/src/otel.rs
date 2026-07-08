//! Deferred activation of the OTLP tracing layer (0.22.0, reshaped 0.23.0).
//!
//! `main::init_tracing` installs the OTLP layer **concrete** (never behind
//! `tracing_subscriber::reload`) with [`LazyGlobalTracer`] as its tracer and
//! a *reloadable per-layer filter* that starts at `LevelFilter::OFF`. The
//! [`Runtime`](crate::Runtime), which has the config and installs the
//! process-global OTLP provider, then calls [`OtelActivation::activate`] to
//! open the filter. When `[observability.otlp]` is disabled, `activate` is
//! simply never called: the `OFF` filter keeps the layer at zero per-span
//! cost.
//!
//! Why the filter reloads and not the layer: W3C trace propagation (0.23.0)
//! extracts the current span's OTel context via
//! `OpenTelemetrySpanExt::context()`, which finds the layer through a
//! `downcast_ref::<WithContext>()` on the subscriber stack —
//! and `reload::Layer::downcast_raw` deliberately refuses to forward
//! downcasts (the pointer could dangle across a reload). A layer behind
//! `reload` therefore exports spans fine but is *invisible* to context
//! extraction; a `Filtered` layer forwards the downcast and reloading a
//! `LevelFilter` is supported.
//!
//! The activation is a boxed closure so the concrete `reload::Handle<...>`
//! type (which names the whole subscriber-layer stack) stays inside
//! `init_tracing` and never has to be spelled out here or on `Runtime`.

use std::sync::OnceLock;

use opentelemetry::global::{self, BoxedSpan, BoxedTracer};
use opentelemetry::trace::{SpanBuilder, Tracer};
use opentelemetry::Context;
use siphon_ai_telemetry::otel::OTEL_SCOPE;
use tracing::warn;

/// A [`Tracer`] that resolves `opentelemetry::global::tracer(OTEL_SCOPE)`
/// **on first span build**, not at construction.
///
/// The OTLP layer must be constructed inside `init_tracing`, before config
/// is loaded — so the real provider doesn't exist yet, and a tracer grabbed
/// then would be permanently bound to the no-op global. The per-layer filter
/// guarantees no span reaches this tracer until [`OtelActivation::activate`]
/// runs, which the runtime only does *after* installing the real provider —
/// so the lazy lookup always lands on the OTLP provider.
#[derive(Default)]
pub struct LazyGlobalTracer {
    inner: OnceLock<BoxedTracer>,
}

impl Tracer for LazyGlobalTracer {
    type Span = BoxedSpan;

    fn build_with_context(&self, builder: SpanBuilder, parent_cx: &Context) -> Self::Span {
        self.inner
            .get_or_init(|| global::tracer(OTEL_SCOPE))
            .build_with_context(builder, parent_cx)
    }
}

/// A one-shot handle that turns the dormant OTLP tracing layer live. Built by
/// `init_tracing`, consumed by the runtime after the OTLP provider is set.
pub struct OtelActivation {
    reload: Box<dyn FnOnce() -> Result<(), tracing_subscriber::reload::Error> + Send>,
}

impl OtelActivation {
    /// Wrap the filter-reload closure. The closure opens the OTLP layer's
    /// per-layer filter (`OFF` → everything); it must run only after the
    /// global OTLP provider is installed, so the first span build resolves
    /// [`LazyGlobalTracer`] against the real provider.
    pub fn new(
        reload: Box<dyn FnOnce() -> Result<(), tracing_subscriber::reload::Error> + Send>,
    ) -> Self {
        Self { reload }
    }

    /// Open the OTLP layer's filter so spans flow to the (now-installed)
    /// global provider. Best-effort — a reload error is logged, never fatal.
    pub fn activate(self) {
        if let Err(e) = (self.reload)() {
            warn!(error = %e, "failed to activate OTLP tracing layer; spans will not export");
        }
    }
}
