//! Regression test for the bug the first live run against a collector found
//! (#589).
//!
//! The OTel batch log processor emits `tracing` events of its own from
//! inside `emit` — "queue full, dropping records", "emitted after
//! Shutdown". Dispatched from within a layer's `on_event`, a nested event
//! runs a second filter pass over the same thread-local `FilterState` the
//! outer event is still using, and leaves its bits behind. In a debug build
//! that is a `debug_assert` panic inside `tracing-subscriber`
//! (`FilterMap { disabled_by: {2} }`); in a release build it is silently
//! wrong per-layer filtering, which is worse.
//!
//! The fix is that nothing calls into the SDK from within `on_event`:
//! `LazyLogger::emit` stamps the trace context and hands the record to a
//! bounded queue, and a worker thread does the rest.
//!
//! The processor here logs from `emit` deliberately — that is the invariant
//! under test, and standing in for the SDK's own warnings makes the test
//! independent of when upstream chooses to raise them.
//!
//! Its own test binary because it drives the process-wide log sink, which
//! is installed exactly once.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry::InstrumentationScope;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogProcessor, SdkLogRecord, SdkLoggerProvider};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer as _;

/// Collects what the WARN-filtered layer was actually asked to format.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("buffer poisoned")).into_owned()
    }
}

impl io::Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Buffer {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A processor that raises a `tracing` event from inside `emit`, the way
/// `BatchLogProcessor` does when its queue is full or it has been shut down.
#[derive(Debug, Clone, Default)]
struct ChattyProcessor(Arc<AtomicUsize>);

impl LogProcessor for ChattyProcessor {
    fn emit(&self, _data: &mut SdkLogRecord, _scope: &InstrumentationScope) {
        self.0.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(target: "opentelemetry_sdk", "processor is unhappy");
    }
    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }
    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

/// A processor that logs from `emit` must not corrupt per-layer filter
/// state.
///
/// The layer stack mirrors the daemon's: the appender is not the last
/// filtered layer, and the events under test are ones a *later* filter
/// disables, which is what leaves a bit set for the nested pass to trip
/// over.
///
/// The assertion is on the *observable* symptom rather than on the panic,
/// because the panic is a `debug_assert` — it is the release build, where
/// the filtering just quietly goes wrong, that needs guarding. With
/// `LazyLogger::emit` calling the SDK inline, the WARN-filtered layer below
/// receives the INFO events.
#[test]
fn a_processor_that_logs_from_emit_does_not_corrupt_filter_state() {
    let warn_only = Buffer::default();
    let processor = ChattyProcessor::default();
    let provider = SdkLoggerProvider::builder()
        .with_log_processor(processor.clone())
        .build();

    let (filter, filter_handle) =
        tracing_subscriber::reload::Layer::new(tracing_subscriber::EnvFilter::new("off"));
    let subscriber = tracing_subscriber::registry()
        .with(
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                &siphon_ai::otel::LazyLoggerProvider,
            )
            .with_filter(filter),
        )
        // Stands in for the daemon's error ring: a later filtered layer
        // that disables the INFO events below, so their `FilterMap` bit is
        // set while the appender's `on_event` is running.
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(warn_only.clone())
                .with_filter(LevelFilter::WARN),
        );
    let control = siphon_ai::OtelLogControl::new(Box::new(move |f| filter_handle.reload(f)));

    // **Global**, not `set_default`. This matters: with a thread-local
    // subscriber, `tracing_core`'s `get_default` re-entrancy guard hands a
    // nested dispatch `Dispatch::none()` and the bug cannot happen. The
    // daemon installs its subscriber globally (`try_init`), where there is
    // no such guard — which is why this only ever showed up in a real run.
    tracing::subscriber::set_global_default(subscriber).expect("first global subscriber");
    control.activate(provider, "info");

    // Several, because the corruption shows on the event *after* the nested
    // dispatch, not on the one that caused it.
    for i in 0..8 {
        tracing::info!(i, "into the void");
    }

    // Drains the queue and joins the worker, so the processor has actually
    // run by the time we assert.
    control.quiesce();
    assert!(
        processor.0.load(Ordering::Relaxed) >= 8,
        "the records must still have reached the processor, just not from \
         inside on_event; saw {}",
        processor.0.load(Ordering::Relaxed)
    );

    tracing::warn!("still filtering correctly");

    let seen = warn_only.contents();
    assert!(
        !seen.contains("into the void"),
        "a WARN-filtered layer received INFO events — per-layer filter state \
         was corrupted by the nested dispatch:\n{seen}"
    );
    assert!(
        seen.contains("still filtering correctly"),
        "the layer must still be filtering, not simply muted:\n{seen}"
    );
}
