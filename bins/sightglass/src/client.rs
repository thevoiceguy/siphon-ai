//! Per-node admin API client.
//!
//! One [`AdminClient`] per configured node, built once at startup.
//! Every method returns `Result<T, String>` — the error string is
//! operator-facing (it lands in the node's health row), so it favors
//! "connect error: …" over debug dumps.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use siphon_ai_admin_api_types::{CallsResponse, DrainStatus, ErrorBody, RegistrationsResponse};

use crate::config::Node;

pub struct AdminClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl AdminClient {
    pub fn new(node: &Node) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5));
        if let Some(pem) = &node.ca_pem {
            let cert = reqwest::Certificate::from_pem(pem)
                .with_context(|| format!("node {:?}: CA bundle is not valid PEM", node.name))?;
            builder = builder.add_root_certificate(cert);
        }
        Ok(Self {
            http: builder
                .build()
                .with_context(|| format!("node {:?}: building HTTP client", node.name))?,
            base: node.url.clone(),
            token: node.token.clone(),
        })
    }

    pub async fn calls(&self) -> Result<CallsResponse, String> {
        self.get_json("/admin/v1/calls").await
    }

    pub async fn registrations(&self) -> Result<RegistrationsResponse, String> {
        self.get_json("/admin/v1/registrations").await
    }

    pub async fn drain(&self) -> Result<DrainStatus, String> {
        self.get_json("/admin/v1/drain").await
    }

    /// Live quality snapshot for one active call. Kept as a loose
    /// `Value` until the stats shape joins `admin-api-types`
    /// (DESIGN_SIGHTGLASS.md §6.4, PR 5).
    pub async fn call_stats(&self, call_id: &str) -> Result<serde_json::Value, String> {
        self.get_json(&format!("/admin/v1/calls/{call_id}/stats"))
            .await
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let mut req = self.http.get(format!("{}{}", self.base, path));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(connect_error)?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(connect_error)?;
        if !status.is_success() {
            // Admin endpoints answer failures with `{"error": …}`;
            // fall back to the raw status for anything else (an
            // intercepting proxy, an HTML error page).
            let detail = serde_json::from_slice::<ErrorBody>(&body)
                .map(|e| e.error)
                .unwrap_or_else(|_| status.to_string());
            return Err(format!("{} {}: {}", status.as_u16(), path, detail));
        }
        serde_json::from_slice(&body).map_err(|e| format!("bad response from {path}: {e}"))
    }
}

/// Reqwest errors chain the useful part ("connection refused") a few
/// sources deep; surface the innermost message.
fn connect_error(e: reqwest::Error) -> String {
    let mut source: &dyn std::error::Error = &e;
    while let Some(inner) = source.source() {
        source = inner;
    }
    source.to_string()
}
