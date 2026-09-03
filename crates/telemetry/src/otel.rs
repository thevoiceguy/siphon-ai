//! OpenTelemetry OTLP trace export (0.22.0) and log export (0.51.0).
//!
//! Builds an OTLP/gRPC span exporter + a batch-exporting `SdkTracerProvider`
//! and installs it as the process-global tracer provider, so the
//! `tracing-opentelemetry` layer wired in the daemon binary ships per-call
//! spans to a collector (Tempo / Jaeger / an OTel Collector).
//!
//! Optionally builds a second, log-side pipeline over the same endpoint and
//! the same resource ([`OtelConfig::logs`]), so a log record lands beside
//! the span it was emitted inside. Unlike the tracer there is no
//! process-global logger provider in `opentelemetry` 0.32, so the built
//! `SdkLoggerProvider` is handed back to the caller, which wires it into
//! its own subscriber.
//!
//! Off by default. **Best-effort**, mirroring the HEP worker (CLAUDE.md §4.7):
//! spans batch on a background worker and drop on overflow; a slow or
//! unreachable collector never blocks the call path. Config is passed as
//! primitives (not `siphon-ai-config`) to keep the dep graph minimal, same as
//! [`crate::hep::HepTelemetry::build`].

use std::time::Duration;

use metrics::{counter, gauge};
use opentelemetry::KeyValue;
use opentelemetry_otlp::{LogExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogBatch, SdkLoggerProvider};
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider, SpanData};
use opentelemetry_sdk::Resource;
use thiserror::Error;
use tracing::{info, warn};

use crate::metrics::{OTLP_COLLECTOR_UP, OTLP_LOG_RECORDS_DROPPED_TOTAL, OTLP_SPANS_DROPPED_TOTAL};

/// Record the outcome of one batch export against the shared collector
/// gauge, and the batch's size against `dropped` when it failed (#596).
///
/// This is the one place a dead collector becomes visible on `/metrics`.
/// The SDK's `BatchLogProcessor` / `BatchSpanProcessor` accept records
/// instantly and discard a batch whose export fails, logging only through
/// the `opentelemetry_sdk` target — which any target-named log filter
/// mutes (#597). Wrapping the exporter is the only seam that sees both
/// the failure and the batch size; runs on the SDK's own batch worker, so
/// it is off every call path and outside any `on_event`.
fn observe_export(dropped_metric: &'static str, batch_len: usize, result: &OTelSdkResult) {
    match result {
        Ok(()) => gauge!(OTLP_COLLECTOR_UP).set(1.0),
        Err(_) => {
            gauge!(OTLP_COLLECTOR_UP).set(0.0);
            counter!(dropped_metric, "reason" => "collector_down").increment(batch_len as u64);
        }
    }
}

/// Publish the outage series at their resting values so an alert on
/// them never has to reason about an absent series (same rationale as
/// `hep::publish_counters` at startup).
fn publish_export_baseline(logs: bool) {
    gauge!(OTLP_COLLECTOR_UP).set(1.0);
    counter!(OTLP_SPANS_DROPPED_TOTAL, "reason" => "collector_down").absolute(0);
    if logs {
        counter!(OTLP_LOG_RECORDS_DROPPED_TOTAL, "reason" => "collector_down").absolute(0);
    }
}

/// [`SpanExporter`] that reports every batch's outcome to the metrics
/// registry before returning it to the SDK unchanged.
#[derive(Debug)]
struct ObservedSpanExporter<E>(E);

impl<E: opentelemetry_sdk::trace::SpanExporter> opentelemetry_sdk::trace::SpanExporter
    for ObservedSpanExporter<E>
{
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let n = batch.len();
        let result = self.0.export(batch).await;
        observe_export(OTLP_SPANS_DROPPED_TOTAL, n, &result);
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource)
    }
}

/// [`LogExporter`] twin of [`ObservedSpanExporter`].
#[derive(Debug)]
struct ObservedLogExporter<E>(E);

impl<E: opentelemetry_sdk::logs::LogExporter> opentelemetry_sdk::logs::LogExporter
    for ObservedLogExporter<E>
{
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        let n = batch.iter().count();
        let result = self.0.export(batch).await;
        observe_export(OTLP_LOG_RECORDS_DROPPED_TOTAL, n, &result);
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn event_enabled(
        &self,
        level: opentelemetry::logs::Severity,
        target: &str,
        name: Option<&str>,
    ) -> bool {
        self.0.event_enabled(level, target, name)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource)
    }
}

/// The instrumentation-scope name the daemon's `tracing-opentelemetry` layer
/// uses (`opentelemetry::global::tracer(OTEL_SCOPE)`); kept here so the
/// producer and the exporter agree on one name.
pub const OTEL_SCOPE: &str = "siphon-ai";

#[derive(Debug, Error)]
pub enum OtelError {
    #[error("failed to build OTLP span exporter for {endpoint}: {detail}")]
    Exporter { endpoint: String, detail: String },

    #[error("failed to build OTLP log exporter for {endpoint}: {detail}")]
    LogExporter { endpoint: String, detail: String },
}

/// Resolved OTLP export plan. Primitives so `siphon-ai-config` isn't a dep here.
#[derive(Debug, Clone)]
pub struct OtelConfig {
    /// OTLP/gRPC collector endpoint, e.g. `http://localhost:4317`.
    pub endpoint: String,
    /// Per-export gRPC timeout.
    pub timeout: Duration,
    /// Head sampling ratio in `[0.0, 1.0]`; `>= 1.0` = always sample.
    pub sample_ratio: f64,
    /// `service.name` resource attribute.
    pub service_name: String,
    /// `service.instance.id` resource attribute (the node id).
    pub node_id: String,
    /// Extra `key=value` resource attributes (e.g. `deployment.environment`).
    pub extra_attributes: Vec<(String, String)>,
    /// Also build a log-export pipeline over the same endpoint, timeout and
    /// resource (`[observability.otlp.logs]`). The *level* to export at is
    /// deliberately not here: it is a `tracing` filter, and it belongs on the
    /// appender layer in the daemon binary, not on the exporter.
    pub logs: bool,
}

/// A live OTLP tracer provider. Held for the process lifetime; call
/// [`OtelTelemetry::shutdown`] on daemon shutdown to flush pending spans.
#[derive(Clone)]
pub struct OtelTelemetry {
    provider: SdkTracerProvider,
    /// `Some` when `[observability.otlp.logs]` is on. Nothing installs it
    /// process-globally — `opentelemetry` 0.32 has no global logger provider
    /// the way it has a global tracer provider — so the caller takes it with
    /// [`OtelTelemetry::logger_provider`] and wires it into its subscriber.
    logger_provider: Option<SdkLoggerProvider>,
}

impl OtelTelemetry {
    /// Build the OTLP/gRPC exporter + batch provider and install it as the
    /// process-global tracer provider. After this returns, a
    /// `tracing-opentelemetry` layer built from
    /// `opentelemetry::global::tracer(`[`OTEL_SCOPE`]`)` exports to
    /// `cfg.endpoint`. Fails only if the exporter can't be constructed (bad
    /// endpoint / TLS backend) — surfaced at startup so a misconfig fails loud
    /// (CLAUDE.md §4.6). A collector that's merely *down* is not an error:
    /// the batch worker retries/drops in the background.
    pub fn build(cfg: OtelConfig) -> Result<Self, OtelError> {
        let exporter = SpanExporter::builder()
            .with_tonic()
            .with_endpoint(cfg.endpoint.clone())
            .with_timeout(cfg.timeout)
            .build()
            .map_err(|e| OtelError::Exporter {
                endpoint: cfg.endpoint.clone(),
                detail: e.to_string(),
            })?;

        let mut attrs = vec![
            KeyValue::new("service.name", cfg.service_name.clone()),
            KeyValue::new("service.instance.id", cfg.node_id.clone()),
        ];
        for (k, v) in cfg.extra_attributes {
            attrs.push(KeyValue::new(k, v));
        }
        let resource = Resource::builder().with_attributes(attrs).build();

        // ParentBased so a sampled parent keeps its children — and so a future
        // inbound W3C traceparent from the WS server (v0.23.0) is honoured.
        let ratio = cfg.sample_ratio.clamp(0.0, 1.0);
        let sampler = if ratio >= 1.0 {
            Sampler::ParentBased(Box::new(Sampler::AlwaysOn))
        } else {
            Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)))
        };

        // Exporters are wrapped so a failed batch is counted and the
        // collector gauge flips (#596); the SDK sees the same result.
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(ObservedSpanExporter(exporter))
            .with_sampler(sampler)
            .with_resource(resource.clone())
            .build();

        // Install as the process global so `global::tracer()` (called by the
        // daemon's tracing layer once it activates) routes here.
        opentelemetry::global::set_tracer_provider(provider.clone());

        info!(
            endpoint = %cfg.endpoint,
            sample_ratio = ratio,
            "OTLP trace export active"
        );

        // The log pipeline: its own exporter and batch worker over the same
        // endpoint and timeout, sharing the resource so a record's
        // `service.name` / `service.instance.id` match the spans it will be
        // shown against. Built after the tracer so a bad endpoint is reported
        // once, by the span exporter, rather than twice.
        let logger_provider = if cfg.logs {
            let exporter = LogExporter::builder()
                .with_tonic()
                .with_endpoint(cfg.endpoint.clone())
                .with_timeout(cfg.timeout)
                .build()
                .map_err(|e| OtelError::LogExporter {
                    endpoint: cfg.endpoint.clone(),
                    detail: e.to_string(),
                })?;
            // No "active" line here on purpose: the caller logs it after
            // it has opened the appender layer, so the line that announces
            // the pipeline is itself the first record to travel it.
            Some(
                SdkLoggerProvider::builder()
                    .with_batch_exporter(ObservedLogExporter(exporter))
                    .with_resource(resource)
                    .build(),
            )
        } else {
            None
        };
        publish_export_baseline(cfg.logs);

        Ok(Self {
            provider,
            logger_provider,
        })
    }

    /// The log-export provider, when `[observability.otlp.logs]` is on.
    /// Cheap to clone — the SDK type is an `Arc` inside.
    pub fn logger_provider(&self) -> Option<SdkLoggerProvider> {
        self.logger_provider.clone()
    }

    /// Flush + shut down the provider, giving batched spans a bounded window to
    /// reach the collector. Best-effort — errors are logged, never fatal.
    pub fn shutdown(&self, timeout: Duration) {
        match self.provider.shutdown_with_timeout(timeout) {
            Ok(()) => info!("OTLP tracer flushed + shut down"),
            Err(e) => {
                warn!(error = %e, "OTLP tracer shutdown/flush error; some spans may be lost")
            }
        }
        // Same bounded window for the log pipeline. The records pending at a
        // SIGTERM are the ones that say why the daemon is going away, so
        // dropping them is exactly the wrong trade.
        if let Some(logs) = &self.logger_provider {
            match logs.shutdown_with_timeout(timeout) {
                Ok(()) => info!("OTLP logger flushed + shut down"),
                Err(e) => {
                    warn!(error = %e, "OTLP logger shutdown/flush error; some log records may be lost")
                }
            }
        }
    }
}

/// W3C Trace Context headers ([`https://www.w3.org/TR/trace-context/`])
/// rendered from a live span, ready to be sent verbatim on an outgoing
/// request — the WS-upgrade propagation surface of 0.23.0.
///
/// Plain strings (not `opentelemetry` types) so consumers — `siphon-ai-core`
/// stamping the bridge `start` — don't need an OTel dep of their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContextHeaders {
    /// `traceparent` value, e.g. `00-<32 hex trace-id>-<16 hex span-id>-01`.
    pub traceparent: String,
    /// `tracestate` value (vendor-specific key/value list); `None` when
    /// there is none to forward, so callers can omit the header entirely.
    pub tracestate: Option<String>,
}

/// Render the **current tracing span**'s OTel context as W3C trace-context
/// headers, or `None` when there is no exportable context to propagate —
/// i.e. the OTLP layer is inactive (`[observability.otlp]` disabled) or the
/// caller isn't inside an instrumented span. An *unsampled* span still
/// returns `Some` (with the `00` flags byte), per the W3C spec: downstream
/// services should see the trace id even when this hop chose not to record.
///
/// Cheap and lock-free (a registry lookup + formatting); still, callers on
/// per-call paths should invoke it once per session, not per frame.
pub fn current_trace_context() -> Option<TraceContextHeaders> {
    use opentelemetry::propagation::TextMapPropagator;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let ctx = tracing::Span::current().context();
    let mut carrier: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // The propagator injects nothing when the span context is invalid
    // (no OTel layer / no span) — that absence is our `None`.
    TraceContextPropagator::new().inject_context(&ctx, &mut carrier);
    let traceparent = carrier.remove("traceparent")?;
    let tracestate = carrier.remove("tracestate").filter(|s| !s.is_empty());
    Some(TraceContextHeaders {
        traceparent,
        tracestate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    /// `traceparent` is `00-<32 hex>-<16 hex>-<2 hex>` — assert shape
    /// without pulling in a regex dep.
    fn assert_traceparent_shape(value: &str) {
        let parts: Vec<&str> = value.split('-').collect();
        assert_eq!(parts.len(), 4, "traceparent must have 4 fields: {value}");
        assert_eq!(parts[0], "00", "version field: {value}");
        assert_eq!(parts[1].len(), 32, "trace-id length: {value}");
        assert_eq!(parts[2].len(), 16, "span-id length: {value}");
        assert_eq!(parts[3].len(), 2, "flags length: {value}");
        for field in &parts[1..] {
            assert!(
                field.chars().all(|c| c.is_ascii_hexdigit()),
                "non-hex field in {value}"
            );
        }
        assert_ne!(
            parts[1], "00000000000000000000000000000000",
            "all-zero trace-id is invalid"
        );
    }

    #[test]
    fn no_otel_layer_yields_none() {
        // A subscriber without the OTel layer: spans exist but carry no
        // OTel context, so there is nothing to propagate.
        let subscriber = tracing_subscriber::registry();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("call");
            let _e = span.enter();
            assert_eq!(current_trace_context(), None);
        });
    }

    #[test]
    fn live_otel_span_yields_valid_traceparent() {
        use opentelemetry::trace::TracerProvider as _;
        // A provider with no exporter: spans are created (and dropped),
        // which is all context extraction needs.
        let provider = SdkTracerProvider::builder().build();
        let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("call");
            let _e = span.enter();
            let headers = current_trace_context().expect("live span must yield a traceparent");
            assert_traceparent_shape(&headers.traceparent);
        });
    }

    #[test]
    fn outside_any_span_yields_none() {
        use opentelemetry::trace::TracerProvider as _;
        let provider = SdkTracerProvider::builder().build();
        let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(current_trace_context(), None);
        });
    }
}
