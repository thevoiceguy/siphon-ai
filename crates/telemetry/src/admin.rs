//! Admin HTTP endpoints.
//!
//! Operator surface for poking the daemon at runtime without
//! restarting it. All endpoints live under `/admin/*` on the same
//! observability HTTP listener as `/health` / `/ready` / `/metrics`.
//!
//! ## Endpoints
//!
//! | Method | Path                          | Purpose                              |
//! |--------|-------------------------------|--------------------------------------|
//! | GET    | `/admin/calls`                | List active per-call SIP Call-IDs    |
//! | POST   | `/admin/calls/:id/hangup`     | Force-shutdown a call by Call-ID     |
//! | GET    | `/admin/registrations`        | Snapshot of every `[[register]]` row |
//! | GET    | `/admin/log`                  | Current `tracing` filter directive   |
//! | PUT    | `/admin/log`                  | Replace the filter (body = directive)|
//! | POST   | `/admin/hep/test`             | Emit a probe HEP log packet          |
//!
//! ## Threat model
//!
//! These endpoints expose enough power to take calls down. They
//! MUST bind on a trusted address (loopback, k8s pod-internal, etc.)
//! per CLAUDE.md §12 / docs/DEV_PLAN.md §12.1. The daemon does NOT
//! authenticate them — front with an authenticating reverse proxy
//! if you expose them publicly.
//!
//! ## Dependency injection
//!
//! Each handler takes its dependencies (CallRegistry, RegistrationManager,
//! LogFilterHandle, HepTelemetry) by `Option<Arc<…>>` on the
//! [`AdminState`] — `None` makes the corresponding endpoint return
//! 503 instead of 500. That way a daemon configured without HEP
//! doesn't panic on `/admin/hep/test`; tests can plug in only the
//! pieces they care about.

use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::CONTENT_TYPE;
use hyper::{Response, StatusCode};
use serde::Serialize;
use serde_json::json;

use crate::log_filter::LogFilterHandle;
use crate::HepTelemetry;

/// Bundle of dependencies the admin handlers may need. Each is
/// optional so partially-configured deployments don't crash on
/// unrelated routes.
#[derive(Clone, Default)]
pub struct AdminState {
    pub call_registry: Option<AdminCallRegistry>,
    pub registration_snapshot: Option<RegistrationSnapshotFn>,
    pub log_filter: Option<LogFilterHandle>,
    pub hep: Option<Arc<HepTelemetry>>,
}

/// Minimal trait surface the admin endpoints need on the call
/// registry. Avoids a hard dep on `siphon-ai-core` here — the
/// runtime adapter passes a closure-wrapping object that delegates
/// to `CallRegistry`.
pub trait CallRegistryHandle: Send + Sync + 'static {
    fn snapshot_ids(&self) -> Vec<String>;
    /// Best-effort: returns `true` iff a call with that SIP Call-ID
    /// existed and was signalled to shut down.
    fn hangup(&self, sip_call_id: &str) -> bool;
}

/// Boxed clone-friendly handle the runtime constructs.
pub type AdminCallRegistry = Arc<dyn CallRegistryHandle>;

/// Closure type producing a fresh registration snapshot on each
/// admin request. Same indirection rationale as `CallRegistryHandle`
/// — keeps `siphon-ai-sip-glue` out of the telemetry crate's deps.
pub type RegistrationSnapshotFn = Arc<dyn Fn() -> Vec<RegistrationRow> + Send + Sync>;

/// One row of the `GET /admin/registrations` response. Mirrors
/// `sip_glue::RegistrationState` but defined here so telemetry
/// doesn't depend on the upstream crate.
#[derive(Debug, Clone, Serialize)]
pub struct RegistrationRow {
    pub name: String,
    pub server_addr: String,
    pub status: String,
    pub last_attempt_at: Option<String>,
    pub expires_at: Option<String>,
    pub last_error: Option<String>,
}

// ─── Dispatcher ────────────────────────────────────────────────────

/// Returns `Some(response)` when `path` is an admin route, `None`
/// otherwise. Lets the parent dispatcher fall through to its own
/// 404 logic when this isn't ours.
pub async fn dispatch(
    method: &hyper::Method,
    path: &str,
    body: Bytes,
    state: &AdminState,
) -> Option<Response<Full<Bytes>>> {
    if !path.starts_with("/admin") {
        return None;
    }

    let resp = match (method, path) {
        (&hyper::Method::GET, "/admin/calls") => list_calls(state),
        (&hyper::Method::GET, "/admin/registrations") => list_registrations(state),
        (&hyper::Method::GET, "/admin/log") => get_log_filter(state),
        (&hyper::Method::PUT, "/admin/log") => set_log_filter(state, &body),
        (&hyper::Method::POST, "/admin/hep/test") => hep_test(state),
        (m, p)
            if m == hyper::Method::POST
                && p.starts_with("/admin/calls/")
                && p.ends_with("/hangup") =>
        {
            // /admin/calls/:id/hangup — pull the id from the middle.
            let id = p
                .strip_prefix("/admin/calls/")
                .and_then(|s| s.strip_suffix("/hangup"))
                .unwrap_or("");
            hangup_call(state, id)
        }
        _ => not_found(),
    };
    Some(resp)
}

// ─── Handlers ──────────────────────────────────────────────────────

fn list_calls(state: &AdminState) -> Response<Full<Bytes>> {
    let Some(reg) = state.call_registry.as_ref() else {
        return service_unavailable("call registry not installed");
    };
    let ids = reg.snapshot_ids();
    json_response(StatusCode::OK, &json!({ "count": ids.len(), "calls": ids }))
}

fn hangup_call(state: &AdminState, sip_call_id: &str) -> Response<Full<Bytes>> {
    if sip_call_id.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "empty sip call_id" }),
        );
    }
    let Some(reg) = state.call_registry.as_ref() else {
        return service_unavailable("call registry not installed");
    };
    let hit = reg.hangup(sip_call_id);
    if hit {
        json_response(
            StatusCode::OK,
            &json!({ "shutdown_signalled": true, "sip_call_id": sip_call_id }),
        )
    } else {
        json_response(
            StatusCode::NOT_FOUND,
            &json!({ "shutdown_signalled": false, "sip_call_id": sip_call_id }),
        )
    }
}

fn list_registrations(state: &AdminState) -> Response<Full<Bytes>> {
    let Some(snapshot_fn) = state.registration_snapshot.as_ref() else {
        return service_unavailable("registration manager not installed");
    };
    let rows = snapshot_fn();
    json_response(
        StatusCode::OK,
        &json!({ "count": rows.len(), "registrations": rows }),
    )
}

fn get_log_filter(state: &AdminState) -> Response<Full<Bytes>> {
    let Some(handle) = state.log_filter.as_ref() else {
        return service_unavailable("log filter reload handle not installed");
    };
    json_response(StatusCode::OK, &json!({ "filter": handle.current() }))
}

fn set_log_filter(state: &AdminState, body: &Bytes) -> Response<Full<Bytes>> {
    let Some(handle) = state.log_filter.as_ref() else {
        return service_unavailable("log filter reload handle not installed");
    };

    // Accept either:
    //   * plaintext body: `siphon_ai=debug`
    //   * JSON body: `{"filter":"siphon_ai=debug"}`
    // Convention here mirrors `kubectl set image` style: prefer
    // simplicity over a strict content-type contract.
    let directive = parse_filter_body(body);
    let directive = match directive {
        Ok(s) => s,
        Err(e) => {
            return json_response(StatusCode::BAD_REQUEST, &json!({ "error": e }));
        }
    };

    match handle.set(&directive) {
        Ok(prev) => json_response(
            StatusCode::OK,
            &json!({ "filter": directive, "previous": prev }),
        ),
        Err(e) => json_response(StatusCode::BAD_REQUEST, &json!({ "error": e.to_string() })),
    }
}

fn parse_filter_body(body: &Bytes) -> Result<String, String> {
    if body.is_empty() {
        return Err("empty body; expected directive string or JSON {filter:...}".into());
    }
    // Try JSON first; fall through to plaintext.
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(s) = v.get("filter").and_then(|f| f.as_str()) {
            return Ok(s.trim().to_string());
        }
        // JSON parsed but no `filter` field — fall through to
        // raw-bytes interpretation in case the operator sent a
        // bare string with curly braces by accident.
    }
    let text = std::str::from_utf8(body)
        .map_err(|_| "body is not UTF-8".to_string())?
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("body is whitespace-only".into());
    }
    Ok(text)
}

fn hep_test(state: &AdminState) -> Response<Full<Bytes>> {
    let Some(hep) = state.hep.as_ref() else {
        return service_unavailable("HEP shipping not enabled");
    };
    hep.emit_log(
        &format!("siphon-ai admin probe from node={}", hep.node_id()),
        Some("admin-probe"),
        None,
    );
    json_response(
        StatusCode::OK,
        &json!({
            "emitted": true,
            "correlation_id": "admin-probe",
            "hint": "look for a chunk-type 100 log packet at the collector",
        }),
    )
}

// ─── Helpers ───────────────────────────────────────────────────────

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    let body = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder accepts the headers we set")
}

fn service_unavailable(reason: &str) -> Response<Full<Bytes>> {
    json_response(StatusCode::SERVICE_UNAVAILABLE, &json!({ "error": reason }))
}

fn not_found() -> Response<Full<Bytes>> {
    json_response(
        StatusCode::NOT_FOUND,
        &json!({ "error": "unknown admin route" }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct StubRegistry {
        ids: Mutex<Vec<String>>,
        hung_up: Mutex<Vec<String>>,
    }

    impl CallRegistryHandle for StubRegistry {
        fn snapshot_ids(&self) -> Vec<String> {
            self.ids.lock().unwrap().clone()
        }
        fn hangup(&self, sip_call_id: &str) -> bool {
            let mut ids = self.ids.lock().unwrap();
            if let Some(idx) = ids.iter().position(|x| x == sip_call_id) {
                ids.remove(idx);
                self.hung_up.lock().unwrap().push(sip_call_id.to_string());
                true
            } else {
                false
            }
        }
    }

    fn empty_state() -> AdminState {
        AdminState::default()
    }

    #[tokio::test]
    async fn unknown_admin_route_is_404() {
        let resp = dispatch(
            &hyper::Method::GET,
            "/admin/nope",
            Bytes::new(),
            &empty_state(),
        )
        .await
        .expect("admin dispatch");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn non_admin_path_passes_through() {
        let resp = dispatch(
            &hyper::Method::GET,
            "/metrics",
            Bytes::new(),
            &empty_state(),
        )
        .await;
        assert!(resp.is_none(), "non-admin path must fall through");
    }

    #[tokio::test]
    async fn list_calls_503s_when_registry_absent() {
        let resp = dispatch(
            &hyper::Method::GET,
            "/admin/calls",
            Bytes::new(),
            &empty_state(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn list_calls_returns_snapshot() {
        let stub = Arc::new(StubRegistry {
            ids: Mutex::new(vec!["abc@host".into(), "def@host".into()]),
            hung_up: Mutex::new(vec![]),
        });
        let state = AdminState {
            call_registry: Some(stub.clone() as AdminCallRegistry),
            ..AdminState::default()
        };
        let resp = dispatch(&hyper::Method::GET, "/admin/calls", Bytes::new(), &state)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn hangup_signals_existing_call() {
        let stub = Arc::new(StubRegistry {
            ids: Mutex::new(vec!["abc@host".into()]),
            hung_up: Mutex::new(vec![]),
        });
        let state = AdminState {
            call_registry: Some(stub.clone() as AdminCallRegistry),
            ..AdminState::default()
        };

        let resp = dispatch(
            &hyper::Method::POST,
            "/admin/calls/abc@host/hangup",
            Bytes::new(),
            &state,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(stub.hung_up.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn hangup_unknown_call_is_404() {
        let stub = Arc::new(StubRegistry {
            ids: Mutex::new(vec![]),
            hung_up: Mutex::new(vec![]),
        });
        let state = AdminState {
            call_registry: Some(stub as AdminCallRegistry),
            ..AdminState::default()
        };
        let resp = dispatch(
            &hyper::Method::POST,
            "/admin/calls/missing@host/hangup",
            Bytes::new(),
            &state,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn parses_filter_body_plaintext() {
        assert_eq!(
            parse_filter_body(&Bytes::from_static(b"siphon_ai=debug")).unwrap(),
            "siphon_ai=debug"
        );
    }

    #[test]
    fn parses_filter_body_json() {
        assert_eq!(
            parse_filter_body(&Bytes::from_static(b"{\"filter\":\"siphon_ai=info\"}")).unwrap(),
            "siphon_ai=info"
        );
    }

    #[test]
    fn parse_filter_body_rejects_empty() {
        assert!(parse_filter_body(&Bytes::new()).is_err());
    }

    #[test]
    fn parse_filter_body_rejects_whitespace_only() {
        assert!(parse_filter_body(&Bytes::from_static(b"   \n   ")).is_err());
    }
}
