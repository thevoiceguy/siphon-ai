//! Retrying JSON-over-HTTP POST delivery.
//!
//! [`RetryingPoster`] is the shared transport behind SiphonAI's two
//! outbound webhook sinks — the CDR webhook sink (`siphon-ai-cdr`)
//! and the lifecycle webhook sink (`siphon-ai-webhooks`). Both POST a
//! JSON body to an operator-supplied URL, retry transient failures
//! with exponential backoff, and never block the per-call task; the
//! only thing that differs between them is the payload type and the
//! log labels. Keeping the retry / backoff / transient-classification
//! / auth / signing logic here means a change to any of it happens
//! once and both sinks gain it.
//!
//! Per CLAUDE.md §4.7 delivery is best-effort: a failure after the
//! retry budget is exhausted is logged and the payload dropped, and
//! nothing on this path panics.
//!
//! ## Trust + observability (0.11.0)
//!
//! Every delivery carries:
//!
//! - **`X-SiphonAI-Event-Id`** (+ an `Idempotency-Key` alias) — a
//!   UUIDv4 generated once per [`post`](RetryingPoster::post) call and
//!   reused across every retry, so a receiver can dedupe an
//!   at-least-once redelivery.
//! - **`X-SiphonAI-Signature`** (when a `secret` is configured) —
//!   `t=<unix>,v1=<hex>` where the HMAC-SHA256 is computed over
//!   `"<unix>.<raw-body>"`. The timestamp is inside the signed string,
//!   so the receiver gets replay protection from a freshness window
//!   without a second header. The signature is recomputed per attempt
//!   (a future spool replay is a fresh, in-window send).
//!
//! The body is serialized **once** up front so the bytes that are
//! signed are exactly the bytes that are sent. Delivery outcomes feed
//! the `siphon_ai_webhook_*` metrics, labeled by [`SinkKind`].

pub mod kms;
pub mod s3;
pub mod sigv4;
pub mod upload;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use reqwest::{header::CONTENT_TYPE, Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use tracing::{debug, info, warn};

type HmacSha256 = Hmac<Sha256>;

/// Metric labels: `siphon_ai_webhook_deliveries_total{sink=...}`.
const DELIVERIES_TOTAL: &str = "siphon_ai_webhook_deliveries_total";
const DELIVERY_ATTEMPTS_TOTAL: &str = "siphon_ai_webhook_delivery_attempts_total";
const DELIVERY_SECONDS: &str = "siphon_ai_webhook_delivery_seconds";
const SPOOL_DEPTH: &str = "siphon_ai_webhook_spool_depth";

/// How often the drain worker re-attempts spooled deliveries. The
/// worker sleeps this long *before* its first pass, so a freshly built
/// poster doesn't immediately hammer a receiver that's already known to
/// be down — and tests that drive `drain_once` by hand aren't raced by
/// the background loop.
const DRAIN_INTERVAL: Duration = Duration::from_secs(10);

/// Hard cap on spooled files per sink. When the spool is at the cap a
/// newly-failed delivery is dropped (with a metric) rather than evicting
/// an already-persisted one — bounding disk without losing older,
/// already-durable deliveries.
const DEFAULT_SPOOL_MAX_FILES: usize = 10_000;

/// After this many drain re-attempts a spooled delivery is treated as a
/// poison entry: removed and counted `dropped`, so one permanently-bad
/// payload can't retry forever.
const MAX_DRAIN_ATTEMPTS: u32 = 100;

/// Which sink a [`RetryingPoster`] serves. Used purely as the bounded
/// `sink` metric label (three values) so lifecycle-webhook, CDR, and
/// audit-stream delivery health are separable on a dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkKind {
    /// `siphon-ai-webhooks` lifecycle events.
    Lifecycle,
    /// `siphon-ai-cdr` call-detail records.
    Cdr,
    /// `siphon-ai-audit` admin/security audit events.
    Audit,
}

impl SinkKind {
    /// Stable, low-cardinality metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            SinkKind::Lifecycle => "lifecycle",
            SinkKind::Cdr => "cdr",
            SinkKind::Audit => "audit",
        }
    }
}

/// Default retry budget for transient delivery failures. `0` means
/// "POST once, don't retry".
pub const DEFAULT_RETRY_MAX: u32 = 3;

/// Default per-attempt HTTP timeout, in milliseconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;

/// Building a [`RetryingPoster`] failed. Either the underlying
/// `reqwest::Client` could not be constructed (e.g. the TLS backend
/// failed to initialise), or the configured spool directory could not
/// be created / written. Both surface at daemon startup so a bad spool
/// path fails loud rather than at the first failed delivery (CLAUDE.md
/// §4.6).
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("failed to build HTTP client: {0}")]
    Client(#[from] reqwest::Error),
    #[error("failed to prepare spool dir {dir}: {source}")]
    Spool { dir: String, source: std::io::Error },
}

/// Identity fields stamped onto a delivery's `tracing` lines so an
/// operator can correlate a webhook log back to the call.
#[derive(Debug, Clone, Copy)]
pub struct PostLog<'a> {
    /// Human label for the sink, e.g. `"CDR webhook"`.
    pub kind: &'a str,
    /// `call_id` for correlation; pass `"-"` when there is none.
    pub call_id: &'a str,
    /// Optional extra detail, e.g. a webhook event type. Logged as
    /// `"-"` when `None`.
    pub detail: Option<&'a str>,
}

/// How to build a [`RetryingPoster`]. A struct (rather than a long
/// positional constructor) so the two sink crates set fields by name
/// and new knobs (signing secret, and the spool dir in a later chunk)
/// don't churn every call site.
#[derive(Debug, Clone)]
pub struct PosterConfig {
    /// Target URL.
    pub url: String,
    /// Optional `Authorization` header value, sent verbatim.
    pub auth_header: Option<String>,
    /// Retries *after* the first attempt. `0` = post once.
    pub retry_max: u32,
    /// Per-attempt HTTP timeout, in milliseconds.
    pub timeout_ms: u64,
    /// HMAC-SHA256 signing secret. `None` ⇒ no `X-SiphonAI-Signature`
    /// header (delivery is unsigned, today's behavior).
    pub secret: Option<String>,
    /// Durable spool directory. `None` ⇒ best-effort delivery (a
    /// failed delivery is dropped after the in-memory retry budget).
    /// `Some` ⇒ a delivery that exhausts the in-memory budget is
    /// persisted here and re-attempted by a background drain worker
    /// that survives restarts. Created + write-probed at build time.
    pub spool_dir: Option<PathBuf>,
    /// Which sink this poster serves (the `sink` metric label).
    pub sink: SinkKind,
}

/// A connection-pooling HTTP client that POSTs JSON payloads and
/// retries transient failures with exponential backoff.
///
/// Cheap to clone — the inner `reqwest::Client` is reference-counted,
/// so a sink can hand a fresh clone to each spawned delivery task.
#[derive(Debug, Clone)]
pub struct RetryingPoster {
    client: Client,
    url: String,
    auth_header: Option<String>,
    retry_max: u32,
    /// Signing key bytes (the `secret` UTF-8). `None` ⇒ unsigned.
    secret: Option<Vec<u8>>,
    /// Durable spool dir, validated at build time. `None` ⇒ no spool.
    spool_dir: Option<PathBuf>,
    sink: SinkKind,
}

impl RetryingPoster {
    /// Build a poster from [`PosterConfig`].
    ///
    /// The `reqwest::Client` is created once here so connection
    /// pooling spans deliveries. When `spool_dir` is set it is created
    /// (recursively) and write-probed now, so a bad path fails the
    /// daemon at startup. This does **not** spawn the drain worker —
    /// call [`spawn_drainer`](Self::spawn_drainer) once after building
    /// (the sink layer does this) so cloning a poster per delivery
    /// doesn't start a worker each time.
    pub fn new(config: PosterConfig) -> Result<Self, BuildError> {
        let PosterConfig {
            url,
            auth_header,
            retry_max,
            timeout_ms,
            secret,
            spool_dir,
            sink,
        } = config;
        if let Some(dir) = spool_dir.as_deref() {
            prepare_spool_dir(dir).map_err(|source| BuildError::Spool {
                dir: dir.display().to_string(),
                source,
            })?;
        }
        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()?;
        Ok(Self {
            client,
            url,
            auth_header,
            retry_max,
            secret: secret.filter(|s| !s.is_empty()).map(String::into_bytes),
            spool_dir,
            sink,
        })
    }

    /// Spawn the background drain worker, if this poster has a spool.
    /// Idempotent in practice because the sink layer calls it exactly
    /// once on the single poster it builds (clones don't re-call it).
    /// Must be called from within a Tokio runtime.
    pub fn spawn_drainer(&self) {
        let Some(dir) = self.spool_dir.clone() else {
            return;
        };
        let worker = self.clone();
        tokio::spawn(async move {
            // Sleep-then-drain (not interval.tick(), whose first tick is
            // immediate) so a known-down receiver isn't hammered the
            // instant the daemon starts.
            loop {
                tokio::time::sleep(DRAIN_INTERVAL).await;
                worker.drain_once(&dir).await;
            }
        });
    }

    /// POST `payload` as JSON, retrying transient failures.
    ///
    /// Transient failures (connect errors, 5xx, 408, 429) retry up to
    /// the configured budget with exponential backoff. Other 4xx are
    /// not retried — they mean the receiver is rejecting our payload
    /// shape, and retrying would just amplify load. A failure after
    /// the budget is exhausted is logged and the payload dropped.
    ///
    /// Every attempt carries the `X-SiphonAI-Event-Id` /
    /// `Idempotency-Key` (stable across this call's retries) and, when
    /// a `secret` is set, the per-attempt `X-SiphonAI-Signature`. The
    /// payload is serialized once so signed bytes == sent bytes.
    ///
    /// `log` only feeds `tracing`; it does not affect delivery.
    pub async fn post<T: Serialize>(&self, payload: &T, log: PostLog<'_>) {
        let PostLog {
            kind,
            call_id,
            detail,
        } = log;
        let detail = detail.unwrap_or("-");
        let sink = self.sink.as_str();

        // Serialize once: the signed bytes must be the sent bytes, and
        // re-serializing per attempt would risk a mismatch. A
        // serialize failure is a programming error on the payload type,
        // not a transient one — count it dropped and bail.
        let body = match serde_json::to_vec(payload) {
            Ok(b) => b,
            Err(e) => {
                warn!(kind, call_id, detail, error = %e, "webhook payload not serializable; dropped");
                metrics::counter!(DELIVERIES_TOTAL, "sink" => sink, "result" => "dropped")
                    .increment(1);
                return;
            }
        };

        // One idempotency id per logical delivery, reused on every
        // retry (and on a later spool replay) so a receiver can dedupe.
        let event_id = uuid::Uuid::new_v4().to_string();
        let started = Instant::now();
        let mut attempt: u32 = 0;
        loop {
            match self.send_once(&body, &event_id).await {
                Attempt::Ok => {
                    debug!(kind, url = %self.url, call_id, detail, event_id, "webhook delivered");
                    metrics::counter!(DELIVERIES_TOTAL, "sink" => sink, "result" => "delivered")
                        .increment(1);
                    metrics::histogram!(DELIVERY_SECONDS, "sink" => sink)
                        .record(started.elapsed().as_secs_f64());
                    return;
                }
                Attempt::Rejected => {
                    warn!(kind, url = %self.url, call_id, detail, event_id, "webhook rejected (4xx); not retrying");
                    metrics::counter!(DELIVERIES_TOTAL, "sink" => sink, "result" => "rejected")
                        .increment(1);
                    return;
                }
                Attempt::Transient => {}
            }
            if attempt >= self.retry_max {
                break;
            }
            tokio::time::sleep(backoff_for(attempt)).await;
            attempt += 1;
        }
        // In-memory budget exhausted. Spool for durable retry if
        // configured, otherwise drop (today's best-effort behavior).
        self.spool_or_drop(&event_id, &body, kind, call_id, detail)
            .await;
    }

    /// One HTTP attempt: build + sign + send, emit the per-attempt
    /// `attempts_total` metric, classify the result. Shared by [`post`]
    /// (the in-memory retry loop) and the spool drain worker. Connect /
    /// timeout errors classify as transient (retryable).
    async fn send_once(&self, body: &[u8], event_id: &str) -> Attempt {
        let sink = self.sink.as_str();
        let ts = unix_secs();
        let mut req = self
            .client
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .header("X-SiphonAI-Event-Id", event_id)
            .header("Idempotency-Key", event_id)
            .body(body.to_vec());
        if let Some(auth) = self.auth_header.as_deref() {
            req = req.header("Authorization", auth);
        }
        if let Some(secret) = self.secret.as_deref() {
            req = req.header("X-SiphonAI-Signature", sign(secret, ts, body));
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let outcome = if status.is_success() {
                    "ok"
                } else if is_transient(status) {
                    debug!(url = %self.url, event_id, status = %status, "delivery attempt: transient failure");
                    "transient"
                } else {
                    debug!(url = %self.url, event_id, status = %status, "delivery attempt: rejected (4xx)");
                    "rejected"
                };
                metrics::counter!(DELIVERY_ATTEMPTS_TOTAL, "sink" => sink, "outcome" => outcome)
                    .increment(1);
                match outcome {
                    "ok" => Attempt::Ok,
                    "rejected" => Attempt::Rejected,
                    _ => Attempt::Transient,
                }
            }
            Err(e) => {
                debug!(url = %self.url, event_id, error = %e, "delivery attempt: request error");
                metrics::counter!(DELIVERY_ATTEMPTS_TOTAL, "sink" => sink, "outcome" => "error")
                    .increment(1);
                Attempt::Transient
            }
        }
    }

    /// Persist a budget-exhausted delivery to the spool (if configured)
    /// or drop it. Emits the terminal `deliveries_total` result.
    async fn spool_or_drop(
        &self,
        event_id: &str,
        body: &[u8],
        kind: &str,
        call_id: &str,
        detail: &str,
    ) {
        let sink = self.sink.as_str();
        if let Some(dir) = self.spool_dir.as_deref() {
            match self.write_spool(dir, event_id, body).await {
                Ok(true) => {
                    info!(kind, url = %self.url, call_id, detail, event_id, "delivery spooled for durable retry");
                    metrics::counter!(DELIVERIES_TOTAL, "sink" => sink, "result" => "spooled")
                        .increment(1);
                    return;
                }
                Ok(false) => {
                    warn!(
                        kind,
                        call_id,
                        event_id,
                        cap = DEFAULT_SPOOL_MAX_FILES,
                        "spool full; delivery dropped"
                    );
                }
                Err(e) => {
                    warn!(kind, call_id, event_id, error = %e, "spool write failed; delivery dropped");
                }
            }
        } else {
            warn!(kind, url = %self.url, call_id, detail, event_id, "webhook giving up; payload dropped");
        }
        metrics::counter!(DELIVERIES_TOTAL, "sink" => sink, "result" => "dropped").increment(1);
    }

    /// Write a fresh spool entry. `Ok(false)` ⇒ the spool is at its
    /// file cap and the delivery was not persisted (caller drops it).
    async fn write_spool(&self, dir: &Path, event_id: &str, body: &[u8]) -> std::io::Result<bool> {
        if count_spool_files(dir).await? >= DEFAULT_SPOOL_MAX_FILES {
            return Ok(false);
        }
        let now = unix_secs();
        let env = SpoolEnvelope {
            id: event_id.to_string(),
            created_at: now,
            attempts: 0,
            // Eligible immediately; the drain worker picks it up on its
            // next pass.
            next_attempt_at: now,
            body: String::from_utf8_lossy(body).into_owned(),
        };
        write_envelope(dir, &env).await?;
        Ok(true)
    }

    /// One drain pass over the spool: re-attempt every due entry,
    /// oldest first. Delivered → removed; rejected (4xx) → removed;
    /// transient → reschedule with backoff (or drop after
    /// [`MAX_DRAIN_ATTEMPTS`]). Resumes pre-existing entries on the
    /// first pass after a restart. Private; the worker loop and tests
    /// call it.
    async fn drain_once(&self, dir: &Path) {
        let sink = self.sink.as_str();
        let mut paths = match list_spool_files(dir).await {
            Ok(p) => p,
            Err(e) => {
                debug!(dir = %dir.display(), error = %e, "spool read failed");
                return;
            }
        };
        // Sampled each pass — self-correcting across restarts and any
        // missed increment/decrement.
        metrics::gauge!(SPOOL_DEPTH, "sink" => sink).set(paths.len() as f64);
        paths.sort(); // zero-padded created_at prefix ⇒ oldest first
        let now = unix_secs();
        for path in paths {
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(_) => continue, // raced with another removal
            };
            let mut env: SpoolEnvelope = match serde_json::from_slice(&bytes) {
                Ok(e) => e,
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "corrupt spool entry; removing");
                    let _ = tokio::fs::remove_file(&path).await;
                    continue;
                }
            };
            if env.next_attempt_at > now {
                continue;
            }
            match self.send_once(env.body.as_bytes(), &env.id).await {
                Attempt::Ok => {
                    let _ = tokio::fs::remove_file(&path).await;
                    let dwell = now.saturating_sub(env.created_at) as f64;
                    metrics::counter!(DELIVERIES_TOTAL, "sink" => sink, "result" => "delivered")
                        .increment(1);
                    metrics::histogram!(DELIVERY_SECONDS, "sink" => sink).record(dwell);
                    info!(sink, event_id = %env.id, dwell_secs = dwell, "spooled delivery succeeded");
                }
                Attempt::Rejected => {
                    let _ = tokio::fs::remove_file(&path).await;
                    metrics::counter!(DELIVERIES_TOTAL, "sink" => sink, "result" => "rejected")
                        .increment(1);
                    warn!(sink, event_id = %env.id, "spooled delivery rejected (4xx); removed");
                }
                Attempt::Transient => {
                    env.attempts += 1;
                    if env.attempts >= MAX_DRAIN_ATTEMPTS {
                        let _ = tokio::fs::remove_file(&path).await;
                        metrics::counter!(DELIVERIES_TOTAL, "sink" => sink, "result" => "dropped")
                            .increment(1);
                        warn!(sink, event_id = %env.id, attempts = env.attempts, "spooled delivery exceeded max attempts; dropped");
                    } else {
                        env.next_attempt_at = now + drain_backoff_secs(env.attempts);
                        if let Err(e) = write_envelope(dir, &env).await {
                            warn!(event_id = %env.id, error = %e, "failed to reschedule spool entry");
                        }
                    }
                }
            }
        }
    }
}

/// Classification of a single HTTP delivery attempt.
#[derive(Clone, Copy)]
enum Attempt {
    /// 2xx — delivered.
    Ok,
    /// Retryable: 5xx / 408 / 429, or a connect/timeout error.
    Transient,
    /// Non-retryable 4xx — the receiver rejects our payload shape.
    Rejected,
}

/// One persisted spool entry. The body is stored as a UTF-8 string (it
/// is JSON, hence valid UTF-8); on replay it is sent verbatim so the
/// idempotency id and signature stay consistent with the original.
#[derive(Debug, Serialize, Deserialize)]
struct SpoolEnvelope {
    id: String,
    created_at: u64,
    attempts: u32,
    next_attempt_at: u64,
    body: String,
}

/// Retryable per RFC 9110 §15: server errors plus the explicit "try
/// again" set. Other 4xx mean our request is malformed; retrying
/// would only amplify the bad-request rate.
fn is_transient(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
}

/// Exponential backoff: 100ms, 200ms, 400ms, 800ms, …, capped at 5s.
fn backoff_for(attempt: u32) -> Duration {
    let ms = 100u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_millis(ms.min(5000))
}

/// Current Unix time in whole seconds. A clock before the epoch (only
/// a grossly-misconfigured host) yields `0` rather than panicking.
fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `t=<unix>,v1=<hex>` — HMAC-SHA256 over `"<unix>.<body>"`. The
/// timestamp is part of the signed string so the receiver's freshness
/// check can't be bypassed by rewriting it.
fn sign(secret: &[u8], ts: u64, body: &[u8]) -> String {
    // `new_from_slice` only errors for key types with a fixed length;
    // HMAC accepts any key length, so this never fails.
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(ts.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let mut out = String::with_capacity(4 + digest.len() * 2);
    out.push_str("t=");
    out.push_str(&ts.to_string());
    out.push_str(",v1=");
    for b in digest.iter() {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Create the spool directory (recursively) and confirm it's writable
/// with a probe file — so a bad path fails the daemon at startup, not
/// at the first failed delivery.
fn prepare_spool_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(".siphon-spool-probe");
    std::fs::write(&probe, b"")?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Per-entry drain backoff in seconds: 20, 40, 80, 160, 320→capped at
/// 300 (5 min). Bounds how fast a recovered receiver drains its
/// backlog without hammering a still-flaky one.
fn drain_backoff_secs(attempts: u32) -> u64 {
    (10u64.saturating_mul(1u64 << attempts.min(5))).min(300)
}

/// Count `*.json` spool entries (ignores `*.json.tmp` in-flight writes).
async fn count_spool_files(dir: &Path) -> std::io::Result<usize> {
    Ok(list_spool_files(dir).await?.len())
}

/// List `*.json` spool entries (ignores `*.json.tmp` in-flight writes).
async fn list_spool_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut rd = tokio::fs::read_dir(dir).await?;
    let mut out = Vec::new();
    while let Some(ent) = rd.next_entry().await? {
        let p = ent.path();
        if p.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(p);
        }
    }
    Ok(out)
}

/// Write a spool entry atomically (write `.tmp`, then rename). The
/// filename is deterministic in `(created_at, id)`, so a drain reschedule
/// overwrites the same entry rather than duplicating it. The
/// zero-padded `created_at` prefix gives lexical oldest-first ordering.
async fn write_envelope(dir: &Path, env: &SpoolEnvelope) -> std::io::Result<()> {
    let json = serde_json::to_vec(env)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let stem = format!("{:020}-{}", env.created_at, env.id);
    let final_path = dir.join(format!("{stem}.json"));
    let tmp_path = dir.join(format!("{stem}.json.tmp"));
    tokio::fs::write(&tmp_path, &json).await?;
    tokio::fs::rename(&tmp_path, &final_path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as AsyncMutex;

    #[derive(Serialize)]
    struct Sample {
        id: String,
        n: u32,
    }

    fn sample(id: &str) -> Sample {
        Sample {
            id: id.into(),
            n: 42,
        }
    }

    fn log(call_id: &str) -> PostLog<'_> {
        PostLog {
            kind: "test webhook",
            call_id,
            detail: None,
        }
    }

    /// Per-attempt recorder; programmable status per attempt. Captures
    /// the raw body bytes + the delivery headers so tests can verify
    /// signing and idempotency.
    struct Recorder {
        payloads: AsyncMutex<Vec<serde_json::Value>>,
        raw_bodies: AsyncMutex<Vec<Vec<u8>>>,
        auth_headers: AsyncMutex<Vec<Option<String>>>,
        signatures: AsyncMutex<Vec<Option<String>>>,
        event_ids: AsyncMutex<Vec<Option<String>>>,
        idempotency_keys: AsyncMutex<Vec<Option<String>>>,
        attempt: AtomicU32,
        status_per_attempt: Vec<u16>,
    }

    impl Recorder {
        fn new(status_per_attempt: Vec<u16>) -> Arc<Self> {
            Arc::new(Self {
                payloads: AsyncMutex::new(Vec::new()),
                raw_bodies: AsyncMutex::new(Vec::new()),
                auth_headers: AsyncMutex::new(Vec::new()),
                signatures: AsyncMutex::new(Vec::new()),
                event_ids: AsyncMutex::new(Vec::new()),
                idempotency_keys: AsyncMutex::new(Vec::new()),
                attempt: AtomicU32::new(0),
                status_per_attempt,
            })
        }
    }

    /// Build a poster: unsigned by default; `secret` set ⇒ signed. No
    /// spool (best-effort).
    fn poster(
        url: String,
        auth_header: Option<&str>,
        retry_max: u32,
        secret: Option<&str>,
    ) -> RetryingPoster {
        RetryingPoster::new(PosterConfig {
            url,
            auth_header: auth_header.map(str::to_string),
            retry_max,
            timeout_ms: 1000,
            secret: secret.map(str::to_string),
            spool_dir: None,
            sink: SinkKind::Lifecycle,
        })
        .unwrap()
    }

    /// Build a poster with a spool dir (no auto drain worker — tests
    /// call `drain_once` directly for determinism).
    fn spooling_poster(url: String, retry_max: u32, dir: PathBuf) -> RetryingPoster {
        RetryingPoster::new(PosterConfig {
            url,
            auth_header: None,
            retry_max,
            timeout_ms: 1000,
            secret: None,
            spool_dir: Some(dir),
            sink: SinkKind::Cdr,
        })
        .unwrap()
    }

    /// Spin up a tiny hyper server on an ephemeral port. The returned
    /// URL is what the poster POSTs to.
    async fn spawn_server(rec: Arc<Recorder>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let rec = Arc::clone(&rec);
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(stream),
                            service_fn(move |req: Request<Incoming>| {
                                let rec = Arc::clone(&rec);
                                async move { handle(rec, req).await }
                            }),
                        )
                        .await;
                });
            }
        });
        format!("http://{addr}/sink")
    }

    async fn handle(
        rec: Arc<Recorder>,
        req: Request<Incoming>,
    ) -> Result<Response<String>, Infallible> {
        let header = |name: &str| {
            req.headers()
                .get(name)
                .and_then(|v| v.to_str().ok().map(str::to_string))
        };
        let auth = header("authorization");
        let signature = header("x-siphonai-signature");
        let event_id = header("x-siphonai-event-id");
        let idempotency_key = header("idempotency-key");
        let body = req.collect().await.expect("collect").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("body is valid JSON");
        rec.payloads.lock().await.push(json);
        rec.raw_bodies.lock().await.push(body.to_vec());
        rec.auth_headers.lock().await.push(auth);
        rec.signatures.lock().await.push(signature);
        rec.event_ids.lock().await.push(event_id);
        rec.idempotency_keys.lock().await.push(idempotency_key);
        let idx = rec.attempt.fetch_add(1, Ordering::Relaxed) as usize;
        let status = rec.status_per_attempt.get(idx).copied().unwrap_or(200);
        Ok(Response::builder()
            .status(status)
            .body(String::new())
            .unwrap())
    }

    #[tokio::test]
    async fn happy_path_posts_payload_with_optional_auth_header() {
        let rec = Recorder::new(vec![200]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let poster = poster(url, Some("Bearer test-token"), 0, None);

        poster.post(&sample("c-1"), log("c-1")).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["id"], serde_json::json!("c-1"));
        assert_eq!(payloads[0]["n"], serde_json::json!(42));
        let auth = rec.auth_headers.lock().await;
        assert_eq!(auth[0].as_deref(), Some("Bearer test-token"));
        // Idempotency id is always present; an unsigned poster sends no
        // signature.
        assert!(rec.event_ids.lock().await[0].is_some());
        assert!(rec.signatures.lock().await[0].is_none());
    }

    #[tokio::test]
    async fn transient_5xx_retries_then_succeeds() {
        // 503, 503, 200 — the poster keeps trying through the budget.
        let rec = Recorder::new(vec![503, 503, 200]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let poster = poster(url, None, 3, None);

        poster.post(&sample("c-retry"), log("c-retry")).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 3, "expected 3 attempts");
    }

    #[tokio::test]
    async fn permanent_4xx_does_not_retry() {
        // 401 is non-retryable; the poster sends once and gives up.
        let rec = Recorder::new(vec![401, 200]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let poster = poster(url, None, 5, None);

        poster.post(&sample("c-401"), log("c-401")).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 1, "4xx must not retry");
    }

    #[tokio::test]
    async fn retry_exhaustion_drops_payload_without_panicking() {
        // Server returns 500 forever; the poster gives up after the
        // budget and must not panic.
        let rec = Recorder::new(vec![500, 500, 500, 500, 500]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let poster = poster(url, None, 2, None);

        poster.post(&sample("c-give-up"), log("c-give-up")).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 3, "first attempt + retry_max=2 retries");
    }

    #[tokio::test]
    async fn signed_delivery_carries_verifiable_signature() {
        let rec = Recorder::new(vec![200]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let secret = "whsec_test";
        let poster = poster(url, None, 0, Some(secret));

        poster.post(&sample("c-sig"), log("c-sig")).await;

        let header = rec.signatures.lock().await[0]
            .clone()
            .expect("signature header present when secret set");
        // Format: t=<unix>,v1=<hex>.
        let (t_part, v1_part) = header.split_once(',').expect("two comma parts");
        let ts: u64 = t_part
            .strip_prefix("t=")
            .expect("t= prefix")
            .parse()
            .unwrap();
        let got = v1_part.strip_prefix("v1=").expect("v1= prefix");
        // Recompute over the EXACT received bytes and compare.
        let body = rec.raw_bodies.lock().await[0].clone();
        let expected = sign(secret.as_bytes(), ts, &body);
        assert_eq!(
            format!("t={ts},v1={got}"),
            expected,
            "signature must verify"
        );
    }

    #[tokio::test]
    async fn idempotency_id_is_stable_across_retries() {
        // 503 then 200: two attempts, same event id + idempotency key,
        // so a receiver dedupes the redelivery.
        let rec = Recorder::new(vec![503, 200]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let poster = poster(url, None, 3, None);

        poster.post(&sample("c-idem"), log("c-idem")).await;

        let ids = rec.event_ids.lock().await;
        let keys = rec.idempotency_keys.lock().await;
        assert_eq!(ids.len(), 2, "expected a retry");
        assert!(ids[0].is_some());
        assert_eq!(ids[0], ids[1], "event id must be stable across retries");
        assert_eq!(
            ids[0], keys[0],
            "Idempotency-Key mirrors X-SiphonAI-Event-Id"
        );
    }

    #[tokio::test]
    async fn exhausted_delivery_is_spooled() {
        // Receiver 500s; with retry_max=0 the single in-memory attempt
        // exhausts and the delivery is persisted to the spool.
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::new(vec![500]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let poster = spooling_poster(url, 0, dir.path().to_path_buf());

        poster.post(&sample("c-spool"), log("c-spool")).await;

        let files = list_spool_files(dir.path()).await.unwrap();
        assert_eq!(files.len(), 1, "one spool entry written");
        // The persisted body round-trips to the original payload.
        let bytes = tokio::fs::read(&files[0]).await.unwrap();
        let env: SpoolEnvelope = serde_json::from_slice(&bytes).unwrap();
        let body: serde_json::Value = serde_json::from_str(&env.body).unwrap();
        assert_eq!(body["id"], serde_json::json!("c-spool"));
        assert_eq!(env.attempts, 0);
    }

    #[tokio::test]
    async fn spooled_delivery_drains_then_is_removed() {
        // post() attempt #0 = 500 (spools); drain attempt #1 = 200
        // (delivers + removes). The receiver sees the SAME id on both.
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::new(vec![500, 200]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let poster = spooling_poster(url, 0, dir.path().to_path_buf());

        poster.post(&sample("c-drain"), log("c-drain")).await;
        assert_eq!(list_spool_files(dir.path()).await.unwrap().len(), 1);

        poster.drain_once(dir.path()).await;
        assert!(
            list_spool_files(dir.path()).await.unwrap().is_empty(),
            "delivered entry removed from spool"
        );
        let ids = rec.event_ids.lock().await;
        assert_eq!(ids.len(), 2, "post + drain attempt");
        assert_eq!(ids[0], ids[1], "spool replay reuses the event id");
    }

    #[tokio::test]
    async fn drain_resumes_preexisting_spool_entries_after_restart() {
        let dir = tempfile::tempdir().unwrap();

        // Process 1: receiver down → spool one entry, then "crash".
        let rec1 = Recorder::new(vec![500]);
        let url1 = spawn_server(Arc::clone(&rec1)).await;
        let p1 = spooling_poster(url1, 0, dir.path().to_path_buf());
        p1.post(&sample("c-restart"), log("c-restart")).await;
        drop(p1);
        assert_eq!(list_spool_files(dir.path()).await.unwrap().len(), 1);

        // Process 2: a fresh poster on the SAME spool dir, receiver now
        // up. Draining delivers the entry the prior process left behind.
        let rec2 = Recorder::new(vec![200]);
        let url2 = spawn_server(Arc::clone(&rec2)).await;
        let p2 = spooling_poster(url2, 0, dir.path().to_path_buf());
        p2.drain_once(dir.path()).await;

        assert!(list_spool_files(dir.path()).await.unwrap().is_empty());
        assert_eq!(
            rec2.payloads.lock().await.len(),
            1,
            "restarted process delivered the spooled entry"
        );
    }

    #[tokio::test]
    async fn bad_spool_dir_fails_at_build() {
        // A spool path under a file (not a dir) can't be created →
        // build fails loud rather than at first delivery.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let bad = tmp.path().join("under-a-file");
        let err = RetryingPoster::new(PosterConfig {
            url: "http://127.0.0.1:1/sink".into(),
            auth_header: None,
            retry_max: 0,
            timeout_ms: 1000,
            secret: None,
            spool_dir: Some(bad),
            sink: SinkKind::Cdr,
        });
        assert!(matches!(err, Err(BuildError::Spool { .. })));
    }
}
