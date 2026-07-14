//! Signed quality-record webhook sink — a thin [`QualitySink`] adapter
//! over [`siphon_ai_http::RetryingPoster`], the same transport the
//! lifecycle-webhook, CDR, and audit sinks use.
//!
//! Every delivery is HMAC-SHA256 signed (`X-SiphonAI-Signature`) when a
//! secret is configured, carries an `X-SiphonAI-Event-Id` for
//! idempotency, retries transient failures with exponential backoff,
//! and — with a `spool_dir` — survives restarts. All of that lives in
//! `siphon-ai-http`; this file only maps config and stamps the log
//! label.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use siphon_ai_http::{
    BuildError, PostLog, PosterConfig, RetryingPoster, SinkKind, DEFAULT_RETRY_MAX,
    DEFAULT_TIMEOUT_MS,
};

use crate::record::QualityRecord;
use crate::sink::QualitySink;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpSinkConfig {
    pub url: String,
    /// Optional `Authorization` header value, sent verbatim.
    #[serde(default)]
    pub auth_header: Option<String>,
    /// Optional HMAC-SHA256 signing secret. When set, every delivery
    /// carries `X-SiphonAI-Signature: t=<unix>,v1=<hex>`. `None` ⇒
    /// unsigned.
    #[serde(default)]
    pub secret: Option<String>,
    /// Optional durable spool directory. When set, a delivery that
    /// exhausts the in-memory retry budget is persisted here and
    /// re-attempted by a background worker that survives restarts.
    #[serde(default)]
    pub spool_dir: Option<String>,
    /// Maximum retries on transient failure. `0` = post once.
    #[serde(default = "default_retry_max")]
    pub retry_max: u32,
    /// Per-attempt timeout, in milliseconds.
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
            secret: None,
            spool_dir: None,
            retry_max: DEFAULT_RETRY_MAX,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

/// HTTP quality-record sink. Cheap to clone (the inner
/// [`RetryingPoster`] holds a reference-counted client).
#[derive(Debug, Clone)]
pub struct HttpSink {
    poster: RetryingPoster,
}

impl HttpSink {
    pub fn new(config: HttpSinkConfig) -> Result<Self, BuildError> {
        let poster = RetryingPoster::new(PosterConfig {
            url: config.url,
            auth_header: config.auth_header,
            retry_max: config.retry_max,
            timeout_ms: config.timeout_ms,
            secret: config.secret,
            spool_dir: config.spool_dir.map(PathBuf::from),
            sink: SinkKind::Quality,
        })?;
        // Start the drain worker once, on the single poster we build
        // (clones made per-emit don't re-spawn it).
        poster.spawn_drainer();
        Ok(Self { poster })
    }
}

#[async_trait]
impl QualitySink for HttpSink {
    async fn emit(&self, record: QualityRecord) {
        // Spawn so the emitting task never blocks on network I/O.
        let poster = self.poster.clone();
        tokio::spawn(async move {
            poster
                .post(
                    &record,
                    PostLog {
                        kind: "quality record",
                        call_id: record.call_id.as_str(),
                        detail: Some(record.kind.as_str()),
                    },
                )
                .await;
        });
    }
}
