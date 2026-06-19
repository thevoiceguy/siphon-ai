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

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use reqwest::{header::CONTENT_TYPE, Client, StatusCode};
use serde::Serialize;
use sha2::Sha256;
use thiserror::Error;
use tracing::{debug, warn};

type HmacSha256 = Hmac<Sha256>;

/// Metric labels: `siphon_ai_webhook_deliveries_total{sink=...}`.
const DELIVERIES_TOTAL: &str = "siphon_ai_webhook_deliveries_total";
const DELIVERY_ATTEMPTS_TOTAL: &str = "siphon_ai_webhook_delivery_attempts_total";
const DELIVERY_SECONDS: &str = "siphon_ai_webhook_delivery_seconds";

/// Which sink a [`RetryingPoster`] serves. Used purely as the bounded
/// `sink` metric label (two values) so lifecycle-webhook and CDR
/// delivery health are separable on a dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkKind {
    /// `siphon-ai-webhooks` lifecycle events.
    Lifecycle,
    /// `siphon-ai-cdr` call-detail records.
    Cdr,
}

impl SinkKind {
    /// Stable, low-cardinality metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            SinkKind::Lifecycle => "lifecycle",
            SinkKind::Cdr => "cdr",
        }
    }
}

/// Default retry budget for transient delivery failures. `0` means
/// "POST once, don't retry".
pub const DEFAULT_RETRY_MAX: u32 = 3;

/// Default per-attempt HTTP timeout, in milliseconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;

/// Building a [`RetryingPoster`] failed: the underlying
/// `reqwest::Client` could not be constructed (e.g. the TLS backend
/// failed to initialise). Wraps the `reqwest` error so consumers
/// don't need a direct dependency on `reqwest`.
#[derive(Debug, Error)]
#[error("failed to build HTTP client: {0}")]
pub struct BuildError(#[from] reqwest::Error);

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
    sink: SinkKind,
}

impl RetryingPoster {
    /// Build a poster from [`PosterConfig`].
    ///
    /// The `reqwest::Client` is created once here so connection
    /// pooling spans deliveries.
    pub fn new(config: PosterConfig) -> Result<Self, BuildError> {
        let PosterConfig {
            url,
            auth_header,
            retry_max,
            timeout_ms,
            secret,
            sink,
        } = config;
        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()?;
        Ok(Self {
            client,
            url,
            auth_header,
            retry_max,
            secret: secret.filter(|s| !s.is_empty()).map(String::into_bytes),
            sink,
        })
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
        // retry so a receiver can dedupe an at-least-once redelivery.
        let event_id = uuid::Uuid::new_v4().to_string();
        let started = Instant::now();
        let mut attempt: u32 = 0;
        loop {
            let ts = unix_secs();
            let mut req = self
                .client
                .post(&self.url)
                .header(CONTENT_TYPE, "application/json")
                .header("X-SiphonAI-Event-Id", &event_id)
                .header("Idempotency-Key", &event_id)
                .body(body.clone());
            if let Some(auth) = self.auth_header.as_deref() {
                req = req.header("Authorization", auth);
            }
            if let Some(secret) = self.secret.as_deref() {
                req = req.header("X-SiphonAI-Signature", sign(secret, ts, &body));
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        debug!(kind, url = %self.url, call_id, detail, event_id, status = %status, "webhook delivered");
                        metrics::counter!(DELIVERY_ATTEMPTS_TOTAL, "sink" => sink, "outcome" => "ok").increment(1);
                        metrics::counter!(DELIVERIES_TOTAL, "sink" => sink, "result" => "delivered").increment(1);
                        metrics::histogram!(DELIVERY_SECONDS, "sink" => sink)
                            .record(started.elapsed().as_secs_f64());
                        return;
                    }
                    if !is_transient(status) {
                        warn!(kind, url = %self.url, call_id, detail, event_id, status = %status, "webhook rejected (4xx); not retrying");
                        metrics::counter!(DELIVERY_ATTEMPTS_TOTAL, "sink" => sink, "outcome" => "rejected").increment(1);
                        metrics::counter!(DELIVERIES_TOTAL, "sink" => sink, "result" => "rejected")
                            .increment(1);
                        return;
                    }
                    warn!(kind, url = %self.url, call_id, detail, event_id, status = %status, attempt = attempt + 1, "webhook transient failure");
                    metrics::counter!(DELIVERY_ATTEMPTS_TOTAL, "sink" => sink, "outcome" => "transient").increment(1);
                }
                Err(e) => {
                    warn!(kind, url = %self.url, call_id, detail, event_id, error = %e, attempt = attempt + 1, "webhook request failed");
                    metrics::counter!(DELIVERY_ATTEMPTS_TOTAL, "sink" => sink, "outcome" => "error").increment(1);
                }
            }
            if attempt >= self.retry_max {
                warn!(kind, url = %self.url, call_id, detail, event_id, attempts = attempt + 1, "webhook giving up; payload dropped");
                metrics::counter!(DELIVERIES_TOTAL, "sink" => sink, "result" => "dropped")
                    .increment(1);
                return;
            }
            tokio::time::sleep(backoff_for(attempt)).await;
            attempt += 1;
        }
    }
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

    /// Build a poster: unsigned by default; `secret` set ⇒ signed.
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
            sink: SinkKind::Lifecycle,
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
}
