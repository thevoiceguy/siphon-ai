//! Retrying JSON-over-HTTP POST delivery.
//!
//! [`RetryingPoster`] is the shared transport behind SiphonAI's two
//! outbound webhook sinks — the CDR webhook sink (`siphon-ai-cdr`)
//! and the lifecycle webhook sink (`siphon-ai-webhooks`). Both POST a
//! JSON body to an operator-supplied URL, retry transient failures
//! with exponential backoff, and never block the per-call task; the
//! only thing that differs between them is the payload type and the
//! log labels. Keeping the retry / backoff / transient-classification
//! / auth logic here means a change to any of it — including future
//! HMAC request signing — happens once.
//!
//! Per CLAUDE.md §4.7 delivery is best-effort: a failure after the
//! retry budget is exhausted is logged and the payload dropped, and
//! nothing on this path panics.

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::Serialize;
use thiserror::Error;
use tracing::{debug, warn};

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
}

impl RetryingPoster {
    /// Build a poster targeting `url`.
    ///
    /// `auth_header`, when set, is sent verbatim as the
    /// `Authorization` header on every attempt. `retry_max` is the
    /// number of retries *after* the first attempt; `timeout_ms`
    /// bounds each individual attempt. The `reqwest::Client` is
    /// created once here so connection pooling spans deliveries.
    pub fn new(
        url: impl Into<String>,
        auth_header: Option<String>,
        retry_max: u32,
        timeout_ms: u64,
    ) -> Result<Self, BuildError> {
        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()?;
        Ok(Self {
            client,
            url: url.into(),
            auth_header,
            retry_max,
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
    /// `log` only feeds `tracing`; it does not affect delivery.
    pub async fn post<T: Serialize>(&self, payload: &T, log: PostLog<'_>) {
        let PostLog {
            kind,
            call_id,
            detail,
        } = log;
        let detail = detail.unwrap_or("-");
        let mut attempt: u32 = 0;
        loop {
            let mut req = self.client.post(&self.url).json(payload);
            if let Some(auth) = self.auth_header.as_deref() {
                req = req.header("Authorization", auth);
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        debug!(kind, url = %self.url, call_id, detail, status = %status, "webhook delivered");
                        return;
                    }
                    if !is_transient(status) {
                        warn!(kind, url = %self.url, call_id, detail, status = %status, "webhook rejected (4xx); not retrying");
                        return;
                    }
                    warn!(kind, url = %self.url, call_id, detail, status = %status, attempt = attempt + 1, "webhook transient failure");
                }
                Err(e) => {
                    warn!(kind, url = %self.url, call_id, detail, error = %e, attempt = attempt + 1, "webhook request failed");
                }
            }
            if attempt >= self.retry_max {
                warn!(kind, url = %self.url, call_id, detail, attempts = attempt + 1, "webhook giving up; payload dropped");
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

    /// Payload + auth-header recorder; programmable status per attempt.
    struct Recorder {
        payloads: AsyncMutex<Vec<serde_json::Value>>,
        auth_headers: AsyncMutex<Vec<Option<String>>>,
        attempt: AtomicU32,
        status_per_attempt: Vec<u16>,
    }

    impl Recorder {
        fn new(status_per_attempt: Vec<u16>) -> Arc<Self> {
            Arc::new(Self {
                payloads: AsyncMutex::new(Vec::new()),
                auth_headers: AsyncMutex::new(Vec::new()),
                attempt: AtomicU32::new(0),
                status_per_attempt,
            })
        }
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
        let auth = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok().map(str::to_string));
        let body = req.collect().await.expect("collect").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("body is valid JSON");
        rec.payloads.lock().await.push(json);
        rec.auth_headers.lock().await.push(auth);
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
        let poster = RetryingPoster::new(url, Some("Bearer test-token".into()), 0, 1000).unwrap();

        poster.post(&sample("c-1"), log("c-1")).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["id"], serde_json::json!("c-1"));
        assert_eq!(payloads[0]["n"], serde_json::json!(42));
        let auth = rec.auth_headers.lock().await;
        assert_eq!(auth[0].as_deref(), Some("Bearer test-token"));
    }

    #[tokio::test]
    async fn transient_5xx_retries_then_succeeds() {
        // 503, 503, 200 — the poster keeps trying through the budget.
        let rec = Recorder::new(vec![503, 503, 200]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let poster = RetryingPoster::new(url, None, 3, 1000).unwrap();

        poster.post(&sample("c-retry"), log("c-retry")).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 3, "expected 3 attempts");
    }

    #[tokio::test]
    async fn permanent_4xx_does_not_retry() {
        // 401 is non-retryable; the poster sends once and gives up.
        let rec = Recorder::new(vec![401, 200]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let poster = RetryingPoster::new(url, None, 5, 1000).unwrap();

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
        let poster = RetryingPoster::new(url, None, 2, 500).unwrap();

        poster.post(&sample("c-give-up"), log("c-give-up")).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 3, "first attempt + retry_max=2 retries");
    }
}
