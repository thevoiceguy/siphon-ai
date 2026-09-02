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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Mutex;

use arc_swap::ArcSwapOption;
use metrics::counter;
use opentelemetry::logs::{LogRecord as _, Logger, LoggerProvider, Severity};
use opentelemetry::trace::{SpanBuilder, TraceContextExt, Tracer};
use opentelemetry::{Context, InstrumentationScope};
use opentelemetry_sdk::logs::{SdkLogRecord, SdkLogger, SdkLoggerProvider};
use siphon_ai_telemetry::metrics::OTLP_LOG_RECORDS_DROPPED_TOTAL;
use siphon_ai_telemetry::otel::OTEL_SCOPE;
use tracing::warn;
use tracing_subscriber::EnvFilter;

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

// ─── OTLP log export (0.51.0) ───────────────────────────────────────────

/// Records buffered between the `tracing` dispatch and the OTel SDK.
///
/// Sized for a burst, not a backlog: a call's whole lifecycle is a few
/// dozen records, so 1024 absorbs several hundred calls arriving at once
/// while a slow collector holds up the SDK's own batch worker. Past that,
/// dropping is the correct answer (§4.7) — log export is observability, and
/// observability never gets to hold up a call.
const LOG_QUEUE_CAPACITY: usize = 1024;

/// The process-wide OTLP log sink, installed by the runtime once config has
/// been read.
///
/// The tracer has an equivalent in `opentelemetry::global`; `opentelemetry`
/// 0.32 has no such global for logs, so we keep our own — same shape, same
/// lifecycle, one cell set exactly once at startup.
static LOG_SINK: OnceLock<LogSink> = OnceLock::new();

/// The hand-off from the `tracing` dispatch to the OTel SDK.
///
/// **Why there is a queue here at all**, when `BatchLogProcessor` is already
/// a background worker: its `emit` is not merely a channel push, it also
/// emits `tracing` events of its own — "queue full, dropping records",
/// "emitted after Shutdown". Called from inside a layer's `on_event`, those
/// nested events run a second filter pass over the same thread-local
/// `FilterState` the outer event is still using and leave their bits
/// behind: a `debug_assert` panic in `tracing-subscriber` in debug builds,
/// and silently wrong per-layer filtering in release. Found on the first
/// live run against a collector, at teardown.
///
/// So nothing calls into the SDK from within `on_event`. The layer stamps
/// the record's trace context — which must happen there, while the span's
/// context is still current — and pushes; a dedicated thread does the rest.
/// That is also the shape §4.7 asks for: queue, return, never block.
struct LogSink {
    /// `None` after [`OtelLogControl::quiesce`]: dropping the last sender
    /// closes the channel, which is how the worker is told to finish.
    tx: ArcSwapOption<SyncSender<SdkLogRecord>>,
    /// Records the queue had no room for. Monotonic; mirrored into
    /// [`OTLP_LOG_RECORDS_DROPPED_TOTAL`] by the worker.
    dropped: AtomicU64,
    /// Joined at shutdown so pending records reach the SDK before the
    /// provider is flushed. Touched at startup and teardown only — never on
    /// the emit path, which is why a plain `Mutex` is fine here.
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// A [`LoggerProvider`] that resolves [`LOG_SINK`] on first use.
///
/// The counterpart to [`LazyGlobalTracer`], and for the same reason: the
/// appender layer is built inside `init_tracing`, which runs *before* config
/// is loaded so config-load warnings still print — and the real provider
/// carries the endpoint, so it cannot exist yet. The layer's per-layer
/// filter starts at `off` and only opens in [`OtelLogControl::activate`],
/// after the sink is in the cell, so the deferred lookup always lands on the
/// real one.
#[derive(Debug, Default)]
pub struct LazyLoggerProvider;

impl LoggerProvider for LazyLoggerProvider {
    type Logger = LazyLogger;

    fn logger_with_scope(&self, scope: InstrumentationScope) -> LazyLogger {
        LazyLogger {
            scope,
            inner: OnceLock::new(),
        }
    }
}

/// The [`Logger`] half of [`LazyLoggerProvider`]. One per bridge layer.
#[derive(Debug)]
pub struct LazyLogger {
    scope: InstrumentationScope,
    /// Used for `create_log_record` and `event_enabled` only — both are
    /// pure, neither touches a processor. `emit` goes through [`LOG_SINK`].
    inner: OnceLock<SdkLogger>,
}

impl LazyLogger {
    fn inner(&self) -> &SdkLogger {
        self.inner.get_or_init(|| {
            LOGGER_PROVIDER
                .get()
                .cloned()
                // Unreachable while the filter is doing its job: nothing
                // reaches this layer until `activate` opens it, and the
                // runtime only calls that after setting the cell. If that
                // invariant ever broke, a provider with no processors drops
                // records — the right failure for a best-effort
                // observability path (§4.7), and better than an `expect`
                // that would panic a call's task.
                .unwrap_or_else(|| SdkLoggerProvider::builder().build())
                .logger_with_scope(self.scope.clone())
        })
    }
}

/// The provider the lazy logger reads its scope-bound `SdkLogger` from, and
/// the one the worker emits through. Set once, with [`LOG_SINK`].
static LOGGER_PROVIDER: OnceLock<SdkLoggerProvider> = OnceLock::new();

impl Logger for LazyLogger {
    type LogRecord = SdkLogRecord;

    fn create_log_record(&self) -> Self::LogRecord {
        self.inner().create_log_record()
    }

    fn emit(&self, mut record: Self::LogRecord) {
        // Stamp the trace context *here*, on the emitting thread, while the
        // span's OpenTelemetry context is still current — this is the whole
        // feature, and it is the one thing that cannot be deferred to the
        // worker. `SdkLogger::emit` would otherwise do it, from a thread
        // where the context is long gone. A record that arrives already
        // carrying a context is left alone by the SDK.
        Context::map_current(|cx| {
            if cx.has_active_span() {
                let span = cx.span();
                let sc = span.span_context();
                record.set_trace_context(sc.trace_id(), sc.span_id(), Some(sc.trace_flags()));
            }
        });

        let Some(tx) = LOG_SINK.get().and_then(|sink| sink.tx.load_full()) else {
            // Before activation or after quiesce. Nothing to count against:
            // the layer's filter is shut in both states, so getting here at
            // all would be a bug, not a dropped record.
            return;
        };
        if tx.try_send(record).is_err() {
            // Full, or the worker is gone. Drop and count — never block, and
            // never call anything that might log (§4.7).
            if let Some(sink) = LOG_SINK.get() {
                sink.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn event_enabled(&self, level: Severity, target: &str, name: Option<&str>) -> bool {
        self.inner().event_enabled(level, target, name)
    }
}

/// Drain the queue into the SDK until the layer is quiesced and the last
/// sender drops.
///
/// Runs on its own thread rather than a tokio task so it is independent of
/// runtime shutdown ordering, and so a slow SDK call can never occupy a
/// worker a call needs.
fn run_log_worker(rx: std::sync::mpsc::Receiver<SdkLogRecord>, logger: SdkLogger) {
    let mut published = 0u64;
    for record in rx {
        logger.emit(record);
        // Mirror the producer's drop counter. Safe to do from here — this
        // thread is not inside anyone's `on_event`, so a `tracing` event
        // raised by the metrics facade or the SDK is just an ordinary event.
        if let Some(sink) = LOG_SINK.get() {
            let dropped = sink.dropped.load(Ordering::Relaxed);
            if dropped != published {
                counter!(OTLP_LOG_RECORDS_DROPPED_TOTAL, "reason" => "queue_full")
                    .absolute(dropped);
                published = dropped;
            }
        }
    }
    if let Some(sink) = LOG_SINK.get() {
        let dropped = sink.dropped.load(Ordering::Relaxed);
        if dropped != published {
            counter!(OTLP_LOG_RECORDS_DROPPED_TOTAL, "reason" => "queue_full").absolute(dropped);
        }
    }
}

/// Targets whose own events must never be *exported* over OTLP.
///
/// Exporting a record makes the OTel SDK and its gRPC stack emit events;
/// shipping those would make export cause export. They still reach the
/// console and the error ring — this mutes them on the wire only.
const EXPORT_MUTED_TARGETS: &[&str] = &[
    "opentelemetry",
    "tonic",
    "h2",
    "hyper",
    "hyper_util",
    "tower",
    "reqwest",
];

/// The per-layer filter for the OTLP log layer: `level`, minus the
/// self-referential targets above.
///
/// `level` has already been validated at config load, and an unparseable
/// directive here would be a bug rather than operator input — so a bad one
/// is skipped rather than panicking a running daemon (§4.7).
pub fn log_export_filter(level: &str) -> EnvFilter {
    let mut filter = EnvFilter::new(level);
    for target in EXPORT_MUTED_TARGETS {
        if let Ok(directive) = format!("{target}=off").parse() {
            filter = filter.add_directive(directive);
        }
    }
    filter
}

/// Controls the OTLP **log** layer — the log-side twin of
/// [`OtelActivation`], except it is not one-shot: the daemon opens the layer
/// at startup and closes it again before flushing at shutdown.
///
/// Built by `init_tracing`, held by the runtime.
pub struct OtelLogControl {
    reload: Box<dyn Fn(EnvFilter) -> Result<(), tracing_subscriber::reload::Error> + Send + Sync>,
}

impl OtelLogControl {
    /// Wrap the filter-reload closure.
    pub fn new(
        reload: Box<
            dyn Fn(EnvFilter) -> Result<(), tracing_subscriber::reload::Error> + Send + Sync,
        >,
    ) -> Self {
        Self { reload }
    }

    /// Install `provider`, start the export worker, and open the layer at
    /// `level`.
    ///
    /// Order matters: the sink exists before the filter opens, so the
    /// layer's first record has somewhere to go. Best-effort throughout — a
    /// failure here costs log export, never a call (§4.7).
    pub fn activate(&self, provider: SdkLoggerProvider, level: &str) {
        let logger = provider.logger(OTEL_SCOPE);
        if LOGGER_PROVIDER.set(provider).is_err() {
            warn!("OTLP logger provider already installed; keeping the first one");
        }
        let (tx, rx) = sync_channel(LOG_QUEUE_CAPACITY);
        let worker = std::thread::Builder::new()
            .name("otlp-log-export".into())
            .spawn(move || run_log_worker(rx, logger));
        let worker = match worker {
            Ok(handle) => handle,
            Err(e) => {
                warn!(error = %e, "could not start the OTLP log worker; log records will not export");
                return;
            }
        };
        let installed = LOG_SINK.set(LogSink {
            tx: ArcSwapOption::from_pointee(tx),
            dropped: AtomicU64::new(0),
            worker: Mutex::new(Some(worker)),
        });
        if installed.is_err() {
            warn!("OTLP log sink already installed; keeping the first one");
            return;
        }
        if let Err(e) = (self.reload)(log_export_filter(level)) {
            warn!(error = %e, "failed to activate OTLP log layer; log records will not export");
        }
    }

    /// Close the layer, then drain what is already queued.
    ///
    /// Called at teardown *before* the provider is flushed. Feeding a
    /// pipeline while flushing it races the flush against its own input, and
    /// emitting into an already-shut-down processor is work with nowhere to
    /// go. Dropping the sender is also how the worker is told to finish, so
    /// joining it here is what makes "records pending at SIGTERM are
    /// flushed" true rather than likely. Everything logged after this point
    /// is console-only — by then the collector has had its window.
    pub fn quiesce(&self) {
        if let Err(e) = (self.reload)(EnvFilter::new("off")) {
            warn!(error = %e, "failed to close OTLP log layer before flush");
        }
        let Some(sink) = LOG_SINK.get() else {
            return;
        };
        // Last sender gone → the worker's `for record in rx` ends once the
        // queue is empty.
        sink.tx.store(None);
        let handle = sink.worker.lock().ok().and_then(|mut g| g.take());
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The export filter must carry the operator's level *and* the mutes —
    /// dropping either turns a working pipeline into a loop or a firehose.
    #[test]
    fn export_filter_keeps_the_level_and_mutes_the_self_referential_targets() {
        let rendered = format!("{}", log_export_filter("warn"));
        assert!(rendered.contains("warn"), "level missing: {rendered}");
        for target in EXPORT_MUTED_TARGETS {
            assert!(
                rendered.contains(&format!("{target}=off")),
                "{target} not muted: {rendered}"
            );
        }
    }
}
