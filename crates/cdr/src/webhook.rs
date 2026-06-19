//! CDR webhook sink — a thin [`CdrSink`] adapter over
//! [`siphon_ai_http::RetryingPoster`].
//!
//! Each `emit` spawns the retrying POST onto a background task so the
//! per-call cleanup that calls us never blocks on network I/O
//! (CLAUDE.md §4.7). The retry budget, exponential backoff,
//! transient-status classification, and `Authorization` header all
//! live in `siphon-ai-http`, shared with the lifecycle webhook sink.
//!
//! Each delivery carries an `X-SiphonAI-Event-Id` (+ `Idempotency-Key`)
//! and, when a `secret` is set, an `X-SiphonAI-Signature` — both
//! provided by `siphon-ai-http`.
//!
//! ## What we don't do (yet)
//!
//! - **No persistent queue.** A daemon restart loses any in-flight
//!   retries. Operators who need durability point the file sink at the
//!   same record stream and tail it on the receiver side. (A disk
//!   spool is the next chunk of the delivery-durability theme.)

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use siphon_ai_http::{
    BuildError, PostLog, PosterConfig, RetryingPoster, SinkKind, DEFAULT_RETRY_MAX,
    DEFAULT_TIMEOUT_MS,
};

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
    /// Optional HMAC-SHA256 signing secret. When set, every record
    /// POST carries `X-SiphonAI-Signature: t=<unix>,v1=<hex>`. `None`
    /// ⇒ unsigned.
    #[serde(default)]
    pub secret: Option<String>,
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
    DEFAULT_RETRY_MAX
}
fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

impl WebhookSinkConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            auth_header: None,
            secret: None,
            retry_max: DEFAULT_RETRY_MAX,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

/// HTTP webhook sink. One per configured URL; cheap to clone (the
/// inner [`RetryingPoster`] holds a reference-counted client).
#[derive(Debug, Clone)]
pub struct WebhookSink {
    poster: RetryingPoster,
}

impl WebhookSink {
    /// Build the sink. The `RetryingPoster` creates its
    /// `reqwest::Client` once so connection pooling spans calls.
    pub fn new(config: WebhookSinkConfig) -> Result<Self, BuildError> {
        let poster = RetryingPoster::new(PosterConfig {
            url: config.url,
            auth_header: config.auth_header,
            retry_max: config.retry_max,
            timeout_ms: config.timeout_ms,
            secret: config.secret,
            sink: SinkKind::Cdr,
        })?;
        Ok(Self { poster })
    }
}

#[async_trait]
impl CdrSink for WebhookSink {
    async fn emit(&self, record: CdrRecord) {
        // Run the POST + retry loop on a spawned task so the per-call
        // cleanup (which calls us) doesn't block on network I/O. The
        // sink contract (CLAUDE.md §4.7) says emission is best-effort
        // — losing a record on a daemon restart is acceptable,
        // blocking call teardown is not.
        let poster = self.poster.clone();
        tokio::spawn(async move {
            poster
                .post(
                    &record,
                    PostLog {
                        kind: "CDR webhook",
                        call_id: record.call_id.as_str(),
                        detail: None,
                    },
                )
                .await;
        });
    }
}
