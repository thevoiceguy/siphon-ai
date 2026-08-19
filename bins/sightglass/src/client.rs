//! Per-node admin API client.
//!
//! One [`AdminClient`] per configured node, built once at startup.
//! Every method returns `Result<T, String>` — the error string is
//! operator-facing (it lands in the node's health row), so it favors
//! "connect error: …" over debug dumps.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use siphon_ai_admin_api_types::{
    CallSipResponse, CallsResponse, ConferencesResponse, DrainStatus, ErrorBody, ErrorsResponse,
    LogFilterResponse, ParkedResponse, RecentCdrsResponse, RegistrationsResponse,
    SetLogFilterResponse, StatusResponse,
};

use crate::config::Node;
use crate::model::Role;

/// Why an action call failed: 403 is split out so the caller can
/// teach the node's role ceiling; everything else is operator-facing
/// text for a toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    Forbidden,
    Other(String),
}

/// Why a SIP-ladder fetch failed. The three named cases each render
/// differently in the pane and **none of them marks the node down** —
/// a node that answers everything else is healthy whether or not it
/// serves a ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LadderError {
    /// `501` — `[observability].sip_ring_size = 0` on that node.
    Disabled,
    /// `403` — the token is `readonly`; the ladder needs `operator`
    /// because it returns unredacted SIP.
    Forbidden,
    /// `404` — either an unknown call id, or a daemon older than the
    /// endpoint. Deliberately not split: the daemon's own text is
    /// shown, and the operator action is the same either way.
    Unavailable(String),
    Other(String),
}

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

    /// Recent-errors ring (0.49.0+ daemons). Callers treat a failure
    /// as "endpoint unavailable", not node-down — pre-0.49 daemons
    /// 404 this.
    pub async fn errors(&self) -> Result<ErrorsResponse, String> {
        self.get_json("/admin/v1/errors").await
    }

    /// Node summary (0.49.0+ daemons); same tolerance as `errors`.
    pub async fn status(&self) -> Result<StatusResponse, String> {
        self.get_json("/admin/v1/status").await
    }

    /// Conference rooms; `501` when `[conference]` is disabled —
    /// callers treat any failure as "no rooms".
    pub async fn conferences(&self) -> Result<ConferencesResponse, String> {
        self.get_json("/admin/v1/conferences").await
    }

    /// Parked calls; `501` when `[park]` is disabled — same tolerance.
    pub async fn parked(&self) -> Result<ParkedResponse, String> {
        self.get_json("/admin/v1/parked").await
    }

    /// Recent completed-call CDRs (0.49.0+ daemons); same tolerance
    /// as `status`/`errors`.
    pub async fn recent_cdrs(&self) -> Result<RecentCdrsResponse, String> {
        self.get_json("/admin/v1/cdrs/recent").await
    }

    /// Per-call SIP ladder (DESIGN_SIP_LADDER.md). Errors are typed
    /// rather than stringly so the pane can distinguish "capture is
    /// off on this node" from "this token may not read raw SIP" from
    /// "this daemon is too old" — three different things for an
    /// operator to do, and all of them are notes in the pane rather
    /// than node-down.
    pub async fn call_sip(&self, call_id: &str) -> Result<CallSipResponse, LadderError> {
        let path = format!("/admin/v1/calls/{call_id}/sip");
        let mut req = self.http.get(format!("{}{}", self.base, path));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| LadderError::Other(connect_error(e)))?;
        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|e| LadderError::Other(connect_error(e)))?;
        if status.is_success() {
            return serde_json::from_slice(&body)
                .map_err(|e| LadderError::Other(format!("bad response from {path}: {e}")));
        }
        let detail = serde_json::from_slice::<ErrorBody>(&body)
            .map(|e| e.error)
            .unwrap_or_else(|_| status.to_string());
        Err(match status.as_u16() {
            501 => LadderError::Disabled,
            403 => LadderError::Forbidden,
            404 => LadderError::Unavailable(detail),
            code => LadderError::Other(format!("{code}: {detail}")),
        })
    }

    /// Live quality snapshot for one active call. Kept as a loose
    /// `Value` until the stats shape joins `admin-api-types`
    /// (DESIGN_SIGHTGLASS.md §6.4, PR 5).
    pub async fn call_stats(&self, call_id: &str) -> Result<serde_json::Value, String> {
        self.get_json(&format!("/admin/v1/calls/{call_id}/stats"))
            .await
    }

    // ─── Actions (DESIGN_SIGHTGLASS.md §5) ─────────────────────────

    pub async fn hangup(&self, call_id: &str) -> Result<String, ApiError> {
        self.post(&format!("/admin/v1/calls/{call_id}/hangup"), None)
            .await
    }

    pub async fn park(&self, call_id: &str) -> Result<String, ApiError> {
        self.post(&format!("/admin/v1/calls/{call_id}/park"), None)
            .await
    }

    pub async fn retrieve(&self, call_id: &str, ws_url: Option<&str>) -> Result<String, ApiError> {
        let body = ws_url.map(|u| serde_json::json!({ "ws_url": u }));
        self.post(&format!("/admin/v1/calls/{call_id}/retrieve"), body)
            .await
    }

    pub async fn add_to_conference(
        &self,
        room_id: &str,
        call_id: &str,
    ) -> Result<String, ApiError> {
        self.post(
            &format!("/admin/v1/conferences/{room_id}/participants"),
            Some(serde_json::json!({ "call_id": call_id })),
        )
        .await
    }

    pub async fn originate(
        &self,
        to: &str,
        gateway: &str,
        ws_url: Option<&str>,
    ) -> Result<String, ApiError> {
        let mut body = serde_json::json!({ "to": to, "gateway": gateway });
        if let Some(u) = ws_url {
            body["ws_url"] = serde_json::Value::String(u.to_string());
        }
        self.post("/admin/v1/calls", Some(body)).await
    }

    /// Current tracing filter directive; readonly, polled with the
    /// snapshot.
    pub async fn log_filter(&self) -> Result<LogFilterResponse, String> {
        self.get_json("/admin/v1/log").await
    }

    /// Replace the tracing filter (admin role). Plaintext body — the
    /// daemon accepts either that or JSON.
    pub async fn set_log_filter(&self, directive: &str) -> Result<String, ApiError> {
        let mut req = self.http.put(format!("{}{}", self.base, "/admin/v1/log"));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req
            .body(directive.to_string())
            .send()
            .await
            .map_err(|e| ApiError::Other(connect_error(e)))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Other(connect_error(e)))?;
        if status.as_u16() == 403 {
            return Err(ApiError::Forbidden);
        }
        if status.is_success() {
            let detail = serde_json::from_slice::<SetLogFilterResponse>(&bytes)
                .map(|r| format!("now {:?} (was {:?})", r.filter, r.previous))
                .unwrap_or_else(|_| format!("accepted ({})", status.as_u16()));
            return Ok(detail);
        }
        let detail = serde_json::from_slice::<ErrorBody>(&bytes)
            .map(|e| e.error)
            .unwrap_or_else(|_| status.to_string());
        Err(ApiError::Other(detail))
    }

    /// Emit a HEP probe packet toward the collector (admin role).
    pub async fn hep_test(&self) -> Result<String, ApiError> {
        self.post("/admin/v1/hep/test", None).await
    }

    /// Start a graceful drain — the programmatic SIGTERM (admin role).
    pub async fn drain_start(&self) -> Result<String, ApiError> {
        self.post("/admin/v1/drain", None).await
    }

    pub async fn end_conference(&self, room_id: &str) -> Result<String, ApiError> {
        self.delete(&format!("/admin/v1/conferences/{room_id}"))
            .await
    }

    pub async fn remove_participant(
        &self,
        room_id: &str,
        call_id: &str,
    ) -> Result<String, ApiError> {
        self.delete(&format!(
            "/admin/v1/conferences/{room_id}/participants/{call_id}"
        ))
        .await
    }

    /// Learn this token's role without side effects
    /// (DESIGN_SIGHTGLASS.md §5). The 403 body carries no role, so we
    /// probe the RBAC gate itself — it runs *before* dispatch:
    ///
    /// 1. `POST /calls/<sentinel>/hangup`: 403 ⇒ `readonly` (below
    ///    operator); any dispatch status (404 for the sentinel) ⇒
    ///    ≥ operator, with no call touched.
    /// 2. `POST /calls` with an empty body: 403 ⇒ `operator`; a
    ///    validation 400 (or 501 outbound-disabled) ⇒ `admin`, with
    ///    nothing dialed.
    ///
    /// `None` on transport errors — the role stays unlearned and
    /// actions stay permissive.
    pub async fn probe_role(&self) -> Option<Role> {
        let status = self
            .post_status("/admin/v1/calls/sightglass-role-probe/hangup", None)
            .await?;
        if status == 403 {
            return Some(Role::ReadOnly);
        }
        let status = self
            .post_status("/admin/v1/calls", Some(serde_json::json!({})))
            .await?;
        Some(if status == 403 {
            Role::Operator
        } else {
            Role::Admin
        })
    }

    /// POST returning a short success line, with 403 split out so the
    /// caller can teach the node's role ceiling.
    async fn post(&self, path: &str, body: Option<serde_json::Value>) -> Result<String, ApiError> {
        let mut req = self.http.post(format!("{}{}", self.base, path));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        if let Some(body) = &body {
            req = req.json(body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Other(connect_error(e)))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Other(connect_error(e)))?;
        if status.as_u16() == 403 {
            return Err(ApiError::Forbidden);
        }
        if status.is_success() {
            return Ok(format!("accepted ({})", status.as_u16()));
        }
        let detail = serde_json::from_slice::<ErrorBody>(&bytes)
            .map(|e| e.error)
            .unwrap_or_else(|_| status.to_string());
        Err(ApiError::Other(detail))
    }

    /// DELETE twin of `post` (conference end / participant remove).
    async fn delete(&self, path: &str) -> Result<String, ApiError> {
        let mut req = self.http.delete(format!("{}{}", self.base, path));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Other(connect_error(e)))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Other(connect_error(e)))?;
        if status.as_u16() == 403 {
            return Err(ApiError::Forbidden);
        }
        if status.is_success() {
            return Ok(format!("accepted ({})", status.as_u16()));
        }
        let detail = serde_json::from_slice::<ErrorBody>(&bytes)
            .map(|e| e.error)
            .unwrap_or_else(|_| status.to_string());
        Err(ApiError::Other(detail))
    }

    /// Bare status probe used by `probe_role`; `None` on transport
    /// errors.
    async fn post_status(&self, path: &str, body: Option<serde_json::Value>) -> Option<u16> {
        let mut req = self.http.post(format!("{}{}", self.base, path));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        if let Some(body) = &body {
            req = req.json(body);
        }
        Some(req.send().await.ok()?.status().as_u16())
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
