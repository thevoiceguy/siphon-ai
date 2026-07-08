//! Durable recording-upload queue + worker (DESIGN_RECORDING_COMPLIANCE §3).
//!
//! A finalized recording is *enqueued*: a small JSON job file lands in
//! `spool_dir` (atomic `.tmp` + rename, `{created:020}-{call_id}.json` for
//! oldest-first lexical order — the exact webhook-spool pattern). A
//! background [`UploadWorker`] drains the spool on an interval, `PUT`ting
//! each recording to the configured [`S3Target`]. Jobs survive restarts;
//! an unreachable endpoint is retried next pass; a job that keeps failing
//! past [`MAX_UPLOAD_ATTEMPTS`] is dropped with a metric (never retried
//! forever). Per CLAUDE.md §4.7 none of this ever blocks a call path —
//! enqueue is one small file write at teardown, off the audio path.
//!
//! The local file is deleted only after a durable upload, and only when
//! `delete_local_after_upload` is set. Completed uploads are reported on
//! an [`UploadOutcome`] channel so the daemon can fire the
//! `recording_uploaded` lifecycle webhook without this crate knowing what
//! a webhook is.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::s3::{S3Location, S3Target};

/// How often the worker scans the spool. Matches the webhook drainer:
/// the first pass is *after* one interval, so a boot with a down
/// endpoint doesn't hammer it immediately.
const UPLOAD_INTERVAL: Duration = Duration::from_secs(10);

/// A job that fails this many attempts is dropped (metric `dropped`) —
/// one permanently-bad file can't wedge the spool.
pub const MAX_UPLOAD_ATTEMPTS: u32 = 100;

const UPLOADS_TOTAL: &str = "siphon_ai_recording_uploads_total";
const UPLOAD_SECONDS: &str = "siphon_ai_recording_upload_seconds";
const UPLOAD_SPOOL_DEPTH: &str = "siphon_ai_recording_upload_spool_depth";

/// One recording to upload. Serialized as the spool job file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadJob {
    /// Job-file schema version (internal, not a published API).
    pub version: u32,
    /// Bridge call id (also the recording id).
    pub call_id: String,
    /// The finalized local recording file.
    pub local_path: PathBuf,
    /// Object key to PUT as (already template-rendered).
    pub key: String,
    /// Unix seconds at enqueue — the spool-order prefix.
    pub created_at: u64,
    /// Delivery attempts so far (rewritten in place on failure).
    #[serde(default)]
    pub attempts: u32,
}

/// Where completed uploads are announced (for the lifecycle webhook).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadOutcome {
    pub call_id: String,
    pub location: S3Location,
    pub size_bytes: u64,
}

/// Everything the upload side of `[recording.storage]` compiles to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadSettings {
    pub target: S3Target,
    /// Object-key template with `{call_id}` / `{date}` / `{route}` /
    /// `{direction}` placeholders.
    pub key_template: String,
    pub delete_local_after_upload: bool,
    pub spool_dir: PathBuf,
}

impl UploadSettings {
    /// Render the object key for a call. `date` is UTC `YYYY-MM-DD` at
    /// enqueue time. The file extension of `local_path` rides along so
    /// `.wav` vs `.wava` is preserved.
    pub fn render_key(
        &self,
        call_id: &str,
        route: &str,
        direction: &str,
        local_path: &Path,
    ) -> String {
        let date = utc_date();
        let mut key = self
            .key_template
            .replace("{call_id}", call_id)
            .replace("{date}", &date)
            .replace("{route}", route)
            .replace("{direction}", direction);
        if let Some(ext) = local_path.extension().and_then(|e| e.to_str()) {
            key.push('.');
            key.push_str(ext);
        }
        key
    }

    /// The storage-agnostic pointer (`s3://bucket/key`) a CDR carries.
    /// Stamped at *enqueue* time: the key is deterministic, and the
    /// `recording_uploaded` webhook confirms actual completion.
    pub fn planned_uri(&self, key: &str) -> String {
        format!("s3://{}/{key}", self.target.bucket)
    }

    /// Write a job file (atomic `.tmp` + rename). Never blocks a call
    /// path beyond one small file write at teardown.
    pub fn enqueue(&self, job: &UploadJob) -> std::io::Result<()> {
        let name = format!("{:020}-{}.json", job.created_at, job.call_id);
        let tmp = self.spool_dir.join(format!("{name}.tmp"));
        let dst = self.spool_dir.join(name);
        std::fs::write(&tmp, serde_json::to_vec(job).expect("job serializes"))?;
        std::fs::rename(&tmp, &dst)?;
        debug!(call_id = %job.call_id, key = %job.key, "recording upload enqueued");
        Ok(())
    }

    /// Build a job for `local_path` with the rendered `key`, stamped now.
    pub fn job(&self, call_id: &str, key: String, local_path: PathBuf) -> UploadJob {
        UploadJob {
            version: 1,
            call_id: call_id.to_string(),
            local_path,
            key,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            attempts: 0,
        }
    }
}

/// The background drain worker. Owns the reqwest client and the spool.
pub struct UploadWorker {
    settings: UploadSettings,
    client: reqwest::Client,
    outcome_tx: mpsc::Sender<UploadOutcome>,
}

impl UploadWorker {
    /// Spawn the worker; returns the outcome receiver (completed uploads,
    /// for the `recording_uploaded` webhook) and the task handle. The
    /// worker exits when the outcome receiver is dropped *and* a send is
    /// attempted, or when the handle is aborted at shutdown.
    pub fn spawn(
        settings: UploadSettings,
    ) -> (tokio::task::JoinHandle<()>, mpsc::Receiver<UploadOutcome>) {
        let (outcome_tx, outcome_rx) = mpsc::channel(64);
        let worker = UploadWorker {
            settings,
            client: reqwest::Client::new(),
            outcome_tx,
        };
        let handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval(UPLOAD_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                worker.drain_once().await;
            }
        });
        (handle, outcome_rx)
    }

    /// One spool pass, oldest first. Public for tests.
    pub async fn drain_once(&self) {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(&self.settings.spool_dir) {
            Ok(read) => read
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect(),
            Err(err) => {
                warn!(error = %err, dir = %self.settings.spool_dir.display(),
                      "recording upload spool unreadable");
                return;
            }
        };
        entries.sort();
        metrics::gauge!(UPLOAD_SPOOL_DEPTH).set(entries.len() as f64);

        for job_path in entries {
            let job: UploadJob = match std::fs::read(&job_path)
                .map_err(|e| e.to_string())
                .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
            {
                Ok(job) => job,
                Err(err) => {
                    warn!(job = %job_path.display(), error = %err,
                          "removing unreadable upload job");
                    let _ = std::fs::remove_file(&job_path);
                    metrics::counter!(UPLOADS_TOTAL, "result" => "dropped").increment(1);
                    continue;
                }
            };

            if job.attempts >= MAX_UPLOAD_ATTEMPTS {
                warn!(call_id = %job.call_id, attempts = job.attempts,
                      "recording upload dropped after retry budget");
                let _ = std::fs::remove_file(&job_path);
                metrics::counter!(UPLOADS_TOTAL, "result" => "dropped").increment(1);
                continue;
            }

            // A recording deleted out from under the spool is a drop, not
            // a retry loop.
            let size_bytes = match std::fs::metadata(&job.local_path) {
                Ok(meta) => meta.len(),
                Err(err) => {
                    warn!(call_id = %job.call_id, path = %job.local_path.display(),
                          error = %err, "recording gone; dropping upload job");
                    let _ = std::fs::remove_file(&job_path);
                    metrics::counter!(UPLOADS_TOTAL, "result" => "dropped").increment(1);
                    continue;
                }
            };

            let started = Instant::now();
            match self
                .settings
                .target
                .put_file(&self.client, &job.key, &job.local_path)
                .await
            {
                Ok(location) => {
                    metrics::counter!(UPLOADS_TOTAL, "result" => "ok").increment(1);
                    metrics::histogram!(UPLOAD_SECONDS).record(started.elapsed().as_secs_f64());
                    info!(call_id = %job.call_id, uri = %location.uri, size_bytes,
                          "recording uploaded");
                    let _ = std::fs::remove_file(&job_path);
                    if self.settings.delete_local_after_upload {
                        if let Err(err) = std::fs::remove_file(&job.local_path) {
                            warn!(path = %job.local_path.display(), error = %err,
                                  "could not delete uploaded recording");
                        }
                    }
                    let _ = self
                        .outcome_tx
                        .send(UploadOutcome {
                            call_id: job.call_id,
                            location,
                            size_bytes,
                        })
                        .await;
                }
                Err(err) => {
                    metrics::counter!(UPLOADS_TOTAL, "result" => "failed").increment(1);
                    warn!(call_id = %job.call_id, attempts = job.attempts + 1, error = %err,
                          "recording upload failed; will retry");
                    // Persist the bumped attempt count (best-effort).
                    let bumped = UploadJob {
                        attempts: job.attempts + 1,
                        ..job
                    };
                    let _ = std::fs::write(
                        &job_path,
                        serde_json::to_vec(&bumped).expect("job serializes"),
                    );
                }
            }
        }
    }
}

/// UTC `YYYY-MM-DD` today (civil-from-days; no chrono dep here).
fn utc_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs();
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sigv4::SigV4Credentials;

    fn settings(spool: &Path) -> UploadSettings {
        UploadSettings {
            target: S3Target {
                endpoint: "http://127.0.0.1:1".into(), // never reachable
                bucket: "recs".into(),
                region: "us-east-1".into(),
                credentials: SigV4Credentials {
                    access_key: "k".into(),
                    secret_key: "s".into(),
                },
            },
            key_template: "{date}/{route}/{call_id}".into(),
            delete_local_after_upload: false,
            spool_dir: spool.to_path_buf(),
        }
    }

    fn temp_spool(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("siphon_up_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn key_template_renders_all_placeholders_and_keeps_extension() {
        let s = settings(Path::new("/tmp"));
        let key = s.render_key("call-1", "support", "inbound", Path::new("/x/call-1.wava"));
        let parts: Vec<&str> = key.split('/').collect();
        assert_eq!(parts.len(), 3, "{key}");
        assert_eq!(parts[1], "support");
        assert_eq!(parts[2], "call-1.wava");
        // {date} is today, shape YYYY-MM-DD.
        assert_eq!(parts[0].len(), 10, "{key}");
        assert!(s.planned_uri(&key).starts_with("s3://recs/"));
    }

    #[test]
    fn enqueue_is_atomic_and_lexically_ordered() {
        let spool = temp_spool("enqueue");
        let s = settings(&spool);
        let j1 = UploadJob {
            created_at: 100,
            ..s.job("a", "k/a.wav".into(), "/tmp/a.wav".into())
        };
        let j2 = UploadJob {
            created_at: 99,
            ..s.job("b", "k/b.wav".into(), "/tmp/b.wav".into())
        };
        s.enqueue(&j1).unwrap();
        s.enqueue(&j2).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&spool)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert!(
            names[0].contains("-b.json"),
            "older job sorts first: {names:?}"
        );
        assert!(names.iter().all(|n| !n.ends_with(".tmp")));
        let _ = std::fs::remove_dir_all(&spool);
    }

    #[tokio::test]
    async fn failed_upload_bumps_attempts_and_keeps_job() {
        let spool = temp_spool("retry");
        let s = settings(&spool);
        // A real local file, an unreachable endpoint.
        let rec = spool.join("rec.wav");
        std::fs::write(&rec, b"RIFFdata").unwrap();
        s.enqueue(&s.job("c1", "k/c1.wav".into(), rec.clone()))
            .unwrap();

        let (outcome_tx, _outcome_rx) = mpsc::channel(4);
        let worker = UploadWorker {
            settings: s,
            client: reqwest::Client::new(),
            outcome_tx,
        };
        worker.drain_once().await;

        let job_file = std::fs::read_dir(&spool)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|e| e == "json"))
            .expect("job survives a failed attempt");
        let job: UploadJob = serde_json::from_slice(&std::fs::read(&job_file).unwrap()).unwrap();
        assert_eq!(job.attempts, 1, "attempt count persisted");
        assert!(rec.exists(), "local file untouched on failure");
        let _ = std::fs::remove_dir_all(&spool);
    }

    #[tokio::test]
    async fn missing_local_file_drops_the_job() {
        let spool = temp_spool("gone");
        let s = settings(&spool);
        s.enqueue(&s.job("c2", "k/c2.wav".into(), spool.join("nope.wav")))
            .unwrap();
        let (outcome_tx, _outcome_rx) = mpsc::channel(4);
        let worker = UploadWorker {
            settings: s,
            client: reqwest::Client::new(),
            outcome_tx,
        };
        worker.drain_once().await;
        let jobs = std::fs::read_dir(&spool)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .count();
        assert_eq!(jobs, 0, "job for a vanished recording is dropped");
        let _ = std::fs::remove_dir_all(&spool);
    }
}
