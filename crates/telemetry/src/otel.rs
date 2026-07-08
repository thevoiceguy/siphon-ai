//! OpenTelemetry OTLP trace export (0.22.0).
//!
//! Builds an OTLP/gRPC span exporter + a batch-exporting `SdkTracerProvider`
//! and installs it as the process-global tracer provider, so the
//! `tracing-opentelemetry` layer wired in the daemon binary ships per-call
//! spans to a collector (Tempo / Jaeger / an OTel Collector).
//!
//! Off by default. **Best-effort**, mirroring the HEP worker (CLAUDE.md §4.7):
//! spans batch on a background worker and drop on overflow; a slow or
//! unreachable collector never blocks the call path. Config is passed as
//! primitives (not `siphon-ai-config`) to keep the dep graph minimal, same as
//! [`crate::hep::HepTelemetry::build`].

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use thiserror::Error;
use tracing::{info, warn};

/// The instrumentation-scope name the daemon's `tracing-opentelemetry` layer
/// uses (`opentelemetry::global::tracer(OTEL_SCOPE)`); kept here so the
/// producer and the exporter agree on one name.
pub const OTEL_SCOPE: &str = "siphon-ai";

#[derive(Debug, Error)]
pub enum OtelError {
    #[error("failed to build OTLP span exporter for {endpoint}: {detail}")]
    Exporter { endpoint: String, detail: String },
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
}

/// A live OTLP tracer provider. Held for the process lifetime; call
/// [`OtelTelemetry::shutdown`] on daemon shutdown to flush pending spans.
#[derive(Clone)]
pub struct OtelTelemetry {
    provider: SdkTracerProvider,
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

        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_sampler(sampler)
            .with_resource(resource)
            .build();

        // Install as the process global so `global::tracer()` (called by the
        // daemon's tracing layer once it activates) routes here.
        opentelemetry::global::set_tracer_provider(provider.clone());

        info!(
            endpoint = %cfg.endpoint,
            sample_ratio = ratio,
            "OTLP trace export active"
        );
        Ok(Self { provider })
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
