//! OTLP log export (0.51.0): the acceptance test for #589.
//!
//! The claim the feature makes is narrow and easy to get wrong — a log
//! record ships carrying the `trace_id` / `span_id` of the span it was
//! emitted inside, so a backend can show it against that span. Nothing in
//! the wiring makes that obvious: the appender bridge never touches trace
//! context itself, the SDK's logger stamps it from whatever OpenTelemetry
//! context happens to be current, and that context only exists because
//! `tracing-opentelemetry` attaches one on span entry. Three crates have to
//! agree, and an upstream bump can break the agreement silently — the
//! records would keep flowing, just uncorrelated, which looks fine.
//!
//! So these tests drive the real layers and read the real records.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry::logs::Severity;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::InstrumentationScope;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogProcessor, SdkLogRecord, SdkLoggerProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer as _;

/// One captured record, reduced to what the test asserts on.
#[derive(Debug, Clone)]
struct Captured {
    body: String,
    severity: Option<Severity>,
    trace_id: Option<String>,
    span_id: Option<String>,
}

/// A `LogProcessor` that keeps records in memory instead of exporting
/// them. Synchronous, so a test never has to wait on a batch worker.
#[derive(Debug, Clone, Default)]
struct CapturingProcessor(Arc<Mutex<Vec<Captured>>>);

impl CapturingProcessor {
    fn records(&self) -> Vec<Captured> {
        self.0.lock().expect("capture poisoned").clone()
    }
}

impl LogProcessor for CapturingProcessor {
    fn emit(&self, data: &mut SdkLogRecord, _scope: &InstrumentationScope) {
        let ctx = data.trace_context();
        self.0.lock().expect("capture poisoned").push(Captured {
            body: match data.body() {
                Some(opentelemetry::logs::AnyValue::String(s)) => s.to_string(),
                other => format!("{other:?}"),
            },
            severity: data.severity_number(),
            trace_id: ctx.map(|c| format!("{:032x}", c.trace_id)),
            span_id: ctx.map(|c| format!("{:016x}", c.span_id)),
        });
    }
    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }
    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

fn capturing_provider() -> (SdkLoggerProvider, CapturingProcessor) {
    let capture = CapturingProcessor::default();
    let provider = SdkLoggerProvider::builder()
        .with_log_processor(capture.clone())
        .build();
    (provider, capture)
}

/// A tracer provider with no exporter: spans are still sampled and still
/// get real ids, which is all the correlation under test needs.
fn tracer_provider() -> SdkTracerProvider {
    SdkTracerProvider::builder().build()
}

/// The headline claim. A record emitted inside an instrumented span
/// carries that span's trace and span ids — which is the whole difference
/// between this and shipping the journal, where the context is gone by the
/// time the text is written.
#[test]
fn a_record_carries_the_trace_context_of_the_span_it_was_emitted_in() {
    let (provider, capture) = capturing_provider();
    let tracers = tracer_provider();

    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(tracers.tracer("test")))
        .with(opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&provider));

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("handle_invite", call_id = "abc123@pbx");
        let _g = span.enter();
        tracing::info!("received invite");
    });

    let records = capture.records();
    let rec = records
        .iter()
        .find(|r| r.body == "received invite")
        .unwrap_or_else(|| panic!("event never reached the log pipeline: {records:?}"));

    let trace_id = rec
        .trace_id
        .as_deref()
        .unwrap_or_else(|| panic!("record shipped with no trace context: {rec:?}"));
    let span_id = rec.span_id.as_deref().expect("span id present");
    assert_ne!(
        trace_id, "00000000000000000000000000000000",
        "an all-zero trace id is the invalid one — the context was not attached: {rec:?}"
    );
    assert_ne!(span_id, "0000000000000000", "{rec:?}");
    assert_eq!(rec.severity, Some(Severity::Info), "{rec:?}");
}

/// The negative control, and the reason
/// `[observability.otlp.logs]` refuses to run without its parent: with no
/// span layer on the registry there is no OpenTelemetry context to pick up,
/// so records ship uncorrelated. Config rejects that combination at load
/// rather than letting it happen quietly.
#[test]
fn without_the_span_layer_a_record_has_no_trace_context() {
    let (provider, capture) = capturing_provider();

    let subscriber = tracing_subscriber::registry()
        .with(opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&provider));

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("handle_invite", call_id = "abc123@pbx");
        let _g = span.enter();
        tracing::info!("received invite");
    });

    let rec = capture
        .records()
        .into_iter()
        .find(|r| r.body == "received invite")
        .expect("record captured");
    assert!(
        rec.trace_id.is_none(),
        "nothing should have supplied a trace context here: {rec:?}"
    );
}

/// Two records inside the *same* call's span share a trace id and differ
/// in span id when they come from different child spans — i.e. the
/// correlation is per-span, not a constant.
#[test]
fn records_group_by_trace_and_separate_by_span() {
    let (provider, capture) = capturing_provider();
    let tracers = tracer_provider();

    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(tracers.tracer("test")))
        .with(opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&provider));

    tracing::subscriber::with_default(subscriber, || {
        let call = tracing::info_span!("call");
        let _c = call.enter();
        {
            let leg = tracing::info_span!("media_leg");
            let _l = leg.enter();
            tracing::info!("media up");
        }
        tracing::info!("call answered");
    });

    let records = capture.records();
    let find = |body: &str| {
        records
            .iter()
            .find(|r| r.body == body)
            .unwrap_or_else(|| panic!("{body:?} not captured: {records:?}"))
            .clone()
    };
    let media = find("media up");
    let answered = find("call answered");

    assert_eq!(
        media.trace_id, answered.trace_id,
        "both records belong to the same call, so the same trace"
    );
    assert_ne!(
        media.span_id, answered.span_id,
        "they were emitted in different spans, so different span ids"
    );
}

/// The export level is a **per-layer** filter, so the collector's appetite
/// and the console's are set separately: a `debug` console with a `warn`
/// collector ships two records, not the firehose.
///
/// The other direction does not hold and is documented rather than
/// pretended away — the global filter gates every layer, so it is a floor
/// this can narrow but never widen.
#[test]
fn the_export_level_narrows_independently_of_the_console_filter() {
    let (provider, capture) = capturing_provider();
    let tracers = tracer_provider();

    let subscriber = tracing_subscriber::registry()
        // Console filter: everything.
        .with(LevelFilter::TRACE)
        .with(tracing_opentelemetry::layer().with_tracer(tracers.tracer("test")))
        // Export filter: warn and above only.
        .with(
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&provider)
                .with_filter(LevelFilter::WARN),
        );

    tracing::subscriber::with_default(subscriber, || {
        tracing::debug!("chatty");
        tracing::info!("routine");
        tracing::warn!("degraded");
        tracing::error!("broken");
    });

    let bodies: Vec<String> = capture.records().into_iter().map(|r| r.body).collect();
    assert_eq!(
        bodies,
        vec!["degraded".to_string(), "broken".to_string()],
        "only warn+ should have shipped"
    );
}

/// The production wiring, end to end: the layer built by `init_tracing` is
/// dark until the runtime hands it a provider, and correlating from the
/// first record after that.
///
/// This is the one test that touches the process-wide `LOGGER_PROVIDER`
/// cell, which is set exactly once — hence one test, not three.
#[test]
fn the_lazy_layer_is_dark_until_activation_then_ships_correlated() {
    let (provider, capture) = capturing_provider();
    let tracers = tracer_provider();

    // Exactly what `init_tracing` builds: the bridge over the lazily
    // resolving provider, behind a reloadable per-layer filter at OFF.
    let (filter, filter_handle) =
        tracing_subscriber::reload::Layer::new(tracing_subscriber::EnvFilter::new("off"));
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(tracers.tracer("test")))
        .with(
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                &siphon_ai::otel::LazyLoggerProvider,
            )
            .with_filter(filter),
        );
    let control = siphon_ai::OtelLogControl::new(Box::new(move |f| filter_handle.reload(f)));

    let _guard = tracing::subscriber::set_default(subscriber);

    // Before activation — this is every config-load warning the daemon
    // prints before it knows whether OTLP is even enabled.
    tracing::info!("before activation");
    assert!(
        capture.records().is_empty(),
        "the layer must be inert until the runtime activates it: {:?}",
        capture.records()
    );

    // The runtime's move: install the provider, then open the filter.
    control.activate(provider, "info");

    {
        let span = tracing::info_span!("call", call_id = "xyz@pbx");
        let _g = span.enter();
        tracing::info!("after activation");
    }

    // Emission is asynchronous by design — the layer hands the record to a
    // bounded queue and returns, and a worker thread drains it into the SDK
    // (see `otel.rs` for why nothing may call into the SDK from inside
    // `on_event`). `quiesce` closes the layer and joins that worker, which
    // is exactly what teardown does, so it doubles as the test's barrier.
    control.quiesce();

    let records = capture.records();
    assert_eq!(
        records.len(),
        1,
        "exactly the post-activation record: {records:?}"
    );
    assert_eq!(records[0].body, "after activation");
    assert!(
        records[0]
            .trace_id
            .as_deref()
            .is_some_and(|t| t != "00000000000000000000000000000000"),
        "the first record after activation must already correlate: {:?}",
        records[0]
    );

    // Quiesced: the layer is shut, so nothing more is queued.
    tracing::info!("after quiesce");
    assert_eq!(
        capture.records().len(),
        1,
        "quiesce must stop the layer: {:?}",
        capture.records()
    );
}
