//! `CdrSink` trait + the low-stakes implementations (no-op, fan-out).
//!
//! ## Failure mode
//!
//! Per CLAUDE.md §4.7 "HEP emission is best-effort, always" — the
//! same applies here. A sink failing to write a CDR MUST NOT take
//! down a call (the call already ended) or other sinks (a webhook
//! 500 shouldn't stop the file sink). Implementations log and move
//! on; aggregate sinks ([`MultiSink`]) catch panics so a buggy sink
//! doesn't bring siblings down.

use async_trait::async_trait;
use std::sync::Arc;
use tracing::warn;

use crate::schema::CdrRecord;

/// Sink for completed CDR records. `emit` is fire-and-forget — it
/// never returns an error, because there's nothing the caller could
/// usefully do with one. Sinks SHOULD be cheap to call (do real
/// I/O on a spawned task or a worker queue, not inline) so the
/// per-call task can return promptly.
#[async_trait]
pub trait CdrSink: Send + Sync {
    async fn emit(&self, record: CdrRecord);
}

/// `Arc<dyn CdrSink>` is what consumers pass around.
pub type CdrSinkHandle = Arc<dyn CdrSink>;

/// No-op sink. Default for deployments without `[cdr]` configured;
/// also useful in tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

#[async_trait]
impl CdrSink for NullSink {
    async fn emit(&self, _record: CdrRecord) {
        // Intentionally nothing.
    }
}

/// Fan-out sink. Emits to every inner sink concurrently. A panic in
/// any single sink is caught (`tokio::task::JoinError`) so siblings
/// keep emitting; the panic is logged.
pub struct MultiSink {
    sinks: Vec<CdrSinkHandle>,
}

impl MultiSink {
    pub fn new(sinks: Vec<CdrSinkHandle>) -> Self {
        Self { sinks }
    }

    pub fn push(&mut self, sink: CdrSinkHandle) {
        self.sinks.push(sink);
    }

    pub fn len(&self) -> usize {
        self.sinks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }
}

#[async_trait]
impl CdrSink for MultiSink {
    async fn emit(&self, record: CdrRecord) {
        // `Clone` on each Arc + the record is cheap; per-sink calls
        // run concurrently so a slow sink doesn't block fast ones.
        let mut handles = Vec::with_capacity(self.sinks.len());
        for sink in &self.sinks {
            let sink = Arc::clone(sink);
            let record = record.clone();
            handles.push(tokio::spawn(async move { sink.emit(record).await }));
        }
        for handle in handles {
            if let Err(join_err) = handle.await {
                warn!(error = %join_err, "CDR sub-sink panicked or was cancelled");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        AudioInfo, CdrRecord, Direction, TerminationCause, TerminationInfo, CDR_VERSION,
    };
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sample(call_id: &str) -> CdrRecord {
        CdrRecord {
            version: CDR_VERSION,
            call_id: call_id.into(),
            sip_call_id: "x@y".into(),
            started_at: chrono::Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 0).unwrap(),
            ended_at: chrono::Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 1).unwrap(),
            duration_ms: 1000,
            from: "+1".into(),
            to: "5000".into(),
            direction: Direction::Inbound,
            route: "default".into(),
            ws_url: "wss://x/y".into(),
            audio: AudioInfo {
                codec: "PCMU".into(),
                payload_type: 0,
                sample_rate: 8000,
            },
            termination: TerminationInfo {
                cause: TerminationCause::ServerHangup,
                bridge_disconnect: String::new(),
                tap_disconnect: String::new(),
            },
            verstat_attest: None,
            verstat_passed: None,
            recording_id: None,
            recording_path: None,
            recording_encrypted: None,
            recording_url: None,
            consent: None,
            park: None,
            hold: None,
            reconnect: None,
        }
    }

    /// Counter sink that tallies emit() calls. Used to verify
    /// MultiSink delivery semantics.
    struct Counter(Arc<AtomicUsize>);
    #[async_trait]
    impl CdrSink for Counter {
        async fn emit(&self, _record: CdrRecord) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Sink that always panics. MultiSink must isolate it.
    struct Exploding;
    #[async_trait]
    impl CdrSink for Exploding {
        async fn emit(&self, _record: CdrRecord) {
            panic!("simulated sink failure");
        }
    }

    #[tokio::test]
    async fn null_sink_is_a_noop() {
        // Just confirm calling NullSink::emit doesn't panic and
        // takes the record by value.
        NullSink.emit(sample("c-null")).await;
    }

    #[tokio::test]
    async fn multi_sink_fans_out_to_every_inner_sink() {
        let count = Arc::new(AtomicUsize::new(0));
        let multi = MultiSink::new(vec![
            Arc::new(Counter(Arc::clone(&count))),
            Arc::new(Counter(Arc::clone(&count))),
            Arc::new(Counter(Arc::clone(&count))),
        ]);
        assert_eq!(multi.len(), 3);
        multi.emit(sample("c-multi")).await;
        assert_eq!(count.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn multi_sink_isolates_panicking_inner_sink() {
        // A panicking sink must not stop siblings from getting the
        // record — operators care that the JSONL file gets written
        // even if a flaky webhook explodes.
        let count = Arc::new(AtomicUsize::new(0));
        let multi = MultiSink::new(vec![
            Arc::new(Exploding),
            Arc::new(Counter(Arc::clone(&count))),
            Arc::new(Exploding),
            Arc::new(Counter(Arc::clone(&count))),
        ]);
        multi.emit(sample("c-isolate")).await;
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn empty_multi_sink_is_a_noop() {
        let multi = MultiSink::new(vec![]);
        assert!(multi.is_empty());
        multi.emit(sample("c-empty")).await; // shouldn't panic
    }
}
