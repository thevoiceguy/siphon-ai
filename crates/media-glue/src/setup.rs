//! End-to-end inbound-call media setup.
//!
//! Where the SDP layer (`sdp.rs`) and the audio tap (`tap.rs`) each
//! own one piece of the puzzle, [`MediaSetup`] wires them together
//! against a forge `SessionManager` so callers can answer an INVITE
//! in one shot:
//!
//! 1. Parse the offer (cheap, fail-fast before allocating ports).
//! 2. Ask forge's `SessionManager` to create a media session for the
//!    call. This allocates the RTP/RTCP port pair from forge's
//!    process-wide pool and stands up the underlying socket pair.
//! 3. Stamp the allocated RTP port into a [`LocalCapabilities`] and
//!    negotiate an answer. The answer's `m=audio` port mirrors the
//!    port forge will be sending and receiving on, which is the
//!    whole reason this step has to happen *after* allocation.
//! 4. Apply the negotiated codec / payload type back to the SIP-side
//!    participant (leg A) so forge's forwarding engine knows what to
//!    expect on the wire and what to emit on playout.
//! 5. Wire the process-wide [`MediaBridgeManager`] into the session
//!    and attach a [`MediaTap`] for it. The tap is the same per-call
//!    object [`crate::tap::MediaTap`] documents — it's just delivered
//!    pre-attached so `CallController` doesn't have to know forge.
//!
//! On any failure between (2) and (5) the half-built session is torn
//! down via `SessionManager::stop_session`, which deallocates ports
//! and drops the socket pair. Without that guard a parse error or a
//! "no common codec" outcome would leak both.
//!
//! ## What this module does NOT do
//!
//! - **Doesn't start RTP forwarding.** Forwarding belongs after the
//!   200 OK is on the wire (and arguably after ACK) — call
//!   `SessionManager::start_session` from the controller's lifecycle.
//! - **Doesn't build the bridge `StartMsg`.** That stitches in
//!   SIP-side facts (From/To/Call-ID) the SDP layer has no view of.
//!   `CallController`'s caller composes the message from the answer
//!   metadata exposed here and the SIP facts it already has.
//! - **Doesn't speak SIP.** The 200 OK is built and sent by the
//!   sip-glue `RoutingHandler`; this module just hands it the answer
//!   text.

use std::sync::Arc;

use forge_core::{CallId, ForgeError, ParticipantId};
use forge_engine::{
    MediaBridgeManager, MediaSession, ParticipantCodecConfig, ParticipantLabel,
    ParticipantMediaUpdate, SessionManager,
};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

use crate::sdp::{
    negotiate_answer, parse_offer, AnswerOutcome, Codec, LocalCapabilities, SdpError,
};
use crate::tap::{MediaTap, MediaTapError};

/// Daemon-wide handles `MediaSetup` needs once at startup. Cheap to
/// clone — both managers are already `Arc`-ed.
pub struct MediaSetup {
    session_manager: Arc<SessionManager>,
    bridge_manager: Arc<MediaBridgeManager>,
    /// IP that goes into the answer's `c=` and `o=` lines. Same
    /// address forge's RTP socket is bound to (or the public-facing
    /// address when behind 1:1 NAT — left to deployment config).
    local_ip: String,
}

/// Per-call inputs to [`MediaSetup::accept_inbound`].
///
/// `codecs` is the priority-ordered list to advertise. Typically the
/// `[media].codecs` global, optionally overridden by the matched
/// `[route.media]` block. `dtmf_payload_type` is the RFC-2833 PT we
/// advertise (commonly 101); `None` disables RFC-2833 negotiation.
///
/// `participant_a` is the SIP caller's id; `participant_b` is the
/// synthetic "other" leg of the forge two-party model. Per
/// `tap.rs` ("Single-leg model") we don't drive B; pass any unique
/// id (typically `ParticipantId::generate()`).
#[derive(Debug, Clone)]
pub struct InboundCall<'a> {
    pub call_id: CallId,
    pub offer_sdp: &'a str,
    pub codecs: Vec<Codec>,
    pub dtmf_payload_type: Option<u8>,
    pub participant_a: ParticipantId,
    pub participant_b: ParticipantId,
    pub from_tag: Option<String>,
    pub to_tag: Option<String>,
}

/// What [`MediaSetup::accept_inbound`] hands back on success.
///
/// `answer.answer_text` goes into the 200 OK body verbatim. The
/// `session` Arc is exposed so the caller can drive lifecycle
/// (`start_session`, `stop_session`) without re-resolving via the
/// manager. `tap` is the pre-attached [`MediaTap`] the controller
/// runs.
pub struct InboundAccepted {
    pub answer: AnswerOutcome,
    pub session: Arc<MediaSession>,
    pub tap: MediaTap,
}

impl std::fmt::Debug for InboundAccepted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `MediaSession` and `MediaTap` (transitively) hold types
        // that don't impl Debug; keep the summary safe to log.
        f.debug_struct("InboundAccepted")
            .field("answer", &self.answer)
            .field("session_call_id", self.session.call_id())
            .field("session_ports", &self.session.ports())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum SetupError {
    /// SDP parse / negotiate / no-common-codec / etc.
    #[error(transparent)]
    Sdp(#[from] SdpError),

    /// Forge couldn't create or update the media session — e.g.,
    /// the port pool is exhausted, or the session id collides.
    #[error("forge session error: {0}")]
    Session(String),

    /// `MediaBridgeManager::attach_call` failed or the negotiated
    /// sample rate isn't supported by the bridge crate.
    #[error(transparent)]
    Tap(#[from] MediaTapError),
}

impl From<ForgeError> for SetupError {
    fn from(value: ForgeError) -> Self {
        SetupError::Session(value.to_string())
    }
}

impl MediaSetup {
    pub fn new(
        session_manager: Arc<SessionManager>,
        bridge_manager: Arc<MediaBridgeManager>,
        local_ip: impl Into<String>,
    ) -> Self {
        Self {
            session_manager,
            bridge_manager,
            local_ip: local_ip.into(),
        }
    }

    pub fn session_manager(&self) -> &Arc<SessionManager> {
        &self.session_manager
    }

    pub fn bridge_manager(&self) -> &Arc<MediaBridgeManager> {
        &self.bridge_manager
    }

    pub fn local_ip(&self) -> &str {
        &self.local_ip
    }

    /// Accept an inbound INVITE: allocate a forge session, negotiate
    /// the answer, and attach the audio tap.
    ///
    /// On error the partially-built session is torn down before
    /// returning so port pool state stays consistent.
    #[instrument(skip(self, call), fields(call_id = %call.call_id))]
    pub async fn accept_inbound(
        &self,
        call: InboundCall<'_>,
    ) -> Result<InboundAccepted, SetupError> {
        // (1) Parse first — failing here costs us no kernel
        // resources, which matters when an attacker sprays malformed
        // INVITEs.
        let offer = parse_offer(call.offer_sdp)?;

        // (2) Allocate the session. This is what gives us the port.
        let session = self
            .session_manager
            .create_session(
                call.call_id.clone(),
                call.participant_a.clone(),
                call.participant_b.clone(),
                Some(call.offer_sdp.to_string()),
                call.from_tag.clone(),
                call.to_tag.clone(),
            )
            .await?;

        // From here on, any failure must release the session.
        let mut guard = SessionGuard::new(&self.session_manager, &call.call_id);

        let ports = session.ports();
        debug!(
            rtp_port = ports.rtp_port,
            rtcp_port = ports.rtcp_port,
            "forge allocated ports"
        );

        // (3) Negotiate the answer at the allocated port.
        let caps = LocalCapabilities {
            local_ip: self.local_ip.clone(),
            local_port: ports.rtp_port,
            codecs: call.codecs.clone(),
            dtmf_payload_type: call.dtmf_payload_type,
        };
        let answer = negotiate_answer(&offer, &caps)?;

        // (4) Apply the negotiated codec to the SIP leg.
        let codec_config = ParticipantCodecConfig {
            payload_type: answer.negotiated_payload_type,
            codec: forge_audio_codec(answer.negotiated_codec),
            clock_rate: answer.negotiated_clock_rate,
        };
        let media_update = ParticipantMediaUpdate {
            codec_config: Some(codec_config),
            telephone_event_payload_type: call.dtmf_payload_type,
            ..Default::default()
        };
        self.session_manager
            .update_participant_media(&call.call_id, ParticipantLabel::A, media_update)
            .await
            .map_err(SetupError::from)?;

        // (5) Wire the bridge manager into the session and attach
        //     the tap. These are paired: the session learns where
        //     to push inbound frames; the tap holds the consumer.
        session
            .set_media_bridge_manager(Arc::clone(&self.bridge_manager))
            .await;

        let tap = MediaTap::attach(
            &self.bridge_manager,
            call.call_id.clone(),
            answer.negotiated_audio_sample_rate,
        )?;

        guard.disarm();

        info!(
            negotiated = %answer.negotiated_codec.encoding_name(),
            sample_rate = answer.negotiated_audio_sample_rate,
            rtp_port = ports.rtp_port,
            "inbound call media setup complete"
        );

        Ok(InboundAccepted {
            answer,
            session,
            tap,
        })
    }
}

/// Map our SDP-layer [`Codec`] to forge's `AudioCodec`. The two enums
/// agree on the codecs SiphonAI v1 negotiates — kept as a private
/// helper so any future divergence (e.g., a forge codec we don't
/// advertise) is one place to update.
fn forge_audio_codec(codec: Codec) -> forge_core::AudioCodec {
    match codec {
        Codec::Pcmu => forge_core::AudioCodec::PCMU,
        Codec::Pcma => forge_core::AudioCodec::PCMA,
        Codec::G722 => forge_core::AudioCodec::G722,
        Codec::Opus => forge_core::AudioCodec::Opus,
    }
}

/// Drop guard: stops the in-flight session if `accept_inbound`
/// errors out after `create_session` succeeded. Disarmed on the
/// happy path.
///
/// `stop_session` is async, so we can't run it from `Drop`. Instead
/// we spawn a fire-and-forget task — the price of a stray
/// per-session task is much smaller than the cost of leaking a port
/// pair on every malformed INVITE.
struct SessionGuard {
    manager: Arc<SessionManager>,
    call_id: CallId,
    armed: bool,
}

impl SessionGuard {
    fn new(manager: &Arc<SessionManager>, call_id: &CallId) -> Self {
        Self {
            manager: Arc::clone(manager),
            call_id: call_id.clone(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let manager = Arc::clone(&self.manager);
        let call_id = self.call_id.clone();
        warn!(call_id = %call_id, "rolling back partially-built media session");
        tokio::spawn(async move {
            if let Err(e) = manager.stop_session(&call_id).await {
                warn!(call_id = %call_id, error = %e, "session rollback failed");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_engine::SessionManagerConfig;
    use forge_rtp::PortPoolConfig;

    fn small_session_manager(min: u16, max: u16) -> Arc<SessionManager> {
        let config = SessionManagerConfig {
            port_pool_config: PortPoolConfig::new(min, max).expect("valid port range"),
            ..Default::default()
        };
        SessionManager::new(config, None)
    }

    #[test]
    fn forge_codec_mapping_round_trips_v1_codecs() {
        assert_eq!(forge_audio_codec(Codec::Pcmu), forge_core::AudioCodec::PCMU);
        assert_eq!(forge_audio_codec(Codec::Pcma), forge_core::AudioCodec::PCMA);
        assert_eq!(forge_audio_codec(Codec::G722), forge_core::AudioCodec::G722);
        assert_eq!(forge_audio_codec(Codec::Opus), forge_core::AudioCodec::Opus);
    }

    #[tokio::test]
    async fn parse_error_fails_before_allocating() {
        let session_mgr = small_session_manager(40000, 40100);
        let bridge_mgr = Arc::new(MediaBridgeManager::new());
        let setup = MediaSetup::new(
            Arc::clone(&session_mgr),
            Arc::clone(&bridge_mgr),
            "127.0.0.1",
        );

        let call_id = CallId::new("c-parse-err");
        let result = setup
            .accept_inbound(InboundCall {
                call_id: call_id.clone(),
                offer_sdp: "not actually sdp",
                codecs: vec![Codec::Pcmu],
                dtmf_payload_type: None,
                participant_a: ParticipantId::generate(),
                participant_b: ParticipantId::generate(),
                from_tag: None,
                to_tag: None,
            })
            .await;

        assert!(matches!(result, Err(SetupError::Sdp(SdpError::Parse(_)))));
        // No session should have been created — port pool stays empty.
        let (allocated, _available) = session_mgr.port_pool_stats().await;
        assert_eq!(allocated, 0, "port pool must not allocate on parse failure");
    }
}
