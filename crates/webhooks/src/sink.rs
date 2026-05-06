//! `WebhookSink` trait + low-stakes implementations.
//!
//! Same shape as `siphon-ai-cdr`'s `CdrSink`: emit is fire-and-
//! forget, sinks never panic / never block the call path, failures
//! get logged and dropped (CLAUDE.md §4.7).

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use crate::event::WebhookEvent;

/// Trait every webhook sink implements.
#[async_trait]
pub trait WebhookSink: Send + Sync {
    async fn emit(&self, event: WebhookEvent);
}

/// `Arc<dyn WebhookSink>` is what consumers (the acceptor) hold.
pub type WebhookSinkHandle = Arc<dyn WebhookSink>;

/// No-op sink. Default-when-not-configured; also useful in tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

#[async_trait]
impl WebhookSink for NullSink {
    async fn emit(&self, _event: WebhookEvent) {
        // Intentionally nothing.
    }
}

/// Wraps an inner sink with an event-name allowlist. Events whose
/// `type_str()` isn't in `allowed` are dropped before reaching the
/// inner sink (no HTTP traffic, no log spam).
///
/// An empty allowlist is treated as "deliver every event" so an
/// operator can omit the field for the common case. Building the
/// allowlist is the daemon binary's job; this struct only enforces.
pub struct FilteredSink {
    inner: WebhookSinkHandle,
    allowed: HashSet<String>,
}

impl FilteredSink {
    pub fn new(inner: WebhookSinkHandle, allowed: impl IntoIterator<Item = String>) -> Self {
        Self {
            inner,
            allowed: allowed.into_iter().collect(),
        }
    }

    fn passes(&self, event: &WebhookEvent) -> bool {
        self.allowed.is_empty() || self.allowed.contains(event.type_str())
    }
}

#[async_trait]
impl WebhookSink for FilteredSink {
    async fn emit(&self, event: WebhookEvent) {
        if self.passes(&event) {
            self.inner.emit(event).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CallStartEvent, WEBHOOK_VERSION};
    use chrono::TimeZone;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn start_event(call_id: &str) -> WebhookEvent {
        WebhookEvent::CallStart(CallStartEvent {
            version: WEBHOOK_VERSION,
            call_id: call_id.into(),
            sip_call_id: format!("{call_id}@pbx"),
            timestamp: chrono::Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 0).unwrap(),
            from: "+1".into(),
            to: "5000".into(),
            route: "default".into(),
            ws_url: "wss://x/y".into(),
        })
    }

    /// Counter sink that records every received event.
    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<WebhookEvent>>,
        count: AtomicUsize,
    }

    #[async_trait]
    impl WebhookSink for Recorder {
        async fn emit(&self, event: WebhookEvent) {
            self.events.lock().push(event);
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn null_sink_is_a_noop() {
        NullSink.emit(start_event("c-null")).await;
    }

    #[tokio::test]
    async fn filtered_sink_drops_unallowed_events() {
        let rec = Arc::new(Recorder::default());
        // Allowlist contains only call_end; call_start should be
        // dropped.
        let sink = FilteredSink::new(
            Arc::clone(&rec) as WebhookSinkHandle,
            ["call_end".to_string()],
        );
        sink.emit(start_event("c-1")).await;
        assert_eq!(rec.count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn filtered_sink_passes_allowed_events() {
        let rec = Arc::new(Recorder::default());
        let sink = FilteredSink::new(
            Arc::clone(&rec) as WebhookSinkHandle,
            ["call_start".to_string()],
        );
        sink.emit(start_event("c-1")).await;
        assert_eq!(rec.count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn filtered_sink_with_empty_allowlist_passes_everything() {
        let rec = Arc::new(Recorder::default());
        let sink = FilteredSink::new(Arc::clone(&rec) as WebhookSinkHandle, std::iter::empty());
        sink.emit(start_event("c-1")).await;
        sink.emit(start_event("c-2")).await;
        assert_eq!(rec.count.load(Ordering::Relaxed), 2);
    }
}
