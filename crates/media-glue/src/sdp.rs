//! SDP offer/answer for inbound INVITEs.
//!
//! Per CLAUDE.md §4.8 we don't write our own SDP parser or
//! negotiator — `forge-sdp` (and through it `sip-sdp`) already does
//! both. This module is a thin shim that:
//!
//! 1. Defines the small [`Codec`] enum SiphonAI v1 actually
//!    supports (G.711 µ-law/A-law, G.722, Opus).
//! 2. Builds a [`LocalCapabilities`] SDP matching the
//!    daemon-configured codec list, anchored to the RTP port the
//!    media layer has already allocated. The negotiator uses that
//!    port for the answer's `m=audio` line, so callers get an
//!    answer that reflects reality.
//! 3. Wraps the upstream negotiator behind one error type
//!    ([`SdpError`]) and one outcome type ([`AnswerOutcome`])
//!    carrying the answer text plus the codec metadata the
//!    `CallController` will need (PT, clock rate) to commit forge.
//!
//! ## What this module does NOT do
//!
//! - **Doesn't allocate the RTP port.** The caller already did that
//!   via forge's port pool / `MediaSession`; we only stamp the
//!   chosen port into the SDP.
//! - **Doesn't talk to forge directly.** This is pure SDP work — no
//!   `MediaBridgeManager`, no `SessionManager`. Wiring those up
//!   together with the SDP step is the next layer's job (a future
//!   `MediaSetup` helper or the daemon's `CallAcceptor`).
//! - **Doesn't support SRTP.** v1 is `RTP/AVP` only; SRTP is
//!   listed as deferred in CLAUDE.md §8.

use forge_sdp::{helpers, MediaDescription, MediaType, SessionDescription, SessionDescriptionExt};
use thiserror::Error;

/// Codecs SiphonAI v1 supports. Anything not in this list is
/// rejected at negotiation time.
///
/// We don't add OPUS yet because the v1 dev plan calls it
/// "nice-to-have" (DEV_PLAN.md §3.2) and forge-codecs gates it
/// behind a feature; the workspace already builds with `opus`
/// optional. We *advertise* it but the actual encode/decode on the
/// forge side will Refuse if the feature isn't on. That's fine for
/// negotiation — peers that only speak G.711 will fall back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// G.711 µ-law. Static PT 0, 8 kHz, mono. Mandatory.
    Pcmu,
    /// G.711 A-law. Static PT 8, 8 kHz, mono. Mandatory.
    Pcma,
    /// G.722 wideband. Static PT 9. Per RFC 3551 §4.5.2 the rtpmap
    /// MUST advertise 8000 Hz even though the codec is 16 kHz —
    /// that's the wire-format quirk, not a bug.
    G722,
    /// Opus. Dynamic PT (we use 111 by convention; the offerer's PT
    /// wins on negotiation).
    Opus,
}

impl Codec {
    /// Static RTP payload type for the codec, or our chosen
    /// dynamic PT for codecs that don't have a static assignment.
    pub fn rtp_payload_type(self) -> u8 {
        match self {
            Codec::Pcmu => 0,
            Codec::Pcma => 8,
            Codec::G722 => 9,
            Codec::Opus => 111,
        }
    }

    /// Encoding-name string used in the `a=rtpmap` line.
    pub fn encoding_name(self) -> &'static str {
        match self {
            Codec::Pcmu => "PCMU",
            Codec::Pcma => "PCMA",
            Codec::G722 => "G722",
            Codec::Opus => "opus",
        }
    }

    /// Clock rate as advertised in `a=rtpmap`. Note the G.722 quirk
    /// (8000 on the wire, 16 kHz in the codec).
    pub fn clock_rate(self) -> u32 {
        match self {
            Codec::Pcmu | Codec::Pcma | Codec::G722 => 8000,
            Codec::Opus => 48000,
        }
    }

    /// Channel count for the rtpmap. Opus negotiates `2` even for
    /// mono streams (that's the encoding-params convention); the
    /// rest are unset (mono is the default).
    pub fn rtpmap_channels(self) -> Option<&'static str> {
        match self {
            Codec::Opus => Some("2"),
            _ => None,
        }
    }

    /// Audio sample rate the codec produces / consumes after
    /// decoding. This is what the WS bridge sees — distinct from
    /// the rtpmap clock rate (G.722).
    pub fn audio_sample_rate(self) -> u32 {
        match self {
            Codec::Pcmu | Codec::Pcma => 8000,
            Codec::G722 => 16000,
            Codec::Opus => 48000,
        }
    }

    /// Parse from the rtpmap encoding-name (case-insensitive).
    /// Returns `None` for unsupported codecs — the caller logs and
    /// falls back to the next offered codec.
    pub fn from_encoding_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "PCMU" => Some(Codec::Pcmu),
            "PCMA" => Some(Codec::Pcma),
            "G722" => Some(Codec::G722),
            "OPUS" => Some(Codec::Opus),
            _ => None,
        }
    }
}

/// SDP capabilities for one inbound call.
///
/// `local_ip` and `local_port` come from the SIP/media-allocation
/// layer (forge's `MediaSession::ports()` after the session has
/// been created). `codecs` is the priority-ordered list from the
/// daemon's `[media]` config or the matched `[route.media]` block.
#[derive(Debug, Clone)]
pub struct LocalCapabilities {
    pub local_ip: String,
    pub local_port: u16,
    pub codecs: Vec<Codec>,
    /// Payload type for `telephone-event` (RFC 2833). `Some(101)`
    /// is the typical default; `None` disables RFC-2833-method DTMF.
    pub dtmf_payload_type: Option<u8>,
}

impl LocalCapabilities {
    /// Build the `SessionDescription` the upstream negotiator
    /// expects. The result advertises every configured codec, in
    /// order, at our `local_port`.
    pub fn to_sdp(&self) -> SessionDescription {
        // sip-sdp's negotiator uses `local_media.port` for the
        // answer's `m=audio` port (negotiate.rs §base_answer_media).
        // So whatever we put here ends up on the wire.
        let mut audio = MediaDescription::audio(self.local_port);

        // formats list in priority order — the negotiator iterates
        // the offer's formats and, for each, looks for ours; first
        // match in the offer wins, but having ours in priority
        // order means our preferred codecs land in the answer
        // first when the offer lists them.
        for &codec in &self.codecs {
            audio = audio
                .add_format(codec.rtp_payload_type())
                .expect("static codec PT is in range");
            audio = audio
                .add_rtpmap(
                    codec.rtp_payload_type(),
                    codec.encoding_name(),
                    codec.clock_rate(),
                    codec.rtpmap_channels(),
                )
                .expect("codec rtpmap is well-formed");
        }

        if let Some(pt) = self.dtmf_payload_type {
            audio = audio.add_format(pt).expect("dtmf PT is in range");
            audio = audio
                .add_rtpmap(pt, "telephone-event", 8000, None)
                .expect("telephone-event rtpmap");
        }

        // sendrecv is the v1 default; hold/resume re-INVITE flips
        // direction in a separate exchange.
        let audio = audio
            .with_direction("sendrecv")
            .expect("sendrecv is a valid direction");

        SessionDescription::builder()
            .origin("siphon-ai", &fresh_session_id(), &self.local_ip)
            .expect("origin is well-formed")
            .session_name("siphon-ai")
            .expect("session name is non-empty")
            .connection(&self.local_ip)
            .expect("connection ip is well-formed")
            .time(0, 0)
            .media(audio)
            .expect("audio media is well-formed")
            .build()
    }
}

fn fresh_session_id() -> String {
    // sip-sdp's own helper is private; produce a numeric session
    // ID the way forge does (uuid-derived), bounded to 64-bit
    // because some peers reject anything larger.
    let raw = uuid::Uuid::new_v4().as_u128();
    (raw as u64).to_string()
}

/// Errors surfaced by the SDP layer.
#[derive(Debug, Error)]
pub enum SdpError {
    /// Offer was malformed or not parseable.
    #[error("failed to parse offer SDP: {0}")]
    Parse(String),

    /// The offer didn't advertise an audio media stream at all.
    #[error("offer has no audio media stream")]
    NoAudio,

    /// No codec in the offer matches our local capabilities.
    #[error("no common codec between offer and local capabilities")]
    NoCommonCodec,

    /// `negotiate_answer` returned a media stream we can't make
    /// sense of (e.g., port 0 / rejected media).
    #[error("negotiation rejected the audio stream")]
    AudioRejected,

    /// Anything the upstream negotiator surfaces that we don't
    /// have a finer-grained variant for.
    #[error("SDP negotiation failed: {0}")]
    Negotiate(String),
}

/// Result of a successful negotiation.
///
/// `answer_text` is what goes into the SIP 200 OK body verbatim.
/// The other fields tell the caller what forge needs to be told to
/// expect on the wire (which PT, which sample rate to feed the WS
/// bridge), so they don't have to re-parse the answer.
#[derive(Debug, Clone)]
pub struct AnswerOutcome {
    pub answer: SessionDescription,
    pub answer_text: String,
    pub negotiated_codec: Codec,
    pub negotiated_payload_type: u8,
    /// Codec clock rate, as seen on the wire (matches `a=rtpmap`).
    pub negotiated_clock_rate: u32,
    /// Codec audio sample rate after decode (matches the WS
    /// `start.audio.sample_rate` once we map up: G.722 advertises
    /// 8000 but produces 16 kHz audio).
    pub negotiated_audio_sample_rate: u32,
    /// Direction the answerer (us) committed to. RFC 3264 §6.1
    /// mirroring: offer `sendonly` → answer `recvonly`, etc. Used
    /// by the call layer to surface hold/resume to the WS server
    /// and (eventually) pause forge's outbound RTP.
    pub negotiated_direction: MediaDirection,
}

/// Media direction per RFC 4566 / RFC 3264. The values mirror
/// `sip-sdp::Direction` but live here so consumers of media-glue
/// don't need a second upstream dep just to name the enum.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaDirection {
    /// Bidirectional audio. The normal "talking" state. Default —
    /// matches RFC 4566 §6 ("the default value is sendrecv").
    #[default]
    SendRecv,
    /// Answerer sends, doesn't expect to receive. From a hold
    /// scenario: the held endpoint goes `sendonly` (often with
    /// music-on-hold), so we answer `recvonly`. Net: we keep
    /// receiving but pause our send.
    SendOnly,
    /// Answerer receives, doesn't expect to send. Mirror of the
    /// above — counter-party held us.
    RecvOnly,
    /// Both directions paused. RFC 3264 §6.1 — used during
    /// transient teardown or attended-transfer.
    Inactive,
}

impl MediaDirection {
    /// Parse from an SDP `a=` attribute name. `None` if the value
    /// isn't one of the four direction attributes.
    pub fn from_attr(s: &str) -> Option<Self> {
        Some(match s {
            "sendrecv" => Self::SendRecv,
            "sendonly" => Self::SendOnly,
            "recvonly" => Self::RecvOnly,
            "inactive" => Self::Inactive,
            _ => return None,
        })
    }

    /// The attribute string for serialization (`a=<this>`).
    pub fn as_attr(self) -> &'static str {
        match self {
            Self::SendRecv => "sendrecv",
            Self::SendOnly => "sendonly",
            Self::RecvOnly => "recvonly",
            Self::Inactive => "inactive",
        }
    }

    /// True iff this direction means "audio is paused in at least
    /// one direction" — the hold-style state set by `sendonly` /
    /// `recvonly` / `inactive`. Used by the call layer to decide
    /// whether to emit `hold` / `resume` to the WS server.
    pub fn is_held(self) -> bool {
        !matches!(self, Self::SendRecv)
    }
}

/// Parse an offer SDP string. Surfaces parse errors as
/// [`SdpError::Parse`].
pub fn parse_offer(sdp: &str) -> Result<SessionDescription, SdpError> {
    SessionDescription::from_str(sdp).map_err(|e| SdpError::Parse(e.to_string()))
}

/// The peer's audio RTP endpoint as advertised in an offer's `c=` /
/// `m=audio` lines. The media-level `c=` wins over the session-level
/// `c=` when both are present (RFC 4566 §5.7). Returns `None` if
/// either is absent or the connection address doesn't parse as an IP.
///
/// We hand this to forge as `ParticipantMediaUpdate.remote_addr` so
/// outbound RTP can begin the moment the call answers, instead of
/// waiting for forge's symmetric-RTP latch to learn the address from
/// the first inbound packet — that wait blocks the first ~500 ms of
/// any greeting otherwise.
pub fn audio_remote_addr(session: &SessionDescription) -> Option<std::net::SocketAddr> {
    let media = session.find_media(MediaType::Audio)?;
    let conn = media.connection.as_ref().or(session.connection.as_ref())?;
    let ip: std::net::IpAddr = conn.connection_address.as_str().parse().ok()?;
    Some(std::net::SocketAddr::new(ip, media.port))
}

/// Negotiate an answer for `offer` against `caps`. The answer's
/// `c=` line and `m=audio` port reflect `caps.local_ip` and
/// `caps.local_port`.
pub fn negotiate_answer(
    offer: &SessionDescription,
    caps: &LocalCapabilities,
) -> Result<AnswerOutcome, SdpError> {
    let local_caps = caps.to_sdp();
    let answer = SessionDescription::negotiate_answer(offer, &local_caps, &caps.local_ip)
        .map_err(|e| SdpError::Negotiate(e.to_string()))?;

    let audio = answer
        .find_media(MediaType::Audio)
        .ok_or(SdpError::NoAudio)?;

    if audio.port == 0 {
        // The negotiator returned a "rejected" media stream
        // (port 0). The most common cause is no common codec
        // between offer and caps — if `audio.formats` is empty,
        // surface NoCommonCodec; otherwise bubble the generic
        // rejection.
        return Err(if audio.formats.is_empty() {
            SdpError::NoCommonCodec
        } else {
            SdpError::AudioRejected
        });
    }

    // sip-sdp's negotiate sets the answer's first format to the
    // first negotiated codec. Pull our metadata from that PT.
    let primary = helpers::extract_primary_codec(audio).ok_or(SdpError::AudioRejected)?;
    let codec = Codec::from_encoding_name(&primary.encoding_name).ok_or(SdpError::NoCommonCodec)?;

    // The upstream negotiator already mirrors direction per RFC
    // 3264 §6.1; we just read it back via the typed accessor.
    // `direction()` returns None when the attribute is absent,
    // which RFC 4566 §6 maps to sendrecv.
    // The upstream `MediaDescription::direction()` returns the
    // typed enum from sip-sdp's `attrs` module, which isn't on our
    // dep graph directly — go through its canonical-token string
    // form so we don't have to pull sip-sdp in as a separate dep
    // just to name the enum. Skipping the token also future-
    // proofs against an upstream rename.
    let negotiated_direction = audio
        .direction()
        .as_ref()
        .map(|d| d.as_token())
        .and_then(MediaDirection::from_attr)
        .unwrap_or_default();

    let answer_text = answer.serialize();
    Ok(AnswerOutcome {
        answer,
        answer_text,
        negotiated_codec: codec,
        negotiated_payload_type: primary.payload_type,
        negotiated_clock_rate: primary.clock_rate,
        negotiated_audio_sample_rate: codec.audio_sample_rate(),
        negotiated_direction,
    })
}

/// Convenience: parse + negotiate in one step. Use this when you
/// have the raw offer body from a SIP INVITE.
pub fn build_answer(offer_sdp: &str, caps: &LocalCapabilities) -> Result<AnswerOutcome, SdpError> {
    let offer = parse_offer(offer_sdp)?;
    negotiate_answer(&offer, caps)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDP_SESSION_LEVEL_C: &str = "\
v=0\r\n\
o=- 1 1 IN IP4 198.51.100.10\r\n\
s=-\r\n\
c=IN IP4 198.51.100.10\r\n\
t=0 0\r\n\
m=audio 27492 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n";

    const SDP_MEDIA_LEVEL_C_WINS: &str = "\
v=0\r\n\
o=- 1 1 IN IP4 198.51.100.10\r\n\
s=-\r\n\
c=IN IP4 198.51.100.10\r\n\
t=0 0\r\n\
m=audio 27492 RTP/AVP 0\r\n\
c=IN IP4 203.0.113.7\r\n\
a=rtpmap:0 PCMU/8000\r\n";

    #[test]
    fn audio_remote_addr_from_session_level_connection() {
        let s = parse_offer(SDP_SESSION_LEVEL_C).unwrap();
        let addr = audio_remote_addr(&s).expect("address present");
        assert_eq!(addr.to_string(), "198.51.100.10:27492");
    }

    #[test]
    fn audio_remote_addr_prefers_media_level_connection() {
        let s = parse_offer(SDP_MEDIA_LEVEL_C_WINS).unwrap();
        let addr = audio_remote_addr(&s).expect("address present");
        assert_eq!(addr.to_string(), "203.0.113.7:27492");
    }
}
