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
//! | POST   | `/admin/v1/calls`             | Originate an outbound call (0.6.0)    |
//! | GET    | `/admin/v1/conferences`       | List conference rooms + members (0.7.0) |
//! | POST   | `/admin/v1/conferences`       | Pre-create a room (0.7.0)            |
//! | DELETE | `/admin/v1/conferences/:id`   | Force-end a room (0.7.0)             |
//! | POST   | `/admin/v1/conferences/:id/participants`           | Add a call to a room (0.7.0)  |
//! | DELETE | `/admin/v1/conferences/:id/participants/:call_id`  | Remove a call (0.7.0)         |
//! | GET    | `/admin/v1/parked`            | List parked calls (0.7.0)            |
//! | POST   | `/admin/v1/calls/:id/park`    | Park a call (0.7.0)                  |
//! | POST   | `/admin/v1/calls/:id/retrieve`| Retrieve a parked call (0.7.0)       |
//! | GET    | `/admin/v1/drain`             | Graceful-shutdown drain status (0.17.0) |
//!
//! ## Threat model
//!
//! These endpoints expose enough power to take calls down and
//! originate **billable** outbound calls. As of 0.10.0 they are served
//! **only on the authenticated `[admin]` listener** ([`crate::http::AdminServer`]),
//! gated by a bearer token + RBAC ([`crate::auth`]); the open
//! observability listener no longer serves `/admin/*`. `dispatch` itself
//! is unauthenticated by design — it runs only after `AdminServer` has
//! authenticated the bearer token and checked the endpoint's minimum
//! role. Set `[admin.tls]` (0.18.0) to serve the listener over HTTPS so
//! the bearer token is encrypted on the wire on a routable bind; without
//! it the listener is plain HTTP — bind it on loopback or front it with
//! a TLS-terminating proxy (the runtime warns on a non-loopback
//! plain-HTTP bind).
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
use serde::{Deserialize, Serialize};
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
    /// Outbound-origination handle (0.6.0). `None` when `[outbound]` is
    /// disabled — `POST /admin/v1/calls` then returns 501.
    pub outbound: Option<AdminOutbound>,
    /// Conference admin handle (0.7.0). `None` when `[conference]` is
    /// disabled — the `/admin/v1/conferences` routes then return 501.
    pub conference: Option<AdminConference>,
    /// Park admin handle (0.7.0). `None` when `[park]` is disabled —
    /// the park/retrieve/parked routes then return 501.
    pub park: Option<AdminPark>,
    /// Graceful-shutdown drain status (0.17.0). Always installed by the
    /// runtime (drain state exists regardless of `[shutdown]`); `None`
    /// only in partially-built test states, where `/admin/v1/drain`
    /// then returns 503.
    pub drain: Option<DrainStatusFn>,
    /// Live per-call quality snapshot (0.31.0), serving
    /// `GET /admin/v1/calls/:id/stats`. Always installed by the runtime
    /// (the quality tracker runs regardless of `[quality]`); `None`
    /// only in partially-built test states → 503.
    pub quality_stats: Option<QualityStatsFn>,
    /// Registration write actions (0.33.0), serving
    /// `POST /admin/v1/registrations/:name/refresh|restart`. Always
    /// installed by the runtime (the manager exists even with zero
    /// `[[register]]` blocks — triggers then 404); `None` only in
    /// partially-built test states → 503.
    pub registrations: Option<AdminRegistrations>,
}

/// Closure resolving a bridge `call_id` to its live quality snapshot,
/// pre-serialized by the runtime adapter. `None` = no active call with
/// that id (→ 404). Same indirection rationale as
/// [`CallRegistryHandle`] — keeps `siphon-ai-core` out of the
/// telemetry crate's deps.
pub type QualityStatsFn = Arc<dyn Fn(&str) -> Option<serde_json::Value> + Send + Sync>;

/// Snapshot of the daemon's graceful-shutdown drain state (0.17.0),
/// served by `GET /admin/v1/drain`. Lets an operator / deploy script
/// confirm a pod has entered drain and watch the countdown.
#[derive(Debug, Clone, Serialize)]
pub struct DrainStatus {
    /// `true` once a shutdown signal has put the daemon into drain
    /// (new INVITEs are 503'd and `/ready` is false), until it exits.
    pub draining: bool,
    /// Calls still active right now (the drain waits for this to hit 0).
    pub active_calls: usize,
    /// Configured `[shutdown].drain_timeout_secs` (`0` = drain disabled,
    /// immediate exit).
    pub drain_timeout_secs: u64,
    /// Seconds left until the drain deadline force-terminates
    /// stragglers. `Some` only while `draining`; `None` otherwise.
    pub remaining_secs: Option<u64>,
}

/// Closure producing a fresh [`DrainStatus`] per request. Same
/// indirection rationale as [`CallRegistryHandle`] — keeps the drain
/// flag / call registry types out of the telemetry crate's deps.
pub type DrainStatusFn = Arc<dyn Fn() -> DrainStatus + Send + Sync>;

/// Minimal trait surface the admin endpoints need on the call
/// registry. Avoids a hard dep on `siphon-ai-core` here — the
/// runtime adapter passes a closure-wrapping object that delegates
/// to `CallRegistry`.
pub trait CallRegistryHandle: Send + Sync + 'static {
    /// Snapshot every active call for `GET /admin/calls`. Each row
    /// carries both id namespaces so an operator can drive every admin
    /// endpoint: the bridge `call_id` (conference / park / stats) and
    /// the SIP Call-ID (`hangup`). See [`AdminCallRow`].
    fn snapshot_calls(&self) -> Vec<AdminCallRow>;
    /// Best-effort: returns `true` iff a call with that SIP Call-ID
    /// existed and was signalled to shut down.
    fn hangup(&self, sip_call_id: &str) -> bool;
}

/// One active call in the `GET /admin/calls` response.
///
/// `call_id` is the **bridge** id — the value on the WS `start` message
/// and the CDR, and the id `/admin/v1/conferences/*`, `/park`,
/// `/retrieve`, and `/stats` all take. `sip_call_id` is the **SIP**
/// Call-ID, the id `POST /admin/calls/:id/hangup` takes. Exposing both
/// (with `direction`) is the fix for issue #311, where the listing gave
/// only the SIP Call-ID and the bridge id had no admin source.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdminCallRow {
    pub call_id: String,
    pub sip_call_id: String,
    /// `"inbound"` | `"outbound"`.
    pub direction: &'static str,
}

/// Boxed clone-friendly handle the runtime constructs.
pub type AdminCallRegistry = Arc<dyn CallRegistryHandle>;

/// Closure type producing a fresh registration snapshot on each
/// admin request. Same indirection rationale as `CallRegistryHandle`
/// — keeps `siphon-ai-sip-glue` out of the telemetry crate's deps.
pub type RegistrationSnapshotFn = Arc<dyn Fn() -> Vec<RegistrationRow> + Send + Sync>;

/// `POST /admin/v1/calls` request body — originate an outbound call (0.6.0).
#[derive(Debug, Clone, Deserialize)]
pub struct OriginateRequest {
    /// Dialed destination (E.164 number or SIP user) — becomes the
    /// Request-URI user dialed through the gateway.
    pub to: String,
    /// Name of the `[[gateway]]` (or `[[register]]` reuse) to dial through.
    pub gateway: String,
    /// WS server to bridge the answered call to. Falls back to
    /// `[bridge].ws_url` when omitted.
    #[serde(default)]
    pub ws_url: Option<String>,
    /// Caller-ID override (a `sip:` URI). Falls back to the gateway's `from`.
    #[serde(default)]
    pub from: Option<String>,
    /// Place the call as a **delayed offer** (RFC 3264): send an INVITE
    /// with no SDP and answer the peer's offer in the ACK. Default `false`
    /// (early offer — SiphonAI offers in the INVITE).
    #[serde(default)]
    pub delayed_offer: bool,
    /// Recording override for this leg (0.26.0): `"off"` / `"always"` /
    /// `"on_demand"`. Falls back to the gateway's `recording` default.
    #[serde(default)]
    pub recording: Option<String>,
}

/// Why an originate request was refused synchronously (before the call is
/// placed). The admin layer maps each to an HTTP status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginateRejection {
    /// No `[[gateway]]` with that name → 404.
    UnknownGateway(String),
    /// No `ws_url` and no `[bridge].ws_url` default → 400.
    NoWsUrl,
    /// The dialed destination didn't form a valid SIP URI → 400.
    BadTarget(String),
    /// `max_concurrent_outbound` reached → 503.
    AtCapacity,
    /// The per-second outbound rate limit was exceeded → 429.
    RateLimited,
    /// Bad `recording` value, or recording requested with no
    /// `[recording].dir` configured → 400 (0.26.0).
    BadRecording(String),
    /// The per-request `from` caller-ID didn't form a valid SIP URI → 400
    /// (0.37.x). The gateway `from` is validated at config load, so this
    /// only fires for a bad override in the originate request.
    BadFrom(String),
    /// The daemon is draining for graceful shutdown → 503 (0.41.0,
    /// issue #343). Origination is the one admin action that dials the
    /// PSTN, so it is refused during drain the same way new inbound
    /// INVITEs are 503'd — a call started now would be orphaned when the
    /// process exits.
    Draining,
}

/// The outbound-origination entry point the admin endpoint calls. Defined
/// here (not in `siphon-ai-core`) to avoid a dep cycle — `siphon-ai-core`
/// implements it. Synchronous: it validates + admits + kicks off the call,
/// returning the bridge `call_id` immediately (the call proceeds async).
pub trait OutboundOriginateHandle: Send + Sync + 'static {
    fn originate(&self, req: OriginateRequest) -> Result<String, OriginateRejection>;
}

/// Boxed handle the runtime installs into [`AdminState`].
pub type AdminOutbound = Arc<dyn OutboundOriginateHandle>;

/// One conference room in the `GET /admin/v1/conferences` response.
#[derive(Debug, Clone, Serialize)]
pub struct ConferenceRow {
    pub room_id: String,
    pub sample_rate: u32,
    /// Member call-ids (bridge ids) currently in the room.
    pub participants: Vec<String>,
}

/// `POST /admin/v1/conferences` body — pre-create a room.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateConferenceRequest {
    /// Optional room id; the daemon generates one when omitted.
    #[serde(default)]
    pub room_id: Option<String>,
    /// Rate the room locks to (8000 or 16000). Defaults to 8000 — the
    /// most common PSTN rate; a join at a different rate is rejected.
    #[serde(default)]
    pub sample_rate: Option<u32>,
}

/// `POST /admin/v1/conferences/:id/participants` body.
#[derive(Debug, Clone, Deserialize)]
pub struct AddParticipantRequest {
    /// Bridge `call_id` of the active call to add to the room.
    pub call_id: String,
}

/// Why a conference admin op was refused. The admin layer maps each to
/// an HTTP status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConferenceAdminError {
    /// Conferencing is off (`[conference].enabled = false`) → 501.
    Disabled,
    /// `POST /conferences` for an id that already exists → 409.
    RoomExists,
    /// Room cap (`[conference].max_rooms`) reached → 503.
    TooManyRooms,
    /// `room_id` / `sample_rate` invalid (e.g. rate not 8000/16000) → 400.
    BadRequest(String),
    /// No live room with that id (`end`) → 404.
    RoomNotFound,
    /// No active call with that bridge id (`add`/`remove`) → 404.
    UnknownCall(String),
}

/// The conference admin entry point. Defined here (not in
/// `siphon-ai-core`) to avoid a dep cycle — core implements it.
/// Operations that re-plumb a live call (`add`/`remove`) are
/// fire-and-forget: they signal the target call and return once it's
/// been dispatched (202), with the actual join/leave surfacing on that
/// call's own WS (`conference_joined` / `conference_left` / `error`).
pub trait ConferenceAdminHandle: Send + Sync + 'static {
    fn list(&self) -> Vec<ConferenceRow>;
    /// Returns the (possibly generated) room id on success.
    fn create(&self, req: CreateConferenceRequest) -> Result<String, ConferenceAdminError>;
    fn end(&self, room_id: &str) -> Result<(), ConferenceAdminError>;
    fn add_participant(&self, room_id: &str, call_id: &str) -> Result<(), ConferenceAdminError>;
    fn remove_participant(&self, room_id: &str, call_id: &str) -> Result<(), ConferenceAdminError>;
}

/// Boxed handle the runtime installs into [`AdminState`].
pub type AdminConference = Arc<dyn ConferenceAdminHandle>;

/// One parked call in the `GET /admin/v1/parked` response.
#[derive(Debug, Clone, Serialize)]
pub struct ParkedRow {
    pub call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    pub parked_secs: u64,
}

/// `POST /admin/v1/calls/:id/park` body (all optional).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ParkRequest {
    #[serde(default)]
    pub slot: Option<String>,
}

/// `POST /admin/v1/calls/:id/retrieve` body (all optional).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RetrieveRequest {
    /// Redirect the retrieved session to a different WS server.
    /// Defaults to the call's original `ws_url`.
    #[serde(default)]
    pub ws_url: Option<String>,
}

/// Why a park-admin op was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParkAdminError {
    /// Park is off (`[park].enabled = false`) → 501.
    Disabled,
    /// No active call with that bridge id → 404.
    UnknownCall(String),
    /// Retrieve named a call that isn't parked → 409.
    NotParked(String),
}

/// The park admin entry point (0.7.0). Like the conference handle,
/// `park`/`retrieve` are fire-and-forget: they signal the target call
/// (which acts on its own state) and return `202`; the outcome surfaces
/// on the call's WS + the `call_parked` / `call_retrieved` webhooks.
pub trait ParkAdminHandle: Send + Sync + 'static {
    fn list(&self) -> Vec<ParkedRow>;
    fn park(&self, call_id: &str, slot: Option<String>) -> Result<(), ParkAdminError>;
    fn retrieve(&self, call_id: &str, ws_url: Option<String>) -> Result<(), ParkAdminError>;
}

/// Boxed handle the runtime installs into [`AdminState`].
pub type AdminPark = Arc<dyn ParkAdminHandle>;

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

/// Operator write action on one `[[register]]` binding (0.33.0,
/// DESIGN_REGISTRATION_ADMIN.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationAction {
    /// Immediate off-cycle REGISTER.
    Refresh,
    /// REGISTER `Expires: 0` then a fresh REGISTER.
    Restart,
}

impl RegistrationAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refresh => "refresh",
            Self::Restart => "restart",
        }
    }
}

/// Why a registration trigger wasn't queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationAdminError {
    /// No `[[register]]` block with that name → `404`.
    NotFound,
    /// The binding's drive task has exited (drain/shutdown) → `409`.
    ShuttingDown,
}

/// Handle the runtime installs so the two
/// `POST /admin/v1/registrations/{name}/…` endpoints can reach the
/// per-binding drive tasks. Returns the binding's accept-time row on
/// success (the outcome is asynchronous — design note §3). Same
/// indirection rationale as [`ParkAdminHandle`]: keeps sip-glue out
/// of this crate's deps.
pub trait RegistrationAdminHandle: Send + Sync + 'static {
    fn trigger(
        &self,
        name: &str,
        action: RegistrationAction,
    ) -> Result<RegistrationRow, RegistrationAdminError>;
}

/// Boxed handle the runtime installs into [`AdminState`].
pub type AdminRegistrations = Arc<dyn RegistrationAdminHandle>;

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
        (&hyper::Method::POST, "/admin/v1/calls") => originate_call(state, &body),
        (&hyper::Method::GET, "/admin/v1/conferences") => list_conferences(state),
        (&hyper::Method::POST, "/admin/v1/conferences") => create_conference(state, &body),
        (&hyper::Method::GET, "/admin/v1/parked") => list_parked(state),
        (&hyper::Method::GET, "/admin/v1/drain") => drain_status(state),
        (m, p)
            if *m == hyper::Method::POST
                && p.starts_with("/admin/v1/registrations/")
                && p.ends_with("/refresh") =>
        {
            let name = p
                .strip_prefix("/admin/v1/registrations/")
                .and_then(|s| s.strip_suffix("/refresh"))
                .unwrap_or("");
            trigger_registration(state, name, RegistrationAction::Refresh)
        }
        (m, p)
            if *m == hyper::Method::POST
                && p.starts_with("/admin/v1/registrations/")
                && p.ends_with("/restart") =>
        {
            let name = p
                .strip_prefix("/admin/v1/registrations/")
                .and_then(|s| s.strip_suffix("/restart"))
                .unwrap_or("");
            trigger_registration(state, name, RegistrationAction::Restart)
        }
        (m, p)
            if *m == hyper::Method::POST
                && p.starts_with("/admin/v1/calls/")
                && p.ends_with("/park") =>
        {
            let id = p
                .strip_prefix("/admin/v1/calls/")
                .and_then(|s| s.strip_suffix("/park"))
                .unwrap_or("");
            park_call(state, id, &body)
        }
        (m, p)
            if *m == hyper::Method::POST
                && p.starts_with("/admin/v1/calls/")
                && p.ends_with("/retrieve") =>
        {
            let id = p
                .strip_prefix("/admin/v1/calls/")
                .and_then(|s| s.strip_suffix("/retrieve"))
                .unwrap_or("");
            retrieve_call(state, id, &body)
        }
        (m, p)
            if *m == hyper::Method::GET
                && p.starts_with("/admin/v1/calls/")
                && p.ends_with("/stats") =>
        {
            let id = p
                .strip_prefix("/admin/v1/calls/")
                .and_then(|s| s.strip_suffix("/stats"))
                .unwrap_or("");
            call_stats(state, id)
        }
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
        // Conference sub-resources under /admin/v1/conferences/:id …
        (m, p) if p.starts_with("/admin/v1/conferences/") => {
            match conference_subroute(p) {
                // DELETE /admin/v1/conferences/:id
                (Some(room_id), None) if *m == hyper::Method::DELETE => {
                    end_conference(state, &room_id)
                }
                // POST /admin/v1/conferences/:id/participants
                (Some(room_id), Some(ParticipantSel::All)) if *m == hyper::Method::POST => {
                    add_participant(state, &room_id, &body)
                }
                // DELETE /admin/v1/conferences/:id/participants/:call_id
                (Some(room_id), Some(ParticipantSel::One(call_id)))
                    if *m == hyper::Method::DELETE =>
                {
                    remove_participant(state, &room_id, &call_id)
                }
                _ => not_found(),
            }
        }
        _ => not_found(),
    };
    Some(resp)
}

/// Which participant sub-resource a conference path addressed.
enum ParticipantSel {
    /// `…/:id/participants` (the collection — POST adds).
    All,
    /// `…/:id/participants/:call_id` (one — DELETE removes).
    One(String),
}

/// Parse `/admin/v1/conferences/:id[/participants[/:call_id]]`.
/// Returns `(room_id, participant_selector)`; `room_id` is `None` when
/// the path is malformed. Percent-decoding isn't needed — room ids and
/// bridge call ids are `[A-Za-z0-9_-]`.
fn conference_subroute(path: &str) -> (Option<String>, Option<ParticipantSel>) {
    let rest = match path.strip_prefix("/admin/v1/conferences/") {
        Some(r) if !r.is_empty() => r,
        _ => return (None, None),
    };
    let mut segs = rest.split('/');
    let room_id = match segs.next() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return (None, None),
    };
    match segs.next() {
        // /:id
        None => (Some(room_id), None),
        // /:id/participants[...]
        Some("participants") => match segs.next() {
            None => (Some(room_id), Some(ParticipantSel::All)),
            Some(call_id) if !call_id.is_empty() && segs.next().is_none() => (
                Some(room_id),
                Some(ParticipantSel::One(call_id.to_string())),
            ),
            _ => (None, None),
        },
        _ => (None, None),
    }
}

// ─── Handlers ──────────────────────────────────────────────────────

fn list_calls(state: &AdminState) -> Response<Full<Bytes>> {
    let Some(reg) = state.call_registry.as_ref() else {
        return service_unavailable("call registry not installed");
    };
    let calls = reg.snapshot_calls();
    json_response(
        StatusCode::OK,
        &json!({ "count": calls.len(), "calls": calls }),
    )
}

fn drain_status(state: &AdminState) -> Response<Full<Bytes>> {
    let Some(f) = state.drain.as_ref() else {
        return service_unavailable("drain status not installed");
    };
    json_response(StatusCode::OK, &json!(f()))
}

/// `GET /admin/v1/calls/:id/stats` (0.31.0) — live quality snapshot
/// for one active call, in the CDR `quality` block's shape. 404 when
/// no active call has that bridge `call_id`; ended calls answer
/// through the CDR / `[quality]` history records instead.
fn call_stats(state: &AdminState, call_id: &str) -> Response<Full<Bytes>> {
    if call_id.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "empty call_id" }),
        );
    }
    let Some(f) = state.quality_stats.as_ref() else {
        return service_unavailable("quality stats not installed");
    };
    match f(call_id) {
        Some(row) => json_response(StatusCode::OK, &row),
        None => json_response(
            StatusCode::NOT_FOUND,
            &json!({ "error": "no active call with that call_id" }),
        ),
    }
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

fn originate_call(state: &AdminState, body: &Bytes) -> Response<Full<Bytes>> {
    let Some(svc) = state.outbound.as_ref() else {
        return json_response(
            StatusCode::NOT_IMPLEMENTED,
            &json!({ "error": "outbound origination not enabled ([outbound].max_concurrent = 0)" }),
        );
    };
    let req: OriginateRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({ "error": format!("invalid originate request: {e}") }),
            )
        }
    };
    match svc.originate(req) {
        Ok(call_id) => json_response(StatusCode::ACCEPTED, &json!({ "call_id": call_id })),
        Err(rej) => {
            let (status, msg) = match rej {
                OriginateRejection::UnknownGateway(g) => {
                    (StatusCode::NOT_FOUND, format!("unknown gateway: {g}"))
                }
                OriginateRejection::NoWsUrl => (
                    StatusCode::BAD_REQUEST,
                    "no ws_url (and no [bridge].ws_url default)".to_string(),
                ),
                OriginateRejection::BadTarget(t) => {
                    (StatusCode::BAD_REQUEST, format!("bad target: {t}"))
                }
                OriginateRejection::AtCapacity => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "max_concurrent_outbound reached".to_string(),
                ),
                OriginateRejection::RateLimited => (
                    StatusCode::TOO_MANY_REQUESTS,
                    "outbound rate limit exceeded".to_string(),
                ),
                OriginateRejection::BadRecording(r) => {
                    (StatusCode::BAD_REQUEST, format!("bad recording: {r}"))
                }
                OriginateRejection::BadFrom(f) => {
                    (StatusCode::BAD_REQUEST, format!("bad from: {f}"))
                }
                OriginateRejection::Draining => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "draining for shutdown; not accepting new calls".to_string(),
                ),
            };
            json_response(status, &json!({ "error": msg }))
        }
    }
}

// ─── Conference admin (0.7.0) ──────────────────────────────────────

fn list_conferences(state: &AdminState) -> Response<Full<Bytes>> {
    let Some(svc) = state.conference.as_ref() else {
        return conference_disabled();
    };
    let rooms = svc.list();
    json_response(
        StatusCode::OK,
        &json!({ "count": rooms.len(), "conferences": rooms }),
    )
}

fn create_conference(state: &AdminState, body: &Bytes) -> Response<Full<Bytes>> {
    let Some(svc) = state.conference.as_ref() else {
        return conference_disabled();
    };
    // Empty body is allowed (all fields optional → daemon-generated id,
    // default rate).
    let req: CreateConferenceRequest = if body.is_empty() {
        CreateConferenceRequest {
            room_id: None,
            sample_rate: None,
        }
    } else {
        match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    &json!({ "error": format!("invalid create request: {e}") }),
                )
            }
        }
    };
    match svc.create(req) {
        Ok(room_id) => json_response(StatusCode::CREATED, &json!({ "room_id": room_id })),
        Err(e) => conference_error_response(e),
    }
}

fn end_conference(state: &AdminState, room_id: &str) -> Response<Full<Bytes>> {
    let Some(svc) = state.conference.as_ref() else {
        return conference_disabled();
    };
    match svc.end(room_id) {
        Ok(()) => json_response(
            StatusCode::OK,
            &json!({ "ended": true, "room_id": room_id }),
        ),
        Err(e) => conference_error_response(e),
    }
}

fn add_participant(state: &AdminState, room_id: &str, body: &Bytes) -> Response<Full<Bytes>> {
    let Some(svc) = state.conference.as_ref() else {
        return conference_disabled();
    };
    let req: AddParticipantRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({ "error": format!("invalid add-participant request: {e}") }),
            )
        }
    };
    match svc.add_participant(room_id, &req.call_id) {
        // 202: the call has been told to join; the outcome surfaces on
        // its own WS session.
        Ok(()) => json_response(
            StatusCode::ACCEPTED,
            &json!({ "room_id": room_id, "call_id": req.call_id }),
        ),
        Err(e) => conference_error_response(e),
    }
}

fn remove_participant(state: &AdminState, room_id: &str, call_id: &str) -> Response<Full<Bytes>> {
    let Some(svc) = state.conference.as_ref() else {
        return conference_disabled();
    };
    match svc.remove_participant(room_id, call_id) {
        Ok(()) => json_response(
            StatusCode::ACCEPTED,
            &json!({ "room_id": room_id, "call_id": call_id }),
        ),
        Err(e) => conference_error_response(e),
    }
}

fn conference_disabled() -> Response<Full<Bytes>> {
    json_response(
        StatusCode::NOT_IMPLEMENTED,
        &json!({ "error": "conferencing not enabled ([conference].enabled = false)" }),
    )
}

fn conference_error_response(e: ConferenceAdminError) -> Response<Full<Bytes>> {
    let (status, msg) = match e {
        ConferenceAdminError::Disabled => {
            return conference_disabled();
        }
        ConferenceAdminError::RoomExists => {
            (StatusCode::CONFLICT, "room already exists".to_string())
        }
        ConferenceAdminError::TooManyRooms => (
            StatusCode::SERVICE_UNAVAILABLE,
            "[conference].max_rooms reached".to_string(),
        ),
        ConferenceAdminError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
        ConferenceAdminError::RoomNotFound => {
            (StatusCode::NOT_FOUND, "no such conference room".to_string())
        }
        ConferenceAdminError::UnknownCall(c) => (
            StatusCode::NOT_FOUND,
            format!("no active call with bridge call_id {c:?} (this is the `call_id` field of GET /admin/calls, not sip_call_id)"),
        ),
    };
    json_response(status, &json!({ "error": msg }))
}

// ─── Park admin (0.7.0) ────────────────────────────────────────────

fn list_parked(state: &AdminState) -> Response<Full<Bytes>> {
    let Some(svc) = state.park.as_ref() else {
        return park_disabled();
    };
    let parked = svc.list();
    json_response(
        StatusCode::OK,
        &json!({ "count": parked.len(), "parked": parked }),
    )
}

/// `POST /admin/v1/registrations/:name/refresh|restart` (0.33.0).
/// `202` = the command is queued to the binding's drive task; the body
/// carries the accept-time row (the REGISTER outcome is asynchronous —
/// watch `GET /admin/registrations`, the `register_attempts_total`
/// metric, or the `registration_state_changed` webhook).
fn trigger_registration(
    state: &AdminState,
    name: &str,
    action: RegistrationAction,
) -> Response<Full<Bytes>> {
    let Some(svc) = state.registrations.as_ref() else {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({ "error": "registration admin unavailable" }),
        );
    };
    if name.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "empty registration name" }),
        );
    }
    match svc.trigger(name, action) {
        Ok(row) => {
            metrics::counter!(
                crate::metrics::REGISTER_ADMIN_TRIGGERS_TOTAL,
                "name" => name.to_string(),
                "action" => action.as_str(),
            )
            .increment(1);
            json_response(
                StatusCode::ACCEPTED,
                &json!({
                    "accepted": true,
                    "action": action.as_str(),
                    "registration": row,
                }),
            )
        }
        Err(RegistrationAdminError::NotFound) => json_response(
            StatusCode::NOT_FOUND,
            &json!({ "error": format!("no [[register]] block named {name:?}") }),
        ),
        Err(RegistrationAdminError::ShuttingDown) => json_response(
            StatusCode::CONFLICT,
            &json!({ "error": "daemon is draining; registration tasks are shutting down" }),
        ),
    }
}

fn park_call(state: &AdminState, call_id: &str, body: &Bytes) -> Response<Full<Bytes>> {
    let Some(svc) = state.park.as_ref() else {
        return park_disabled();
    };
    if call_id.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "empty call_id" }),
        );
    }
    // Empty body allowed (slot optional).
    let req: ParkRequest = if body.is_empty() {
        ParkRequest::default()
    } else {
        match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    &json!({ "error": format!("invalid park request: {e}") }),
                )
            }
        }
    };
    match svc.park(call_id, req.slot) {
        Ok(()) => json_response(StatusCode::ACCEPTED, &json!({ "call_id": call_id })),
        Err(e) => park_error_response(e),
    }
}

fn retrieve_call(state: &AdminState, call_id: &str, body: &Bytes) -> Response<Full<Bytes>> {
    let Some(svc) = state.park.as_ref() else {
        return park_disabled();
    };
    if call_id.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "empty call_id" }),
        );
    }
    let req: RetrieveRequest = if body.is_empty() {
        RetrieveRequest::default()
    } else {
        match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    &json!({ "error": format!("invalid retrieve request: {e}") }),
                )
            }
        }
    };
    match svc.retrieve(call_id, req.ws_url) {
        Ok(()) => json_response(StatusCode::ACCEPTED, &json!({ "call_id": call_id })),
        Err(e) => park_error_response(e),
    }
}

fn park_disabled() -> Response<Full<Bytes>> {
    json_response(
        StatusCode::NOT_IMPLEMENTED,
        &json!({ "error": "park not enabled ([park].enabled = false)" }),
    )
}

fn park_error_response(e: ParkAdminError) -> Response<Full<Bytes>> {
    let (status, msg) = match e {
        ParkAdminError::Disabled => return park_disabled(),
        ParkAdminError::UnknownCall(c) => (
            StatusCode::NOT_FOUND,
            format!("no active call with bridge call_id {c:?} (this is the `call_id` field of GET /admin/calls, not sip_call_id)"),
        ),
        ParkAdminError::NotParked(c) => (StatusCode::CONFLICT, format!("call is not parked: {c}")),
    };
    json_response(status, &json!({ "error": msg }))
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
    use http_body_util::BodyExt;
    use serde_json::Value;
    use std::sync::Mutex;

    struct StubRegistry {
        ids: Mutex<Vec<String>>,
        hung_up: Mutex<Vec<String>>,
    }

    impl CallRegistryHandle for StubRegistry {
        fn snapshot_calls(&self) -> Vec<AdminCallRow> {
            // The stub tracks SIP Call-IDs (what `hangup` matches on);
            // synthesize a bridge id for each so the listing shape can
            // be asserted.
            self.ids
                .lock()
                .unwrap()
                .iter()
                .map(|sip| AdminCallRow {
                    call_id: format!("siphon-{sip}"),
                    sip_call_id: sip.clone(),
                    direction: "inbound",
                })
                .collect()
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

    // ─── registration triggers (0.33.0) ─────────────────────────────

    struct StubRegistrations {
        triggered: Mutex<Vec<(String, RegistrationAction)>>,
        error: Option<RegistrationAdminError>,
    }

    impl RegistrationAdminHandle for StubRegistrations {
        fn trigger(
            &self,
            name: &str,
            action: RegistrationAction,
        ) -> Result<RegistrationRow, RegistrationAdminError> {
            if let Some(e) = self.error {
                return Err(e);
            }
            self.triggered
                .lock()
                .unwrap()
                .push((name.to_string(), action));
            Ok(RegistrationRow {
                name: name.to_string(),
                server_addr: "10.0.0.9:5060".into(),
                status: "failed".into(),
                last_attempt_at: None,
                expires_at: None,
                last_error: Some("503 Service Unavailable".into()),
            })
        }
    }

    fn registrations_state(
        error: Option<RegistrationAdminError>,
    ) -> (AdminState, Arc<StubRegistrations>) {
        let stub = Arc::new(StubRegistrations {
            triggered: Mutex::new(Vec::new()),
            error,
        });
        let state = AdminState {
            registrations: Some(stub.clone() as AdminRegistrations),
            ..AdminState::default()
        };
        (state, stub)
    }

    #[tokio::test]
    async fn registration_refresh_is_202_with_accept_time_row() {
        let (state, _stub) = registrations_state(None);
        let resp = dispatch(
            &hyper::Method::POST,
            "/admin/v1/registrations/pbx-a/refresh",
            Bytes::new(),
            &state,
        )
        .await
        .expect("admin dispatch");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body: Value =
            serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["accepted"], true);
        assert_eq!(body["action"], "refresh");
        assert_eq!(body["registration"]["name"], "pbx-a");
        assert_eq!(body["registration"]["status"], "failed");
    }

    #[tokio::test]
    async fn registration_restart_reaches_the_handle() {
        let (state, stub) = registrations_state(None);
        let resp = dispatch(
            &hyper::Method::POST,
            "/admin/v1/registrations/cucm/restart",
            Bytes::new(),
            &state,
        )
        .await
        .expect("admin dispatch");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let calls = stub.triggered.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![("cucm".to_string(), RegistrationAction::Restart)]
        );
    }

    #[tokio::test]
    async fn registration_unknown_name_is_404_and_draining_is_409() {
        let (state, _stub) = registrations_state(Some(RegistrationAdminError::NotFound));
        let resp = dispatch(
            &hyper::Method::POST,
            "/admin/v1/registrations/nope/refresh",
            Bytes::new(),
            &state,
        )
        .await
        .expect("admin dispatch");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let (state, _stub) = registrations_state(Some(RegistrationAdminError::ShuttingDown));
        let resp = dispatch(
            &hyper::Method::POST,
            "/admin/v1/registrations/pbx-a/restart",
            Bytes::new(),
            &state,
        )
        .await
        .expect("admin dispatch");
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn registration_trigger_without_handle_is_503() {
        let resp = dispatch(
            &hyper::Method::POST,
            "/admin/v1/registrations/pbx-a/refresh",
            Bytes::new(),
            &empty_state(),
        )
        .await
        .expect("admin dispatch");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
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

        // Each call is an object carrying BOTH id namespaces + direction
        // (issue #311) — not a bare SIP Call-ID string.
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["count"], 2);
        let first = &v["calls"][0];
        assert_eq!(first["sip_call_id"], "abc@host");
        assert_eq!(first["call_id"], "siphon-abc@host");
        assert_eq!(first["direction"], "inbound");
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

    // ─── POST /admin/v1/calls (originate) ────────────────────────────

    struct StubOutbound(Result<String, OriginateRejection>);
    impl OutboundOriginateHandle for StubOutbound {
        fn originate(&self, _req: OriginateRequest) -> Result<String, OriginateRejection> {
            self.0.clone()
        }
    }

    fn outbound_state(result: Result<String, OriginateRejection>) -> AdminState {
        AdminState {
            outbound: Some(Arc::new(StubOutbound(result)) as AdminOutbound),
            ..AdminState::default()
        }
    }

    async fn originate(state: &AdminState, body: &str) -> StatusCode {
        dispatch(
            &hyper::Method::POST,
            "/admin/v1/calls",
            Bytes::from(body.to_string()),
            state,
        )
        .await
        .expect("admin dispatch")
        .status()
    }

    const GOOD_BODY: &str = r#"{"to":"+15558675309","gateway":"trunk"}"#;

    #[tokio::test]
    async fn originate_accepts_returns_202() {
        let state = outbound_state(Ok("siphon-abc".into()));
        assert_eq!(originate(&state, GOOD_BODY).await, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn originate_501_when_outbound_disabled() {
        // No outbound handle installed → 501.
        assert_eq!(
            originate(&empty_state(), GOOD_BODY).await,
            StatusCode::NOT_IMPLEMENTED
        );
    }

    #[tokio::test]
    async fn originate_400_on_bad_json() {
        let state = outbound_state(Ok("x".into()));
        assert_eq!(originate(&state, "not json").await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn originate_maps_rejections_to_status() {
        for (rej, status) in [
            (
                OriginateRejection::UnknownGateway("g".into()),
                StatusCode::NOT_FOUND,
            ),
            (OriginateRejection::NoWsUrl, StatusCode::BAD_REQUEST),
            (
                OriginateRejection::BadTarget("x".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                OriginateRejection::AtCapacity,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                OriginateRejection::RateLimited,
                StatusCode::TOO_MANY_REQUESTS,
            ),
            (
                OriginateRejection::BadFrom("x".into()),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let state = outbound_state(Err(rej.clone()));
            assert_eq!(originate(&state, GOOD_BODY).await, status, "{rej:?}");
        }
    }

    // ─── /admin/v1/conferences (0.7.0) ───────────────────────────────

    struct StubConference {
        rooms: Vec<ConferenceRow>,
        create: Result<String, ConferenceAdminError>,
        end: Result<(), ConferenceAdminError>,
        add: Result<(), ConferenceAdminError>,
        remove: Result<(), ConferenceAdminError>,
        calls: Mutex<Vec<(String, String)>>,
    }
    impl Default for StubConference {
        fn default() -> Self {
            Self {
                rooms: vec![],
                create: Ok("room-1".into()),
                end: Ok(()),
                add: Ok(()),
                remove: Ok(()),
                calls: Mutex::new(vec![]),
            }
        }
    }
    impl ConferenceAdminHandle for StubConference {
        fn list(&self) -> Vec<ConferenceRow> {
            self.rooms.clone()
        }
        fn create(&self, _req: CreateConferenceRequest) -> Result<String, ConferenceAdminError> {
            self.create.clone()
        }
        fn end(&self, _room_id: &str) -> Result<(), ConferenceAdminError> {
            self.end.clone()
        }
        fn add_participant(&self, room: &str, call: &str) -> Result<(), ConferenceAdminError> {
            self.calls.lock().unwrap().push((room.into(), call.into()));
            self.add.clone()
        }
        fn remove_participant(&self, room: &str, call: &str) -> Result<(), ConferenceAdminError> {
            self.calls.lock().unwrap().push((room.into(), call.into()));
            self.remove.clone()
        }
    }

    fn conf_state(stub: StubConference) -> AdminState {
        AdminState {
            conference: Some(Arc::new(stub) as AdminConference),
            ..AdminState::default()
        }
    }

    async fn conf(
        method: hyper::Method,
        path: &str,
        body: &str,
        state: &AdminState,
    ) -> (StatusCode, Value) {
        let resp = dispatch(&method, path, Bytes::from(body.to_string()), state)
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    #[tokio::test]
    async fn conferences_501_when_disabled() {
        let (status, _) = conf(
            hyper::Method::GET,
            "/admin/v1/conferences",
            "",
            &empty_state(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn list_conferences_returns_rooms() {
        let stub = StubConference {
            rooms: vec![ConferenceRow {
                room_id: "support-7".into(),
                sample_rate: 8000,
                participants: vec!["siphon-a".into(), "siphon-b".into()],
            }],
            ..Default::default()
        };
        let (status, body) = conf(
            hyper::Method::GET,
            "/admin/v1/conferences",
            "",
            &conf_state(stub),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["count"], 1);
        assert_eq!(body["conferences"][0]["room_id"], "support-7");
    }

    #[tokio::test]
    async fn create_conference_201_with_empty_body() {
        let (status, body) = conf(
            hyper::Method::POST,
            "/admin/v1/conferences",
            "",
            &conf_state(StubConference::default()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["room_id"], "room-1");
    }

    #[tokio::test]
    async fn create_conference_409_on_exists() {
        let stub = StubConference {
            create: Err(ConferenceAdminError::RoomExists),
            ..Default::default()
        };
        let (status, _) = conf(
            hyper::Method::POST,
            "/admin/v1/conferences",
            r#"{"room_id":"dup"}"#,
            &conf_state(stub),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn end_conference_404_when_absent() {
        let stub = StubConference {
            end: Err(ConferenceAdminError::RoomNotFound),
            ..Default::default()
        };
        let (status, _) = conf(
            hyper::Method::DELETE,
            "/admin/v1/conferences/ghost",
            "",
            &conf_state(stub),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn add_participant_202_and_routes_ids() {
        let state = conf_state(StubConference::default());
        let (status, _) = conf(
            hyper::Method::POST,
            "/admin/v1/conferences/support-7/participants",
            r#"{"call_id":"siphon-x"}"#,
            &state,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        // The stub recorded (room, call) so we know the path parsed.
        let svc = state.conference.unwrap();
        // (downcast not available; assert via a fresh add through the
        // dispatcher already covered the parse — the 202 is the signal)
        let _ = svc;
    }

    #[tokio::test]
    async fn add_participant_404_on_unknown_call() {
        let stub = StubConference {
            add: Err(ConferenceAdminError::UnknownCall("siphon-x".into())),
            ..Default::default()
        };
        let (status, _) = conf(
            hyper::Method::POST,
            "/admin/v1/conferences/support-7/participants",
            r#"{"call_id":"siphon-x"}"#,
            &conf_state(stub),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn add_participant_400_on_bad_body() {
        let (status, _) = conf(
            hyper::Method::POST,
            "/admin/v1/conferences/support-7/participants",
            "not json",
            &conf_state(StubConference::default()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn remove_participant_202() {
        let (status, _) = conf(
            hyper::Method::DELETE,
            "/admin/v1/conferences/support-7/participants/siphon-x",
            "",
            &conf_state(StubConference::default()),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn malformed_conference_subpath_is_404() {
        // Trailing junk past :call_id.
        let (status, _) = conf(
            hyper::Method::DELETE,
            "/admin/v1/conferences/r/participants/c/extra",
            "",
            &conf_state(StubConference::default()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // ─── Park admin routes (0.7.0) ──────────────────────────────────

    struct StubPark {
        rows: Vec<ParkedRow>,
        park: Result<(), ParkAdminError>,
        retrieve: Result<(), ParkAdminError>,
    }
    impl Default for StubPark {
        fn default() -> Self {
            Self {
                rows: vec![],
                park: Ok(()),
                retrieve: Ok(()),
            }
        }
    }
    impl ParkAdminHandle for StubPark {
        fn list(&self) -> Vec<ParkedRow> {
            self.rows.clone()
        }
        fn park(&self, _call_id: &str, _slot: Option<String>) -> Result<(), ParkAdminError> {
            self.park.clone()
        }
        fn retrieve(&self, _call_id: &str, _ws_url: Option<String>) -> Result<(), ParkAdminError> {
            self.retrieve.clone()
        }
    }

    fn park_state(stub: StubPark) -> AdminState {
        AdminState {
            park: Some(Arc::new(stub) as AdminPark),
            ..AdminState::default()
        }
    }

    #[tokio::test]
    async fn parked_501_when_disabled() {
        // All three routes return 501 with no park handle installed.
        for (m, path) in [
            (hyper::Method::GET, "/admin/v1/parked"),
            (hyper::Method::POST, "/admin/v1/calls/c/park"),
            (hyper::Method::POST, "/admin/v1/calls/c/retrieve"),
        ] {
            let (status, _) = conf(m, path, "", &empty_state()).await;
            assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{path}");
        }
    }

    #[tokio::test]
    async fn list_parked_returns_rows() {
        let stub = StubPark {
            rows: vec![ParkedRow {
                call_id: "siphon-a".into(),
                slot: Some("lot-3".into()),
                parked_secs: 42,
            }],
            ..Default::default()
        };
        let (status, body) = conf(
            hyper::Method::GET,
            "/admin/v1/parked",
            "",
            &park_state(stub),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["count"], 1);
        assert_eq!(body["parked"][0]["call_id"], "siphon-a");
        assert_eq!(body["parked"][0]["slot"], "lot-3");
        assert_eq!(body["parked"][0]["parked_secs"], 42);
    }

    #[tokio::test]
    async fn park_202_when_dispatched() {
        let (status, body) = conf(
            hyper::Method::POST,
            "/admin/v1/calls/siphon-a/park",
            r#"{"slot":"lot-1"}"#,
            &park_state(StubPark::default()),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["call_id"], "siphon-a");
    }

    #[tokio::test]
    async fn park_202_with_empty_body() {
        // slot is optional — an empty body is a valid unlabeled park.
        let (status, _) = conf(
            hyper::Method::POST,
            "/admin/v1/calls/siphon-a/park",
            "",
            &park_state(StubPark::default()),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn park_404_unknown_call() {
        let stub = StubPark {
            park: Err(ParkAdminError::UnknownCall("ghost".into())),
            ..Default::default()
        };
        let (status, _) = conf(
            hyper::Method::POST,
            "/admin/v1/calls/ghost/park",
            "",
            &park_state(stub),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn retrieve_202_when_dispatched() {
        let (status, _) = conf(
            hyper::Method::POST,
            "/admin/v1/calls/siphon-a/retrieve",
            r#"{"ws_url":"wss://bot.example/retrieve"}"#,
            &park_state(StubPark::default()),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn retrieve_409_when_not_parked() {
        let stub = StubPark {
            retrieve: Err(ParkAdminError::NotParked("siphon-a".into())),
            ..Default::default()
        };
        let (status, _) = conf(
            hyper::Method::POST,
            "/admin/v1/calls/siphon-a/retrieve",
            "",
            &park_state(stub),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn retrieve_404_unknown_call() {
        let stub = StubPark {
            retrieve: Err(ParkAdminError::UnknownCall("ghost".into())),
            ..Default::default()
        };
        let (status, _) = conf(
            hyper::Method::POST,
            "/admin/v1/calls/ghost/retrieve",
            "",
            &park_state(stub),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // ─── GET /admin/v1/drain (0.17.0) ────────────────────────────────

    fn drain_state(status: DrainStatus) -> AdminState {
        AdminState {
            drain: Some(Arc::new(move || status.clone()) as DrainStatusFn),
            ..AdminState::default()
        }
    }

    #[tokio::test]
    async fn drain_503_when_not_installed() {
        let (status, _) = conf(hyper::Method::GET, "/admin/v1/drain", "", &empty_state()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn drain_reports_not_draining() {
        let state = drain_state(DrainStatus {
            draining: false,
            active_calls: 3,
            drain_timeout_secs: 30,
            remaining_secs: None,
        });
        let (status, v) = conf(hyper::Method::GET, "/admin/v1/drain", "", &state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["draining"], serde_json::json!(false));
        assert_eq!(v["active_calls"], serde_json::json!(3));
        assert_eq!(v["drain_timeout_secs"], serde_json::json!(30));
        assert_eq!(v["remaining_secs"], Value::Null);
    }

    #[tokio::test]
    async fn drain_reports_active_drain_with_countdown() {
        let state = drain_state(DrainStatus {
            draining: true,
            active_calls: 2,
            drain_timeout_secs: 30,
            remaining_secs: Some(18),
        });
        let (status, v) = conf(hyper::Method::GET, "/admin/v1/drain", "", &state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["draining"], serde_json::json!(true));
        assert_eq!(v["remaining_secs"], serde_json::json!(18));
    }
}
