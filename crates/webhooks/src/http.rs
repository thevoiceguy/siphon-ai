//! HTTP POST sink with retry + exponential backoff.
//!
//! Mirrors `siphon-ai-cdr`'s `WebhookSink` shape: each `emit`
//! spawns the POST so the per-call task never blocks on network
//! I/O. Transient failures (5xx, 408, 429) retry; 4xx never
//! retries; failures after `retry_max` are logged and dropped.
//!
//! ## Why a separate impl from `siphon-ai-cdr`'s webhook?
//!
//! The shape is similar but the lifetimes are different — CDR
//! webhooks fire one record per call, lifecycle webhooks fire two
//! (or more, when ws_failure / etc. land). Keeping them apart
//! avoids an awkward "is this a CDR or an event?" generic. If the
//! shared logic ever grows, factoring out an internal
//! `http-retry-post` helper crate would be the right move; v1
//! stays simple.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::event::WebhookEvent;
use crate::sink::WebhookSink;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpSinkConfig {
    pub url: String,
    /// Optional `Authorization` header value. Sent verbatim — the
    /// caller decides between `Bearer ...`, basic, or other schemes.
    #[serde(default)]
    pub auth_header: Option<String>,
    /// Maximum retries on transient failure. `0` = post once.
    #[serde(default = "default_retry_max")]
    pub retry_max: u32,
    /// Per-attempt timeout. Total worst-case time for one event is
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

impl HttpSinkConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            auth_header: None,
            retry_max: default_retry_max(),
            timeout_ms: default_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HttpSink {
    client: Client,
    config: HttpSinkConfig,
}

impl HttpSink {
    pub fn new(config: HttpSinkConfig) -> reqwest::Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()?;
        Ok(Self { client, config })
    }
}

#[async_trait]
impl WebhookSink for HttpSink {
    async fn emit(&self, event: WebhookEvent) {
        // Spawn so the per-call task never blocks on network I/O.
        let client = self.client.clone();
        let config = self.config.clone();
        tokio::spawn(async move {
            post_with_retry(&client, &config, event).await;
        });
    }
}

async fn post_with_retry(client: &Client, config: &HttpSinkConfig, event: WebhookEvent) {
    let mut attempt: u32 = 0;
    loop {
        let mut req = client.post(&config.url).json(&event);
        if let Some(auth) = config.auth_header.as_deref() {
            req = req.header("Authorization", auth);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    debug!(
                        url = %config.url,
                        call_id = event.call_id().unwrap_or("-"),
                        event_type = event.type_str(),
                        status = %status,
                        "lifecycle webhook delivered"
                    );
                    return;
                }
                if !is_transient(status) {
                    warn!(
                        url = %config.url,
                        call_id = event.call_id().unwrap_or("-"),
                        event_type = event.type_str(),
                        status = %status,
                        "lifecycle webhook rejected (4xx); not retrying"
                    );
                    return;
                }
                warn!(
                    url = %config.url,
                    call_id = event.call_id().unwrap_or("-"),
                    event_type = event.type_str(),
                    status = %status,
                    attempt = attempt + 1,
                    "lifecycle webhook transient failure"
                );
            }
            Err(e) => {
                warn!(
                    url = %config.url,
                    call_id = event.call_id().unwrap_or("-"),
                    event_type = event.type_str(),
                    error = %e,
                    attempt = attempt + 1,
                    "lifecycle webhook request failed"
                );
            }
        }
        if attempt >= config.retry_max {
            warn!(
                url = %config.url,
                call_id = event.call_id().unwrap_or("-"),
                event_type = event.type_str(),
                attempts = attempt + 1,
                "lifecycle webhook giving up; event dropped"
            );
            return;
        }
        tokio::time::sleep(backoff_for(attempt)).await;
        attempt += 1;
    }
}

fn is_transient(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
}

/// Exponential backoff: 100ms, 200ms, 400ms, 800ms, capped at 5s.
/// Same shape as the CDR webhook for predictable operator behaviour.
fn backoff_for(attempt: u32) -> Duration {
    let ms = 100u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_millis(ms.min(5000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CallEndEvent, CallStartEvent, WEBHOOK_VERSION};
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

    fn start_event(call_id: &str) -> WebhookEvent {
        WebhookEvent::CallStart(CallStartEvent {
            version: WEBHOOK_VERSION,
            call_id: call_id.into(),
            sip_call_id: format!("{call_id}@pbx"),
            timestamp: chrono::Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 0).unwrap(),
            from: "+1".into(),
            to: "5000".into(),
            route: "default".into(),
            ws_url: "wss://x/y".into(),
        })
    }

    fn end_event(call_id: &str) -> WebhookEvent {
        WebhookEvent::CallEnd(CallEndEvent {
            version: WEBHOOK_VERSION,
            call_id: call_id.into(),
            sip_call_id: format!("{call_id}@pbx"),
            timestamp: chrono::Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 1).unwrap(),
            from: "+1".into(),
            to: "5000".into(),
            route: "default".into(),
            ws_url: "wss://x/y".into(),
            duration_ms: 1000,
            termination_cause: "server_hangup".into(),
        })
    }

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
        format!("http://{addr}/webhooks")
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
        Ok(Response::builder()
            .status(status_code)
            .body(String::new())
            .unwrap())
    }

    #[tokio::test]
    async fn happy_path_posts_event_with_auth_header() {
        let rec = Recorder::new(vec![200]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let sink = HttpSink::new(HttpSinkConfig {
            url,
            auth_header: Some("Bearer slack-token".into()),
            retry_max: 0,
            timeout_ms: 1000,
        })
        .unwrap();

        sink.emit(start_event("c-1")).await;
        tokio::time::sleep(Duration::from_millis(150)).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["type"], serde_json::json!("call_start"));
        assert_eq!(payloads[0]["call_id"], serde_json::json!("c-1"));

        let auth = rec.auth_headers.lock().await;
        assert_eq!(auth[0].as_deref(), Some("Bearer slack-token"));
    }

    #[tokio::test]
    async fn transient_5xx_retries_then_succeeds() {
        let rec = Recorder::new(vec![503, 503, 200]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let sink = HttpSink::new(HttpSinkConfig {
            url,
            auth_header: None,
            retry_max: 3,
            timeout_ms: 1000,
        })
        .unwrap();

        sink.emit(end_event("c-retry")).await;
        // Backoff: 100 + 200 = 300ms before attempt 3; pad.
        tokio::time::sleep(Duration::from_millis(800)).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 3);
        assert_eq!(payloads[2]["type"], serde_json::json!("call_end"));
    }

    #[tokio::test]
    async fn permanent_4xx_does_not_retry() {
        let rec = Recorder::new(vec![401, 200]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let sink = HttpSink::new(HttpSinkConfig {
            url,
            auth_header: None,
            retry_max: 5,
            timeout_ms: 1000,
        })
        .unwrap();

        sink.emit(start_event("c-401")).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        let payloads = rec.payloads.lock().await;
        assert_eq!(payloads.len(), 1);
    }

    #[tokio::test]
    async fn retry_exhaustion_drops_event_without_panicking() {
        let rec = Recorder::new(vec![500, 500, 500, 500, 500]);
        let url = spawn_server(Arc::clone(&rec)).await;
        let sink = HttpSink::new(HttpSinkConfig {
            url,
            auth_header: None,
            retry_max: 2,
            timeout_ms: 500,
        })
        .unwrap();

        sink.emit(start_event("c-give-up")).await;
        tokio::time::sleep(Duration::from_millis(900)).await;

        let payloads = rec.payloads.lock().await;
        // initial attempt + retry_max=2 retries
        assert_eq!(payloads.len(), 3);
    }
}
