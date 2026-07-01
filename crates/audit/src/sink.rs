//! `AuditSink` trait + composition helpers.
//!
//! Same shape as `siphon-ai-webhooks`' `WebhookSink` and
//! `siphon-ai-cdr`'s `CdrSink`: `emit` is fire-and-forget, sinks never
//! panic and never block the call path, failures get logged and
//! dropped (CLAUDE.md §4.7). Audit is observability, not call control.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use crate::event::AuditEvent;

/// Trait every audit sink implements.
#[async_trait]
pub trait AuditSink: Send + Sync {
    async fn emit(&self, event: AuditEvent);
}

/// `Arc<dyn AuditSink>` is what the facade and consumers hold.
pub type AuditSinkHandle = Arc<dyn AuditSink>;

/// No-op sink. Default-when-not-configured; also useful in tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

#[async_trait]
impl AuditSink for NullSink {
    async fn emit(&self, _event: AuditEvent) {
        // Intentionally nothing.
    }
}

/// Fans one event out to several inner sinks (e.g. local JSONL file
/// *and* the signed webhook). Each inner `emit` is awaited in turn;
/// since the file sink is a quick locked write and the webhook sink
/// spawns internally, the fan-out never blocks meaningfully.
pub struct FanoutSink {
    sinks: Vec<AuditSinkHandle>,
}

impl FanoutSink {
    pub fn new(sinks: impl IntoIterator<Item = AuditSinkHandle>) -> Self {
        Self {
            sinks: sinks.into_iter().collect(),
        }
    }
}

#[async_trait]
impl AuditSink for FanoutSink {
    async fn emit(&self, event: AuditEvent) {
        // Clone per inner sink; the last one could take by move but
        // keeping it uniform is clearer and audit volume is low.
        for sink in &self.sinks {
            sink.emit(event.clone()).await;
        }
    }
}

/// Wraps an inner sink with an event-name allowlist. Events whose
/// `type_str()` isn't in `allowed` are dropped before reaching the
/// inner sink. An empty allowlist means "emit every event" so an
/// operator can omit the field for the common case. Building the
/// allowlist is the daemon binary's job; this struct only enforces.
pub struct FilteredSink {
    inner: AuditSinkHandle,
    allowed: HashSet<String>,
}

impl FilteredSink {
    pub fn new(inner: AuditSinkHandle, allowed: impl IntoIterator<Item = String>) -> Self {
        Self {
            inner,
            allowed: allowed.into_iter().collect(),
        }
    }

    fn passes(&self, event: &AuditEvent) -> bool {
        self.allowed.is_empty() || self.allowed.contains(event.type_str())
    }
}

#[async_trait]
impl AuditSink for FilteredSink {
    async fn emit(&self, event: AuditEvent) {
        if self.passes(&event) {
            self.inner.emit(event).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ev(result: &str) -> AuditEvent {
        AuditEvent::sip_auth("1.2.3.4:5060", None, result)
    }

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<AuditEvent>>,
        count: AtomicUsize,
    }

    #[async_trait]
    impl AuditSink for Recorder {
        async fn emit(&self, event: AuditEvent) {
            self.events.lock().push(event);
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn null_sink_is_a_noop() {
        NullSink.emit(ev("ok")).await;
    }

    #[tokio::test]
    async fn filtered_sink_drops_unallowed_events() {
        let rec = Arc::new(Recorder::default());
        let sink = FilteredSink::new(
            Arc::clone(&rec) as AuditSinkHandle,
            ["admin_request".to_string()],
        );
        sink.emit(ev("failed")).await; // sip_auth not allowed
        assert_eq!(rec.count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn filtered_sink_passes_allowed_events() {
        let rec = Arc::new(Recorder::default());
        let sink = FilteredSink::new(
            Arc::clone(&rec) as AuditSinkHandle,
            ["sip_auth".to_string()],
        );
        sink.emit(ev("failed")).await;
        assert_eq!(rec.count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn empty_allowlist_passes_everything() {
        let rec = Arc::new(Recorder::default());
        let sink = FilteredSink::new(Arc::clone(&rec) as AuditSinkHandle, std::iter::empty());
        sink.emit(ev("ok")).await;
        sink.emit(ev("failed")).await;
        assert_eq!(rec.count.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn fanout_reaches_every_inner_sink() {
        let a = Arc::new(Recorder::default());
        let b = Arc::new(Recorder::default());
        let fan = FanoutSink::new([
            Arc::clone(&a) as AuditSinkHandle,
            Arc::clone(&b) as AuditSinkHandle,
        ]);
        fan.emit(ev("ok")).await;
        assert_eq!(a.count.load(Ordering::Relaxed), 1);
        assert_eq!(b.count.load(Ordering::Relaxed), 1);
    }
}
