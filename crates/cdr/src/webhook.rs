//! HTTP POST sink with retry + exponential backoff.
//!
//! Each `emit` spawns a background task that runs the POST out of
//! the per-call task's hot path. The spawned task retries
//! transient failures (connect errors, 5xx) up to `retry_max`,
//! then logs and gives up. 4xx are NOT retried — those mean the
//! receiver is rejecting our payload shape, and retrying would
//! just amplify load. Per CLAUDE.md §4.7 the sink never panics
//! and never blocks the controller.
//!
//! ## What we don't do (yet)
//!
//! - **No persistent queue.** A daemon restart loses any in-flight
//!   retries. Operators who need durability point the file sink at
//!   the same record stream and tail it on the receiver side.
//! - **No HMAC signature.** A future webhook secret would be a
//!   computed `X-SiphonAI-Signature` header; CLAUDE.md §11.6 sketches
//!   this. v1 callers either trust the network or use mTLS upstream.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::schema::CdrRecord;
use crate::sink::CdrSink;

/// Knobs the daemon's `[cdr.webhook]` block resolves into.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookSinkConfig {
    pub url: String,
    /// Optional `Authorization` header value. Sent verbatim — the
    /// caller decides between `Bearer ...`, basic, or other schemes.
    #[serde(default)]
    pub auth_header: Option<String>,
    /// Maximum retries on transient failure. `0` means "post once,
    /// don't retry."
    #[serde(default = "default_retry_max")]
    pub retry_max: u32,
    /// Per-attempt timeout. Total worst-case time for one record is
    /// roughly `(retry_max + 1) * timeout_ms` plus backoff sleeps.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_retry_max() -> u32 {
    3
}
fn default_timeout_ms() -> u64 {
    5000
}

impl WebhookSinkConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            auth_header: None,
            retry_max: default_retry_max(),
            timeout_ms: default_timeout_ms(),
        }
    }
}

/// HTTP webhook sink. One per configured URL; cheap to clone (the
/// inner `reqwest::Client` is cloneable).
#[derive(Debug, Clone)]
pub struct WebhookSink {
    client: Client,
    config: WebhookSinkConfig,
}

impl WebhookSink {
    /// Build the sink. The `reqwest::Client` is created once per
    /// sink so connection pooling kicks in across calls.
    pub fn new(config: WebhookSinkConfig) -> reqwest::Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()?;
        Ok(Self { client, config })
    }
}

#[async_trait]
impl CdrSink for WebhookSink {
    async fn emit(&self, record: CdrRecord) {
        // Run the POST + retry loop on a spawned task so the
        // per-call cleanup (which calls us) doesn't block on
        // network I/O. The sink contract (CLAUDE.md §4.7) says
        // emission is best-effort — losing a record on a daemon
        // restart is acceptable, blocking call teardown is not.
        let client = self.client.clone();
        let config = self.config.clone();
        tokio::spawn(async move {
            post_with_retry(&client, &config, record).await;
        });
    }
}

async fn post_with_retry(client: &Client, config: &WebhookSinkConfig, record: CdrRecord) {
    let mut attempt: u32 = 0;
    loop {
        let mut req = client.post(&config.url).json(&record);
        if let Some(auth) = config.auth_header.as_deref() {
            req = req.header("Authorization", auth);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    debug!(
                        url = %config.url,
                        call_id = %record.call_id,
                        status = %status,
                        "CDR webhook delivered"
                    );
                    return;
                }
                if !is_transient(status) {
                    warn!(
                        url = %config.url,
                        call_id = %record.call_id,
                        status = %status,
                        "CDR webhook rejected (4xx); not retrying"
                    );
                    return;
                }
                warn!(
                    url = %config.url,
                    call_id = %record.call_id,
                    status = %status,
                    attempt = attempt + 1,
                    "CDR webhook transient failure"
                );
            }
            Err(e) => {
                warn!(
                    url = %config.url,
                    call_id = %record.call_id,
                    error = %e,
                    attempt = attempt + 1,
                    "CDR webhook request failed"
                );
            }
        }
        if attempt >= config.retry_max {
            warn!(
                url = %config.url,
                call_id = %record.call_id,
                attempts = attempt + 1,
                "CDR webhook giving up; record dropped"
            );
            return;
        }
        let backoff = backoff_for(attempt);
        tokio::time::sleep(backoff).await;
        attempt += 1;
    }
}

fn is_transient(status: StatusCode) -> bool {
    // Retryable per RFC 7231 §6.6: server errors and the explicit
    // "try again" set. 4xx other than 408 / 429 means our request
    // is wrong; retrying just amplifies the bad-request rate.
    status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
}

/// Exponential backoff: 100ms, 200ms, 400ms, 800ms, capped at 5s.
fn backoff_for(attempt: u32) -> Duration {
    let ms = 100u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_millis(ms.min(5000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        AudioInfo, CdrRecord, Direction, TerminationCause, TerminationInfo, CDR_VERSION,
    };
    use chrono::TimeZone;
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

    fn sample(call_id: &str) -> CdrRecord {
        CdrRecord {
            version: CDR_VERSION,
            call_id: call_id.into(),
            sip_call_id: format!("{call_id}@pbx"),
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
        }
    }

    /// Payload + auth-header recorder; each request mutates the
    /// inner state. Programmable status code per attempt.
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

    /// Spin up a tiny hyper server on an ephemeral port. The
    /// returned URL is what the sink POSTs to.
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
        format!("http://{addr}/cdr")
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

        let attempt_idx = rec.attempt.fetch_add(1, Ordering::Relaxed) as usize;
        let status_code = rec
            .status_per_attempt
            .get(attempt_idx)
            .copied()
            .unwrap_or(200);
        let resp = Response::builder()
            .status(status_code)
            .body(String::new())
            .unwrap();
        Ok(resp)
    }

    #[tokio::test]
    async fn happy_path_posts_record_with_optional_auth_header() {
        let rec = Recorder::new(vec![200]);
        let url = spawn_server(Arc::clone(&rec)).await;

        let sink = WebhookSink::new(WebhookSinkConfig {
            url,
            auth_header: Some("Bearer test-token".into()),
            retry_max: 0,
            timeout_ms: 1000,
        })
        .unwrap();

        sink.emit(sample("c-1")).await;
        // emit() spawns; give the spawned POST time to land.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["call_id"], serde_json::json!("c-1"));
        assert_eq!(payloads[0]["version"], serde_json::json!(1));

        let auth = rec.auth_headers.lock().await;
        assert_eq!(auth[0].as_deref(), Some("Bearer test-token"));
    }

    #[tokio::test]
    async fn transient_5xx_retries_then_succeeds() {
        // 503, 503, 200 — sink should hit the third attempt and
        // give up before that with retry_max=2 succeeded path.
        let rec = Recorder::new(vec![503, 503, 200]);
        let url = spawn_server(Arc::clone(&rec)).await;

        let sink = WebhookSink::new(WebhookSinkConfig {
            url,
            auth_header: None,
            retry_max: 3,
            timeout_ms: 1000,
        })
        .unwrap();

        sink.emit(sample("c-retry")).await;
        // Backoff: 100 + 200 = 300ms before attempt 3; pad
        // generously for slow runners.
        tokio::time::sleep(Duration::from_millis(800)).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 3, "expected 3 attempts");
    }

    #[tokio::test]
    async fn permanent_4xx_does_not_retry() {
        // 401 is non-retryable; sink posts once and gives up.
        let rec = Recorder::new(vec![401, 200]);
        let url = spawn_server(Arc::clone(&rec)).await;

        let sink = WebhookSink::new(WebhookSinkConfig {
            url,
            auth_header: None,
            retry_max: 5,
            timeout_ms: 1000,
        })
        .unwrap();

        sink.emit(sample("c-401")).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 1, "4xx must not retry");
    }

    #[tokio::test]
    async fn retry_exhaustion_drops_record_without_panicking() {
        // Server returns 500 forever; sink should give up after
        // retry_max attempts and not panic.
        let rec = Recorder::new(vec![500, 500, 500, 500, 500]);
        let url = spawn_server(Arc::clone(&rec)).await;

        let sink = WebhookSink::new(WebhookSinkConfig {
            url,
            auth_header: None,
            retry_max: 2,
            timeout_ms: 500,
        })
        .unwrap();

        sink.emit(sample("c-give-up")).await;
        // Backoff is 100 + 200 = 300ms; total ~ 500ms. Pad.
        tokio::time::sleep(Duration::from_millis(900)).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 3, "first attempt + retry_max=2 retries");
    }
}
