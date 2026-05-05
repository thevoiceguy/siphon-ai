//! CDR record schema — what gets serialized to JSONL files and
//! POSTed to webhooks.
//!
//! The shape is a published API: every consumer (file reader,
//! webhook receiver, Homer's sip-cdr ingestor) parses it. CLAUDE.md
//! §7.7 says any schema change that could break parsers REQUIRES a
//! version bump.
//!
//! ## Versioning
//!
//! [`CDR_VERSION`] starts at 1. Adding a new optional field is
//! additive (parsers tolerant to unknown fields don't break). Removing
//! a field, renaming, or changing a type is breaking — bump the
//! version, document the change in `docs/CDR.md` (when that lands),
//! and link the PR in the commit message per CLAUDE.md §7.7.
//!
//! ## What we record vs. what we don't
//!
//! - **From / To** users only — full SIP URIs are recorded as the
//!   user part because route matching is what operators care about,
//!   and full URIs leak more than necessary into log streams.
//! - **No SDP body** — the negotiated codec / sample rate / payload
//!   type are flat fields; the raw SDP would balloon the record and
//!   isn't operator-readable.
//! - **No call audio** — recording is a different feature
//!   (CLAUDE.md §8 calls it post-v1).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Schema version of the CDR record. Bump per CLAUDE.md §7.7
/// whenever a change could break consumer parsers.
pub const CDR_VERSION: u32 = 1;

/// One call's complete record. Always serialised as a single JSON
/// object on a single line for JSONL file sinks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdrRecord {
    /// Schema version. Bump per CLAUDE.md §7.7.
    pub version: u32,

    /// SiphonAI bridge call id (the same one used in the WS
    /// `start` message). Distinct from `sip_call_id` — that's the
    /// SIP `Call-ID` header.
    pub call_id: String,
    pub sip_call_id: String,

    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub duration_ms: u64,

    pub from: String,
    pub to: String,
    pub direction: Direction,

    /// Name of the matched `[[route]]` block (or `"unmatched"` if
    /// the route table didn't match — we don't currently emit CDRs
    /// for those, but reserved).
    pub route: String,
    /// WebSocket URL the bridge connected to.
    pub ws_url: String,

    pub audio: AudioInfo,
    pub termination: TerminationInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// The only direction in v1; reserved for symmetry with future
    /// outbound originated calls.
    Inbound,
}

/// Audio metadata from the negotiated SDP answer. The wire-side
/// codec lives in `codec` / `payload_type`; `sample_rate` is the
/// post-decode rate the WS bridge saw (G.722 advertises 8000 in
/// SDP but produces 16k samples — `sample_rate` is the latter).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioInfo {
    pub codec: String,
    pub payload_type: u8,
    pub sample_rate: u32,
}

/// How the call ended, plus the underlying cause string from
/// whichever sub-task wrapped it up. We keep both — operators want
/// to know whether the WS server hung up on us versus the SIP peer
/// sent BYE, and the cause strings give them the why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminationInfo {
    pub cause: TerminationCause,
    /// Free-form cause from the bridge sub-task, when available.
    /// Empty string when the bridge ended via a non-error path or
    /// before reporting.
    #[serde(default)]
    pub bridge_disconnect: String,
    /// Same shape, for the media tap.
    #[serde(default)]
    pub tap_disconnect: String,
}

/// High-level cause classification mirroring
/// [`siphon_ai_core::CallTermination`]. Stable strings on the wire
/// per CLAUDE.md §4.2 ("WS protocol is a public API"); CDR consumers
/// pin against these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationCause {
    /// WS server sent a `hangup` over the bridge.
    ServerHangup,
    /// Local control path (BYE/CANCEL/admin) asked the controller
    /// to shut down.
    LocalShutdown,
    /// Bridge sub-task ended first (clean WS close, server
    /// disconnect, or a bridge-side error).
    BridgeEnded,
    /// Media tap sub-task ended first (RTP ended, tap detached).
    TapEnded,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> CdrRecord {
        CdrRecord {
            version: CDR_VERSION,
            call_id: "siphon-7f3a".into(),
            sip_call_id: "abc-123@pbx.example.com".into(),
            started_at: Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 0).unwrap(),
            ended_at: Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 42).unwrap(),
            duration_ms: 42_000,
            from: "+13125551234".into(),
            to: "5000".into(),
            direction: Direction::Inbound,
            route: "main_reception".into(),
            ws_url: "wss://reception.example.com/sip-bridge".into(),
            audio: AudioInfo {
                codec: "PCMU".into(),
                payload_type: 0,
                sample_rate: 8000,
            },
            termination: TerminationInfo {
                cause: TerminationCause::ServerHangup,
                bridge_disconnect: "stop_sent".into(),
                tap_disconnect: "controller_hung_up".into(),
            },
        }
    }

    #[test]
    fn round_trips_through_json() {
        let record = sample();
        let serialized = serde_json::to_string(&record).expect("serialize");
        let parsed: CdrRecord = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(record, parsed);
    }

    #[test]
    fn renders_one_line_with_jsonl_friendly_shape() {
        // JSONL needs no embedded newlines; serde_json::to_string
        // produces a single line by default.
        let s = serde_json::to_string(&sample()).expect("serialize");
        assert!(!s.contains('\n'), "JSONL line must be newline-free");
    }

    #[test]
    fn termination_cause_uses_snake_case_on_wire() {
        let v = serde_json::to_value(TerminationCause::ServerHangup).unwrap();
        assert_eq!(v, serde_json::json!("server_hangup"));
        let v = serde_json::to_value(TerminationCause::LocalShutdown).unwrap();
        assert_eq!(v, serde_json::json!("local_shutdown"));
    }

    #[test]
    fn direction_uses_snake_case_on_wire() {
        let v = serde_json::to_value(Direction::Inbound).unwrap();
        assert_eq!(v, serde_json::json!("inbound"));
    }

    #[test]
    fn version_field_is_present_and_starts_at_1() {
        assert_eq!(CDR_VERSION, 1);
        let v: serde_json::Value = serde_json::to_value(sample()).unwrap();
        assert_eq!(v["version"], serde_json::json!(1));
    }

    #[test]
    fn missing_optional_fields_default_to_empty_strings() {
        // Forward-compatibility: a CDR emitted before the
        // bridge/tap disconnect strings landed should still parse.
        let raw = serde_json::json!({
            "version": 1,
            "call_id": "c",
            "sip_call_id": "s",
            "started_at": "2026-05-05T14:30:00Z",
            "ended_at": "2026-05-05T14:30:01Z",
            "duration_ms": 1000,
            "from": "+1",
            "to": "5000",
            "direction": "inbound",
            "route": "default",
            "ws_url": "wss://x/y",
            "audio": { "codec": "PCMU", "payload_type": 0, "sample_rate": 8000 },
            "termination": { "cause": "server_hangup" },
        });
        let r: CdrRecord = serde_json::from_value(raw).expect("parses");
        assert_eq!(r.termination.bridge_disconnect, "");
        assert_eq!(r.termination.tap_disconnect, "");
    }
}
