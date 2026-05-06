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
//! - `registration_state_changed` — needs REGISTER mode (deferred).
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
}

impl WebhookEvent {
    /// The `type` discriminator string — useful for the
    /// `events` allowlist filter without reaching into
    /// `serde_json::to_value`.
    pub fn type_str(&self) -> &'static str {
        match self {
            WebhookEvent::CallStart(_) => "call_start",
            WebhookEvent::CallEnd(_) => "call_end",
        }
    }

    /// Bridge call id, present on every event variant. Lets the
    /// sink correlate retries / dedupe.
    pub fn call_id(&self) -> &str {
        match self {
            WebhookEvent::CallStart(e) => &e.call_id,
            WebhookEvent::CallEnd(e) => &e.call_id,
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
