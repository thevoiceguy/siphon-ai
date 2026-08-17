//! Recent-errors ring buffer (0.49.0, DESIGN_SIGHTGLASS.md §6.1).
//!
//! A `tracing` [`Layer`] captures every `warn!`/`error!` event into a
//! bounded in-memory ring, served newest-first by
//! `GET /admin/v1/errors` so an operator (sightglass's Errors tab, or
//! plain curl) can see the last N problems without shell access to
//! the journal.
//!
//! Off the hot path by construction: capture is a short
//! `parking_lot`-style mutex push on the thread that logged — and
//! `warn!`/`error!` must never fire on the steady-state audio path in
//! the first place (CLAUDE.md §4.3). The ring is process-global for
//! the same reason `core::quality_live` is: the layer is installed
//! before config loads (so config-load warnings are themselves
//! captured), while the capacity knob arrives with config — the
//! runtime resizes it via [`set_capacity`].
//!
//! `call_id` correlation: per-call code is instrumented with
//! `fields(call_id = …)` spans. The layer records that field at span
//! creation and stamps each captured event with the nearest enclosing
//! span's `call_id`, so the Errors tab can jump-link to the call.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use siphon_ai_admin_api_types::ErrorEntry;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Default ring capacity; `[observability].error_ring_size` overrides.
pub const DEFAULT_CAPACITY: usize = 256;

struct Inner {
    cap: usize,
    buf: VecDeque<ErrorEntry>,
}

fn ring() -> &'static Mutex<Inner> {
    static RING: OnceLock<Mutex<Inner>> = OnceLock::new();
    RING.get_or_init(|| {
        Mutex::new(Inner {
            cap: DEFAULT_CAPACITY,
            buf: VecDeque::with_capacity(DEFAULT_CAPACITY),
        })
    })
}

/// Resize the ring (config load / reload). `0` disables capture;
/// shrinking drops the oldest entries.
pub fn set_capacity(cap: usize) {
    let mut inner = ring().lock().expect("error ring poisoned");
    inner.cap = cap;
    while inner.buf.len() > cap {
        inner.buf.pop_front();
    }
}

/// Snapshot the ring, newest first.
pub fn snapshot() -> Vec<ErrorEntry> {
    let inner = ring().lock().expect("error ring poisoned");
    inner.buf.iter().rev().cloned().collect()
}

#[cfg(test)]
fn clear() {
    ring().lock().expect("error ring poisoned").buf.clear();
}

fn push(entry: ErrorEntry) {
    let mut inner = ring().lock().expect("error ring poisoned");
    if inner.cap == 0 {
        return;
    }
    if inner.buf.len() == inner.cap {
        inner.buf.pop_front();
    }
    inner.buf.push_back(entry);
}

/// The capture layer. Install with a `WARN` per-layer filter; the
/// level is re-checked here anyway so a filterless install (tests)
/// behaves identically.
#[derive(Default)]
pub struct ErrorRingLayer;

/// A `call_id` recorded off a span's fields at creation, stashed in
/// that span's extensions.
struct SpanCallId(String);

impl<S> Layer<S> for ErrorRingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        // Only per-call spans carry a call_id field; recording it once
        // at creation keeps on_event to a cheap extensions lookup.
        let mut visitor = CallIdVisitor(None);
        attrs.record(&mut visitor);
        if let Some(call_id) = visitor.0 {
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().insert(SpanCallId(call_id));
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let level = *event.metadata().level();
        if level > Level::WARN {
            return;
        }
        let mut visitor = MessageVisitor {
            message: String::new(),
            fields: String::new(),
            call_id: None,
        };
        event.record(&mut visitor);

        // Nearest enclosing span with a recorded call_id, unless the
        // event itself carried one.
        let call_id = visitor.call_id.or_else(|| {
            ctx.event_scope(event)?
                .find_map(|span| span.extensions().get::<SpanCallId>().map(|c| c.0.clone()))
        });

        let mut message = visitor.message;
        if !visitor.fields.is_empty() {
            if !message.is_empty() {
                message.push(' ');
            }
            message.push_str(visitor.fields.trim_end());
        }
        let level_str = if level == Level::ERROR {
            "error"
        } else {
            "warn"
        };
        metrics::counter!(crate::metrics::ERROR_RING_CAPTURED_TOTAL, "level" => level_str)
            .increment(1);
        push(ErrorEntry {
            ts_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            level: level_str.to_string(),
            target: event.metadata().target().to_string(),
            message,
            call_id,
        });
    }
}

/// Extracts a `call_id` field (span attrs or event fields).
struct CallIdVisitor(Option<String>);

impl Visit for CallIdVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "call_id" {
            self.0 = Some(format!("{value:?}").trim_matches('"').to_string());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "call_id" {
            self.0 = Some(value.to_string());
        }
    }
}

/// Builds `message` + trailing `key=value` pairs off an event.
struct MessageVisitor {
    message: String,
    fields: String,
    call_id: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}").trim_matches('"').to_string(),
            "call_id" => self.call_id = Some(format!("{value:?}").trim_matches('"').to_string()),
            name => {
                self.fields
                    .push_str(&format!("{name}={:?} ", value).replace('"', ""));
            }
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_string(),
            "call_id" => self.call_id = Some(value.to_string()),
            name => self.fields.push_str(&format!("{name}={value} ")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;

    // The ring is process-global while `with_default` is only
    // thread-local, so parallel tests race each other's capacity and
    // clear calls — every test holds this lock for its whole body.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn with_layer(f: impl FnOnce()) {
        clear();
        set_capacity(DEFAULT_CAPACITY);
        let subscriber = tracing_subscriber::registry().with(ErrorRingLayer);
        with_default(subscriber, f);
    }

    #[test]
    fn captures_warn_and_error_not_info() {
        let _serial = serial();
        with_layer(|| {
            tracing::info!("ring-test not captured");
            tracing::warn!(deadline = 5, "ring-test slow");
            tracing::error!("ring-test broken");
        });
        let snap = snapshot();
        let ours: Vec<_> = snap
            .iter()
            .filter(|e| e.message.contains("ring-test"))
            .collect();
        assert_eq!(ours.len(), 2, "{snap:?}");
        // Newest first.
        assert_eq!(ours[0].level, "error");
        assert_eq!(ours[1].level, "warn");
        assert!(ours[1].message.contains("deadline=5"), "{:?}", ours[1]);
        assert!(ours[0].ts_ms > 0);
    }

    #[test]
    fn call_id_comes_from_the_enclosing_span() {
        let _serial = serial();
        with_layer(|| {
            let span = tracing::warn_span!("run", call_id = "siphon-ring-test");
            let _g = span.enter();
            tracing::warn!("ring-span-test inside");
        });
        let snap = snapshot();
        let entry = snap
            .iter()
            .find(|e| e.message.contains("ring-span-test"))
            .expect("captured");
        assert_eq!(entry.call_id.as_deref(), Some("siphon-ring-test"));
    }

    #[test]
    fn ring_is_bounded_and_resizable() {
        let _serial = serial();
        with_layer(|| {
            set_capacity(3);
            for i in 0..10 {
                tracing::warn!("ring-cap-test {i}");
            }
        });
        let snap = snapshot();
        assert_eq!(snap.len(), 3);
        // Newest three survive, newest first.
        assert!(snap[0].message.contains('9'), "{snap:?}");
        assert!(snap[2].message.contains('7'), "{snap:?}");

        set_capacity(0);
        assert!(snapshot().is_empty(), "cap 0 drops everything");
        with_layer(|| {
            set_capacity(0);
            tracing::warn!("ring-disabled-test");
        });
        assert!(
            !snapshot()
                .iter()
                .any(|e| e.message.contains("ring-disabled")),
            "cap 0 disables capture"
        );
        set_capacity(DEFAULT_CAPACITY);
    }
}
