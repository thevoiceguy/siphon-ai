//! Append-only JSONL audit-log file sink.
//!
//! One event = one JSON object on one line, LF-terminated. This is the
//! on-box tamper-*evident* trail: it keeps recording even when the SIEM
//! or the network is down, and the usual SIEM ingestion path is a log
//! shipper (Vector / Filebeat / Fluent Bit) tailing this file. For
//! tamper-*resistance* beyond an append-only file, ship it off-box
//! promptly (that's what the signed webhook sink is for) and/or point
//! `path` at an append-only / WORM-backed mount.
//!
//! ## Concurrency & atomicity
//!
//! Identical to `siphon-ai-cdr`'s file sink: the handle lives behind a
//! `tokio::sync::Mutex<BufWriter>`; each `emit` takes the lock, writes
//! one line + LF, flushes, drops the lock. Audit volume is low (admin
//! requests, auth failures, config reloads — not per-frame), so the
//! lock is never meaningfully contended. A single small `write_all` +
//! `flush` is atomic in practice for the line sizes we produce.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::event::AuditEvent;
use crate::sink::AuditSink;

#[derive(Debug, Error)]
pub enum FileSinkError {
    /// Couldn't open / create the target file. Surfaced at
    /// construction time so a misconfigured path fails loud at
    /// startup, not on the first security event.
    #[error("failed to open audit file {path:?}: {source}")]
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
    /// exist — we don't `mkdir -p`, matching the CDR file sink, because
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
        debug!(path = %path.display(), "opened audit file");
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
impl AuditSink for FileSink {
    async fn emit(&self, event: AuditEvent) {
        // Serialize outside the lock — the lock only guards the handle.
        let line = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(e) => {
                // Unreachable for our schema; record it and keep going.
                warn!(kind = event.type_str(), error = %e, "audit JSON serialize failed");
                return;
            }
        };

        let mut guard = self.writer.lock().await;
        if let Err(e) = guard.write_all(line.as_bytes()).await {
            warn!(path = %self.path.display(), error = %e, "audit file write failed");
            return;
        }
        if let Err(e) = guard.write_all(b"\n").await {
            warn!(path = %self.path.display(), error = %e, "audit file write (newline) failed");
            return;
        }
        if let Err(e) = guard.flush().await {
            warn!(path = %self.path.display(), error = %e, "audit file flush failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn appends_one_json_line_per_event() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let sink = FileSink::open(tmp.path()).await.unwrap();
        sink.emit(AuditEvent::sip_auth("1.2.3.4:5060", None, "failed"))
            .await;
        sink.emit(AuditEvent::invite_rejected("5.6.7.8:5060", "rate_limited"))
            .await;

        let body = tokio::fs::read_to_string(tmp.path()).await.unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["type"], "sip_auth");
        assert_eq!(first["result"], "failed");
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["type"], "invite_rejected");
    }

    #[tokio::test]
    async fn open_fails_loud_on_bad_path() {
        let err = FileSink::open("/nonexistent-dir-xyz/audit.jsonl").await;
        assert!(err.is_err());
    }
}
