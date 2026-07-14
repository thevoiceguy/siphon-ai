//! Append-only JSONL quality-record file sink.
//!
//! One record = one JSON object on one line, LF-terminated. The usual
//! ingestion path is a log shipper (Vector / Filebeat / Fluent Bit)
//! tailing this file into Loki / Elasticsearch / a TSDB — see the
//! ingestion guide in `docs/OPERATIONS.md`.
//!
//! ## Concurrency & atomicity
//!
//! Identical to the CDR / audit file sinks: the handle lives behind a
//! `tokio::sync::Mutex<BufWriter>`; each `emit` takes the lock, writes
//! one line + LF, flushes, drops the lock. Record volume is low (one
//! per call per `[quality].interval_secs`, not per-frame), so the lock
//! is never meaningfully contended.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::record::QualityRecord;
use crate::sink::QualitySink;

#[derive(Debug, Error)]
pub enum FileSinkError {
    /// Couldn't open / create the target file. Surfaced at
    /// construction time so a misconfigured path fails loud at
    /// startup, not on the first record.
    #[error("failed to open quality-record file {path:?}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub struct FileSink {
    path: PathBuf,
    writer: Arc<Mutex<BufWriter<File>>>,
}

impl std::fmt::Debug for FileSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Tokio's BufWriter doesn't impl Debug; redact it.
        f.debug_struct("FileSink")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl FileSink {
    /// Open / create the file in append mode. The parent directory must
    /// exist — no `mkdir -p`, matching the CDR and audit file sinks:
    /// the daemon typically runs under a service account and failing
    /// loudly on a bad path is the right behaviour.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, FileSinkError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|source| FileSinkError::Open {
                path: path.clone(),
                source,
            })?;
        debug!(path = %path.display(), "opened quality-record file");
        Ok(Self {
            path,
            writer: Arc::new(Mutex::new(BufWriter::new(file))),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl QualitySink for FileSink {
    async fn emit(&self, record: QualityRecord) {
        // Serialize outside the lock — the lock only guards the handle.
        let line = match serde_json::to_string(&record) {
            Ok(s) => s,
            Err(e) => {
                // Unreachable for our schema; record it and keep going.
                warn!(call_id = %record.call_id, error = %e, "quality record JSON serialize failed");
                return;
            }
        };

        let mut guard = self.writer.lock().await;
        if let Err(e) = guard.write_all(line.as_bytes()).await {
            warn!(path = %self.path.display(), error = %e, "quality-record file write failed");
            return;
        }
        if let Err(e) = guard.write_all(b"\n").await {
            warn!(path = %self.path.display(), error = %e, "quality-record file write (newline) failed");
            return;
        }
        if let Err(e) = guard.flush().await {
            warn!(path = %self.path.display(), error = %e, "quality-record file flush failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordKind, QUALITY_RECORD_VERSION};

    fn rec(kind: RecordKind, seq: u64) -> QualityRecord {
        QualityRecord {
            version: QUALITY_RECORD_VERSION,
            kind,
            call_id: "siphon-file-test".into(),
            ts: chrono::Utc::now(),
            seq,
            quality: Default::default(),
        }
    }

    #[tokio::test]
    async fn appends_one_json_line_per_record() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let sink = FileSink::open(tmp.path()).await.unwrap();
        sink.emit(rec(RecordKind::Interval, 0)).await;
        sink.emit(rec(RecordKind::Final, 1)).await;

        let body = tokio::fs::read_to_string(tmp.path()).await.unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["kind"], "interval");
        assert_eq!(first["seq"], 0);
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["kind"], "final");
    }

    #[tokio::test]
    async fn open_fails_loud_on_bad_path() {
        let err = FileSink::open("/nonexistent-dir-xyz/quality.jsonl").await;
        assert!(err.is_err());
    }
}
