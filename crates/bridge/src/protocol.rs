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
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
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
        /// `true` when this event armed a pause-mode barge-in
        /// arbitration (0.32.0, `[bridge.barge_in].mode = "pause"`):
        /// playout is paused with its tail retained, and SiphonAI
        /// expects [`BridgeIn::BargeInConfirm`] /
        /// [`BridgeIn::BargeInReject`] within `decision_deadline_ms`.
        /// Additive — omitted (and `false`) in every other mode, so
        /// pre-0.32.0 servers see the exact shape they always saw.
        #[serde(default, skip_serializing_if = "is_false")]
        decision_pending: bool,
        /// Milliseconds the server has to rule before
        /// `[bridge.barge_in].on_timeout` applies. Present exactly
        /// when `decision_pending` is `true`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision_deadline_ms: Option<u64>,
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
    ///
    /// Two viewpoints ride together (0.30.0): the original three fields
    /// are **remote-reported** (RTCP RRs: how the far end receives the
    /// stream SiphonAI sends), while the `rx_*` fields are **locally
    /// measured** on the stream SiphonAI receives from the caller. A
    /// congested path is often asymmetric — compare the two sides.
    ///
    /// The `tx_*` fields (0.38.0) complete the picture: `tx_packets_sent`
    /// / `tx_octets_sent` are locally measured on what SiphonAI put on
    /// the wire, and `tx_packets_lost_reported` is the far end's own
    /// absolute loss total for that stream. Before these existed the
    /// transmit direction had only ratios, so "the outbound leg was
    /// clean" could never be backed by a packet count.
    RtpStats {
        call_id: CallId,
        seq: Seq,
        /// Estimated inter-arrival jitter in milliseconds, or `null`.
        /// Remote-reported (RTCP RR): describes SiphonAI→caller audio.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        jitter_ms: Option<f32>,
        /// Packet loss as a ratio in `[0.0, 1.0]`, or `null`.
        /// Remote-reported (RTCP RR): describes SiphonAI→caller audio.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        packet_loss_ratio: Option<f32>,
        /// Mean round-trip time over the reporting window in milliseconds,
        /// or `null` until forge-engine originates its own RTCP SRs
        /// (deferred to 0.3.1 per DEV_PLAN_0.3.0.md §9 decision 10).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        rtcp_rtt_ms: Option<f32>,
        /// Locally-measured interarrival jitter (RFC 3550 §6.4.1) on the
        /// caller→SiphonAI stream, in milliseconds. `null` until the
        /// first local media-stats snapshot (0.30.0).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        rx_jitter_ms: Option<f32>,
        /// Unique RTP packets received from the caller since call start
        /// (duplicates excluded). Cumulative, or `null` (0.30.0).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        rx_packets_received: Option<u64>,
        /// Packets lost in transit on the caller→SiphonAI stream
        /// (sequence-gap count; late arrivals repair it, so it can
        /// shrink between snapshots). Cumulative, or `null` (0.30.0).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        rx_packets_lost: Option<u64>,
        /// Packets that arrived after a newer sequence number had
        /// already been seen. Cumulative, or `null` (0.30.0).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        rx_packets_out_of_order: Option<u64>,
        /// Re-receives of a recently seen sequence number. Cumulative,
        /// or `null` (0.30.0).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        rx_packets_duplicate: Option<u64>,
        /// RTP packets SiphonAI transmitted toward the caller (WS-server
        /// audio played out, plus injected RFC 2833 DTMF), **cumulative
        /// since call start**. `null` until the first local media-stats
        /// snapshot (0.38.0).
        ///
        /// This is the denominator `packet_loss_ratio` never had: the
        /// remote-reported loss fields describe *this* stream.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        tx_packets_sent: Option<u64>,
        /// RTP *payload* octets transmitted toward the caller, excluding
        /// RTP headers and SRTP overhead (RFC 3550 §6.4.1
        /// sender-octet-count basis). Cumulative, or `null` (0.38.0).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        tx_octets_sent: Option<u64>,
        /// The far end's own **absolute** count of packets it lost on the
        /// SiphonAI→caller stream, taken from the latest RTCP RR's
        /// cumulative-lost field (RFC 3550 §6.4.1). `null` until the
        /// first RR arrives (0.38.0).
        ///
        /// May be **negative** — RFC 3550 defines the field as signed
        /// because duplicates can push the far end's packets-received
        /// past packets-expected. Consumers should parse it as a signed
        /// integer and not clamp: a negative value is real information
        /// (a duplicating path), not an error.
        ///
        /// Contrast `packet_loss_ratio`, which comes from the RR's
        /// `fraction_lost` and covers only the interval since the
        /// previous report. Pair this field with `tx_packets_sent` for a
        /// whole-call loss rate that reconciles against a carrier's own
        /// cumulative figure.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        tx_packets_lost_reported: Option<i64>,
        /// Transport-only MOS-CQE estimate in `[1.0, 5.0]` (simplified
        /// E-model from local RX jitter/loss plus RTCP RTT — the same
        /// math heplify-server applies to HEP QoS chunks, so Homer-side
        /// and WS-side numbers agree). Reflects transport impairment
        /// only, not codec or content quality. `null` until enough
        /// inputs exist (0.30.0).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        mos_estimate: Option<f32>,
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

    /// This call successfully joined a conference room, in response to
    /// the server's [`BridgeIn::ConferenceJoin`] (0.7.0). The call's
    /// audio is now mixed into the room; the bot hears the room minus
    /// its own playout. `participants` is the member-call count at the
    /// moment of joining (this call included) — a snapshot; subsequent
    /// changes arrive as [`BridgeOut::ParticipantJoined`] /
    /// [`BridgeOut::ParticipantLeft`].
    ConferenceJoined {
        call_id: CallId,
        seq: Seq,
        room_id: String,
        participants: usize,
    },

    /// This call left a conference room — either because the server
    /// sent [`BridgeIn::ConferenceLeave`] (`reason = "left"`) or
    /// because the room ended underneath it (`reason = "room_closed"`,
    /// e.g. an operator force-ended the room). The direct caller↔WS
    /// audio pair is restored; the call continues. Added in 0.7.0.
    ConferenceLeft {
        call_id: CallId,
        seq: Seq,
        room_id: String,
        reason: ConferenceLeftReason,
    },

    /// ANOTHER call joined the room this call is in (0.7.0; fan-out to
    /// every other member). `participant_call_id` is the bridge
    /// `call_id` of the call that joined — distinct from the envelope
    /// `call_id`, which is always this receiving session's own call.
    ParticipantJoined {
        call_id: CallId,
        seq: Seq,
        room_id: String,
        participant_call_id: String,
    },

    /// ANOTHER call left the room this call is in (0.7.0). See
    /// [`BridgeOut::ParticipantJoined`] for the `participant_call_id`
    /// vs `call_id` distinction.
    ParticipantLeft {
        call_id: CallId,
        seq: Seq,
        room_id: String,
        participant_call_id: String,
    },

    /// Confirmation that a server-requested [`BridgeIn::Hold`] took
    /// effect (0.7.2): the caller's hold re-INVITE was acknowledged and
    /// they're now on hold music. **Distinct from the peer-initiated
    /// [`BridgeOut::Hold`] event**, which reports that the *far end* held
    /// *us*. A server that sent `hold` waits for this before relying on
    /// the hold; on failure it gets `error { code: "hold_failed" }`.
    Held { call_id: CallId, seq: Seq },

    /// Confirmation that a server-requested [`BridgeIn::Resume`] restored
    /// two-way audio (0.7.2). Mirror of [`BridgeOut::Held`].
    Resumed { call_id: CallId, seq: Seq },

    /// A pause-mode barge-in arbitration resolved (0.32.0). Emitted for
    /// **every** resolution — a server verdict, the decision deadline
    /// (`outcome: "timeout"`, the one case the server can't know about),
    /// or a preempting command (mute / clear / hold / park / announce /
    /// conference join, reported as `"confirmed"`). See
    /// [`BridgeOut::SpeechStarted`]'s `decision_pending` for how an
    /// arbitration arms.
    BargeInResolved {
        call_id: CallId,
        seq: Seq,
        outcome: BargeInOutcome,
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
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
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
    /// `true` when this `start` opens a *retrieve* session on a
    /// previously-parked call (0.7.0, §2.4) — the WS server is picking
    /// the call back up, not handling a fresh inbound one. `seq` still
    /// restarts at 0 (a fresh session, no replay). Additive — omitted
    /// (and `false`) on every non-retrieve `start`, so a pre-0.7.0
    /// server sees the exact shape it always did.
    #[serde(default, skip_serializing_if = "is_false")]
    pub retrieved: bool,
    /// `true` when this `start` *resumes* a call after an unexpected WS
    /// drop (0.7.3, `[bridge].ws_reconnect_enabled`) — SiphonAI re-dialed
    /// the same `ws_url` for the same `call_id`; the server should tear
    /// down whatever handler it still has for this call and treat this
    /// socket as the live one. `seq` restarts at 0 (a fresh session, no
    /// replay of pre-drop audio/events). Distinct from [`retrieved`] (an
    /// operator picking up a *parked* call). Additive — omitted (and
    /// `false`) on every non-reconnect `start`, so a server that ignores
    /// it safely treats the call as brand-new.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reconnected: bool,
    /// W3C trace context for this call's daemon-side trace (0.23.0), when
    /// `[observability.otlp]` is enabled. The same values are sent as
    /// `traceparent` / `tracestate` headers on the WS upgrade request; this
    /// field is the copy for servers whose WS library hides upgrade
    /// headers. A server that continues the trace from either place sees
    /// its own spans nested under the daemon's call trace in one waterfall.
    ///
    /// Like `srtp` / `verstat`, additive: `skip_serializing_if` keeps it
    /// off the wire when OTLP is disabled (the default), so the `start`
    /// shape — and the protocol `version` (`"1"`) — are unchanged for
    /// servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_context: Option<TraceContext>,
    /// The call's resolved barge-in mode (0.32.0) — the per-route merge
    /// of `[bridge.barge_in]` — so servers/SDKs can tell whether
    /// pause-mode verdicts ([`BridgeIn::BargeInConfirm`] /
    /// [`BridgeIn::BargeInReject`]) are expected on this call. Like the
    /// other post-v1 `start` fields, additive: `skip_serializing_if`
    /// keeps it off the wire when unset, and a server that ignores it
    /// behaves exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barge_in_mode: Option<BargeInModeInfo>,
}

/// serde `skip_serializing_if` helper — keep `retrieved: false` off the
/// wire so the `start` shape is byte-identical for non-retrieve calls.
fn is_false(b: &bool) -> bool {
    !*b
}

/// W3C Trace Context (<https://www.w3.org/TR/trace-context/>) surfaced on
/// [`StartMsg::trace_context`] and mirrored as upgrade-request headers.
///
/// Values are produced by the daemon's OTel propagator and passed through
/// verbatim — this crate neither parses nor constructs them (no OTel dep
/// here; `siphon-ai-core` stamps the field from `siphon-ai-telemetry`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
pub struct TraceContext {
    /// `traceparent` header value: `00-<32 hex trace-id>-<16 hex
    /// span-id>-<2 hex flags>`. The trace-id is the whole call's trace;
    /// the span-id is the daemon's call-root span.
    pub traceparent: String,
    /// `tracestate` header value (vendor key/value list). Omitted from the
    /// wire when there is nothing to forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
}

/// SRTP details surfaced on [`StartMsg::srtp`].
///
/// W1 ships the type; the field stays `None` on real calls until
/// the Sprint 1 Week 2 / 3 wiring lands. Defined now so the WS
/// protocol shape is pinned before any code path produces a
/// non-`None` value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
pub struct SipMeta {
    /// The SIP `Call-ID` from the inbound INVITE.
    pub call_id: String,
    /// Selected SIP headers, configured via `bridge.forward_headers`.
    /// Servers MUST NOT assume any specific header is present.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// SiphonAI answered an inbound call. The bot reacts to the caller.
    Inbound,
    /// SiphonAI placed the call (outbound origination, 0.6.0). The bot
    /// drives — it typically speaks first. Additive on the `start` message;
    /// the protocol version stays `"1"`.
    Outbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AudioEncoding {
    Pcm16le,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DtmfMethod {
    Rfc2833,
    Inband,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    CallerHangup,
    ServerHangup,
    Transfer,
    WsDisconnect,
    /// The call was parked (0.7.0): the WS session is being detached but
    /// the call stays alive (caller hears hold music). A later retrieve
    /// opens a *fresh* WS session — this `stop` is the last message on
    /// *this* session, not the end of the call.
    Park,
    Error,
}

/// How a pause-mode barge-in arbitration resolved
/// ([`BridgeOut::BargeInResolved`], 0.32.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BargeInOutcome {
    /// The speech was a real interruption: the retained playout tail
    /// was dropped and the bot stays quiet. Reported for server
    /// [`BridgeIn::BargeInConfirm`] verdicts AND for preempting
    /// commands that moot the arbitration.
    Confirmed,
    /// False positive (cough / backchannel / noise): the retained
    /// tail was re-queued and playout resumed where it stopped.
    Rejected,
    /// No verdict arrived within the decision window;
    /// `[bridge.barge_in].on_timeout` was applied.
    Timeout,
}

/// The call's resolved barge-in mode, announced on
/// [`StartMsg::barge_in_mode`] (0.32.0) so servers and SDKs can tell
/// whether pause-mode verdicts are expected without out-of-band config
/// agreement. Wire values match `[bridge.barge_in].mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BargeInModeInfo {
    /// SiphonAI flushes playout itself on `speech_started`.
    AutoClear,
    /// Events only; the server drives `clear` if it wants a flush.
    /// Also announced when `[bridge.barge_in].enabled = false`.
    NotifyOnly,
    /// Reversible barge-in: `speech_started` may arm an arbitration
    /// (`decision_pending: true`) the server rules on via
    /// [`BridgeIn::BargeInConfirm`] / [`BridgeIn::BargeInReject`].
    Pause,
}

/// Why a [`BridgeOut::ConferenceLeft`] fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ConferenceLeftReason {
    /// The server asked to leave via [`BridgeIn::ConferenceLeave`].
    Left,
    /// The room ended underneath this call (e.g. operator force-end,
    /// or the room task stopped). The call reverts to its direct pair.
    RoomClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    RtpTimeout,
    CodecUnsupported,
    AudioFormat,
    ProtocolError,
    ServerTooSlow,
    TransferFailed,
    /// A [`BridgeIn::ConferenceJoin`] was refused (conferencing
    /// disabled, room/participant cap reached, sample-rate mismatch,
    /// or already joined). The call continues on its direct pair.
    /// Added in 0.7.0.
    ConferenceFailed,
    /// A [`BridgeIn::Park`] was refused (park disabled or
    /// `[park].max_parked` reached). The call continues unparked.
    /// Added in 0.7.0.
    ParkFailed,
    /// A [`BridgeIn::Hold`] or [`BridgeIn::Resume`] re-INVITE was
    /// rejected by the peer, timed out, or lost glare resolution (0.7.2).
    /// The call stays in its prior media state — a failed hold never
    /// drops the call.
    HoldFailed,
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
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
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

    /// Initiate a call transfer (REFER). Without `replaces_call_id`
    /// this is a blind transfer to `target`. With it, an attended
    /// transfer (0.6.1): the REFER carries a `Replaces` parameter
    /// referencing the named consult call's dialog, so the transferee
    /// connects to the party the server already consulted.
    Transfer {
        call_id: CallId,
        /// Refer-To URI (MUST be SIP or SIPS). Required for blind
        /// transfer. Optional with `replaces_call_id` — the default
        /// is the consult dialog's remote target (its Contact), which
        /// is normally what you want; sending it overrides.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        /// Attended transfer: the bridge `call_id` of an ANSWERED
        /// outbound call (the consult leg, placed via
        /// `POST /admin/v1/calls`) that the transferee should replace.
        /// Additive in 0.6.1 — protocol stays version "1".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replaces_call_id: Option<String>,
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

    /// Verdict on a pending pause-mode barge-in arbitration (0.32.0,
    /// `[bridge.barge_in].mode = "pause"`): the caller's speech was a
    /// real interruption — drop the retained playout tail and stay
    /// quiet. Audio the server streamed since the pause plays
    /// immediately. A no-op when no arbitration is pending (verdicts
    /// race with the deadline by nature, so a late one must be
    /// harmless). A [`BridgeIn::Clear`] during a pending arbitration
    /// has the same effect.
    BargeInConfirm { call_id: CallId },

    /// Verdict: the speech was a false positive (cough / backchannel /
    /// noise) — re-queue the retained tail and resume playout where it
    /// stopped (0.32.0). Same no-op semantics as
    /// [`BridgeIn::BargeInConfirm`]. SiphonAI replies with
    /// [`BridgeOut::BargeInResolved`] either way.
    BargeInReject { call_id: CallId },

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

    /// Record the fact that the server captured recording consent
    /// (0.26.0) — e.g. a DTMF "press 1 to consent" or a verbal yes your
    /// bot recognized. SiphonAI stores the note on the call's CDR
    /// (`consent.server`) for the audit trail; it does not gate
    /// recording (use `on_demand` + `start_recording` for gating).
    /// `note` is a short free-form description ("dtmf-1",
    /// "verbal-yes@12s", …), truncated to 256 bytes.
    SetRecordingConsent {
        call_id: CallId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },

    /// Park this call (0.7.0, §2.4): detach the WS session and shelve
    /// the call playing hold music, keeping the SIP dialog + RTP alive.
    /// Self-scoped. SiphonAI replies `stop { reason: "park" }` and
    /// closes this WS; the call is later retrieved (operator action)
    /// onto a fresh WS session. `slot` is an optional human label for
    /// the hold lot. Refused (`error { code: "park_failed" }`, call
    /// continues) when park is disabled or `[park].max_parked` is hit.
    Park {
        call_id: CallId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot: Option<String>,
    },

    /// Join this call into a conference room (0.7.0, §2.1). Creates
    /// the room if it doesn't exist yet (subject to
    /// `[conference]` caps). Self-scoped: a session may only join its
    /// OWN call — cross-call composition (adding another participant)
    /// is the admin API's job (§9.2). SiphonAI replies with
    /// [`BridgeOut::ConferenceJoined`] or
    /// [`BridgeOut::Error`]`{ code: "conference_failed" }`. While
    /// joined, the bot hears the room mix minus its own playout and
    /// speaks into the room.
    ConferenceJoin {
        call_id: CallId,
        /// Operator-meaningful room name; calls naming the same
        /// `room_id` share a room. The room locks to the first
        /// joiner's sample rate.
        room_id: String,
    },

    /// Leave the conference room this call is in (0.7.0). No-op if the
    /// call isn't in a room. SiphonAI replies with
    /// [`BridgeOut::ConferenceLeft`]`{ reason: "left" }` and restores
    /// the direct caller↔WS audio pair.
    ConferenceLeave { call_id: CallId },

    /// Put this call's caller on hold (0.7.2): SiphonAI re-INVITEs the
    /// caller so their media goes on hold (`a=sendonly`/`recvonly`, RFC
    /// 3264) — they hear hold music and stop sending — while the WS
    /// session stays open. Self-scoped. SiphonAI replies
    /// [`BridgeOut::Held`] once the re-INVITE is acknowledged, or
    /// `error { code: "hold_failed" }` (the call stays as it was — a
    /// failed hold never drops it). No-op if already held. Distinct from
    /// [`BridgeIn::Mute`], which only silences the bot's own audio.
    Hold { call_id: CallId },

    /// Resume a held call (0.7.2): re-INVITE back to two-way audio.
    /// SiphonAI replies [`BridgeOut::Resumed`]. No-op if not held.
    Resume { call_id: CallId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
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
    fn bridge_out_start_outbound_direction() {
        // Outbound origination (0.6.0): `direction: "outbound"`, additive —
        // protocol version stays "1".
        let raw = r#"{
          "type": "start",
          "version": "1",
          "call_id": "siphon-out-1",
          "seq": 0,
          "from": "+13125551234",
          "to": "+15558675309",
          "direction": "outbound",
          "audio": { "encoding": "pcm16le", "sample_rate": 8000, "channels": 1, "frame_ms": 20 },
          "sip": { "call_id": "out-abc@trunk.example", "headers": {} }
        }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::Start(start) = msg else {
            panic!("expected Start variant");
        };
        assert_eq!(start.direction, Direction::Outbound);
        assert_eq!(start.to, "+15558675309");
    }

    #[test]
    fn bridge_out_start_with_barge_in_mode() {
        // 0.32.0: the resolved barge-in policy rides on `start` —
        // additive, absent on the legacy shape (tests above).
        let raw = r#"{
          "type": "start",
          "version": "1",
          "call_id": "siphon-7f3a9b21",
          "seq": 0,
          "from": "+13125551212",
          "to": "5000",
          "direction": "inbound",
          "audio": { "encoding": "pcm16le", "sample_rate": 8000, "channels": 1, "frame_ms": 20 },
          "sip": { "call_id": "abc123@pbx.example.com", "headers": {} },
          "barge_in_mode": "pause"
        }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::Start(start) = msg else {
            panic!("expected Start variant");
        };
        assert_eq!(start.barge_in_mode, Some(BargeInModeInfo::Pause));
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
            BridgeOut::SpeechStarted { ref call_id, seq: 42, ts_ms: 1234, .. } if call_id.as_str() == "c"
        ));
    }

    #[test]
    fn bridge_out_speech_started_with_pending_decision() {
        // Pause mode (0.32.0): the event doubles as the arbitration
        // request. The legacy shape (test above) must stay byte-stable —
        // both fields are skip_serializing_if.
        let raw = r#"{ "type": "speech_started", "call_id": "c", "seq": 42, "ts_ms": 1234, "decision_pending": true, "decision_deadline_ms": 500 }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        assert!(matches!(
            msg,
            BridgeOut::SpeechStarted {
                decision_pending: true,
                decision_deadline_ms: Some(500),
                ..
            }
        ));
    }

    #[test]
    fn bridge_out_barge_in_resolved() {
        for (raw, expect) in [
            (
                r#"{ "type": "barge_in_resolved", "call_id": "c", "seq": 44, "outcome": "confirmed" }"#,
                BargeInOutcome::Confirmed,
            ),
            (
                r#"{ "type": "barge_in_resolved", "call_id": "c", "seq": 45, "outcome": "rejected" }"#,
                BargeInOutcome::Rejected,
            ),
            (
                r#"{ "type": "barge_in_resolved", "call_id": "c", "seq": 46, "outcome": "timeout" }"#,
                BargeInOutcome::Timeout,
            ),
        ] {
            let msg: BridgeOut = assert_round_trip(raw);
            let BridgeOut::BargeInResolved { outcome, .. } = msg else {
                panic!("expected BargeInResolved");
            };
            assert_eq!(outcome, expect);
        }
    }

    #[test]
    fn bridge_in_barge_in_verdicts() {
        let msg: BridgeIn = assert_round_trip(r#"{ "type": "barge_in_confirm", "call_id": "c" }"#);
        assert!(matches!(msg, BridgeIn::BargeInConfirm { .. }));
        let msg: BridgeIn = assert_round_trip(r#"{ "type": "barge_in_reject", "call_id": "c" }"#);
        assert!(matches!(msg, BridgeIn::BargeInReject { .. }));
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
            rx_jitter_ms: None,
            rx_packets_received: None,
            rx_packets_lost: None,
            rx_packets_out_of_order: None,
            rx_packets_duplicate: None,
            tx_packets_sent: None,
            tx_octets_sent: None,
            tx_packets_lost_reported: None,
            mos_estimate: None,
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
        // 0.30.0 RX-side fields are likewise absent (not null) pre-data.
        for k in [
            "rx_jitter_ms",
            "rx_packets_received",
            "rx_packets_lost",
            "rx_packets_out_of_order",
            "rx_packets_duplicate",
            "tx_packets_sent",
            "tx_octets_sent",
            "tx_packets_lost_reported",
            "mos_estimate",
        ] {
            assert!(!obj.contains_key(k), "{k} must be absent when None");
        }
    }

    #[test]
    fn bridge_out_rtp_stats_with_rx_fields() {
        // 0.30.0: both viewpoints populated — remote-reported TX side
        // plus locally-measured RX side and the derived MOS estimate.
        let raw = r#"{ "type": "rtp_stats", "call_id": "c", "seq": 51, "jitter_ms": 12.5, "packet_loss_ratio": 0.004, "rtcp_rtt_ms": 42.0, "rx_jitter_ms": 3.75, "rx_packets_received": 1500, "rx_packets_lost": 6, "rx_packets_out_of_order": 2, "rx_packets_duplicate": 1, "mos_estimate": 4.2 }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::RtpStats {
            rx_jitter_ms,
            rx_packets_received,
            rx_packets_lost,
            rx_packets_out_of_order,
            rx_packets_duplicate,
            mos_estimate,
            ..
        } = msg
        else {
            panic!("expected RtpStats");
        };
        assert_eq!(rx_jitter_ms, Some(3.75));
        assert_eq!(rx_packets_received, Some(1500));
        assert_eq!(rx_packets_lost, Some(6));
        assert_eq!(rx_packets_out_of_order, Some(2));
        assert_eq!(rx_packets_duplicate, Some(1));
        assert_eq!(mos_estimate, Some(4.2));
    }

    #[test]
    fn bridge_out_rtp_stats_with_tx_fields() {
        // 0.38.0: the transmit direction finally has counts, not just
        // ratios — "we sent 1,914 packets; the far end reported 12 lost."
        let raw = r#"{ "type": "rtp_stats", "call_id": "c", "seq": 51, "jitter_ms": 12.5, "packet_loss_ratio": 0.004, "rtcp_rtt_ms": 42.0, "tx_packets_sent": 1914, "tx_octets_sent": 306240, "tx_packets_lost_reported": 12, "mos_estimate": 4.2 }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::RtpStats {
            tx_packets_sent,
            tx_octets_sent,
            tx_packets_lost_reported,
            ..
        } = msg
        else {
            panic!("expected RtpStats");
        };
        assert_eq!(tx_packets_sent, Some(1914));
        assert_eq!(tx_octets_sent, Some(306_240));
        assert_eq!(tx_packets_lost_reported, Some(12));
    }

    #[test]
    fn bridge_out_rtp_stats_negative_cumulative_lost_round_trips() {
        // RFC 3550 §6.4.1 makes cumulative-lost signed: duplicates can
        // push the far end's packets-received past packets-expected. A
        // consumer parsing this as unsigned would read -3 as ~16.7M, so
        // pin the sign through a full serialize/deserialize cycle.
        let raw =
            r#"{ "type": "rtp_stats", "call_id": "c", "seq": 9, "tx_packets_lost_reported": -3 }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::RtpStats {
            tx_packets_lost_reported,
            ..
        } = msg
        else {
            panic!("expected RtpStats");
        };
        assert_eq!(tx_packets_lost_reported, Some(-3));
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
    fn bridge_in_set_recording_consent() {
        let raw = r#"{ "type": "set_recording_consent", "call_id": "c", "note": "dtmf-1" }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        let BridgeIn::SetRecordingConsent { note, .. } = msg else {
            panic!("expected SetRecordingConsent");
        };
        assert_eq!(note.as_deref(), Some("dtmf-1"));

        // `note` is optional — the bare form parses too.
        let raw = r#"{ "type": "set_recording_consent", "call_id": "c" }"#;
        let msg: BridgeIn = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            msg,
            BridgeIn::SetRecordingConsent { note: None, .. }
        ));
    }

    #[test]
    fn bridge_in_transfer() {
        let raw = r#"{ "type": "transfer", "call_id": "c", "target": "sip:agent@example.com" }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        let BridgeIn::Transfer {
            target,
            replaces_call_id,
            ..
        } = msg
        else {
            panic!("expected Transfer");
        };
        assert_eq!(target.as_deref(), Some("sip:agent@example.com"));
        assert_eq!(replaces_call_id, None);
    }

    #[test]
    fn bridge_in_transfer_attended() {
        // 0.6.1: replaces_call_id names the consult call; target is
        // optional (defaults to the consult dialog's Contact).
        let raw = r#"{ "type": "transfer", "call_id": "c", "replaces_call_id": "siphon-C" }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        let BridgeIn::Transfer {
            target,
            replaces_call_id,
            ..
        } = msg
        else {
            panic!("expected Transfer");
        };
        assert_eq!(target, None);
        assert_eq!(replaces_call_id.as_deref(), Some("siphon-C"));

        // Explicit target alongside replaces_call_id (the override).
        let raw = r#"{ "type": "transfer", "call_id": "c",
                       "target": "sip:agent@sbc.example.com",
                       "replaces_call_id": "siphon-C" }"#;
        let msg: BridgeIn = serde_json::from_str(raw).unwrap();
        let BridgeIn::Transfer {
            target,
            replaces_call_id,
            ..
        } = msg
        else {
            panic!("expected Transfer");
        };
        assert_eq!(target.as_deref(), Some("sip:agent@sbc.example.com"));
        assert_eq!(replaces_call_id.as_deref(), Some("siphon-C"));
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

    // ─── Conference (0.7.0) ─────────────────────────────────────────────

    #[test]
    fn bridge_in_conference_join() {
        let raw = r#"{ "type": "conference_join", "call_id": "siphon-a", "room_id": "support-7" }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        let BridgeIn::ConferenceJoin { call_id, room_id } = msg else {
            panic!("expected ConferenceJoin");
        };
        assert_eq!(call_id.as_str(), "siphon-a");
        assert_eq!(room_id, "support-7");
    }

    #[test]
    fn bridge_in_conference_leave() {
        let raw = r#"{ "type": "conference_leave", "call_id": "siphon-a" }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        assert!(matches!(msg, BridgeIn::ConferenceLeave { .. }));
    }

    #[test]
    fn bridge_out_conference_joined() {
        let raw = r#"{ "type": "conference_joined", "call_id": "siphon-a", "seq": 4, "room_id": "support-7", "participants": 2 }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::ConferenceJoined {
            room_id,
            participants,
            ..
        } = msg
        else {
            panic!("expected ConferenceJoined");
        };
        assert_eq!(room_id, "support-7");
        assert_eq!(participants, 2);
    }

    #[test]
    fn bridge_out_conference_left_reasons() {
        let left: BridgeOut = assert_round_trip(
            r#"{ "type": "conference_left", "call_id": "siphon-a", "seq": 9, "room_id": "support-7", "reason": "left" }"#,
        );
        assert!(matches!(
            left,
            BridgeOut::ConferenceLeft {
                reason: ConferenceLeftReason::Left,
                ..
            }
        ));
        let closed: BridgeOut = assert_round_trip(
            r#"{ "type": "conference_left", "call_id": "siphon-a", "seq": 9, "room_id": "support-7", "reason": "room_closed" }"#,
        );
        assert!(matches!(
            closed,
            BridgeOut::ConferenceLeft {
                reason: ConferenceLeftReason::RoomClosed,
                ..
            }
        ));
    }

    #[test]
    fn bridge_out_participant_joined_left() {
        let joined: BridgeOut = assert_round_trip(
            r#"{ "type": "participant_joined", "call_id": "siphon-a", "seq": 5, "room_id": "support-7", "participant_call_id": "siphon-b" }"#,
        );
        let BridgeOut::ParticipantJoined {
            participant_call_id,
            ..
        } = joined
        else {
            panic!("expected ParticipantJoined");
        };
        assert_eq!(participant_call_id, "siphon-b");
        let left: BridgeOut = assert_round_trip(
            r#"{ "type": "participant_left", "call_id": "siphon-a", "seq": 6, "room_id": "support-7", "participant_call_id": "siphon-b" }"#,
        );
        assert!(matches!(left, BridgeOut::ParticipantLeft { .. }));
    }

    #[test]
    fn bridge_out_error_conference_failed() {
        let raw = r#"{ "type": "error", "call_id": "siphon-a", "seq": 3, "code": "conference_failed", "message": "room is full (8 calls)" }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::Error { code, .. } = msg else {
            panic!("expected Error");
        };
        assert_eq!(code, ErrorCode::ConferenceFailed);
    }

    // ─── Park (0.7.0) ────────────────────────────────────────────────────

    #[test]
    fn bridge_in_park_with_slot() {
        let raw = r#"{ "type": "park", "call_id": "siphon-a", "slot": "lot-3" }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        let BridgeIn::Park { slot, .. } = msg else {
            panic!("expected Park");
        };
        assert_eq!(slot.as_deref(), Some("lot-3"));
    }

    #[test]
    fn bridge_in_park_slot_optional() {
        let raw = r#"{ "type": "park", "call_id": "siphon-a" }"#;
        let msg: BridgeIn = assert_round_trip(raw);
        assert!(matches!(msg, BridgeIn::Park { slot: None, .. }));
    }

    #[test]
    fn stop_reason_park_round_trips() {
        let raw = r#"{ "type": "stop", "call_id": "c", "seq": 9, "reason": "park" }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        assert!(matches!(
            msg,
            BridgeOut::Stop {
                reason: StopReason::Park,
                ..
            }
        ));
    }

    #[test]
    fn start_retrieved_omitted_when_false_present_when_true() {
        // false ⇒ field absent (byte-identical to pre-0.7.0 start).
        let start_false = StartMsg {
            version: "1".into(),
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
            verstat: None,
            retrieved: false,
            reconnected: false,
            trace_context: None,
            barge_in_mode: None,
        };
        let v = serde_json::to_value(&start_false).unwrap();
        assert!(
            v.get("retrieved").is_none(),
            "retrieved must be absent when false"
        );

        // true ⇒ present.
        let start_true = StartMsg {
            retrieved: true,
            reconnected: false,
            ..start_false
        };
        let v = serde_json::to_value(&start_true).unwrap();
        assert_eq!(v["retrieved"], serde_json::json!(true));
        // And a retrieve-session start round-trips through BridgeOut.
        let _ = assert_round_trip::<BridgeOut>(
            &serde_json::to_string(&BridgeOut::Start(start_true)).unwrap(),
        );
    }

    #[test]
    fn start_reconnected_omitted_when_false_present_when_true() {
        // Mirror of `retrieved` (0.7.3): absent on the wire when false,
        // present when true, and independent of `retrieved`.
        let base = StartMsg {
            version: "1".into(),
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
            verstat: None,
            retrieved: false,
            reconnected: false,
            trace_context: None,
            barge_in_mode: None,
        };
        let v = serde_json::to_value(&base).unwrap();
        assert!(
            v.get("reconnected").is_none(),
            "reconnected must be absent when false"
        );

        let reconnected = StartMsg {
            reconnected: true,
            ..base
        };
        let v = serde_json::to_value(&reconnected).unwrap();
        assert_eq!(v["reconnected"], serde_json::json!(true));
        // `retrieved` stays absent — the two flags are independent.
        assert!(v.get("retrieved").is_none());
        let _ = assert_round_trip::<BridgeOut>(
            &serde_json::to_string(&BridgeOut::Start(reconnected)).unwrap(),
        );
    }

    #[test]
    fn bridge_out_error_park_failed() {
        let raw = r#"{ "type": "error", "call_id": "c", "seq": 3, "code": "park_failed", "message": "max_parked reached" }"#;
        let msg: BridgeOut = assert_round_trip(raw);
        let BridgeOut::Error { code, .. } = msg else {
            panic!("expected Error");
        };
        assert_eq!(code, ErrorCode::ParkFailed);
    }

    // ─── Hold / resume (0.7.2) ───────────────────────────────────────────

    #[test]
    fn bridge_in_hold_and_resume() {
        assert!(matches!(
            assert_round_trip::<BridgeIn>(r#"{ "type": "hold", "call_id": "siphon-a" }"#),
            BridgeIn::Hold { .. }
        ));
        assert!(matches!(
            assert_round_trip::<BridgeIn>(r#"{ "type": "resume", "call_id": "siphon-a" }"#),
            BridgeIn::Resume { .. }
        ));
    }

    #[test]
    fn bridge_out_held_and_resumed() {
        // The bot-initiated acks — distinct `type`s from the peer-initiated
        // `hold`/`resume` events (which stay as-is).
        assert!(matches!(
            assert_round_trip::<BridgeOut>(r#"{ "type": "held", "call_id": "c", "seq": 11 }"#),
            BridgeOut::Held { .. }
        ));
        assert!(matches!(
            assert_round_trip::<BridgeOut>(r#"{ "type": "resumed", "call_id": "c", "seq": 12 }"#),
            BridgeOut::Resumed { .. }
        ));
    }

    #[test]
    fn peer_hold_event_still_distinct_from_held_ack() {
        // Sanity: the peer-initiated `hold` event and the `held` ack are
        // different messages, so a server can tell "the far end held me"
        // from "my hold request succeeded".
        let peer = assert_round_trip::<BridgeOut>(
            r#"{ "type": "hold", "call_id": "c", "seq": 5, "direction": "sendonly" }"#,
        );
        assert!(matches!(peer, BridgeOut::Hold { .. }));
    }

    #[test]
    fn bridge_out_error_hold_failed() {
        let raw = r#"{ "type": "error", "call_id": "c", "seq": 7, "code": "hold_failed", "message": "488 not acceptable" }"#;
        let BridgeOut::Error { code, .. } = assert_round_trip::<BridgeOut>(raw) else {
            panic!("expected Error");
        };
        assert_eq!(code, ErrorCode::HoldFailed);
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
            retrieved: false,
            reconnected: false,
            trace_context: None,
            barge_in_mode: None,
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
                retrieved: false,
                reconnected: false,
                trace_context: None,
                barge_in_mode: None,
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

    /// `trace_context` (0.23.0) mirrors the `srtp` contract: absent when
    /// `None` (the OTLP-disabled default — the v1 shape is unchanged),
    /// and `{ "traceparent": …, "tracestate"?: … }` when present, with
    /// `tracestate` itself skipped when there's nothing to forward.
    #[test]
    fn start_trace_context_field_serialization() {
        let mk = |trace_context: Option<TraceContext>| {
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
                verstat: None,
                retrieved: false,
                reconnected: false,
                trace_context,
                barge_in_mode: None,
            })
        };

        // (1) None ⇒ field absent. The v1 contract.
        let v: Value = serde_json::to_value(mk(None)).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("trace_context"),
            "trace_context must be absent from JSON when None, got: {v}"
        );

        // (2) Some ⇒ present + round-trips; absent tracestate is omitted,
        // not null.
        let tc = TraceContext {
            traceparent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into(),
            tracestate: None,
        };
        let v: Value = serde_json::to_value(mk(Some(tc.clone()))).unwrap();
        assert_eq!(
            v["trace_context"]["traceparent"],
            json!("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01")
        );
        assert!(
            !v["trace_context"]
                .as_object()
                .unwrap()
                .contains_key("tracestate"),
            "tracestate must be omitted when None, got: {v}"
        );
        let round: BridgeOut = serde_json::from_value(v).unwrap();
        match round {
            BridgeOut::Start(s) => assert_eq!(s.trace_context, Some(tc)),
            other => panic!("expected Start, got {other:?}"),
        }

        // (3) tracestate rides along when present.
        let tc = TraceContext {
            traceparent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into(),
            tracestate: Some("vendor=value,other=thing".into()),
        };
        let v: Value = serde_json::to_value(mk(Some(tc))).unwrap();
        assert_eq!(
            v["trace_context"]["tracestate"],
            json!("vendor=value,other=thing")
        );
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
                retrieved: false,
                reconnected: false,
                trace_context: None,
                barge_in_mode: None,
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
