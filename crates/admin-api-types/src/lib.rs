//! Admin API wire types.
//!
//! The request/response JSON shapes served under `/admin/v1/*`,
//! extracted from `siphon-ai-telemetry` so the daemon (which
//! serializes them) and admin *clients* — sightglass first
//! (DESIGN_SIGHTGLASS.md §2) — share one definition and cannot
//! drift. Everything here derives both `Serialize` and `Deserialize`
//! for that reason, even though each side only exercises one
//! direction.
//!
//! **These shapes are a public wire contract.** The admin API is ops
//! tooling surface: existing fields don't change shape or meaning;
//! additions are new optional fields. Field *order* in each struct is
//! kept as-serialized today — the wire snapshot tests at the bottom
//! lock the exact JSON.
//!
//! Server-side traits, error enums, and dispatch stay in
//! `siphon-ai-telemetry` (they are implementation, not wire).

use serde::{Deserialize, Serialize};

// ─── GET /admin/v1/calls ───────────────────────────────────────────

/// One active call in the `GET /admin/v1/calls` response.
///
/// `call_id` is the **bridge** id — the value on the WS `start` message
/// and the CDR, and the id every `/admin/v1/calls/:id/…` route
/// (`/hangup`, `/park`, `/retrieve`, `/stats`) and `/admin/v1/conferences/*`
/// take. `sip_call_id` is the **SIP** Call-ID, the id the deprecated
/// `POST /admin/calls/:id/hangup` alias takes. Exposing both
/// (with `direction`) is the fix for issue #311, where the listing gave
/// only the SIP Call-ID and the bridge id had no admin source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminCallRow {
    pub call_id: String,
    pub sip_call_id: String,
    /// `"inbound"` | `"outbound"`.
    pub direction: String,
}

/// `GET /admin/v1/calls` response envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallsResponse {
    pub count: usize,
    pub calls: Vec<AdminCallRow>,
}

// ─── GET /admin/v1/registrations ───────────────────────────────────

/// One row of the `GET /admin/v1/registrations` response. Mirrors
/// `sip_glue::RegistrationState` but defined here so neither
/// telemetry nor clients depend on the upstream crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationRow {
    pub name: String,
    pub server_addr: String,
    pub status: String,
    pub last_attempt_at: Option<String>,
    pub expires_at: Option<String>,
    pub last_error: Option<String>,
}

/// `GET /admin/v1/registrations` response envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationsResponse {
    pub count: usize,
    pub registrations: Vec<RegistrationRow>,
}

// ─── GET /admin/v1/conferences ─────────────────────────────────────

/// One conference room in the `GET /admin/v1/conferences` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConferenceRow {
    pub room_id: String,
    pub sample_rate: u32,
    /// Member call-ids (bridge ids) currently in the room.
    pub participants: Vec<String>,
}

/// `GET /admin/v1/conferences` response envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConferencesResponse {
    pub count: usize,
    pub conferences: Vec<ConferenceRow>,
}

// ─── GET /admin/v1/parked ──────────────────────────────────────────

/// One parked call in the `GET /admin/v1/parked` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedRow {
    pub call_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub slot: Option<String>,
    pub parked_secs: u64,
}

/// `GET /admin/v1/parked` response envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedResponse {
    pub count: usize,
    pub parked: Vec<ParkedRow>,
}

// ─── GET /admin/v1/cdrs/recent ─────────────────────────────────────

/// `GET /admin/v1/cdrs/recent` (0.49.0, DESIGN_SIGHTGLASS.md §6.3):
/// the last N completed calls' CDRs, newest first. Entries are
/// **verbatim serialized CDR records** — the CDR schema
/// (`crates/cdr/src/schema.rs`, versioned via its `version` field) is
/// the one schema, deliberately not re-modeled here; consumers read
/// the fields the CDR docs promise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentCdrsResponse {
    pub count: usize,
    /// Newest first.
    pub cdrs: Vec<serde_json::Value>,
}

// ─── GET /admin/v1/status ──────────────────────────────────────────

/// Registration rollup inside [`StatusResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationsSummary {
    /// Bindings currently in the `registered` state.
    pub registered: usize,
    /// Configured `[[register]]` bindings.
    pub total: usize,
}

/// `GET /admin/v1/status` (0.49.0, DESIGN_SIGHTGLASS.md §6.2): a
/// one-request JSON summary of the node for dashboards and fleet
/// tooling — the live snapshot only. Cumulative counters
/// (`calls_total`, HEP delivery) stay on `/metrics`; collector
/// *health* is `siphon_ai_hep_*` territory too — `hep_enabled` here
/// just says whether HEP shipping is configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    /// Daemon version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// Seconds since the daemon finished starting.
    pub uptime_secs: u64,
    pub active_calls: usize,
    pub registrations: RegistrationsSummary,
    /// Graceful-shutdown drain in progress (`/admin/v1/drain` has the
    /// countdown detail).
    pub draining: bool,
    /// HEP/Homer shipping configured on this node.
    pub hep_enabled: bool,
}

// ─── /admin/v1/log ─────────────────────────────────────────────────

/// `GET /admin/v1/log` response — the active `tracing` filter
/// directive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogFilterResponse {
    pub filter: String,
}

/// `PUT /admin/v1/log` success response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetLogFilterResponse {
    /// The directive now in effect.
    pub filter: String,
    /// What it replaced.
    pub previous: String,
}

// ─── POST /admin/v1/drain ──────────────────────────────────────────

/// `POST /admin/v1/drain` (0.49.0, DESIGN_SIGHTGLASS.md §6.5)
/// response. The programmatic twin of SIGTERM: the daemon enters the
/// same graceful drain (`GET /admin/v1/drain` has the countdown).
/// Idempotent — a repeat while draining changes nothing (and, unlike
/// a second SIGTERM, never forces immediate teardown).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainStartResponse {
    /// Always `true` on a 202 — the drain signal was delivered.
    pub drain_signalled: bool,
    /// The daemon was already draining when this request landed.
    pub already_draining: bool,
}

// ─── GET /admin/v1/errors ──────────────────────────────────────────

/// One captured `warn!`/`error!` event in the `GET /admin/v1/errors`
/// response (0.49.0, DESIGN_SIGHTGLASS.md §6.1). Captured by the
/// daemon's error-ring tracing layer; newest-first in the listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEntry {
    /// Capture time, milliseconds since the Unix epoch.
    pub ts_ms: u64,
    /// `"warn"` | `"error"`.
    pub level: String,
    /// The `tracing` target (module path unless overridden).
    pub target: String,
    /// The event's message, with any structured fields appended as
    /// `key=value`.
    pub message: String,
    /// The `call_id` span field nearest the event, when the event
    /// fired inside a per-call span.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub call_id: Option<String>,
}

/// `GET /admin/v1/errors` response envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorsResponse {
    pub count: usize,
    /// Newest first.
    pub errors: Vec<ErrorEntry>,
}

// ─── GET /admin/v1/calls/{id}/sip ──────────────────────────────────

/// One captured SIP message in the `GET /admin/v1/calls/{id}/sip`
/// response (DESIGN_SIP_LADDER.md). The message verbatim — this
/// endpoint is deliberately unredacted and gated at `operator`
/// (design §2), because a partial message invites the wrong
/// conclusion about what actually went over the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SipMessageEntry {
    /// Capture time, milliseconds since the Unix epoch. Matches
    /// [`ErrorEntry::ts_ms`] rather than the design note's RFC3339
    /// sketch — the ring endpoints share one time encoding, and a
    /// client rendering a ladder wants to subtract them anyway.
    pub ts_ms: u64,
    /// `"in"` | `"out"` | `"unknown"`, derived by matching `src`/`dst`
    /// against the node's own addresses (`[node].public_address`, a
    /// non-wildcard SIP bind IP, and an unspecified IP — siphon-rs
    /// stamps our end `0.0.0.0` on a wildcard bind). When both ends
    /// look local (loopback), the SIP bind port breaks the tie; port
    /// is never consulted first, because peers send *from* 5060 too.
    /// `"unknown"` when neither end is recognisably this node —
    /// guessing would be worse than saying so, and `src`/`dst` are
    /// kept so the derivation stays checkable.
    pub direction: String,
    pub src: String,
    pub dst: String,
    /// The raw message, lossy-UTF-8 decoded. SIP is text; a message
    /// that isn't valid UTF-8 is malformed and worth seeing as-is.
    pub payload: String,
}

/// `GET /admin/v1/calls/{id}/sip` response envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSipResponse {
    /// The internal call id the request addressed.
    pub call_id: String,
    /// The SIP `Call-ID` the ring is keyed by.
    pub sip_call_id: String,
    /// `true` when the per-call cap evicted older messages — so a
    /// client says "truncated" instead of silently showing a
    /// ladder that starts mid-dialog.
    pub truncated: bool,
    pub count: usize,
    /// Oldest first: a ladder reads down the page in wire order,
    /// unlike the newest-first `errors`/`cdrs` listings.
    pub messages: Vec<SipMessageEntry>,
}

// ─── GET /admin/v1/drain ───────────────────────────────────────────

/// Snapshot of the daemon's graceful-shutdown drain state (0.17.0),
/// served by `GET /admin/v1/drain`. Lets an operator / deploy script
/// confirm a pod has entered drain and watch the countdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

// ─── Request bodies (server deserializes, clients serialize) ───────

/// `POST /admin/v1/calls` request body — originate an outbound call (0.6.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// `POST /admin/v1/conferences` body — pre-create a room.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddParticipantRequest {
    /// Bridge `call_id` of the active call to add to the room.
    pub call_id: String,
}

/// `POST /admin/v1/calls/:id/park` body (all optional).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParkRequest {
    #[serde(default)]
    pub slot: Option<String>,
}

/// `POST /admin/v1/calls/:id/retrieve` body (all optional).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetrieveRequest {
    /// Redirect the retrieved session to a different WS server.
    /// Defaults to the call's original `ws_url`.
    #[serde(default)]
    pub ws_url: Option<String>,
}

// ─── Error envelope ────────────────────────────────────────────────

/// The `{"error": "…"}` body every admin endpoint answers non-2xx
/// statuses with. Defined here for clients; the server builds these
/// inline at each rejection site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The wire snapshots below are the *contract*: they assert the
    /// exact JSON the daemon serves today (pre-extraction `json!`
    /// output, byte-for-byte modulo key order which serde_json
    /// preserves from struct declaration order). A failure here means
    /// the admin API wire format changed — that needs a deliberate
    /// decision, not a type tweak.
    #[test]
    fn calls_response_wire_shape() {
        let resp = CallsResponse {
            count: 1,
            calls: vec![AdminCallRow {
                call_id: "siphon-abc".into(),
                sip_call_id: "abc@host".into(),
                direction: "inbound".into(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            json!({
                "count": 1,
                "calls": [{
                    "call_id": "siphon-abc",
                    "sip_call_id": "abc@host",
                    "direction": "inbound",
                }],
            })
        );
    }

    #[test]
    fn registrations_response_wire_shape() {
        let resp = RegistrationsResponse {
            count: 1,
            registrations: vec![RegistrationRow {
                name: "pbx-a".into(),
                server_addr: "10.0.0.9:5060".into(),
                status: "registered".into(),
                last_attempt_at: Some("2026-08-17T00:00:00Z".into()),
                expires_at: None,
                last_error: None,
            }],
        };
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            json!({
                "count": 1,
                "registrations": [{
                    "name": "pbx-a",
                    "server_addr": "10.0.0.9:5060",
                    "status": "registered",
                    "last_attempt_at": "2026-08-17T00:00:00Z",
                    "expires_at": null,
                    "last_error": null,
                }],
            })
        );
    }

    #[test]
    fn conferences_response_wire_shape() {
        let resp = ConferencesResponse {
            count: 1,
            conferences: vec![ConferenceRow {
                room_id: "room-1".into(),
                sample_rate: 8000,
                participants: vec!["siphon-abc".into()],
            }],
        };
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            json!({
                "count": 1,
                "conferences": [{
                    "room_id": "room-1",
                    "sample_rate": 8000,
                    "participants": ["siphon-abc"],
                }],
            })
        );
    }

    #[test]
    fn parked_row_omits_absent_slot() {
        let resp = ParkedResponse {
            count: 1,
            parked: vec![ParkedRow {
                call_id: "siphon-abc".into(),
                slot: None,
                parked_secs: 12,
            }],
        };
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            json!({
                "count": 1,
                "parked": [{ "call_id": "siphon-abc", "parked_secs": 12 }],
            })
        );
    }

    #[test]
    fn drain_start_and_log_filter_wire_shapes() {
        assert_eq!(
            serde_json::to_value(DrainStartResponse {
                drain_signalled: true,
                already_draining: false,
            })
            .unwrap(),
            json!({ "drain_signalled": true, "already_draining": false })
        );
        assert_eq!(
            serde_json::to_value(SetLogFilterResponse {
                filter: "siphon_ai=debug".into(),
                previous: "siphon_ai=info".into(),
            })
            .unwrap(),
            json!({ "filter": "siphon_ai=debug", "previous": "siphon_ai=info" })
        );
    }

    #[test]
    fn recent_cdrs_passthrough_round_trips() {
        let resp = RecentCdrsResponse {
            count: 1,
            cdrs: vec![json!({ "version": 8, "call_id": "siphon-abc", "duration_ms": 1200 })],
        };
        let back: RecentCdrsResponse =
            serde_json::from_value(serde_json::to_value(&resp).unwrap()).unwrap();
        assert_eq!(back.cdrs[0]["call_id"], "siphon-abc");
        assert_eq!(back.count, 1);
    }

    #[test]
    fn status_response_wire_shape() {
        let resp = StatusResponse {
            version: "0.49.0".into(),
            uptime_secs: 86_461,
            active_calls: 2,
            registrations: RegistrationsSummary {
                registered: 1,
                total: 2,
            },
            draining: false,
            hep_enabled: true,
        };
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            json!({
                "version": "0.49.0",
                "uptime_secs": 86_461,
                "active_calls": 2,
                "registrations": { "registered": 1, "total": 2 },
                "draining": false,
                "hep_enabled": true,
            })
        );
    }

    #[test]
    fn errors_response_wire_shape() {
        let resp = ErrorsResponse {
            count: 1,
            errors: vec![ErrorEntry {
                ts_ms: 1_755_000_000_123,
                level: "warn".into(),
                target: "siphon_ai_bridge::conn".into(),
                message: "server sent no audio within start-deadline deadline=5s".into(),
                call_id: Some("siphon-abc".into()),
            }],
        };
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            json!({
                "count": 1,
                "errors": [{
                    "ts_ms": 1_755_000_000_123u64,
                    "level": "warn",
                    "target": "siphon_ai_bridge::conn",
                    "message": "server sent no audio within start-deadline deadline=5s",
                    "call_id": "siphon-abc",
                }],
            })
        );
        // call_id is omitted, not null, when the event fired outside
        // a per-call span.
        let no_call = ErrorEntry {
            ts_ms: 0,
            level: "error".into(),
            target: "t".into(),
            message: "m".into(),
            call_id: None,
        };
        assert!(!serde_json::to_string(&no_call).unwrap().contains("call_id"));
    }

    #[test]
    fn call_sip_response_wire_shape() {
        let resp = CallSipResponse {
            call_id: "siphon-abc".into(),
            sip_call_id: "a84b4c76e66710@pbx.example.com".into(),
            truncated: false,
            count: 1,
            messages: vec![SipMessageEntry {
                ts_ms: 1_755_000_000_123,
                direction: "out".into(),
                src: "139.177.205.140:5060".into(),
                dst: "194.195.208.34:5060".into(),
                payload: "INVITE sip:+15551234@example.com SIP/2.0\r\n\r\n".into(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            json!({
                "call_id": "siphon-abc",
                "sip_call_id": "a84b4c76e66710@pbx.example.com",
                "truncated": false,
                "count": 1,
                "messages": [{
                    "ts_ms": 1_755_000_000_123u64,
                    "direction": "out",
                    "src": "139.177.205.140:5060",
                    "dst": "194.195.208.34:5060",
                    "payload": "INVITE sip:+15551234@example.com SIP/2.0\r\n\r\n",
                }],
            })
        );
    }

    #[test]
    fn drain_status_wire_shape() {
        let status = DrainStatus {
            draining: true,
            active_calls: 3,
            drain_timeout_secs: 30,
            remaining_secs: Some(21),
        };
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            json!({
                "draining": true,
                "active_calls": 3,
                "drain_timeout_secs": 30,
                "remaining_secs": 21,
            })
        );
    }

    #[test]
    fn originate_request_round_trips_with_defaults() {
        // A client-minimal body deserializes with every optional field
        // defaulted — the same leniency the daemon has always had.
        let req: OriginateRequest =
            serde_json::from_value(json!({ "to": "+15550100", "gateway": "twilio" })).unwrap();
        assert_eq!(req.to, "+15550100");
        assert!(req.ws_url.is_none() && req.from.is_none() && !req.delayed_offer);

        let full: OriginateRequest = serde_json::from_value(
            serde_json::to_value(&OriginateRequest {
                to: "+15550100".into(),
                gateway: "twilio".into(),
                ws_url: Some("wss://bot.example/ws".into()),
                from: None,
                delayed_offer: true,
                recording: Some("always".into()),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(full.ws_url.as_deref(), Some("wss://bot.example/ws"));
        assert!(full.delayed_offer);
    }

    #[test]
    fn responses_deserialize_from_wire_json() {
        // The client direction: parse exactly what the daemon emits.
        let parsed: CallsResponse = serde_json::from_value(json!({
            "count": 2,
            "calls": [
                { "call_id": "a", "sip_call_id": "a@h", "direction": "inbound" },
                { "call_id": "b", "sip_call_id": "b@h", "direction": "outbound" },
            ],
        }))
        .unwrap();
        assert_eq!(parsed.count, 2);
        assert_eq!(parsed.calls[1].direction, "outbound");

        let parked: ParkedResponse = serde_json::from_value(json!({
            "count": 1,
            "parked": [{ "call_id": "a", "parked_secs": 5 }],
        }))
        .unwrap();
        assert!(parked.parked[0].slot.is_none());
    }
}
