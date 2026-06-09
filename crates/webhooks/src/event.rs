//! Lifecycle webhook event schema.
//!
//! These payloads are a published API: every consumer (Slack
//! integration, ops automation, billing tap) parses them. CLAUDE.md
//! §7.9 says "add new event types per the same versioning rules
//! as the WS protocol and the CDR" — meaning a new variant is
//! additive (parsers tolerant to unknown `type` values don't
//! break), but renaming or changing existing fields requires a
//! version bump.
//!
//! ## Naming
//!
//! - `version` is the schema version. Starts at 1.
//! - `type` is the event discriminator on the wire (snake_case),
//!   per §7.9 ("event types are stable strings").
//!
//! ## What's NOT here yet
//!
//! - `ws_failure` — needs richer hooks into the bridge connection
//!   lifecycle than `BridgingAcceptor` currently exposes.
//! - HMAC `X-SiphonAI-Signature` header — sketched in CLAUDE.md
//!   §11.6, deferred to a follow-up.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Version of this lifecycle event schema. Bump only on
/// breaking changes (rename / type change / removal).
pub const WEBHOOK_VERSION: u32 = 1;

/// Lifecycle event payload. Serialised as a tagged JSON object —
/// `type` is the discriminator, peer fields hang off as siblings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebhookEvent {
    /// Fired immediately after the daemon sends 200 OK to the
    /// inbound INVITE and registers the call. The controller has
    /// been spawned but the WS bridge handshake may not be
    /// complete yet — operators who care about that distinction
    /// should look at `siphon_ai_ws_connect_seconds` in metrics.
    CallStart(CallStartEvent),

    /// Fired after the controller exits and the CDR has been
    /// emitted. The same `call_id` always pairs `CallStart` and
    /// `CallEnd` exactly once each.
    CallEnd(CallEndEvent),

    /// Fired on every `[[register]]` block status transition —
    /// e.g. `pending → registered`, `registered → failed`,
    /// `failed → registered`. Ops integrations subscribe to this
    /// to alert when an upstream PBX deregisters us.
    RegistrationStateChanged(RegistrationStateChangedEvent),

    /// Fired when an outbound call (`POST /admin/v1/calls`) is
    /// admitted and the INVITE is about to be sent. Exactly one of
    /// `OutboundAnswered` or `OutboundFailed` follows with the same
    /// `call_id`.
    OutboundInitiated(OutboundInitiatedEvent),

    /// Fired when the callee answers an outbound call (2xx received,
    /// media bound, WS bridge starting). A `CallEnd` with the same
    /// `call_id` follows when the call ends — answered outbound calls
    /// share the inbound end-of-call shape (and get a CDR).
    OutboundAnswered(OutboundAnsweredEvent),

    /// Fired when an outbound call ends without being answered —
    /// busy / declined / no-answer / rejected / unreachable / setup
    /// failure. Terminal: no `CallEnd` (and no CDR) follows.
    OutboundFailed(OutboundFailedEvent),
}

impl WebhookEvent {
    /// The `type` discriminator string — useful for the
    /// `events` allowlist filter without reaching into
    /// `serde_json::to_value`.
    pub fn type_str(&self) -> &'static str {
        match self {
            WebhookEvent::CallStart(_) => "call_start",
            WebhookEvent::CallEnd(_) => "call_end",
            WebhookEvent::RegistrationStateChanged(_) => "registration_state_changed",
            WebhookEvent::OutboundInitiated(_) => "outbound_initiated",
            WebhookEvent::OutboundAnswered(_) => "outbound_answered",
            WebhookEvent::OutboundFailed(_) => "outbound_failed",
        }
    }

    /// Bridge call id when the event is call-scoped; `None` for
    /// non-call events (e.g. `RegistrationStateChanged`). Lets the
    /// sink correlate retries / dedupe by call when relevant.
    pub fn call_id(&self) -> Option<&str> {
        match self {
            WebhookEvent::CallStart(e) => Some(&e.call_id),
            WebhookEvent::CallEnd(e) => Some(&e.call_id),
            WebhookEvent::RegistrationStateChanged(_) => None,
            WebhookEvent::OutboundInitiated(e) => Some(&e.call_id),
            WebhookEvent::OutboundAnswered(e) => Some(&e.call_id),
            WebhookEvent::OutboundFailed(e) => Some(&e.call_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallStartEvent {
    /// Schema version. Bumped per CLAUDE.md §7.9.
    pub version: u32,
    /// SiphonAI bridge call id (matches the WS `start.call_id`).
    pub call_id: String,
    /// SIP `Call-ID` header from the inbound INVITE.
    pub sip_call_id: String,
    /// When 200 OK was sent (UTC).
    pub timestamp: DateTime<Utc>,
    /// `From` user (full URI is intentionally omitted; user-part
    /// is what dashboards / Slack prefer).
    pub from: String,
    /// Request-URI user.
    pub to: String,
    /// Matched `[[route]].name`.
    pub route: String,
    /// WS URL the bridge will connect to.
    pub ws_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallEndEvent {
    pub version: u32,
    pub call_id: String,
    pub sip_call_id: String,
    /// When the controller exited (UTC).
    pub timestamp: DateTime<Utc>,
    pub from: String,
    pub to: String,
    pub route: String,
    pub ws_url: String,
    /// Call duration in milliseconds. Same value as
    /// `CdrRecord::duration_ms`.
    pub duration_ms: u64,
    /// Termination cause as a snake_case string mirroring
    /// `siphon_ai_cdr::TerminationCause` so dashboards can
    /// correlate the two without a re-mapping table.
    pub termination_cause: String,
}

/// State transition for one `[[register]]` block.
///
/// Status strings mirror `siphon_ai_sip_glue::RegistrationStatus::as_str`
/// so dashboards correlate the webhook with the
/// `siphon_ai_register_state{name,state}` metric without a re-mapping
/// table. `previous_status` is `None` only on the very first emit
/// after process start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationStateChangedEvent {
    pub version: u32,
    /// `[[register]].name`.
    pub name: String,
    /// When the transition was observed (UTC).
    pub timestamp: DateTime<Utc>,
    /// New status (`pending`/`registered`/`failed`/`disabled`).
    pub status: String,
    /// Status before this transition. `None` on the first emit.
    pub previous_status: Option<String>,
    /// Free-form failure description when transitioning to
    /// `failed` (`"401 Unauthorized"`, `"timeout"`, …). `None` for
    /// success transitions.
    pub last_error: Option<String>,
    /// When the registration expires per the registrar's grant.
    /// Present only when `status = registered`.
    pub expires_at: Option<DateTime<Utc>>,
}

/// An outbound call was admitted and is being placed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundInitiatedEvent {
    /// Schema version. Bumped per CLAUDE.md §7.9.
    pub version: u32,
    /// SiphonAI bridge call id — the one `POST /admin/v1/calls`
    /// returned, shared by the follow-up answered/failed/end events.
    pub call_id: String,
    /// When the INVITE was dispatched (UTC).
    pub timestamp: DateTime<Utc>,
    /// Destination as given in the originate request (user part —
    /// the gateway supplies host/port).
    pub to: String,
    /// `[[gateway]].name` the call is placed through.
    pub gateway: String,
}

/// The callee answered an outbound call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundAnsweredEvent {
    pub version: u32,
    /// Bridge call id, pairing with `OutboundInitiated`.
    pub call_id: String,
    /// SIP `Call-ID` of the established dialog — correlates with
    /// HEP/Homer captures.
    pub sip_call_id: String,
    /// When the 2xx answer was processed (UTC).
    pub timestamp: DateTime<Utc>,
}

/// An outbound call ended without an answer. Terminal for its
/// `call_id` — no `CallEnd` or CDR follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundFailedEvent {
    pub version: u32,
    /// Bridge call id, pairing with `OutboundInitiated`.
    pub call_id: String,
    /// When the failure was observed (UTC).
    pub timestamp: DateTime<Utc>,
    /// Why the call didn't connect, mirroring the
    /// `siphon_ai_outbound_calls_total{result}` metric labels so
    /// dashboards correlate without a re-mapping table: `busy` /
    /// `declined` / `no_answer` / `rejected` / `unreachable` /
    /// `failed`.
    pub cause: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::{json, Value};

    fn sample_start() -> WebhookEvent {
        WebhookEvent::CallStart(CallStartEvent {
            version: WEBHOOK_VERSION,
            call_id: "siphon-7f3a".into(),
            sip_call_id: "abc-123@pbx".into(),
            timestamp: Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 0).unwrap(),
            from: "+13125551234".into(),
            to: "5000".into(),
            route: "main_reception".into(),
            ws_url: "wss://reception.example.com/sip-bridge".into(),
        })
    }

    fn sample_end() -> WebhookEvent {
        WebhookEvent::CallEnd(CallEndEvent {
            version: WEBHOOK_VERSION,
            call_id: "siphon-7f3a".into(),
            sip_call_id: "abc-123@pbx".into(),
            timestamp: Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 42).unwrap(),
            from: "+13125551234".into(),
            to: "5000".into(),
            route: "main_reception".into(),
            ws_url: "wss://reception.example.com/sip-bridge".into(),
            duration_ms: 42_000,
            termination_cause: "server_hangup".into(),
        })
    }

    fn sample_outbound() -> [WebhookEvent; 3] {
        let ts = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        [
            WebhookEvent::OutboundInitiated(OutboundInitiatedEvent {
                version: WEBHOOK_VERSION,
                call_id: "siphon-9b2c".into(),
                timestamp: ts,
                to: "+13125550000".into(),
                gateway: "twilio_main".into(),
            }),
            WebhookEvent::OutboundAnswered(OutboundAnsweredEvent {
                version: WEBHOOK_VERSION,
                call_id: "siphon-9b2c".into(),
                sip_call_id: "xyz-789@siphon".into(),
                timestamp: ts,
            }),
            WebhookEvent::OutboundFailed(OutboundFailedEvent {
                version: WEBHOOK_VERSION,
                call_id: "siphon-9b2c".into(),
                timestamp: ts,
                cause: "busy".into(),
            }),
        ]
    }

    #[test]
    fn outbound_events_round_trip_through_json() {
        for event in sample_outbound() {
            let s = serde_json::to_string(&event).unwrap();
            let parsed: WebhookEvent = serde_json::from_str(&s).unwrap();
            assert_eq!(event, parsed);
        }
    }

    #[test]
    fn outbound_type_strs_match_wire_discriminators() {
        let expected = ["outbound_initiated", "outbound_answered", "outbound_failed"];
        for (event, want) in sample_outbound().iter().zip(expected) {
            assert_eq!(event.type_str(), want);
            let v: Value = serde_json::to_value(event).unwrap();
            assert_eq!(v["type"], json!(want));
        }
    }

    #[test]
    fn outbound_events_are_call_scoped() {
        for event in sample_outbound() {
            assert_eq!(event.call_id(), Some("siphon-9b2c"));
        }
    }

    #[test]
    fn call_start_round_trips_through_json() {
        let event = sample_start();
        let s = serde_json::to_string(&event).unwrap();
        let parsed: WebhookEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn call_end_round_trips_through_json() {
        let event = sample_end();
        let s = serde_json::to_string(&event).unwrap();
        let parsed: WebhookEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn type_field_uses_snake_case_on_wire() {
        let v: Value = serde_json::to_value(sample_start()).unwrap();
        assert_eq!(v["type"], json!("call_start"));
        let v: Value = serde_json::to_value(sample_end()).unwrap();
        assert_eq!(v["type"], json!("call_end"));
    }

    #[test]
    fn type_str_matches_wire_discriminator() {
        // Pin the invariant so the allowlist filter and the wire
        // shape never drift.
        let s = sample_start();
        let serialized = serde_json::to_value(&s).unwrap();
        assert_eq!(s.type_str(), serialized["type"].as_str().unwrap());
        let e = sample_end();
        let serialized = serde_json::to_value(&e).unwrap();
        assert_eq!(e.type_str(), serialized["type"].as_str().unwrap());
    }

    #[test]
    fn version_field_starts_at_1() {
        assert_eq!(WEBHOOK_VERSION, 1);
        let v: Value = serde_json::to_value(sample_start()).unwrap();
        assert_eq!(v["version"], json!(1));
    }

    #[test]
    fn unknown_event_type_fails_to_parse() {
        // Confirms that adding a new variant in a future version
        // won't be silently accepted by an older parser as a
        // malformed CallStart — the discriminator is required.
        let raw = r#"{"type":"yodel","version":1,"call_id":"c"}"#;
        assert!(serde_json::from_str::<WebhookEvent>(raw).is_err());
    }

    #[test]
    fn renders_one_line_with_no_embedded_newlines() {
        // Most webhook receivers tolerate any JSON, but logs that
        // pipe `req.body` to `tail` benefit from line-shape.
        let s = serde_json::to_string(&sample_end()).unwrap();
        assert!(!s.contains('\n'));
    }
}
