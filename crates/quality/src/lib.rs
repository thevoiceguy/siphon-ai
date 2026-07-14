//! Per-call quality history records (0.31.0): a clean, signed, durable
//! feed of quality summaries operators ingest into their own store
//! (Loki / Influx / a TSDB) — SiphonAI ships records, it does not run a
//! database. Second half of the per-call quality telemetry theme; see
//! `docs/design/DESIGN_QUALITY_TELEMETRY.md` (D3).
//!
//! ## Pipeline
//!
//! ```text
//!   per-call record task (core) ─► quality::emit(QualityRecord)
//!        │   (process-global facade, installed from [quality])
//!        ▼
//!   QualitySink ─┬─► FileSink   (append-only JSONL, on-box)
//!                └─► HttpSink   (HMAC-signed POST, durable spool)
//! ```
//!
//! One record per call per `[quality].interval_secs`, plus a final
//! end-of-call summary. The stats payload is the CDR `quality` block
//! (`siphon_ai_cdr::QualityInfo`) verbatim — flattened into the record
//! so the CDR and the history feed can never drift.
//!
//! Per CLAUDE.md §4.7 sinks are best-effort and never block the call
//! path: records are sampled from the tap's watch feed by a per-call
//! worker task, and every sink write happens off the emitting task.

pub mod facade;
pub mod file;
pub mod http;
pub mod record;
pub mod sink;

pub use facade::{emit, install, is_enabled, record_interval};
pub use file::{FileSink, FileSinkError};
pub use http::{HttpSink, HttpSinkConfig};
pub use record::{QualityRecord, RecordKind, QUALITY_RECORD_VERSION};
pub use sink::{FanoutSink, NullSink, QualitySink, QualitySinkHandle};
