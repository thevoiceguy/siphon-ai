//! WebSocket bridge protocol types — v1.
//!
//! Canonical wire format spec: `docs/PROTOCOL.md`. The Rust types here
//! and the spec MUST stay in sync; protocol changes get a doc update in
//! the same PR (CLAUDE.md §4.2).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use siphon_ai_security::VerificationResult;

/// The protocol version SiphonAI sends in [`StartMsg::version`]. Bumped only
/// for breaking changes; additive changes (new optional fields, new enum
/// variants) do not bump it.
pub const PROTOCOL_VERSION: &str = "1";

/// The WebSocket subprotocol SiphonAI advertises during the upgrade
/// handshake. Servers SHOULD echo it; SiphonAI proceeds either way.
pub const WS_SUBPROTOCOL: &str = "siphon-ai.v1";

/// SiphonAI's per-call identifier, distinct from the SIP `Call-ID`.
///
/// Serialized transparently as a string. Servers MUST echo this on every
/// message they send for the call.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallId(pub String);

impl CallId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Per-call monotonic sequence number on SiphonAI→server messages.
///
/// Starts at 0 on `start`, increments by 1 with every subsequent message
/// SiphonAI sends. Servers MUST NOT include `seq` in their messages.
pub type Seq = u64;

// ============================================================================
// SiphonAI → Server (BridgeOut)
// ============================================================================

/// Messages SiphonAI sends to the WebSocket server.
///
/// Wire format: each variant serializes to a single JSON object with a
/// `"type"` discriminator. Audio frames travel separately as binary
/// WebSocket frames (see `docs/PROTOCOL.md` §2.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeOut {
    /// First message on the connection. Carries call metadata and the
    /// audio format both directions will use for the lifetime of the call.
    Start(StartMsg),

    /// VAD detected the caller starting to speak. Emitted only when
    /// `bridge.vad = true`.
    SpeechStarted {
        call_id: CallId,
        seq: Seq,
        /// Milliseconds since `start` was sent (monotonic, NOT wall-clock).
        ts_ms: u64,
    },

    /// VAD detected the caller stopping speaking.
    SpeechStopped {
        call_id: CallId,
        seq: Seq,
        ts_ms: u64,
        duration_ms: u64,
    },

    /// Mid-dialog re-INVITE flipped the audio direction to
    /// something other than `sendrecv` — typically a soft-phone
    /// hold (`sendonly`) or full pause (`inactive`). The server
    /// SHOULD stop sending audio for the duration; the peer isn't
    /// listening. The matching `Resume` event arrives when the
    /// direction returns to `sendrecv`.
    Hold {
        call_id: CallId,
        seq: Seq,
        /// One of `"sendonly"`, `"recvonly"`, `"inactive"` —
        /// mirrors the peer's offered direction per RFC 3264 §6.1.
        direction: String,
    },

    /// Direction returned to `sendrecv` after a [`BridgeOut::Hold`].
    /// The server may resume sending audio.
    Resume { call_id: CallId, seq: Seq },

    /// Caller has been silent (no VAD speech) for at least
    /// `duration_ms`. Configurable via `[bridge].silence_threshold_ms`;
    /// `0` disables emission. Fires once per silence stretch (the
    /// next event only after a speech → silence cycle); a long
    /// silence does not generate a stream of these.
    SilenceDetected {
        call_id: CallId,
        seq: Seq,
        duration_ms: u64,
    },

    /// No audio observed in EITHER direction (no caller VAD speech
    /// AND no outbound playout from the WS server) for at least
    /// `duration_ms`. Suspect connectivity or a hung call.
    /// Configurable via `[bridge].dead_air_threshold_ms`; `0`
    /// disables emission. Fires every time the threshold elapses
    /// without either side producing audio.
    DeadAirDetected {
        call_id: CallId,
        seq: Seq,
        duration_ms: u64,
    },

    /// Periodic snapshot of RTP / RTCP quality, emitted every
    /// `[bridge].rtp_stats_interval_ms` (default 5 s, configurable
    /// per-route; `0` disables). Fields are JSON `null` until forge
    /// has reported its first quality assessment for the call.
    /// Codec / sample rate are constant for the call — consumers
    /// should correlate to the `start` message.
    RtpStats {
        call_id: CallId,
        seq: Seq,
        /// Estimated inter-arrival jitter in milliseconds, or `null`.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        jitter_ms: Option<f32>,
        /// Packet loss as a ratio in `[0.0, 1.0]`, or `null`.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        packet_loss_ratio: Option<f32>,
        /// Mean round-trip time over the reporting window in milliseconds,
        /// or `null` until forge-engine originates its own RTCP SRs
        /// (deferred to 0.3.1 per DEV_PLAN_0.3.0.md §9 decision 10).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        rtcp_rtt_ms: Option<f32>,
    },

    /// The caller pressed a DTMF key.
    Dtmf {
        call_id: CallId,
        seq: Seq,
        /// One of `0-9 * # A B C D`.
        digit: char,
        duration_ms: u32,
        method: DtmfMethod,
    },

    /// Acknowledgement of a server-initiated [`BridgeIn::Mark`]: the audio
    /// queued before the marker has been fully played out into the call.
    Mark {
        call_id: CallId,
        seq: Seq,
        name: String,
    },

    /// A recording has begun (auto on `mode = "always"`, or in response to
    /// [`BridgeIn::StartRecording`]). `recording_id` identifies it for
    /// correlation. Added in 0.5.0.
    RecordingStarted {
        call_id: CallId,
        seq: Seq,
        recording_id: String,
    },

    /// A recording finalized (call ended, or [`BridgeIn::StopRecording`]).
    RecordingStopped {
        call_id: CallId,
        seq: Seq,
        recording_id: String,
    },

    /// A recording could not start or write (e.g. disk error). The call is
    /// unaffected — recording is best-effort.
    RecordingFailed {
        call_id: CallId,
        seq: Seq,
        recording_id: String,
        /// Human-readable reason.
        reason: String,
    },

    /// Last message SiphonAI sends. Followed by a clean WS close (1000).
    Stop {
        call_id: CallId,
        seq: Seq,
        reason: StopReason,
    },

    /// Fatal error. Always followed by `stop { reason: "error" }` and a
    /// clean close.
    Error {
        call_id: CallId,
        seq: Seq,
        code: ErrorCode,
        message: String,
    },
}

/// Body of the [`BridgeOut::Start`] message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartMsg {
    /// Currently `"1"`.
    pub version: String,
    pub call_id: CallId,
    pub seq: Seq,
    /// E.164 number or SIP user from the inbound INVITE; may be empty
    /// if the PBX strips it.
    pub from: String,
    /// Dialed digits / extension / SIP user.
    pub to: String,
    pub direction: Direction,
    pub audio: AudioFormat,
    pub sip: SipMeta,
    /// SRTP profile / key-exchange in use for this call's media leg,
    /// when SRTP was negotiated. `None` means the media is plaintext
    /// `RTP/AVP` (the v0.1.0 / v0.2.0 behaviour, and the default
    /// when `[media].srtp = "off"`).
    ///
    /// The protocol stays at `version = "1"` — `srtp` is `#[serde]`
    /// `skip_serializing_if = "Option::is_none"`, so a 0.1.x / 0.2.x
    /// WS server's parser sees the same `start` shape it always
    /// saw. Servers that *want* to know whether the leg is encrypted
    /// just check whether the field is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srtp: Option<SrtpInfo>,
    /// STIR/SHAKEN verification verdict (RFC 8224/8225) for this inbound
    /// call, when `[security.stir_shaken].enabled`. `None` — and omitted
    /// from the wire — when call authentication is off (the default) or no
    /// `Identity` header was processed.
    ///
    /// The shape is [`siphon_ai_security::VerificationResult`], reused
    /// verbatim so the wire format and the internal verdict can't drift.
    /// `attest` is trustworthy only when the booleans all hold; servers
    /// applying their own policy should treat a present-but-failed verdict
    /// (e.g. `signature_valid: false`) as untrusted. Like `srtp`, this is
    /// `skip_serializing_if = "Option::is_none"`, so a v1 server built
    /// before 0.4.0 sees the exact `start` shape it always saw.
    ///
    /// Boxed so the (already large) `Start` variant of [`BridgeOut`] doesn't
    /// grow by another ~64 bytes for a field that's `None` on most calls.
    /// `Box<T>` is serde-transparent — the wire JSON is identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verstat: Option<Box<VerificationResult>>,
}

/// SRTP details surfaced on [`StartMsg::srtp`].
///
/// W1 ships the type; the field stays `None` on real calls until
/// the Sprint 1 Week 2 / 3 wiring lands. Defined now so the WS
/// protocol shape is pinned before any code path produces a
/// non-`None` value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SrtpInfo {
    /// How the SRTP master key was negotiated.
    pub exchange: SrtpExchange,
    /// The negotiated SRTP crypto suite identifier, exactly as it
    /// appears on the wire (`a=crypto:` `crypto-suite` token for
    /// SDES; the DTLS-SRTP profile name for DTLS-SRTP). Examples:
    /// `"AES_CM_128_HMAC_SHA1_80"`, `"AES_256_CM_HMAC_SHA1_80"`,
    /// `"AEAD_AES_256_GCM"`.
    ///
    /// String rather than enum because new suites land at the IANA
    /// registry independent of our release cadence — we'd rather
    /// pass through an unrecognised suite name than block negotiation
    /// on a missing variant.
    pub profile: String,
}

/// Key-exchange family that produced [`SrtpInfo::profile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SrtpExchange {
    /// RFC 4568 SDES — master key exchanged via `a=crypto:` on the
    /// SIP signalling plane. Used by classic SIP carriers
    /// (Twilio Elastic SIP Trunk Secure Media etc).
    Sdes,
    /// RFC 5764 DTLS-SRTP — master key derived from a DTLS handshake
    /// over the media path, with fingerprint authenticated via SIP
    /// `a=fingerprint:`. Used by WebRTC-side peers.
    Dtls,
}

/// Audio format declaration. Fixed for the lifetime of the call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioFormat {
    pub encoding: AudioEncoding,
    /// `8000` or `16000` in v1.
    pub sample_rate: u32,
    /// `1` only in v1 (mono).
    pub channels: u8,
    /// `20` only in v1.
    pub frame_ms: u32,
}

/// SIP-side metadata forwarded on the `start` message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SipMeta {
    /// The SIP `Call-ID` from the inbound INVITE.
    pub call_id: String,
    /// Selected SIP headers, configured via `bridge.forward_headers`.
    /// Servers MUST NOT assume any specific header is present.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// The only valid direction in v1.
    Inbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioEncoding {
    Pcm16le,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DtmfMethod {
    Rfc2833,
    Inband,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    CallerHangup,
    ServerHangup,
    Transfer,
    WsDisconnect,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    RtpTimeout,
    CodecUnsupported,
    AudioFormat,
    ProtocolError,
    ServerTooSlow,
    TransferFailed,
    Internal,
}

// ============================================================================
// Server → SiphonAI (BridgeIn)
// ============================================================================

/// Messages a WebSocket server sends to SiphonAI.
///
/// Servers MUST include `call_id` matching the value SiphonAI sent in
/// `start`. Servers MUST NOT include `seq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeIn {
    /// Discard any audio queued for playout but not yet sent into the
    /// call. Used for barge-in.
    Clear { call_id: CallId },

    /// Insert a marker at the current tail of the outbound queue. When
    /// the marker reaches the head, SiphonAI emits a [`BridgeOut::Mark`]
    /// with the same `name`.
    Mark {
        call_id: CallId,
        /// Server-chosen, opaque to SiphonAI. ASCII, ≤64 chars.
        name: String,
    },

    /// Terminate the call.
    Hangup {
        call_id: CallId,
        #[serde(default)]
        cause: HangupCause,
    },

    /// Initiate a blind transfer (REFER) to `target`.
    Transfer {
        call_id: CallId,
        /// MUST be a SIP or SIPS URI.
        target: String,
    },

    /// Generate an RFC 2833 DTMF event toward the caller.
    SendDtmf {
        call_id: CallId,
        /// One of `0-9 * # A B C D`.
        digit: char,
        /// Clamped to `[40, 2000]` ms by SiphonAI.
        duration_ms: u32,
    },

    /// Suspend AI-side playout to the caller until a matching
    /// [`BridgeIn::Unmute`] arrives. SiphonAI drops audio bytes the
    /// WS server keeps streaming during the mute, AND flushes audio
    /// already queued into the media engine — so the caller hears
    /// silence immediately, not after the queued tail plays out.
    ///
    /// Distinct from [`BridgeIn::Clear`], which is a one-shot
    /// barge-in flush. `Mute` is sustained and `Unmute` is required
    /// to resume.
    Mute { call_id: CallId },

    /// Resume AI-side playout after a [`BridgeIn::Mute`]. A no-op
    /// if the call is not muted.
    Unmute { call_id: CallId },

    /// Begin recording this call (when `[recording].mode = "on_demand"`).
    /// No-op if recording is off for the call or already in progress.
    /// SiphonAI replies with [`BridgeOut::RecordingStarted`] (or
    /// [`BridgeOut::RecordingFailed`]).
    StartRecording { call_id: CallId },

    /// Finalize the recording now (close the file early). SiphonAI replies
    /// with [`BridgeOut::RecordingStopped`].
    StopRecording { call_id: CallId },

    /// Suspend recording — the paused span is **omitted** from the file
    /// (e.g. while the caller reads a card number), not silenced. No-op if
    /// not recording.
    PauseRecording { call_id: CallId },

    /// Resume recording after a [`BridgeIn::PauseRecording`].
    ResumeRecording { call_id: CallId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HangupCause {
    /// BYE on an established dialog, or 487 on an early dialog.
    #[default]
    Normal,
    /// 603 Decline (the call hasn't been answered).
    Rejected,
    /// 486 Busy Here.
    Busy,
    /// 488 Not Acceptable Here.
    NotAcceptable,
}

#[cfg(test)]
mod tests {
    //! Wire-format round-trip tests.
    //!
    //! Every canonical example from `docs/PROTOCOL.md` MUST appear here as
    //! a literal JSON string. If the spec doc and this module disagree,
    //! one of them is wrong — fix both in the same PR (CLAUDE.md §4.2).

    use super::*;
    use serde_json::{json, Value};

    /// Parse `s` as JSON, then assert it round-trips: deserialize to `T`,
    /// re-serialize, and confirm the resulting JSON is structurally equal
    /// to the input. Returns the parsed value for variant assertions.
    fn assert_round_trip<T>(s: &str) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let parsed: T = serde_json::from_str(s).expect("deserialize");
        let reserialized = serde_json::to_string(&parsed).expect("serialize");
        let original_value: Value = serde_json::from_str(s).expect("input is valid JSON");
        let round_trip_value: Value =
            serde_json::from_str(&reserialized).expect("reserialized is valid JSON");
        assert_eq!(
            original_value, round_trip_value,
            "round-trip mismatch:\n  in  = {original_value}\n  out = {round_trip_value}"
        );
        parsed
    }

    // ─── BridgeOut ──────────────────────────────────────────────────────

    #[test]
    fn bridge_out_start() {
        let raw = r#"{
          "type": "start",
          "version": "1",
          "call_id": "siphon-7f3a9b21",
          "seq": 0,
          "from": "+13125551212",
          "to": "5000",
          "direction": "inbound",
          "audio": {
            "encoding": "pcm16le",
            "sample_rate": 8000,
            "channels": 1,
            "frame_ms": 20
          },
          "sip": {
            "call_id": "abc123@pbx.example.com",
            "headers": {
              "User-Agent": "Cisco-CP8841"
            }
          }
        }"#;

        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::Start(start) = msg else {
            panic!("expected Start variant");
        };
        assert_eq!(start.version, PROTOCOL_VERSION);
        assert_eq!(start.call_id.as_str(), "siphon-7f3a9b21");
        assert_eq!(start.seq, 0);
        assert_eq!(start.from, "+13125551212");
        assert_eq!(start.to, "5000");
        assert_eq!(start.direction, Direction::Inbound);
        assert_eq!(start.audio.encoding, AudioEncoding::Pcm16le);
        assert_eq!(start.audio.sample_rate, 8000);
        assert_eq!(start.audio.channels, 1);
        assert_eq!(start.audio.frame_ms, 20);
        assert_eq!(start.sip.call_id, "abc123@pbx.example.com");
        assert_eq!(
            start.sip.headers.get("User-Agent").map(String::as_str),
            Some("Cisco-CP8841")
        );
    }

    #[test]
    fn bridge_out_start_omits_headers_when_absent() {
        // sip.headers is optional; missing in JSON → empty map.
        let raw = r#"{
          "type": "start",
          "version": "1",
          "call_id": "c",
          "seq": 0,
          "from": "",
          "to": "5000",
          "direction": "inbound",
          "audio": { "encoding": "pcm16le", "sample_rate": 16000, "channels": 1, "frame_ms": 20 },
          "sip": { "call_id": "x@y" }
        }"#;
        let msg: BridgeOut = serde_json::from_str(raw).expect("deserialize");
        let BridgeOut::Start(start) = msg else {
            panic!("expected Start")
        };
        assert!(start.sip.headers.is_empty());
    }

    #[test]
    fn bridge_out_speech_started() {
        let raw = r#"{ "type": "speech_started", "call_id": "c", "seq": 42, "ts_ms": 1234 }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        assert!(matches!(
            msg,
            BridgeOut::SpeechStarted { ref call_id, seq: 42, ts_ms: 1234 } if call_id.as_str() == "c"
        ));
    }

    #[test]
    fn bridge_out_speech_stopped() {
        let raw = r#"{ "type": "speech_stopped", "call_id": "c", "seq": 67, "ts_ms": 1890, "duration_ms": 656 }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        assert!(matches!(
            msg,
            BridgeOut::SpeechStopped {
                seq: 67,
                ts_ms: 1890,
                duration_ms: 656,
                ..
            }
        ));
    }

    #[test]
    fn bridge_out_silence_detected() {
        let raw =
            r#"{ "type": "silence_detected", "call_id": "c", "seq": 12, "duration_ms": 3000 }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        assert!(matches!(
            msg,
            BridgeOut::SilenceDetected {
                seq: 12,
                duration_ms: 3000,
                ..
            }
        ));
    }

    #[test]
    fn bridge_out_rtp_stats_with_values() {
        let raw = r#"{ "type": "rtp_stats", "call_id": "c", "seq": 50, "jitter_ms": 12.5, "packet_loss_ratio": 0.004, "rtcp_rtt_ms": 42.0 }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::RtpStats {
            jitter_ms,
            packet_loss_ratio,
            rtcp_rtt_ms,
            ..
        } = msg
        else {
            panic!("expected RtpStats");
        };
        assert_eq!(jitter_ms, Some(12.5));
        assert_eq!(packet_loss_ratio, Some(0.004));
        assert_eq!(rtcp_rtt_ms, Some(42.0));
    }

    #[test]
    fn bridge_out_rtp_stats_with_nulls() {
        // First snapshot before any RTCP report has arrived: all three
        // fields are absent / null. Test deserialize-only because
        // skip_serializing_if drops them on the way back out.
        let raw = r#"{ "type": "rtp_stats", "call_id": "c", "seq": 1 }"#;
        let msg: BridgeOut = serde_json::from_str(raw).expect("deserialize");
        let BridgeOut::RtpStats {
            jitter_ms,
            packet_loss_ratio,
            rtcp_rtt_ms,
            ..
        } = msg
        else {
            panic!("expected RtpStats");
        };
        assert!(jitter_ms.is_none());
        assert!(packet_loss_ratio.is_none());
        assert!(rtcp_rtt_ms.is_none());
    }

    #[test]
    fn bridge_out_rtp_stats_jitter_loss_without_rtt() {
        // Common shape pre-0.3.1: jitter and loss populate from
        // RtcpReportReceived, but rtt_ms stays None until forge
        // originates its own SRs. Verify the field is *omitted*
        // (not present as JSON null) to match skip_serializing_if.
        let msg = BridgeOut::RtpStats {
            call_id: CallId("c".to_string()),
            seq: 7,
            jitter_ms: Some(11.2),
            packet_loss_ratio: Some(0.012),
            rtcp_rtt_ms: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("jitter_ms"));
        assert!(obj.contains_key("packet_loss_ratio"));
        assert!(
            !obj.contains_key("rtcp_rtt_ms"),
            "rtcp_rtt_ms must be absent (not null) when None"
        );
    }

    #[test]
    fn bridge_out_dead_air_detected() {
        let raw =
            r#"{ "type": "dead_air_detected", "call_id": "c", "seq": 13, "duration_ms": 10000 }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        assert!(matches!(
            msg,
            BridgeOut::DeadAirDetected {
                seq: 13,
                duration_ms: 10000,
                ..
            }
        ));
    }

    #[test]
    fn bridge_out_dtmf_rfc2833() {
        let raw = r#"{ "type": "dtmf", "call_id": "c", "seq": 88, "digit": "5", "duration_ms": 120, "method": "rfc2833" }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::Dtmf {
            digit,
            duration_ms,
            method,
            ..
        } = msg
        else {
            panic!("expected Dtmf variant");
        };
        assert_eq!(digit, '5');
        assert_eq!(duration_ms, 120);
        assert_eq!(method, DtmfMethod::Rfc2833);
    }

    #[test]
    fn bridge_out_dtmf_inband() {
        let raw = r##"{ "type": "dtmf", "call_id": "c", "seq": 1, "digit": "#", "duration_ms": 80, "method": "inband" }"##;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::Dtmf { digit, method, .. } = msg else {
            panic!("expected Dtmf");
        };
        assert_eq!(digit, '#');
        assert_eq!(method, DtmfMethod::Inband);
    }

    #[test]
    fn bridge_out_mark() {
        let raw = r#"{ "type": "mark", "call_id": "c", "seq": 91, "name": "greeting_done" }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::Mark { name, .. } = msg else {
            panic!("expected Mark");
        };
        assert_eq!(name, "greeting_done");
    }

    #[test]
    fn bridge_out_stop_all_reasons() {
        for (wire, expected) in [
            ("caller_hangup", StopReason::CallerHangup),
            ("server_hangup", StopReason::ServerHangup),
            ("transfer", StopReason::Transfer),
            ("ws_disconnect", StopReason::WsDisconnect),
            ("error", StopReason::Error),
        ] {
            let raw =
                format!(r#"{{ "type": "stop", "call_id": "c", "seq": 200, "reason": "{wire}" }}"#);
            let msg: BridgeOut = assert_round_trip(&raw);
            let BridgeOut::Stop { reason, .. } = msg else {
                panic!("expected Stop variant for reason {wire}");
            };
            assert_eq!(reason, expected, "reason {wire} mismatched");
        }
    }

    #[test]
    fn bridge_out_error_all_codes() {
        for (wire, expected) in [
            ("rtp_timeout", ErrorCode::RtpTimeout),
            ("codec_unsupported", ErrorCode::CodecUnsupported),
            ("audio_format", ErrorCode::AudioFormat),
            ("protocol_error", ErrorCode::ProtocolError),
            ("server_too_slow", ErrorCode::ServerTooSlow),
            ("transfer_failed", ErrorCode::TransferFailed),
            ("internal", ErrorCode::Internal),
        ] {
            let raw = format!(
                r#"{{ "type": "error", "call_id": "c", "seq": 1, "code": "{wire}", "message": "x" }}"#
            );
            let msg: BridgeOut = assert_round_trip(&raw);
            let BridgeOut::Error { code, .. } = msg else {
                panic!("expected Error variant for code {wire}");
            };
            assert_eq!(code, expected, "code {wire} mismatched");
        }
    }

    // ─── BridgeIn ───────────────────────────────────────────────────────

    #[test]
    fn bridge_in_clear() {
        let raw = r#"{ "type": "clear", "call_id": "c" }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        assert!(matches!(msg, BridgeIn::Clear { ref call_id } if call_id.as_str() == "c"));
    }

    #[test]
    fn bridge_in_mark() {
        let raw = r#"{ "type": "mark", "call_id": "c", "name": "greeting_done" }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        let BridgeIn::Mark { name, .. } = msg else {
            panic!("expected Mark");
        };
        assert_eq!(name, "greeting_done");
    }

    #[test]
    fn bridge_in_hangup_explicit_cause() {
        for (wire, expected) in [
            ("normal", HangupCause::Normal),
            ("rejected", HangupCause::Rejected),
            ("busy", HangupCause::Busy),
            ("not_acceptable", HangupCause::NotAcceptable),
        ] {
            let raw = format!(r#"{{ "type": "hangup", "call_id": "c", "cause": "{wire}" }}"#);
            let msg: BridgeIn = assert_round_trip(&raw);
            let BridgeIn::Hangup { cause, .. } = msg else {
                panic!("expected Hangup for {wire}");
            };
            assert_eq!(cause, expected);
        }
    }

    #[test]
    fn bridge_in_hangup_default_cause_when_field_absent() {
        // cause is optional, defaults to Normal.
        let raw = r#"{ "type": "hangup", "call_id": "c" }"#;
        let msg: BridgeIn = serde_json::from_str(raw).expect("deserialize");
        let BridgeIn::Hangup { cause, .. } = msg else {
            panic!("expected Hangup");
        };
        assert_eq!(cause, HangupCause::Normal);
    }

    #[test]
    fn bridge_in_transfer() {
        let raw = r#"{ "type": "transfer", "call_id": "c", "target": "sip:agent@example.com" }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        let BridgeIn::Transfer { target, .. } = msg else {
            panic!("expected Transfer");
        };
        assert_eq!(target, "sip:agent@example.com");
    }

    #[test]
    fn bridge_in_send_dtmf() {
        let raw = r#"{ "type": "send_dtmf", "call_id": "c", "digit": "1", "duration_ms": 200 }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        let BridgeIn::SendDtmf {
            digit, duration_ms, ..
        } = msg
        else {
            panic!("expected SendDtmf");
        };
        assert_eq!(digit, '1');
        assert_eq!(duration_ms, 200);
    }

    #[test]
    fn bridge_in_mute() {
        let raw = r#"{ "type": "mute", "call_id": "c" }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        assert!(matches!(msg, BridgeIn::Mute { ref call_id } if call_id.as_str() == "c"));
    }

    #[test]
    fn bridge_in_unmute() {
        let raw = r#"{ "type": "unmute", "call_id": "c" }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        assert!(matches!(msg, BridgeIn::Unmute { ref call_id } if call_id.as_str() == "c"));
    }

    #[test]
    fn bridge_in_recording_controls() {
        for (wire, ok) in [
            (
                "start_recording",
                matches!(
                    serde_json::from_str::<BridgeIn>(r#"{"type":"start_recording","call_id":"c"}"#)
                        .unwrap(),
                    BridgeIn::StartRecording { .. }
                ),
            ),
            (
                "stop_recording",
                matches!(
                    serde_json::from_str::<BridgeIn>(r#"{"type":"stop_recording","call_id":"c"}"#)
                        .unwrap(),
                    BridgeIn::StopRecording { .. }
                ),
            ),
            (
                "pause_recording",
                matches!(
                    serde_json::from_str::<BridgeIn>(r#"{"type":"pause_recording","call_id":"c"}"#)
                        .unwrap(),
                    BridgeIn::PauseRecording { .. }
                ),
            ),
            (
                "resume_recording",
                matches!(
                    serde_json::from_str::<BridgeIn>(
                        r#"{"type":"resume_recording","call_id":"c"}"#
                    )
                    .unwrap(),
                    BridgeIn::ResumeRecording { .. }
                ),
            ),
        ] {
            assert!(ok, "{wire} did not parse to its variant");
            let raw = format!(r#"{{ "type": "{wire}", "call_id": "c" }}"#);
            let _: BridgeIn = assert_round_trip(&raw);
        }
    }

    #[test]
    fn bridge_out_recording_started_stopped() {
        let started: BridgeOut = assert_round_trip(
            r#"{ "type": "recording_started", "call_id": "c", "seq": 5, "recording_id": "c" }"#,
        );
        assert!(matches!(started, BridgeOut::RecordingStarted { .. }));
        let stopped: BridgeOut = assert_round_trip(
            r#"{ "type": "recording_stopped", "call_id": "c", "seq": 6, "recording_id": "c" }"#,
        );
        assert!(matches!(stopped, BridgeOut::RecordingStopped { .. }));
    }

    #[test]
    fn bridge_out_recording_failed() {
        let raw = r#"{ "type": "recording_failed", "call_id": "c", "seq": 7, "recording_id": "c", "reason": "disk full" }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::RecordingFailed { reason, .. } = msg else {
            panic!("expected RecordingFailed");
        };
        assert_eq!(reason, "disk full");
    }

    // ─── Negative cases ─────────────────────────────────────────────────

    #[test]
    fn unknown_bridge_in_type_fails() {
        let raw = r#"{ "type": "yodel", "call_id": "c" }"#;
        let err = serde_json::from_str::<BridgeIn>(raw).unwrap_err();
        // Per spec §4: unknown `type` triggers protocol_error. The Rust
        // type rejects it; the WS handler is responsible for translating
        // the deserialize failure into BridgeOut::Error.
        assert!(err.to_string().contains("yodel") || err.is_data());
    }

    #[test]
    fn unknown_stop_reason_fails() {
        let raw = r#"{ "type": "stop", "call_id": "c", "seq": 1, "reason": "asteroid" }"#;
        let err = serde_json::from_str::<BridgeOut>(raw).unwrap_err();
        assert!(err.is_data(), "want a data error, got {err}");
    }

    #[test]
    fn missing_required_seq_fails() {
        // BridgeOut messages require seq.
        let raw = r#"{ "type": "speech_started", "call_id": "c", "ts_ms": 1 }"#;
        let err = serde_json::from_str::<BridgeOut>(raw).unwrap_err();
        assert!(
            err.to_string().contains("seq"),
            "expected seq error, got {err}"
        );
    }

    // ─── Constants ──────────────────────────────────────────────────────

    #[test]
    fn protocol_version_matches_spec() {
        assert_eq!(PROTOCOL_VERSION, "1");
    }

    #[test]
    fn ws_subprotocol_matches_spec() {
        assert_eq!(WS_SUBPROTOCOL, "siphon-ai.v1");
    }

    // ─── Cross-check: serialized field ordering doesn't matter ──────────

    #[test]
    fn json_field_order_does_not_matter() {
        // Reorder the keys; result must be identical.
        let a = serde_json::from_str::<BridgeIn>(
            r#"{ "type": "send_dtmf", "call_id": "c", "digit": "1", "duration_ms": 200 }"#,
        )
        .unwrap();
        let b = serde_json::from_str::<BridgeIn>(
            r#"{ "duration_ms": 200, "digit": "1", "call_id": "c", "type": "send_dtmf" }"#,
        )
        .unwrap();
        assert_eq!(a, b);
    }

    // ─── Audio frame size invariants (documentation, not a parser) ──────

    #[test]
    fn audio_frame_byte_sizes_match_spec() {
        // Spec §2.2: 8000 Hz / 20 ms = 160 samples, 320 bytes.
        // Spec §2.2: 16000 Hz / 20 ms = 320 samples, 640 bytes.
        // PCM16LE = 2 bytes per sample, mono = 1 channel.
        for (rate, expected_samples, expected_bytes) in
            [(8000u32, 160u32, 320u32), (16000, 320, 640)]
        {
            let samples = rate * 20 / 1000;
            let bytes = samples * 2;
            assert_eq!(samples, expected_samples, "samples for {rate} Hz");
            assert_eq!(bytes, expected_bytes, "bytes for {rate} Hz");
        }
    }

    // ─── Sanity: a freshly built Start serializes with the expected key set ──

    #[test]
    fn start_msg_serializes_with_expected_keys() {
        let start = BridgeOut::Start(StartMsg {
            version: PROTOCOL_VERSION.to_string(),
            call_id: CallId::new("siphon-1"),
            seq: 0,
            from: "+1".into(),
            to: "5000".into(),
            direction: Direction::Inbound,
            audio: AudioFormat {
                encoding: AudioEncoding::Pcm16le,
                sample_rate: 8000,
                channels: 1,
                frame_ms: 20,
            },
            sip: SipMeta {
                call_id: "x@y".into(),
                headers: HashMap::new(),
            },
            srtp: None,
            verstat: None,
        });
        let v: Value = serde_json::to_value(&start).unwrap();
        let obj = v.as_object().unwrap();
        for key in [
            "type",
            "version",
            "call_id",
            "seq",
            "from",
            "to",
            "direction",
            "audio",
            "sip",
        ] {
            assert!(
                obj.contains_key(key),
                "Start serialization missing key {key}"
            );
        }
        assert_eq!(obj["type"], json!("start"));
        assert_eq!(obj["version"], json!("1"));
    }

    /// Two contracts in one test:
    /// 1. When `srtp` is `None` the field is **absent** from the
    ///    JSON, not present-as-null. A 0.1.x / 0.2.x WS server
    ///    parsing the message must see exactly the same shape it
    ///    always saw — otherwise we've made the "protocol stays
    ///    at v1" claim a lie.
    /// 2. When `srtp` is `Some(SrtpInfo)`, the wire format is
    ///    `{ "exchange": "sdes" | "dtls", "profile": "<string>" }`
    ///    and round-trips cleanly.
    #[test]
    fn start_srtp_field_serialization() {
        // Skeleton reused for both cases.
        let mk = |srtp: Option<SrtpInfo>| {
            BridgeOut::Start(StartMsg {
                version: PROTOCOL_VERSION.to_string(),
                call_id: CallId::new("c"),
                seq: 0,
                from: "+1".into(),
                to: "5000".into(),
                direction: Direction::Inbound,
                audio: AudioFormat {
                    encoding: AudioEncoding::Pcm16le,
                    sample_rate: 8000,
                    channels: 1,
                    frame_ms: 20,
                },
                sip: SipMeta {
                    call_id: "x@y".into(),
                    headers: HashMap::new(),
                },
                srtp,
                verstat: None,
            })
        };

        // (1) None ⇒ field absent. The v1 contract.
        let v: Value = serde_json::to_value(mk(None)).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("srtp"),
            "srtp must be absent from JSON when None (skip_serializing_if), \
             got payload: {v}"
        );

        // (2) Some ⇒ field present + round-trips.
        let info = SrtpInfo {
            exchange: SrtpExchange::Sdes,
            profile: "AES_CM_128_HMAC_SHA1_80".into(),
        };
        let v: Value = serde_json::to_value(mk(Some(info.clone()))).unwrap();
        assert_eq!(v["srtp"]["exchange"], json!("sdes"));
        assert_eq!(v["srtp"]["profile"], json!("AES_CM_128_HMAC_SHA1_80"));
        let round: BridgeOut = serde_json::from_value(v).unwrap();
        match round {
            BridgeOut::Start(s) => assert_eq!(s.srtp, Some(info)),
            other => panic!("expected Start, got {other:?}"),
        }

        // (3) DTLS exchange serialises to "dtls" (rename_all = snake_case).
        let info = SrtpInfo {
            exchange: SrtpExchange::Dtls,
            profile: "SRTP_AES128_CM_SHA1_80".into(),
        };
        let v: Value = serde_json::to_value(mk(Some(info))).unwrap();
        assert_eq!(v["srtp"]["exchange"], json!("dtls"));
    }

    /// `verstat` mirrors the `srtp` contract: absent when `None`, and a
    /// stable wire shape (attest letter + the four booleans, optionals
    /// skipped when empty) when present.
    #[test]
    fn start_verstat_field_serialization() {
        use siphon_ai_security::{AttestationLevel, VerificationResult};

        let mk = |verstat: Option<VerificationResult>| {
            BridgeOut::Start(StartMsg {
                version: PROTOCOL_VERSION.to_string(),
                call_id: CallId::new("c"),
                seq: 0,
                from: "+1".into(),
                to: "5000".into(),
                direction: Direction::Inbound,
                audio: AudioFormat {
                    encoding: AudioEncoding::Pcm16le,
                    sample_rate: 8000,
                    channels: 1,
                    frame_ms: 20,
                },
                sip: SipMeta {
                    call_id: "x@y".into(),
                    headers: HashMap::new(),
                },
                srtp: None,
                verstat: verstat.map(Box::new),
            })
        };

        // (1) None ⇒ field absent. The v1 contract.
        let v: Value = serde_json::to_value(mk(None)).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("verstat"),
            "verstat must be absent from JSON when None, got: {v}"
        );

        // (2) Some ⇒ present, correct shape, round-trips.
        let verdict = VerificationResult {
            attest: Some(AttestationLevel::A),
            orig_tn: Some("+12155551212".into()),
            orig_passed: true,
            dest_passed: true,
            cert_chain_valid: true,
            signature_valid: true,
            iat_passed: true,
            error: None,
        };
        let v: Value = serde_json::to_value(mk(Some(verdict.clone()))).unwrap();
        assert_eq!(v["verstat"]["attest"], json!("A"));
        assert_eq!(v["verstat"]["orig_tn"], json!("+12155551212"));
        assert_eq!(v["verstat"]["signature_valid"], json!(true));
        assert_eq!(v["verstat"]["iat_passed"], json!(true));
        // error is None → omitted; attest present → no null.
        assert!(!v["verstat"].as_object().unwrap().contains_key("error"));
        let round: BridgeOut = serde_json::from_value(v).unwrap();
        match round {
            BridgeOut::Start(s) => assert_eq!(s.verstat, Some(Box::new(verdict))),
            other => panic!("expected Start, got {other:?}"),
        }

        // (3) A failed verdict still serialises its booleans (untrusted but
        //     surfaced) — `attest` omitted when the claim was absent.
        let failed = VerificationResult::unsigned();
        let v: Value = serde_json::to_value(mk(Some(failed))).unwrap();
        assert_eq!(v["verstat"]["signature_valid"], json!(false));
        assert!(!v["verstat"].as_object().unwrap().contains_key("attest"));
    }
}
