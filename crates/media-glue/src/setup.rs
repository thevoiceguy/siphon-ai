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

use forge_core::{CallId, EventBus, ForgeError, ParticipantId};
use forge_engine::srtp_install::install_srtp_keys;
use forge_engine::{
    MediaBridgeManager, MediaSession, ParticipantCodecConfig, ParticipantLabel,
    ParticipantMediaUpdate, SessionManager,
};
use forge_sdp::sdes::{CryptoAttribute, CryptoSuite};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

use crate::sdp::{
    generate_offer, negotiate_answer, negotiate_offer_answer, parse_offer, AnswerOutcome, Codec,
    LocalCapabilities, SdpError,
};
use crate::tap::{BargeInAction, MediaTap, MediaTapError};

/// Daemon-wide handles `MediaSetup` needs once at startup. Cheap to
/// clone — every field is already `Arc`-ed.
#[derive(Clone)]
pub struct MediaSetup {
    session_manager: Arc<SessionManager>,
    bridge_manager: Arc<MediaBridgeManager>,
    /// Same `EventBus` the [`SessionManager`] publishes to. Each tap
    /// `subscribe()`s here so per-call DTMF / VAD / quality events
    /// reach the bridge layer without going through forge's
    /// session-internal channels.
    event_bus: Arc<EventBus>,
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
    /// What the tap does when forge-vad reports speech-started.
    /// `[BargeInAction::Notify]` (just forward the WS event) or
    /// `[BargeInAction::AutoClear]` (drop pending outbound playout
    /// before forwarding). Set from `[bridge].barge_in.mode`.
    pub barge_in_action: BargeInAction,
    /// Playout-gated barge-in debounce from `[bridge.barge_in].debounce_ms`
    /// (`None` = immediate flush). Only affects `AutoClear`.
    pub barge_in_debounce: Option<std::time::Duration>,
    /// Tear the call down after this many seconds of no inbound RTP.
    /// `None` disables the watchdog. Resolved by the acceptor from
    /// `[media].inactivity_timeout_secs` and the route's override.
    pub inactivity_timeout: Option<std::time::Duration>,
    /// One-sided silence threshold for the idle detector — emit
    /// `silence_detected` when the caller has been VAD-silent for
    /// this long. `None` disables the event. Resolved by the
    /// acceptor from `[bridge].silence_threshold_ms` plus any
    /// per-route override.
    pub silence_threshold: Option<std::time::Duration>,
    /// Two-sided dead-air threshold — emit `dead_air_detected` when
    /// neither caller speech nor outbound WS audio has been
    /// observed for this long. `None` disables.
    pub dead_air_threshold: Option<std::time::Duration>,
    /// Cadence of periodic `rtp_stats` events. `None` disables.
    /// Resolved by the acceptor from `[bridge].rtp_stats_interval_ms`
    /// plus any per-route override.
    pub rtp_stats_interval: Option<std::time::Duration>,
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

/// Tap-behaviour knobs for [`MediaSetup::apply_answer`] — the same set
/// [`InboundCall`] carries inline, resolved by the acceptor from
/// `[bridge]`/`[media]` plus any route override.
#[derive(Debug, Clone)]
pub struct TapOptions {
    pub barge_in_action: BargeInAction,
    /// Playout-gated barge-in debounce (`None` = immediate flush).
    pub barge_in_debounce: Option<std::time::Duration>,
    pub inactivity_timeout: Option<std::time::Duration>,
    pub silence_threshold: Option<std::time::Duration>,
    pub dead_air_threshold: Option<std::time::Duration>,
    pub rtp_stats_interval: Option<std::time::Duration>,
}

/// Inputs to [`MediaSetup::originate_offer`] — allocate a forge session and
/// produce the SDP offer for an outbound INVITE. (Tap-side options come
/// later, at [`MediaSetup::apply_answer`], once the call is answered.)
///
/// `participant_a` is the remote callee's leg (the SIP peer, as for inbound);
/// `participant_b` is the synthetic other leg (pass a fresh
/// [`ParticipantId::generate`]).
#[derive(Debug, Clone)]
pub struct OutboundOfferRequest {
    pub call_id: CallId,
    pub codecs: Vec<Codec>,
    pub dtmf_payload_type: Option<u8>,
    pub participant_a: ParticipantId,
    pub participant_b: ParticipantId,
    pub from_tag: Option<String>,
    pub to_tag: Option<String>,
    /// SRTP policy for this originated call. The outbound mirror of the
    /// inbound `[media].srtp` modes; the daemon maps its
    /// `siphon-ai-core::SrtpMode` onto this (media-glue sits below core,
    /// so the enum lives here).
    pub srtp: OutboundSrtp,
}

/// SRTP policy for an **originated** call (RFC 4568 SDES on the offer).
/// Plaintext by default; the secure modes offer `RTP/SAVP` with an
/// `a=crypto:` key we generate, and differ only in how a peer that
/// answers plaintext `RTP/AVP` is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutboundSrtp {
    /// Plaintext `RTP/AVP` offer (the default — unchanged 0.6.x behaviour).
    #[default]
    Off,
    /// Offer SRTP, but accept a plaintext downgrade if the peer answers
    /// `RTP/AVP` (best-effort encryption).
    Preferred,
    /// Offer SRTP and **require** it: a peer that answers plaintext fails
    /// the call.
    Required,
}

/// What [`MediaSetup::originate_offer`] hands back: the allocated session and
/// the offer SDP for the outbound INVITE body. Hold it until the call is
/// answered, then pass it to [`MediaSetup::apply_answer`].
///
/// **Lifecycle:** if the call never answers (busy / decline / timeout), the
/// caller is responsible for tearing the session down via
/// `SessionManager::stop_session(&call_id)`. Once handed to `apply_answer`,
/// teardown-on-error becomes that method's job.
pub struct OutboundOffer {
    pub session: Arc<MediaSession>,
    pub offer_sdp: String,
    /// The capabilities advertised in `offer_sdp`; reused to validate the
    /// peer's answer.
    pub offered: LocalCapabilities,
    pub call_id: CallId,
    /// SRTP policy this offer was built under (decides downgrade handling
    /// in [`MediaSetup::apply_answer`]).
    pub(crate) srtp: OutboundSrtp,
    /// The SDES master key we offered, when `srtp` is a secure mode — our
    /// *send* key. `None` for a plaintext offer. Consumed at `apply_answer`.
    pub(crate) offer_crypto: Option<CryptoAttribute>,
}

impl std::fmt::Debug for OutboundOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundOffer")
            .field("call_id", &self.call_id)
            .field("session_ports", &self.session.ports())
            .field("offer_len", &self.offer_sdp.len())
            .finish_non_exhaustive()
    }
}

/// What [`MediaSetup::apply_answer`] hands back on success — the outbound
/// mirror of [`InboundAccepted`]. `answer` describes the negotiated media
/// (its `answer_text` is the *received* answer, not anything we send).
pub struct OutboundAccepted {
    pub answer: AnswerOutcome,
    pub session: Arc<MediaSession>,
    pub tap: MediaTap,
    /// The negotiated SDES crypto-suite when outbound SRTP was established
    /// (e.g. `"AES_CM_128_HMAC_SHA1_80"`), for `start.srtp.profile`. `None`
    /// for a plaintext call or a `preferred` downgrade. The exchange is
    /// always SDES on the outbound origination path.
    pub srtp_profile: Option<String>,
    /// The SDP **offer** we sent for this call (our local media, `sendrecv`).
    /// Retained so the call layer can build a bot-initiated hold/resume
    /// re-INVITE offer by flipping its direction (0.7.5) — the outbound
    /// analogue of the inbound side's cached answer SDP.
    pub offer_sdp: String,
}

impl std::fmt::Debug for OutboundAccepted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundAccepted")
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

    /// Outbound SRTP (SDES) negotiation failed — bad key material, or the
    /// peer refused SRTP under `[[gateway]].srtp = "required"`.
    #[error("outbound SRTP negotiation failed: {0}")]
    Srtp(String),
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
        event_bus: Arc<EventBus>,
        local_ip: impl Into<String>,
    ) -> Self {
        Self {
            session_manager,
            bridge_manager,
            event_bus,
            local_ip: local_ip.into(),
        }
    }

    pub fn session_manager(&self) -> &Arc<SessionManager> {
        &self.session_manager
    }

    pub fn bridge_manager(&self) -> &Arc<MediaBridgeManager> {
        &self.bridge_manager
    }

    pub fn event_bus(&self) -> &Arc<EventBus> {
        &self.event_bus
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

        // (4) Apply the negotiated codec AND the peer's RTP endpoint
        //     to the SIP leg. Pushing `remote_addr` here means forge's
        //     scheduled-playout loop can fire as soon as the call
        //     answers, rather than waiting for the symmetric-RTP latch
        //     to learn the address from the first inbound packet
        //     (~500 ms gap that swallows the start of any greeting).
        let codec_config = ParticipantCodecConfig {
            payload_type: answer.negotiated_payload_type,
            codec: forge_audio_codec(answer.negotiated_codec),
            clock_rate: answer.negotiated_clock_rate,
        };
        let remote_addr = crate::sdp::audio_remote_addr(&offer);
        let media_update = ParticipantMediaUpdate {
            codec_config: Some(codec_config),
            telephone_event_payload_type: call.dtmf_payload_type,
            remote_addr: remote_addr.map(Some),
            ..Default::default()
        };
        if let Some(addr) = remote_addr {
            debug!(remote_addr = %addr, "seeding leg A remote RTP address from offer SDP");
        }
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

        let tap = MediaTap::attach_with_barge_in(
            &self.bridge_manager,
            &self.event_bus,
            call.call_id.clone(),
            answer.negotiated_audio_sample_rate,
            call.barge_in_action,
        )?
        .with_barge_in_debounce(call.barge_in_debounce)
        .with_inactivity_timeout(call.inactivity_timeout)
        .with_idle_thresholds(call.silence_threshold, call.dead_air_threshold)
        .with_rtp_stats_interval(call.rtp_stats_interval);

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

    /// Originate an outbound call's media: allocate a forge session and build
    /// the SDP **offer** for the INVITE body — the inverse of
    /// [`accept_inbound`](Self::accept_inbound), where we make the first move.
    ///
    /// On success the session is allocated and live; the caller sends the
    /// INVITE carrying `OutboundOffer::offer_sdp` and, on a 2xx, calls
    /// [`apply_answer`](Self::apply_answer). If the call never answers, the
    /// caller tears the session down (see [`OutboundOffer`]).
    #[instrument(skip(self, req), fields(call_id = %req.call_id))]
    pub async fn originate_offer(
        &self,
        req: OutboundOfferRequest,
    ) -> Result<OutboundOffer, SetupError> {
        // Allocate the session — no remote SDP yet, we're the offerer.
        let session = self
            .session_manager
            .create_session(
                req.call_id.clone(),
                req.participant_a.clone(),
                req.participant_b.clone(),
                None,
                req.from_tag.clone(),
                req.to_tag.clone(),
            )
            .await?;

        let ports = session.ports();
        let offered = LocalCapabilities {
            local_ip: self.local_ip.clone(),
            local_port: ports.rtp_port,
            codecs: req.codecs.clone(),
            dtmf_payload_type: req.dtmf_payload_type,
        };

        // SRTP (SDES): mint a master key and offer RTP/SAVP. The key is our
        // *send* key; the peer's answer carries theirs (our recv key),
        // bound onto the session at `apply_answer`. AES_CM_128_HMAC_SHA1_80
        // is the near-universal trunk default (e.g. Twilio).
        let offer_crypto = match req.srtp {
            OutboundSrtp::Off => None,
            OutboundSrtp::Preferred | OutboundSrtp::Required => Some(CryptoAttribute::generate(
                1,
                CryptoSuite::Aes128CmHmacSha1_80,
            )),
        };
        let offer_sdp = generate_offer(&offered, offer_crypto.as_ref());

        debug!(
            rtp_port = ports.rtp_port,
            codecs = req.codecs.len(),
            srtp = ?req.srtp,
            "generated outbound offer"
        );

        Ok(OutboundOffer {
            session,
            offer_sdp,
            offered,
            call_id: req.call_id,
            srtp: req.srtp,
            offer_crypto,
        })
    }

    /// Bind the peer's **answer** (from the outbound INVITE's 2xx) onto the
    /// session and attach the audio tap — the inverse of
    /// [`accept_inbound`](Self::accept_inbound) steps 3–5. On any error the
    /// session is torn down before returning.
    #[instrument(skip(self, offer, answer_sdp, tap), fields(call_id = %offer.call_id))]
    pub async fn apply_answer(
        &self,
        offer: OutboundOffer,
        answer_sdp: &str,
        tap: TapOptions,
    ) -> Result<OutboundAccepted, SetupError> {
        let OutboundOffer {
            session,
            offered,
            call_id,
            srtp,
            offer_crypto,
            offer_sdp,
            ..
        } = offer;

        // From here on, any failure must release the session.
        let mut guard = SessionGuard::new(&self.session_manager, &call_id);

        // (1) Read the negotiated audio out of the peer's answer.
        let answer = negotiate_offer_answer(answer_sdp, &offered)?;

        // (1a) SRTP (SDES): if we offered it, bind keys onto leg A — our
        //      offered key for send, the peer's answered key for recv.
        //      Installing keys is what enables encryption on the leg
        //      (forge's SrtpContext is "enabled" once keyed). A peer that
        //      answered plaintext RTP/AVP is a downgrade: fail under
        //      `required`, continue in the clear under `preferred`.
        let mut srtp_profile: Option<String> = None;
        if let Some(our_key) = &offer_crypto {
            match &answer.peer_srtp {
                Some(peer_key) => {
                    let send = our_key
                        .to_srtp_key_material()
                        .map_err(|e| SetupError::Srtp(format!("our offer key: {e}")))?;
                    let recv = peer_key
                        .to_srtp_key_material()
                        .map_err(|e| SetupError::Srtp(format!("peer answer key: {e}")))?;
                    install_srtp_keys(session.srtp_a(), send, recv).await;
                    srtp_profile = Some(our_key.suite.as_str().to_string());
                    debug!(call_id = %call_id, "outbound SRTP (SDES) keys installed on leg A");
                }
                None if srtp == OutboundSrtp::Required => {
                    return Err(SetupError::Srtp(
                        "peer answered plaintext RTP/AVP but [[gateway]].srtp = required".into(),
                    ));
                }
                None => {
                    warn!(
                        call_id = %call_id,
                        "offered SRTP but peer answered plaintext; continuing unencrypted (srtp = preferred)"
                    );
                }
            }
        }

        // (2) Apply the negotiated codec AND the peer's RTP endpoint to leg A
        //     — same as inbound, but the remote address comes from the answer
        //     (the callee telling us where to send) rather than from an offer.
        let codec_config = ParticipantCodecConfig {
            payload_type: answer.negotiated_payload_type,
            codec: forge_audio_codec(answer.negotiated_codec),
            clock_rate: answer.negotiated_clock_rate,
        };
        let remote_addr = crate::sdp::audio_remote_addr(&answer.answer);
        let media_update = ParticipantMediaUpdate {
            codec_config: Some(codec_config),
            telephone_event_payload_type: offered.dtmf_payload_type,
            remote_addr: remote_addr.map(Some),
            ..Default::default()
        };
        if let Some(addr) = remote_addr {
            debug!(remote_addr = %addr, "seeding leg A remote RTP address from answer SDP");
        }
        self.session_manager
            .update_participant_media(&call_id, ParticipantLabel::A, media_update)
            .await
            .map_err(SetupError::from)?;

        // (3) Wire the bridge manager into the session and attach the tap.
        session
            .set_media_bridge_manager(Arc::clone(&self.bridge_manager))
            .await;

        let media_tap = MediaTap::attach_with_barge_in(
            &self.bridge_manager,
            &self.event_bus,
            call_id.clone(),
            answer.negotiated_audio_sample_rate,
            tap.barge_in_action,
        )?
        .with_barge_in_debounce(tap.barge_in_debounce)
        .with_inactivity_timeout(tap.inactivity_timeout)
        .with_idle_thresholds(tap.silence_threshold, tap.dead_air_threshold)
        .with_rtp_stats_interval(tap.rtp_stats_interval);

        guard.disarm();

        info!(
            negotiated = %answer.negotiated_codec.encoding_name(),
            sample_rate = answer.negotiated_audio_sample_rate,
            rtp_port = session.ports().rtp_port,
            "outbound call media setup complete"
        );

        Ok(OutboundAccepted {
            answer,
            session,
            tap: media_tap,
            srtp_profile,
            offer_sdp,
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
            Arc::new(forge_core::EventBus::new()),
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
                barge_in_action: BargeInAction::Notify,
                barge_in_debounce: None,
                inactivity_timeout: None,
                silence_threshold: None,
                dead_air_threshold: None,
                rtp_stats_interval: None,
            })
            .await;

        assert!(matches!(result, Err(SetupError::Sdp(SdpError::Parse(_)))));
        // No session should have been created — port pool stays empty.
        let (allocated, _available) = session_mgr.port_pool_stats().await;
        assert_eq!(allocated, 0, "port pool must not allocate on parse failure");
    }

    // ─── Outbound origination ────────────────────────────────────────────

    fn setup_with(session_mgr: &Arc<SessionManager>) -> MediaSetup {
        MediaSetup::new(
            Arc::clone(session_mgr),
            Arc::new(MediaBridgeManager::new()),
            Arc::new(forge_core::EventBus::new()),
            "127.0.0.1",
        )
    }

    fn outbound_req(call_id: &str, codecs: Vec<Codec>) -> OutboundOfferRequest {
        OutboundOfferRequest {
            call_id: CallId::new(call_id),
            codecs,
            dtmf_payload_type: Some(101),
            participant_a: ParticipantId::generate(),
            participant_b: ParticipantId::generate(),
            from_tag: Some("ftag".into()),
            to_tag: None,
            srtp: OutboundSrtp::Off,
        }
    }

    fn peer_answer(port: u16, pt: u8, name: &str) -> String {
        format!(
            "v=0\r\n\
o=peer 1 1 IN IP4 198.51.100.20\r\n\
s=-\r\n\
c=IN IP4 198.51.100.20\r\n\
t=0 0\r\n\
m=audio {port} RTP/AVP {pt}\r\n\
a=rtpmap:{pt} {name}/8000\r\n\
a=ptime:20\r\n\
a=sendrecv\r\n"
        )
    }

    fn tap_opts() -> TapOptions {
        TapOptions {
            barge_in_action: BargeInAction::Notify,
            barge_in_debounce: None,
            inactivity_timeout: None,
            silence_threshold: None,
            dead_air_threshold: None,
            rtp_stats_interval: None,
        }
    }

    #[tokio::test]
    async fn originate_offer_allocates_session_and_builds_offer() {
        let session_mgr = small_session_manager(41000, 41100);
        let setup = setup_with(&session_mgr);

        let offer = setup
            .originate_offer(outbound_req("c-out-1", vec![Codec::Pcmu, Codec::Pcma]))
            .await
            .expect("offer generated");

        // A port pair was allocated, and the offer advertises it.
        let (allocated, _) = session_mgr.port_pool_stats().await;
        assert_eq!(allocated, 1, "one session allocated");
        let rtp_port = offer.session.ports().rtp_port;
        assert!(
            offer.offer_sdp.contains(&format!("m=audio {rtp_port} ")),
            "offer advertises the allocated port: {}",
            offer.offer_sdp
        );
        assert!(offer.offer_sdp.contains("a=rtpmap:0 PCMU/8000"));
        assert!(offer.offer_sdp.contains("a=rtpmap:8 PCMA/8000"));
    }

    #[tokio::test]
    async fn apply_answer_binds_codec_and_attaches_tap() {
        let session_mgr = small_session_manager(41200, 41300);
        let setup = setup_with(&session_mgr);

        let offer = setup
            .originate_offer(outbound_req("c-out-2", vec![Codec::Pcmu, Codec::Pcma]))
            .await
            .expect("offer");

        let accepted = setup
            .apply_answer(offer, &peer_answer(4000, 0, "PCMU"), tap_opts())
            .await
            .expect("answer applied");

        assert_eq!(accepted.answer.negotiated_codec, Codec::Pcmu);
        assert_eq!(accepted.answer.negotiated_audio_sample_rate, 8000);
        assert_eq!(accepted.tap.sample_rate(), 8000);
        // The session survived — still allocated, not rolled back.
        let (allocated, _) = session_mgr.port_pool_stats().await;
        assert_eq!(allocated, 1);
    }

    #[tokio::test]
    async fn apply_answer_rejects_unoffered_codec() {
        let session_mgr = small_session_manager(41400, 41500);
        let setup = setup_with(&session_mgr);

        // Offer PCMA only; peer answers PCMU → no common codec.
        let offer = setup
            .originate_offer(outbound_req("c-out-3", vec![Codec::Pcma]))
            .await
            .expect("offer");
        let result = setup
            .apply_answer(offer, &peer_answer(4000, 0, "PCMU"), tap_opts())
            .await;
        assert!(matches!(
            result,
            Err(SetupError::Sdp(SdpError::NoCommonCodec))
        ));
    }

    // ─── Outbound SRTP (SDES) ────────────────────────────────────────────

    fn outbound_req_srtp(
        call_id: &str,
        codecs: Vec<Codec>,
        srtp: OutboundSrtp,
    ) -> OutboundOfferRequest {
        let mut req = outbound_req(call_id, codecs);
        req.srtp = srtp;
        req
    }

    /// A peer answer that ACCEPTED our SRTP offer: RTP/SAVP + a=crypto.
    fn srtp_peer_answer(port: u16, pt: u8, name: &str) -> String {
        use forge_sdp::sdes::{CryptoAttribute, CryptoSuite, MediaSdesAttributesExt};
        use forge_sdp::{MediaType, Protocol, SessionDescriptionExt};
        let crypto = CryptoAttribute::generate(1, CryptoSuite::Aes128CmHmacSha1_80);
        let mut sdp = crate::sdp::parse_offer(&peer_answer(port, pt, name)).expect("base answer");
        let audio = sdp.find_media_mut(MediaType::Audio).expect("audio");
        audio.protocol = Protocol::RtpSavp;
        audio.add_media_crypto(&crypto);
        sdp.serialize()
    }

    #[tokio::test]
    async fn originate_offer_required_emits_savp_crypto() {
        let session_mgr = small_session_manager(41600, 41700);
        let setup = setup_with(&session_mgr);
        let offer = setup
            .originate_offer(outbound_req_srtp(
                "c-srtp-1",
                vec![Codec::Pcmu],
                OutboundSrtp::Required,
            ))
            .await
            .expect("offer");
        assert!(
            offer.offer_sdp.contains("RTP/SAVP"),
            "offer is SAVP: {}",
            offer.offer_sdp
        );
        assert!(
            offer.offer_sdp.contains("a=crypto:"),
            "offer carries a=crypto"
        );
        assert!(offer.offer_crypto.is_some(), "our send key retained");
    }

    #[tokio::test]
    async fn apply_answer_srtp_installs_keys_on_accept() {
        let session_mgr = small_session_manager(41800, 41900);
        let setup = setup_with(&session_mgr);
        let offer = setup
            .originate_offer(outbound_req_srtp(
                "c-srtp-2",
                vec![Codec::Pcmu],
                OutboundSrtp::Required,
            ))
            .await
            .expect("offer");
        // Peer accepted SRTP — keys install, session survives.
        let accepted = setup
            .apply_answer(offer, &srtp_peer_answer(4000, 0, "PCMU"), tap_opts())
            .await
            .expect("SRTP answer applied");
        assert_eq!(accepted.answer.negotiated_codec, Codec::Pcmu);
        let (allocated, _) = session_mgr.port_pool_stats().await;
        assert_eq!(allocated, 1, "session retained after SRTP install");
    }

    #[tokio::test]
    async fn apply_answer_srtp_required_fails_on_plaintext_downgrade() {
        let session_mgr = small_session_manager(42000, 42100);
        let setup = setup_with(&session_mgr);
        let offer = setup
            .originate_offer(outbound_req_srtp(
                "c-srtp-3",
                vec![Codec::Pcmu],
                OutboundSrtp::Required,
            ))
            .await
            .expect("offer");
        // Peer answered plaintext RTP/AVP — required SRTP must reject. (The
        // SessionGuard rolls the session back on the error return; its
        // teardown is async, so we assert the rejection, not the timing —
        // same as `apply_answer_rejects_unoffered_codec`.)
        let result = setup
            .apply_answer(offer, &peer_answer(4000, 0, "PCMU"), tap_opts())
            .await;
        assert!(
            matches!(result, Err(SetupError::Srtp(_))),
            "required downgrade rejected"
        );
    }

    #[tokio::test]
    async fn apply_answer_srtp_preferred_allows_plaintext_downgrade() {
        let session_mgr = small_session_manager(42200, 42300);
        let setup = setup_with(&session_mgr);
        let offer = setup
            .originate_offer(outbound_req_srtp(
                "c-srtp-4",
                vec![Codec::Pcmu],
                OutboundSrtp::Preferred,
            ))
            .await
            .expect("offer");
        // Preferred: a plaintext answer is accepted, call continues unencrypted.
        let accepted = setup
            .apply_answer(offer, &peer_answer(4000, 0, "PCMU"), tap_opts())
            .await
            .expect("preferred downgrade accepted");
        assert_eq!(accepted.answer.negotiated_codec, Codec::Pcmu);
    }
}
