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
//! **v2 (0.9.5):** added five [`TerminationCause`] variants for
//! delayed-offer negotiations that fail *before* the call goes active
//! (`ack_timeout`, `missing_sdp_answer`, `invalid_sdp_answer`,
//! `no_compatible_codec`, `invalid_remote_media`). A strict consumer
//! that exhaustively matched the v1 cause set would not recognise these,
//! so the version is bumped even though the field shape is unchanged.
//!
//! **v3 (0.17.0):** added the `drain_forced` [`TerminationCause`] variant
//! for calls force-ended at the graceful-shutdown drain deadline. Same
//! rationale as v2 — a new cause value, no field shape change.
//!
//! **v4 (0.30.0):** added the optional [`QualityInfo`] block (per-call
//! quality summary: first-audio latency, barge-in count, jitter / loss /
//! RTT aggregates, local RX counters, MOS estimate). Additive-optional,
//! but bumped per the 0.9.5 precedent for new blocks so consumers can
//! gate on `version >= 4` instead of probing for the field.
//!
//! ## What we record vs. what we don't
//!
//! - **From / To** users only — full SIP URIs are recorded as the
//!   user part because route matching is what operators care about,
//!   and full URIs leak more than necessary into log streams.
//! - **No SDP body** — the negotiated codec / sample rate / payload
//!   type are flat fields; the raw SDP would balloon the record and
//!   isn't operator-readable.
//! - **No call audio** — the record carries a `recording_path` *pointer*
//!   when recording is on (`[recording]`), never the audio itself.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Schema version of the CDR record. Bump per CLAUDE.md §7.7
/// whenever a change could break consumer parsers.
///
/// v4 (0.30.0): adds the optional `quality` block (see [`QualityInfo`]).
/// Additive-optional; bumped per the 0.9.5 new-block precedent.
pub const CDR_VERSION: u32 = 4;

/// One call's complete record. Always serialised as a single JSON
/// object on a single line for JSONL file sinks.
///
/// (`PartialEq` only, no `Eq` — the v4 `quality` block carries floats.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// for those, but reserved). For outbound calls this is the
    /// `[[gateway]]` name the call was placed through.
    pub route: String,
    /// WebSocket URL the bridge connected to.
    pub ws_url: String,

    pub audio: AudioInfo,
    pub termination: TerminationInfo,

    /// STIR/SHAKEN attestation for this call: `"A"` / `"B"` / `"C"`, or
    /// absent when call authentication is off or no `Identity` header was
    /// present. This is the *claimed* level — pair it with
    /// [`verstat_passed`](Self::verstat_passed) to know whether it verified.
    /// Additive optional field (emitted only when populated) so the CDR
    /// schema stays at version 1 (plan §9 decision 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verstat_attest: Option<String>,
    /// Composite STIR/SHAKEN pass: signature + cert chain + orig/dest all
    /// verified. Absent when call authentication is off. `Some(false)` means
    /// an `Identity` header was present but did not fully verify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verstat_passed: Option<bool>,

    /// Recording identifier, present when the call was recorded
    /// (`[recording]`). Equals `call_id` in this release. Additive optional
    /// field → CDR schema stays at version 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_id: Option<String>,
    /// Filesystem path of the recording, when one was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_path: Option<String>,
    /// `true` when the recording is sealed at rest under
    /// `[recording.encryption]` (0.24.0) — the file at `recording_path` is
    /// a `.wava` envelope, not a playable WAV. Omitted when the call wasn't
    /// recorded. Additive optional field → CDR schema version unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_encrypted: Option<bool>,
    /// Object-storage destination (`s3://bucket/key`) when
    /// `[recording.storage]` is enabled (0.25.0). Stamped at *enqueue*
    /// time — the key is deterministic; the `recording_uploaded`
    /// lifecycle webhook confirms arrival. Omitted when storage is off
    /// or the call wasn't recorded. Additive → schema version unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_url: Option<String>,

    /// Recording-consent audit trail (0.26.0), present when a "this call
    /// is recorded" announcement played and/or the WS server reported
    /// captured consent (`set_recording_consent`). Omitted otherwise.
    /// Additive optional field → CDR schema version unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent: Option<ConsentInfo>,

    /// Park summary (0.7.0), present when the call was parked at least
    /// once. Omitted otherwise. Additive optional field → CDR schema
    /// stays at version 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub park: Option<ParkInfo>,

    /// Hold summary (0.7.2), present when the bot held its own caller at
    /// least once. Omitted otherwise. Additive optional field → CDR
    /// schema stays at version 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<HoldInfo>,

    /// Reconnect summary (0.7.3), present when the WS dropped and
    /// reconnect ran at least once. Omitted otherwise. Additive optional
    /// field → CDR schema stays at version 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect: Option<ReconnectInfo>,

    /// Per-call quality summary (0.30.0, CDR v4), present when the call
    /// produced any quality signal (media flowed, playout was cleared,
    /// or the WS server sent audio). Omitted for calls that never went
    /// active. Closes the OPERATIONS.md Q5 (`first_audio_out_ms`) and
    /// Q8 (`barge_in_count`) gaps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<QualityInfo>,
}

/// Recording-consent audit trail on the CDR (0.26.0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentInfo {
    /// The daemon played the `[recording.announcement]` file to the
    /// caller before capture started.
    pub announced: bool,
    /// Announcement duration in milliseconds (0 when `announced` is
    /// false).
    pub announcement_ms: u64,
    /// The WS server's consent note (`set_recording_consent`), e.g.
    /// "dtmf-1". Absent when the server never reported consent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

/// Per-call park accounting on the CDR (0.7.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkInfo {
    /// Number of park episodes over the call's lifetime.
    pub count: u32,
    /// Cumulative wall-time the call spent parked, in milliseconds.
    pub total_ms: u64,
}

/// Per-call hold accounting on the CDR (0.7.2). Counts only
/// bot-initiated holds (`BridgeIn::Hold`); a far-end hold is the peer's
/// business and isn't tallied here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldInfo {
    /// Number of bot-initiated hold episodes over the call's lifetime.
    pub count: u32,
    /// Cumulative wall-time the call spent held, in milliseconds.
    pub total_ms: u64,
}

/// Per-call quality summary on the CDR (0.30.0, CDR v4).
///
/// Jitter / loss / RTT aggregates are computed over the RTCP Receiver
/// Reports received during the call (remote-reported: how the far end
/// received the stream SiphonAI sent). The `rx_packets_*` totals and the
/// MOS aggregates come from locally-measured receive-side snapshots
/// (see the `rtp_stats` `rx_*` fields in PROTOCOL.md §3.8). Every field
/// is optional — a signal that never produced data is omitted, not
/// zeroed, so consumers can tell "clean" from "unmeasured".
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct QualityInfo {
    /// Milliseconds from "WS bridge connected" to the first audio frame
    /// from the WS server reaching playout toward the caller — the
    /// operator's "how slow is my STT/LLM/TTS chain at first token".
    /// Absent when the server never sent audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_audio_out_ms: Option<u64>,
    /// Playout clears over the call's lifetime: `auto_clear` firings
    /// (daemon-side barge-in) plus server-sent `clear` commands. Absent
    /// (not `0`) only when the whole block would otherwise be absent.
    pub barge_in_count: u32,
    /// Mean / max of the RR-reported interarrival jitter (ms) across the
    /// call's RTCP reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_jitter_ms: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_jitter_ms: Option<f32>,
    /// Mean / max of the RR-reported **per-interval** loss ratio
    /// `[0.0, 1.0]`. Each sample is one RR's `fraction_lost` — loss over
    /// the interval since the previous report (RFC 3550 §6.4.1) — so
    /// this is a mean of interval fractions, **not** the call's
    /// cumulative loss ratio. Reconciling against a carrier's cumulative
    /// figure needs [`Self::tx_packets_lost_reported`] over
    /// [`Self::tx_packets_sent`] instead.
    ///
    /// (Corrected in 0.38.0 — through 0.37.x these were documented as
    /// the "RR-reported cumulative-loss ratio", which they never were.
    /// The values are unchanged; only the description was wrong.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_packet_loss_ratio: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_packet_loss_ratio: Option<f32>,
    /// Mean RTCP round-trip time (ms) across reports that carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_rtcp_rtt_ms: Option<f32>,
    /// End-of-call totals from the local receive side (caller→SiphonAI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rx_packets_received: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rx_packets_lost: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rx_packets_out_of_order: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rx_packets_duplicate: Option<u64>,
    /// End-of-call totals from the local transmit side
    /// (SiphonAI→caller): RTP packets and *payload* octets we put on the
    /// wire (0.38.0). `tx_octets_sent` excludes RTP headers and SRTP
    /// overhead (RFC 3550 §6.4.1 sender-octet-count basis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_packets_sent: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_octets_sent: Option<u64>,
    /// The far end's own **absolute** count of packets lost on the
    /// stream SiphonAI sent, from the call's last RTCP RR (RFC 3550
    /// §6.4.1 cumulative-lost). Signed — a negative value is legitimate
    /// when duplicates push the far end's packets-received past
    /// packets-expected, so consumers should not clamp it (0.38.0).
    ///
    /// With [`Self::tx_packets_sent`] this answers the question the
    /// block could not before: "we sent N, the far end lost M."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_packets_lost_reported: Option<i64>,
    /// Worst / mean transport-only MOS-CQE estimate over the call
    /// (`[1.0, 5.0]`; see PROTOCOL.md §3.8 `mos_estimate`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mos_estimate_min: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mos_estimate_avg: Option<f32>,
}

/// Per-call WS-reconnect accounting on the CDR (0.7.3). An episode is one
/// unexpected WS drop that entered the reconnect path
/// (`[bridge].ws_reconnect_enabled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconnectInfo {
    /// Number of reconnect episodes over the call's lifetime.
    pub count: u32,
    /// Cumulative wall-time the call spent on reconnect hold music, in
    /// milliseconds.
    pub total_gap_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// SiphonAI answered the call (trunk INVITE or registered line).
    Inbound,
    /// SiphonAI placed the call (`POST /admin/v1/calls` through a
    /// gateway). The field was reserved for this since v1, so the
    /// schema version stays 1 (0.6.0 plan §9.4). For outbound CDRs
    /// `route` carries the gateway name instead of a `[[route]]` name.
    Outbound,
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
    /// The daemon force-terminated this call at the graceful-shutdown
    /// drain deadline (0.17.0): it was still active when
    /// `[shutdown].drain_timeout_secs` elapsed, so it was ended with a
    /// real BYE + WS hangup rather than left to finish. Distinct from
    /// `local_shutdown` so a deploy's forced terminations are
    /// attributable per-call.
    DrainForced,
    /// Bridge sub-task ended first (clean WS close, server
    /// disconnect, or a bridge-side error).
    BridgeEnded,
    /// Media tap sub-task ended first (RTP ended, tap detached).
    TapEnded,

    // ─── Delayed-offer negotiation failures (v2, 0.9.5) ───────────
    // These end a call that was half-established (200 OK with our
    // offer was sent) but never went active — the ACK answer never
    // arrived or was unusable. The call never reached a controller, so
    // `bridge_disconnect` / `tap_disconnect` are empty and `audio` is
    // unpopulated (no codec was negotiated).
    /// No ACK (with the SDP answer) arrived before SIP Timer H (~32 s).
    AckTimeout,
    /// The ACK arrived but carried no SDP body.
    MissingSdpAnswer,
    /// The ACK's SDP answer was present but unparseable.
    InvalidSdpAnswer,
    /// The answer selected no codec we offered.
    NoCompatibleCodec,
    /// The answer's RTP address/port was unusable, the audio stream was
    /// rejected, or its SRTP keying (DTLS/SDES) could not be established.
    InvalidRemoteMedia,
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
            verstat_attest: None,
            verstat_passed: None,
            recording_id: None,
            recording_path: None,
            recording_encrypted: None,
            recording_url: None,
            consent: None,
            park: None,
            hold: None,
            reconnect: None,
            quality: None,
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
        let v = serde_json::to_value(TerminationCause::DrainForced).unwrap();
        assert_eq!(v, serde_json::json!("drain_forced"));
    }

    #[test]
    fn direction_uses_snake_case_on_wire() {
        let v = serde_json::to_value(Direction::Inbound).unwrap();
        assert_eq!(v, serde_json::json!("inbound"));
        let v = serde_json::to_value(Direction::Outbound).unwrap();
        assert_eq!(v, serde_json::json!("outbound"));
    }

    #[test]
    fn outbound_direction_is_additive_and_stays_at_v1() {
        // §9.4: `direction` was reserved for outbound since v1, so an
        // outbound CDR parses under the same schema version.
        let mut rec = sample();
        rec.direction = Direction::Outbound;
        rec.route = "twilio_main".into(); // gateway name, not a route
        let v: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["direction"], serde_json::json!("outbound"));
        let back: CdrRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back.direction, Direction::Outbound);
    }

    #[test]
    fn version_field_is_present_and_is_4() {
        // Bumped to 4 in 0.30.0 (new optional `quality` block — see the
        // module versioning note). Was 3 in 0.17.0 (`drain_forced`) and
        // 2 in 0.9.5 (delayed-offer failure causes).
        assert_eq!(CDR_VERSION, 4);
        let v: serde_json::Value = serde_json::to_value(sample()).unwrap();
        assert_eq!(v["version"], serde_json::json!(4));
    }

    #[test]
    fn quality_block_round_trips_and_omits_when_absent() {
        // Absent block → no `quality` key at all (v3 parsers see the
        // same shape they always did, minus the version number).
        let v: serde_json::Value = serde_json::to_value(sample()).unwrap();
        assert!(v.get("quality").is_none(), "absent block must be omitted");

        // Populated block round-trips; unmeasured fields inside are
        // omitted, not null.
        let mut rec = sample();
        rec.quality = Some(QualityInfo {
            first_audio_out_ms: Some(742),
            barge_in_count: 3,
            avg_jitter_ms: Some(11.5),
            max_jitter_ms: Some(30.0),
            avg_packet_loss_ratio: Some(0.004),
            max_packet_loss_ratio: Some(0.02),
            avg_rtcp_rtt_ms: None, // RTT never measured
            rx_packets_received: Some(14_820),
            rx_packets_lost: Some(12),
            rx_packets_out_of_order: Some(3),
            rx_packets_duplicate: Some(0),
            tx_packets_sent: Some(14_900),
            tx_octets_sent: Some(2_384_000),
            tx_packets_lost_reported: Some(12),
            mos_estimate_min: Some(3.9),
            mos_estimate_avg: Some(4.3),
        });
        let v: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["quality"]["first_audio_out_ms"], 742);
        assert_eq!(v["quality"]["barge_in_count"], 3);
        assert_eq!(v["quality"]["rx_packets_received"], 14_820);
        assert!(
            v["quality"].get("avg_rtcp_rtt_ms").is_none(),
            "unmeasured field omitted, not null"
        );
        let back: CdrRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn delayed_offer_failure_causes_round_trip_snake_case() {
        for (cause, wire) in [
            (TerminationCause::AckTimeout, "ack_timeout"),
            (TerminationCause::MissingSdpAnswer, "missing_sdp_answer"),
            (TerminationCause::InvalidSdpAnswer, "invalid_sdp_answer"),
            (TerminationCause::NoCompatibleCodec, "no_compatible_codec"),
            (TerminationCause::InvalidRemoteMedia, "invalid_remote_media"),
        ] {
            let v = serde_json::to_value(cause).unwrap();
            assert_eq!(v, serde_json::json!(wire));
            let back: TerminationCause = serde_json::from_value(v).unwrap();
            assert_eq!(back, cause);
        }
    }

    #[test]
    fn hold_field_is_additive_and_stays_at_v1() {
        // Never bot-held → omitted, schema still v1.
        let v: serde_json::Value = serde_json::to_value(sample()).unwrap();
        assert!(!v.as_object().unwrap().contains_key("hold"));
        assert_eq!(v["version"], serde_json::json!(CDR_VERSION));

        // Held → nested {count, total_ms}; version unchanged.
        let mut rec = sample();
        rec.hold = Some(HoldInfo {
            count: 2,
            total_ms: 4500,
        });
        let v: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["hold"]["count"], serde_json::json!(2));
        assert_eq!(v["hold"]["total_ms"], serde_json::json!(4500));
        assert_eq!(v["version"], serde_json::json!(CDR_VERSION));
        let back: CdrRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back.hold, rec.hold);
    }

    #[test]
    fn reconnect_field_is_additive_and_stays_at_v1() {
        // Never reconnected → omitted, schema still v1.
        let v: serde_json::Value = serde_json::to_value(sample()).unwrap();
        assert!(!v.as_object().unwrap().contains_key("reconnect"));
        assert_eq!(v["version"], serde_json::json!(CDR_VERSION));

        // Reconnected → nested {count, total_gap_ms}; version unchanged.
        let mut rec = sample();
        rec.reconnect = Some(ReconnectInfo {
            count: 1,
            total_gap_ms: 1200,
        });
        let v: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["reconnect"]["count"], serde_json::json!(1));
        assert_eq!(v["reconnect"]["total_gap_ms"], serde_json::json!(1200));
        assert_eq!(v["version"], serde_json::json!(CDR_VERSION));
        let back: CdrRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back.reconnect, rec.reconnect);
    }

    #[test]
    fn verstat_fields_are_additive_and_stay_at_v1() {
        // Absent verdict (call-auth off) → fields omitted, schema still v1.
        let v: serde_json::Value = serde_json::to_value(sample()).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("verstat_attest"));
        assert!(!obj.contains_key("verstat_passed"));
        assert_eq!(v["version"], serde_json::json!(CDR_VERSION));

        // Populated verdict → fields present; version unchanged.
        let mut rec = sample();
        rec.verstat_attest = Some("A".into());
        rec.verstat_passed = Some(true);
        let v: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["verstat_attest"], serde_json::json!("A"));
        assert_eq!(v["verstat_passed"], serde_json::json!(true));
        assert_eq!(v["version"], serde_json::json!(CDR_VERSION));

        // Round-trips, and a pre-0.4.0 CDR without the fields still parses.
        let back: CdrRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back.verstat_attest.as_deref(), Some("A"));
        let legacy: CdrRecord = serde_json::from_value(serde_json::to_value(sample()).unwrap())
            .expect("CDR without verstat fields parses");
        assert_eq!(legacy.verstat_passed, None);
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
