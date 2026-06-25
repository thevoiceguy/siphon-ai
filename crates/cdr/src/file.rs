//! Append-only JSONL file sink.
//!
//! One record = one JSON object on one line. Lines are terminated
//! with a single `\n` (LF) regardless of platform — JSONL consumers
//! split on `\n`, and Windows operators reading the file in
//! Notepad-modern handle LF fine.
//!
//! ## Concurrency
//!
//! The file handle lives behind a `tokio::sync::Mutex<BufWriter>`.
//! Each `emit` takes the lock, writes one line + LF, flushes, drops
//! the lock. At v1 call rates (one CDR per call, calls measured in
//! per-second-low-double-digits in the worst case) the lock is
//! never contended for more than the time to serialize a line. If
//! we ever push that, the migration is to an mpsc channel + a
//! single writer task — the per-call hot path (`emit`) just sends.
//!
//! ## Atomicity
//!
//! `BufWriter::write_all` + `flush` is one syscall on Linux when the
//! buffer fits in `PIPE_BUF` (4096B) — any single CDR record we'd
//! produce is well under that, so a partial line is essentially
//! impossible. On other platforms the same property holds for
//! `write_all` of small buffers; we don't deliberately enforce
//! atomicity beyond what the OS provides. JSONL parsers tolerant
//! to partial trailing lines (jq, ndjson tools) cope either way.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::schema::CdrRecord;
use crate::sink::CdrSink;

#[derive(Debug, Error)]
pub enum FileSinkError {
    /// Couldn't open / create the target file. Surfaced at
    /// construction time so a misconfigured path fails loud at
    /// startup, not on first call end.
    #[error("failed to open CDR file {path:?}: {source}")]
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
    /// Open / create the file in append mode. Parent directory must
    /// exist; we don't `mkdir -p` because deployments typically run
    /// the daemon under a service account that lacks `~root` write
    /// — failing loudly is the right behaviour.
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
        debug!(path = %path.display(), "opened CDR file");
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
impl CdrSink for FileSink {
    async fn emit(&self, record: CdrRecord) {
        // Serialize outside the lock — the lock only protects the
        // file handle, not the JSON encoder.
        let line = match serde_json::to_string(&record) {
            Ok(s) => s,
            Err(e) => {
                // Should be unreachable for our schema; record it
                // and keep going.
                warn!(call_id = %record.call_id, error = %e, "CDR JSON serialize failed");
                return;
            }
        };

        let mut guard = self.writer.lock().await;
        if let Err(e) = guard.write_all(line.as_bytes()).await {
            warn!(
                path = %self.path.display(),
                call_id = %record.call_id,
                error = %e,
                "CDR file write failed"
            );
            return;
        }
        if let Err(e) = guard.write_all(b"\n").await {
            warn!(
                path = %self.path.display(),
                call_id = %record.call_id,
                error = %e,
                "CDR file write (newline) failed"
            );
            return;
        }
        if let Err(e) = guard.flush().await {
            warn!(
                path = %self.path.display(),
                call_id = %record.call_id,
                error = %e,
                "CDR file flush failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AudioInfo, Direction, TerminationCause, TerminationInfo, CDR_VERSION};
    use chrono::TimeZone;
    use serde_json::Value;
    use tempfile::NamedTempFile;
    use tokio::io::AsyncReadExt;

    fn sample(call_id: &str) -> CdrRecord {
        CdrRecord {
            version: CDR_VERSION,
            call_id: call_id.into(),
            sip_call_id: format!("{call_id}@pbx"),
            started_at: chrono::Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 0).unwrap(),
            ended_at: chrono::Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 5).unwrap(),
            duration_ms: 5000,
            from: "+13125551234".into(),
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
                bridge_disconnect: "stop_sent".into(),
                tap_disconnect: String::new(),
            },
            verstat_attest: None,
            verstat_passed: None,
            recording_id: None,
            recording_path: None,
            park: None,
            hold: None,
            reconnect: None,
        }
    }

    async fn read_all(path: &Path) -> String {
        let mut f = tokio::fs::File::open(path).await.expect("reopen");
        let mut buf = String::new();
        f.read_to_string(&mut buf).await.expect("read");
        buf
    }

    #[tokio::test]
    async fn writes_one_jsonl_line_per_emit() {
        let tmp = NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let sink = FileSink::open(&path).await.expect("open");

        sink.emit(sample("c-1")).await;
        sink.emit(sample("c-2")).await;

        // Drop the sink so the BufWriter flushes to disk.
        drop(sink);

        let body = read_all(&path).await;
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 lines, got: {body:?}");
        for line in &lines {
            let v: Value = serde_json::from_str(line).expect("each line is valid JSON");
            assert_eq!(v["version"], serde_json::json!(CDR_VERSION));
            assert_eq!(v["direction"], serde_json::json!("inbound"));
        }
        let v0: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v0["call_id"], serde_json::json!("c-1"));
        let v1: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v1["call_id"], serde_json::json!("c-2"));
    }

    #[tokio::test]
    async fn append_does_not_truncate_existing_content() {
        let tmp = NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        // Pre-populate the file as if a previous daemon process
        // had already written CDRs to it.
        tokio::fs::write(&path, b"{\"prior\":true}\n")
            .await
            .expect("seed");

        let sink = FileSink::open(&path).await.expect("open");
        sink.emit(sample("c-after")).await;
        drop(sink);

        let body = read_all(&path).await;
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "{\"prior\":true}");
        let v: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v["call_id"], serde_json::json!("c-after"));
    }

    #[tokio::test]
    async fn open_fails_loud_when_directory_missing() {
        let bad = Path::new("/nonexistent-dir-for-cdr-test/foo.jsonl");
        let err = FileSink::open(bad).await.unwrap_err();
        assert!(matches!(err, FileSinkError::Open { .. }));
    }

    #[tokio::test]
    async fn concurrent_emits_do_not_interleave() {
        // Drive 50 concurrent emits and confirm the file ends up
        // with 50 well-formed lines.
        let tmp = NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let sink = Arc::new(FileSink::open(&path).await.expect("open"));

        let mut handles = Vec::with_capacity(50);
        for i in 0..50 {
            let s = Arc::clone(&sink);
            handles.push(tokio::spawn(async move {
                s.emit(sample(&format!("c-{i}"))).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Drop the sink so flush completes.
        drop(sink);

        let body = read_all(&path).await;
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 50);
        for line in &lines {
            let v: Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("torn line {line:?}: {e}"));
            assert_eq!(v["version"], serde_json::json!(CDR_VERSION));
        }
    }
}
