//! `QualitySink` trait + composition helpers.
//!
//! Same shape as the audit / CDR / webhook sinks: `emit` is
//! fire-and-forget, sinks never panic and never block the call path,
//! failures get logged and dropped (CLAUDE.md §4.7). Quality history
//! is observability, not call control.

use std::sync::Arc;

use async_trait::async_trait;

use crate::record::QualityRecord;

/// Trait every quality-record sink implements.
#[async_trait]
pub trait QualitySink: Send + Sync {
    async fn emit(&self, record: QualityRecord);
}

/// `Arc<dyn QualitySink>` is what the facade and consumers hold.
pub type QualitySinkHandle = Arc<dyn QualitySink>;

/// No-op sink. Default-when-not-configured; also useful in tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

#[async_trait]
impl QualitySink for NullSink {
    async fn emit(&self, _record: QualityRecord) {
        // Intentionally nothing.
    }
}

/// Fans one record out to several inner sinks (local JSONL file *and*
/// the signed webhook). Each inner `emit` is awaited in turn; the file
/// sink is a quick locked write and the webhook sink spawns
/// internally, so the fan-out never blocks meaningfully.
pub struct FanoutSink {
    sinks: Vec<QualitySinkHandle>,
}

impl FanoutSink {
    pub fn new(sinks: impl IntoIterator<Item = QualitySinkHandle>) -> Self {
        Self {
            sinks: sinks.into_iter().collect(),
        }
    }
}

#[async_trait]
impl QualitySink for FanoutSink {
    async fn emit(&self, record: QualityRecord) {
        for sink in &self.sinks {
            sink.emit(record.clone()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordKind, QUALITY_RECORD_VERSION};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn rec() -> QualityRecord {
        QualityRecord {
            version: QUALITY_RECORD_VERSION,
            kind: RecordKind::Interval,
            call_id: "siphon-test".into(),
            ts: chrono::Utc::now(),
            seq: 0,
            quality: Default::default(),
        }
    }

    #[derive(Default)]
    struct Counter(AtomicUsize);

    #[async_trait]
    impl QualitySink for Counter {
        async fn emit(&self, _record: QualityRecord) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn null_sink_is_a_noop() {
        NullSink.emit(rec()).await;
    }

    #[tokio::test]
    async fn fanout_reaches_every_inner_sink() {
        let a = Arc::new(Counter::default());
        let b = Arc::new(Counter::default());
        let fan = FanoutSink::new([
            Arc::clone(&a) as QualitySinkHandle,
            Arc::clone(&b) as QualitySinkHandle,
        ]);
        fan.emit(rec()).await;
        assert_eq!(a.0.load(Ordering::Relaxed), 1);
        assert_eq!(b.0.load(Ordering::Relaxed), 1);
    }
}
