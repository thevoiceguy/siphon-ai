//! Lifecycle webhook sink — a thin [`WebhookSink`] adapter over
//! [`siphon_ai_http::RetryingPoster`].
//!
//! Each `emit` spawns the retrying POST so the per-call task never
//! blocks on network I/O. The retry budget, exponential backoff,
//! transient-status classification, and `Authorization` header live
//! in `siphon-ai-http`, shared with the CDR webhook sink — so a
//! change to delivery behavior (or future HMAC signing) happens once.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use siphon_ai_http::{BuildError, PostLog, RetryingPoster, DEFAULT_RETRY_MAX, DEFAULT_TIMEOUT_MS};

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
    DEFAULT_RETRY_MAX
}
fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

impl HttpSinkConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            auth_header: None,
            retry_max: DEFAULT_RETRY_MAX,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

/// HTTP lifecycle-webhook sink. Cheap to clone (the inner
/// [`RetryingPoster`] holds a reference-counted client).
#[derive(Debug, Clone)]
pub struct HttpSink {
    poster: RetryingPoster,
}

impl HttpSink {
    pub fn new(config: HttpSinkConfig) -> Result<Self, BuildError> {
        let poster = RetryingPoster::new(
            config.url,
            config.auth_header,
            config.retry_max,
            config.timeout_ms,
        )?;
        Ok(Self { poster })
    }
}

#[async_trait]
impl WebhookSink for HttpSink {
    async fn emit(&self, event: WebhookEvent) {
        // Spawn so the per-call task never blocks on network I/O.
        let poster = self.poster.clone();
        tokio::spawn(async move {
            poster
                .post(
                    &event,
                    PostLog {
                        kind: "lifecycle webhook",
                        call_id: event.call_id().unwrap_or("-"),
                        detail: Some(event.type_str()),
                    },
                )
                .await;
        });
    }
}
