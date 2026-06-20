//! Outbound call origination (UAC).
//!
//! The inverse of the inbound accept path (`acceptor` / `media-glue`'s
//! `accept_inbound`): instead of answering an INVITE, SiphonAI **places**
//! one. [`OutboundOriginator::place`] allocates media + an SDP offer
//! (media-glue chunk 1), sends the INVITE via the shared
//! [`IntegratedUAC`], awaits the final response, and on a 2xx binds the
//! peer's answer onto the session — handing back an established
//! [`OutboundCall`].
//!
//! ## What this module owns vs. doesn't
//!
//! It owns the **SIP + media establishment** of an outbound call and the
//! teardown BYE ([`OutboundOriginator::hangup`]). It does **not** run the
//! call's audio bridge — that's the direction-agnostic `CallController`,
//! driven by the daemon layer once a trigger (the originate API) is wired.
//! Each call is still one controller with its own owned state (CLAUDE.md
//! §4.4); this struct is process-wide plumbing (one shared UAC + media
//! setup), not per-call state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use forge_core::{CallId, ParticipantId};
use forge_sdp::SessionDescriptionExt as _;
use sip_core::SipUri;
// The SAME sip-sdp sip-uac's SdpAnswerGenerator speaks — distinct from
// forge-media's pinned sip-sdp (whose `SessionDescription` is a different
// type to cargo). We parse our media-glue answer text into this so the UAC
// accepts it for the ACK.
use sip_dialog::Dialog;
use sip_sdp::SessionDescription;
use sip_uac::integrated::{CallHandle, IntegratedUAC, RequestTarget, SdpAnswerGenerator};
use sip_uac::CredentialProvider;
use siphon_ai_media_glue::{
    Codec, InboundAccepted, InboundCall, MediaSetup, OutboundAccepted, OutboundOfferRequest,
    OutboundSrtp, SetupError, TapOptions,
};
use thiserror::Error;
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, info, instrument, warn};

use crate::acceptor::{
    enforce_srtp_mode, maybe_tweak_dtls_srtp_offer, maybe_tweak_sdes_offer,
    post_process_dtls_srtp_answer, post_process_sdes_answer, SrtpMode,
};

/// Per-call state for an outbound **delayed-offer** call (we sent an
/// offerless INVITE; the peer's offer arrives in the 2xx). Parked in the
/// [`DelayedOfferRegistry`] by [`OutboundOriginator::place_delayed`] keyed
/// by SIP Call-ID, consumed by [`DelayedOfferAnswerer::generate_answer`]
/// when the 2xx lands. The fields are exactly what `accept_inbound` needs
/// to build our answer from the peer's offer.
///
/// Public only because it appears in the `pub` [`DelayedOfferRegistry`]
/// alias; its fields are private and it is constructed solely by
/// [`OutboundOriginator::place_delayed`].
pub struct DelayedOfferPending {
    forge_call_id: CallId,
    codecs: Vec<Codec>,
    dtmf_payload_type: Option<u8>,
    participant_a: ParticipantId,
    participant_b: ParticipantId,
    tap: TapOptions,
    /// SRTP answer policy (from the gateway's `srtp` mode). We can't
    /// *offer* SRTP in an offerless INVITE, but if the peer offers SDES in
    /// its 2xx we answer it (Preferred) or require it (Required → a
    /// plaintext offer fails the call). `Off` answers plaintext only.
    srtp_mode: SrtpMode,
    /// Delivers the media-setup result back to `place_delayed` once the
    /// generator has built the answer (or failed).
    result_tx: oneshot::Sender<Result<DelayedMediaResult, SetupError>>,
}

/// What the [`DelayedOfferAnswerer`] hands back to [`OutboundOriginator::place_delayed`]
/// once it has built the answer: the bound media plus the negotiated SDES
/// suite (for `start.srtp`), `None` for a plaintext answer.
struct DelayedMediaResult {
    accepted: InboundAccepted,
    srtp_profile: Option<String>,
    srtp_exchange: siphon_ai_bridge::protocol::SrtpExchange,
}

/// SIP-Call-ID → parked delayed-offer call. Shared between the
/// [`OutboundOriginator`] (which inserts) and the per-UAC
/// [`DelayedOfferAnswerer`] (which removes + finalizes).
pub type DelayedOfferRegistry = Arc<Mutex<HashMap<String, DelayedOfferPending>>>;

/// The UAC's [`SdpAnswerGenerator`] for outbound delayed offer. When a 2xx
/// to an offerless INVITE carries the peer's offer, the UAC calls this to
/// build the SDP answer that goes in the ACK. We answer via `media-glue`
/// (`accept_inbound` — parse offer, allocate, build answer) and hand the
/// resulting session/tap back to `place_delayed` through the registry's
/// oneshot. One answerer per gateway UAC; per-call state is keyed by the
/// dialog's Call-ID, so concurrent calls don't collide.
pub struct DelayedOfferAnswerer {
    media: MediaSetup,
    registry: DelayedOfferRegistry,
    /// Per-process DTLS certificate, for answering a peer's DTLS-SRTP
    /// offer (its SHA-256 fingerprint goes in our answer; it's handed to
    /// `enable_dtls` for the handshake). Shared across this gateway's
    /// delayed calls — same posture as the inbound acceptor's cert.
    dtls_cert: Arc<forge_rtp::dtls::DtlsCertificate>,
}

impl DelayedOfferAnswerer {
    pub fn new(
        media: MediaSetup,
        registry: DelayedOfferRegistry,
        dtls_cert: Arc<forge_rtp::dtls::DtlsCertificate>,
    ) -> Self {
        Self {
            media,
            registry,
            dtls_cert,
        }
    }
}

#[async_trait]
impl SdpAnswerGenerator for DelayedOfferAnswerer {
    async fn generate_answer(
        &self,
        offer: &SessionDescription,
        dialog: &Dialog,
    ) -> anyhow::Result<SessionDescription> {
        let sip_call_id = dialog.id().call_id().to_string();
        // Lock only to pull the parked entry out; never hold across the
        // await below.
        let pending = self
            .registry
            .lock()
            .expect("delayed-offer registry mutex poisoned")
            .remove(&sip_call_id);
        let Some(pending) = pending else {
            return Err(anyhow::anyhow!(
                "no parked delayed-offer call for Call-ID {sip_call_id}"
            ));
        };
        let DelayedOfferPending {
            forge_call_id,
            codecs,
            dtmf_payload_type,
            participant_a,
            participant_b,
            tap,
            srtp_mode,
            result_tx,
        } = pending;

        let offer_sdp = offer.serialize();

        // SRTP answer side: an offerless INVITE can't OFFER SRTP, so we
        // answer the peer's offer per the gateway's policy. Gate first
        // (Required + plaintext peer offer → fail). Then, if the peer
        // offered DTLS-SRTP or SDES, rewrite its secure m-line profile to
        // `RTP/AVP` so the codec negotiator (which doesn't know SAVP/SAVPF)
        // can match — we patch the answer back + install keys / enable DTLS
        // below. Try DTLS first, then SDES (they're mutually exclusive on
        // one m-line). Mirrors the inbound early-offer path.
        if let Ok(parsed) = <forge_sdp::SessionDescription>::from_str(&offer_sdp) {
            if let Err(e) = enforce_srtp_mode(srtp_mode, &parsed) {
                let msg = e.to_string();
                let _ = result_tx.send(Err(SetupError::Srtp(msg.clone())));
                return Err(anyhow::anyhow!("delayed-offer SRTP policy: {msg}"));
            }
        }
        let dtls_tweak = match maybe_tweak_dtls_srtp_offer(&offer_sdp) {
            Ok(t) => t,
            Err(e) => {
                let msg = e.to_string();
                let _ = result_tx.send(Err(SetupError::Srtp(msg.clone())));
                return Err(anyhow::anyhow!("delayed-offer DTLS offer: {msg}"));
            }
        };
        let sdes_tweak = if dtls_tweak.is_none() {
            match maybe_tweak_sdes_offer(&offer_sdp) {
                Ok(t) => t,
                Err(e) => {
                    let msg = e.to_string();
                    let _ = result_tx.send(Err(SetupError::Srtp(msg.clone())));
                    return Err(anyhow::anyhow!("delayed-offer SDES offer: {msg}"));
                }
            }
        } else {
            None
        };
        let offer_for_negotiator = dtls_tweak
            .as_ref()
            .map(|t| t.tweaked_sdp.clone())
            .or_else(|| sdes_tweak.as_ref().map(|t| t.tweaked_sdp.clone()))
            .unwrap_or_else(|| offer_sdp.clone());

        let result = self
            .media
            .accept_inbound(InboundCall {
                call_id: forge_call_id,
                offer_sdp: &offer_for_negotiator,
                codecs,
                dtmf_payload_type,
                participant_a,
                participant_b,
                from_tag: None,
                to_tag: None,
                barge_in_action: tap.barge_in_action,
                barge_in_debounce: tap.barge_in_debounce,
                inactivity_timeout: tap.inactivity_timeout,
                silence_threshold: tap.silence_threshold,
                dead_air_threshold: tap.dead_air_threshold,
                rtp_stats_interval: tap.rtp_stats_interval,
            })
            .await;

        let mut accepted = match result {
            Ok(a) => a,
            Err(e) => {
                let msg = e.to_string();
                let _ = result_tx.send(Err(e));
                return Err(anyhow::anyhow!("delayed-offer answer build failed: {msg}"));
            }
        };

        // Post-negotiation: patch the answer back to the secure profile and
        // bring up keys. DTLS-SRTP installs our fingerprint + starts the
        // handshake (we answer, so DtlsRole::Server); SDES installs the
        // pre-derived keys directly. `(profile, exchange)` rides back so
        // `start.srtp` reports the right exchange.
        let (srtp_profile, srtp_exchange) = if let Some(tweak) = &dtls_tweak {
            let local_fp = self.dtls_cert.fingerprint_sha256().to_string();
            let new_text = match post_process_dtls_srtp_answer(
                &mut accepted.answer.answer,
                tweak,
                &local_fp,
            ) {
                Ok(t) => t,
                Err(e) => {
                    let msg = e.to_string();
                    let _ = result_tx.send(Err(SetupError::Srtp(msg.clone())));
                    return Err(anyhow::anyhow!("delayed-offer DTLS answer: {msg}"));
                }
            };
            accepted.answer.answer_text = new_text;
            if let Err(e) = accepted
                .session
                .enable_dtls(
                    forge_engine::ParticipantLabel::A,
                    Arc::clone(&self.dtls_cert),
                    forge_rtp::dtls::DtlsRole::Server,
                    tweak.remote_fingerprint.1.clone(),
                )
                .await
            {
                let msg = e.to_string();
                let _ = result_tx.send(Err(SetupError::Srtp(format!("enable_dtls: {msg}"))));
                return Err(anyhow::anyhow!("delayed-offer enable_dtls: {msg}"));
            }
            (
                Some("AES_CM_128_HMAC_SHA1_80".to_string()),
                siphon_ai_bridge::protocol::SrtpExchange::Dtls,
            )
        } else if let Some(tweak) = &sdes_tweak {
            match post_process_sdes_answer(&mut accepted.answer.answer, tweak) {
                Ok(new_text) => {
                    accepted.answer.answer_text = new_text;
                    forge_engine::srtp_install::install_srtp_keys(
                        accepted.session.srtp_a(),
                        tweak.sdes_answer.send_key.clone(),
                        tweak.sdes_answer.recv_key.clone(),
                    )
                    .await;
                    (
                        Some(tweak.sdes_answer.local_attribute.suite.as_str().to_string()),
                        siphon_ai_bridge::protocol::SrtpExchange::Sdes,
                    )
                }
                Err(e) => {
                    let msg = e.to_string();
                    let _ = result_tx.send(Err(SetupError::Srtp(msg.clone())));
                    return Err(anyhow::anyhow!("delayed-offer SDES answer: {msg}"));
                }
            }
        } else {
            (None, siphon_ai_bridge::protocol::SrtpExchange::Sdes)
        };

        // Re-parse our (possibly SAVP/SAVPF-patched) answer text into the
        // UAC's sip-sdp type for the ACK, then hand the media back.
        let answer_sd = SessionDescription::parse(&accepted.answer.answer_text)
            .map_err(|e| anyhow::anyhow!("re-parse delayed-offer answer: {e}"))?;
        let _ = result_tx.send(Ok(DelayedMediaResult {
            accepted,
            srtp_profile,
            srtp_exchange,
        }));
        Ok(answer_sd)
    }
}

/// How long `place_delayed` waits for the generator's media result after a
/// 2xx before giving up (the generator runs synchronously during 2xx
/// processing, so this only guards a 2xx that carried no usable offer).
const DELAYED_RESULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Places outbound calls through a shared UAC, allocating + binding media
/// via [`MediaSetup`]. Daemon-wide; cheap to construct.
pub struct OutboundOriginator {
    media: MediaSetup,
    uac: Arc<IntegratedUAC>,
    /// Shared with this gateway's [`DelayedOfferAnswerer`]. `place_delayed`
    /// parks per-call state here for the generator to pick up.
    delayed_registry: DelayedOfferRegistry,
}

/// An established outbound call: the bound media plus the confirmed SIP
/// dialog. Hand `accepted.session` / `accepted.tap` to a `CallController`,
/// and call [`OutboundOriginator::hangup`] once it returns.
pub struct OutboundCall {
    pub accepted: OutboundAccepted,
    /// Confirmed dialog from the 2xx — BYE this to end the call.
    pub dialog: Dialog,
    /// The live INVITE handle (keepalives / session timer / `cancel`).
    pub call_handle: CallHandle,
    /// The forge session / bridge call id, for tearing the media session
    /// down at the end of the call.
    pub call_id: CallId,
}

impl std::fmt::Debug for OutboundCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundCall")
            .field("accepted", &self.accepted)
            .finish_non_exhaustive()
    }
}

/// Why an outbound INVITE didn't establish a call.
#[derive(Debug, Error)]
pub enum OutboundError {
    /// The peer answered with a non-2xx final response.
    #[error("call not answered: {0:?}")]
    NotAnswered(NotAnsweredCause),
    /// Local media setup failed — offer generation or answer binding.
    #[error(transparent)]
    Setup(#[from] SetupError),
    /// The INVITE couldn't be sent, or the client transaction failed with
    /// no usable final response (DNS / transport / timeout).
    #[error("transport/transaction error: {0}")]
    Transport(String),
}

/// Semantic classification of a non-2xx INVITE final response — the basis
/// for the call-progress webhook / CDR result (chunk 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotAnsweredCause {
    /// 486 Busy Here / 600 Busy Everywhere.
    Busy,
    /// 603 Decline / 403 Forbidden — the callee (or trunk) refused.
    Declined,
    /// 408 Request Timeout / 480 Temporarily Unavailable / 487 Request
    /// Terminated — nobody picked up.
    NoAnswer,
    /// Any other 4xx/5xx/6xx, with the code + reason for diagnostics.
    Rejected { code: u16, reason: String },
}

impl OutboundOriginator {
    pub fn new(media: MediaSetup, uac: Arc<IntegratedUAC>) -> Self {
        Self::with_delayed_registry(media, uac, Arc::new(Mutex::new(HashMap::new())))
    }

    /// Construct sharing an explicit [`DelayedOfferRegistry`] with the
    /// gateway's [`DelayedOfferAnswerer`] (so `place_delayed` and the UAC's
    /// answer generator see the same parked calls). The daemon wires both
    /// halves; tests that don't exercise delayed offer use [`Self::new`].
    pub fn with_delayed_registry(
        media: MediaSetup,
        uac: Arc<IntegratedUAC>,
        delayed_registry: DelayedOfferRegistry,
    ) -> Self {
        Self {
            media,
            uac,
            delayed_registry,
        }
    }

    /// This gateway's UAC. The outbound leg's transfer context sends
    /// its in-dialog REFER through this (not the daemon-wide transfer
    /// UAC) so the gateway's digest credentials answer any 401/407
    /// challenge on the REFER (DEV_PLAN_0.6.1 §2.4).
    pub fn uac(&self) -> Arc<IntegratedUAC> {
        Arc::clone(&self.uac)
    }

    /// Place an outbound call to `target`: allocate media + an SDP offer,
    /// send the INVITE, await the final response, and on a 2xx bind the
    /// peer's answer. On any non-answer the partially-built session is
    /// released before returning (so the port pool stays consistent).
    ///
    /// ACK is sent automatically by the UAC on a 2xx; the returned
    /// [`OutboundCall`] carries the confirmed dialog to BYE at teardown.
    #[instrument(skip(self, target, req, tap), fields(call_id = %req.call_id))]
    pub async fn place(
        &self,
        target: SipUri,
        req: OutboundOfferRequest,
        tap: TapOptions,
    ) -> Result<OutboundCall, OutboundError> {
        let call_id = req.call_id.clone();
        debug!(target = %target.as_str(), "placing outbound call");

        // (1) Allocate the session + build the offer (media-glue). A failure
        //     here never created a session, or media-glue rolled it back.
        let offer = self.media.originate_offer(req).await?;

        // (2) Send the INVITE with our offer in the body.
        let handle = match self
            .uac
            .invite(RequestTarget::Uri(target), Some(&offer.offer_sdp))
            .await
        {
            Ok(h) => h,
            Err(e) => {
                self.stop_session(&call_id).await;
                return Err(OutboundError::Transport(e.to_string()));
            }
        };

        // (3) Await the final response (the UAC drains provisionals + sends
        //     ACK on 2xx itself).
        let response = match handle.await_final().await {
            Ok(r) => r,
            Err(e) => {
                self.stop_session(&call_id).await;
                return Err(OutboundError::Transport(e.to_string()));
            }
        };

        let code = response.code();
        if !(200..300).contains(&code) {
            let cause = classify_failure(code, response.reason());
            debug!(code, ?cause, "outbound INVITE not answered");
            self.stop_session(&call_id).await;
            return Err(OutboundError::NotAnswered(cause));
        }

        // (4) 2xx — bind the answer SDP and attach the tap. `apply_answer`
        //     rolls the session back itself on a binding error.
        let answer_sdp = String::from_utf8_lossy(response.body()).into_owned();
        let accepted = self.media.apply_answer(offer, &answer_sdp, tap).await?;
        let dialog = handle.dialog.read().await.clone();

        info!(code, "outbound call answered and media bridged");
        Ok(OutboundCall {
            accepted,
            dialog,
            call_handle: handle,
            call_id,
        })
    }

    /// Place an outbound **delayed-offer** call: send an INVITE with **no
    /// SDP**, let the peer offer in its 2xx, and answer in the ACK. The
    /// inverse of [`Self::place`]'s early offer. Media setup happens inside
    /// the UAC's [`DelayedOfferAnswerer`] (it has the peer's offer); the
    /// session/tap come back here via the registry's oneshot. `req`'s
    /// `srtp` mode governs the **answer** (we can't offer SRTP in an
    /// offerless INVITE): `Preferred` answers the peer's SDES offer when
    /// present, `Required` fails the call on a plaintext peer offer.
    /// DTLS-SRTP offers aren't answered here (a follow-up).
    #[instrument(skip(self, target, req, tap), fields(call_id = %req.call_id))]
    pub async fn place_delayed(
        &self,
        target: SipUri,
        req: OutboundOfferRequest,
        tap: TapOptions,
    ) -> Result<OutboundCall, OutboundError> {
        let call_id = req.call_id.clone();
        debug!(target = %target.as_str(), "placing outbound delayed-offer call");

        // (1) Send the offerless INVITE.
        let handle = match self.uac.invite(RequestTarget::Uri(target), None).await {
            Ok(h) => h,
            Err(e) => {
                self.stop_session(&call_id).await;
                return Err(OutboundError::Transport(e.to_string()));
            }
        };

        // (2) Park per-call media params keyed by the INVITE's Call-ID
        //     BEFORE awaiting the final response. The peer cannot 2xx until
        //     it receives the INVITE we just sent, so the generator (which
        //     fires on that 2xx) can't run before this registration.
        let sip_call_id = handle
            .invite_request()
            .headers()
            .get_smol("Call-ID")
            .map(|s| s.to_string())
            .unwrap_or_default();
        // The gateway's SRTP policy governs the *answer* (we can't offer in
        // an offerless INVITE). OutboundSrtp ↔ SrtpMode are 1:1.
        let srtp_mode = match req.srtp {
            OutboundSrtp::Off => SrtpMode::Off,
            OutboundSrtp::Preferred => SrtpMode::Preferred,
            OutboundSrtp::Required => SrtpMode::Required,
        };
        let (result_tx, result_rx) = oneshot::channel();
        self.delayed_registry
            .lock()
            .expect("delayed-offer registry mutex poisoned")
            .insert(
                sip_call_id.clone(),
                DelayedOfferPending {
                    forge_call_id: call_id.clone(),
                    codecs: req.codecs,
                    dtmf_payload_type: req.dtmf_payload_type,
                    participant_a: req.participant_a,
                    participant_b: req.participant_b,
                    tap,
                    srtp_mode,
                    result_tx,
                },
            );

        // (3) Await the final response. The UAC invokes the answer
        //     generator + sends the ACK on a 2xx itself.
        let unpark = || {
            self.delayed_registry
                .lock()
                .expect("delayed-offer registry mutex poisoned")
                .remove(&sip_call_id);
        };
        let response = match handle.await_final().await {
            Ok(r) => r,
            Err(e) => {
                unpark();
                self.stop_session(&call_id).await;
                return Err(OutboundError::Transport(e.to_string()));
            }
        };
        let code = response.code();
        if !(200..300).contains(&code) {
            // No 2xx → the generator never ran; reclaim the parked entry.
            unpark();
            let cause = classify_failure(code, response.reason());
            debug!(code, ?cause, "outbound delayed-offer INVITE not answered");
            self.stop_session(&call_id).await;
            return Err(OutboundError::NotAnswered(cause));
        }

        // (4) 2xx — the generator built our answer (and ran media setup)
        //     during 2xx processing; collect the session/tap it produced.
        let media = match tokio::time::timeout(DELAYED_RESULT_TIMEOUT, result_rx).await {
            Ok(Ok(Ok(m))) => m,
            Ok(Ok(Err(setup_err))) => {
                // The generator ran but media build failed (it rolled its
                // own session back). The dialog is up; mirror `place`'s
                // post-2xx error handling and surface the setup error.
                return Err(OutboundError::Setup(setup_err));
            }
            Ok(Err(_)) | Err(_) => {
                // The generator never delivered — a 2xx that carried no
                // usable SDP offer (a non-compliant answer to an offerless
                // INVITE), or it timed out.
                unpark();
                warn!("2xx to offerless INVITE carried no usable SDP offer");
                self.stop_session(&call_id).await;
                return Err(OutboundError::Transport(
                    "2xx to offerless INVITE carried no usable SDP offer".into(),
                ));
            }
        };
        let dialog = handle.dialog.read().await.clone();

        // Reshape into `OutboundAccepted` so the shared outbound run_call
        // works unchanged. `offer_sdp` = our answer text, so hold/resume
        // flip the negotiated media the same way the inbound path does.
        // `srtp_profile`/`srtp_exchange` carry the negotiated SRTP (SDES or
        // DTLS) when the peer's offer was answered encrypted — drives
        // `start.srtp` + the outbound SRTP metric; `None` for plaintext.
        let DelayedMediaResult {
            accepted: inbound,
            srtp_profile,
            srtp_exchange,
        } = media;
        let accepted = OutboundAccepted {
            offer_sdp: inbound.answer.answer_text.clone(),
            answer: inbound.answer,
            session: inbound.session,
            tap: inbound.tap,
            srtp_profile,
            srtp_exchange,
        };

        info!(
            code,
            srtp = accepted.srtp_profile.is_some(),
            "outbound delayed-offer call answered and media bridged"
        );
        Ok(OutboundCall {
            accepted,
            dialog,
            call_handle: handle,
            call_id,
        })
    }

    /// Send BYE on an established call's dialog. The daemon calls this after
    /// the `CallController` returns. Best-effort — a BYE failure is logged,
    /// not propagated (the call is over either way).
    pub async fn hangup(&self, dialog: &Dialog) {
        if let Err(e) = self.uac.bye(dialog).await {
            warn!(error = %e, "outbound BYE failed");
        }
    }

    /// Tear down the call's forge media session. Used both to roll back a
    /// failed origination and to clean up after a completed call.
    pub async fn stop_session(&self, call_id: &CallId) {
        if let Err(e) = self.media.session_manager().stop_session(call_id).await {
            warn!(call_id = %call_id, error = %e, "outbound session teardown failed");
        }
    }
}

/// Map a non-2xx INVITE final response to a semantic [`NotAnsweredCause`].
/// (401/407 are auto-retried by the UAC, so a 401/407 reaching here means
/// no/!wrong credentials — it falls through to `Rejected`.)
fn classify_failure(code: u16, reason: &str) -> NotAnsweredCause {
    match code {
        486 | 600 => NotAnsweredCause::Busy,
        403 | 603 => NotAnsweredCause::Declined,
        408 | 480 | 487 => NotAnsweredCause::NoAnswer,
        _ => NotAnsweredCause::Rejected {
            code,
            reason: reason.to_string(),
        },
    }
}

/// A static digest credential source for a gateway's UAC — supplies the
/// configured username/password on any 401/407 challenge so the UAC's
/// auto-retry can authenticate to the trunk. (One credential per UAC, so we
/// answer every realm; build a UAC per gateway.)
pub struct StaticCredentials {
    username: String,
    password: String,
}

impl StaticCredentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }
}

#[async_trait]
impl CredentialProvider for StaticCredentials {
    async fn credentials(&self, _realm: &str) -> Option<(String, String)> {
        Some((self.username.clone(), self.password.clone()))
    }
}

/// Why the [`OutboundGuard`] refused to admit a new outbound call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundRejection {
    /// `max_concurrent_outbound` is already in use.
    AtCapacity,
    /// The per-second rate limit was exceeded.
    RateLimited,
}

/// The native guardrails on the originate path (the originate API has no
/// built-in auth — see `docs/design/DEV_PLAN_0.6.0.md` §9.5/§9.6). A
/// `max_concurrent` semaphore plus an optional per-second token-bucket rate
/// limit. Acquire a permit before placing a call; hold it for the call's
/// lifetime (dropping it frees the slot).
pub struct OutboundGuard {
    concurrency: Arc<Semaphore>,
    rate: Option<Mutex<TokenBucket>>,
}

impl OutboundGuard {
    pub fn new(max_concurrent: usize, rate_limit_per_sec: Option<u32>) -> Self {
        Self {
            concurrency: Arc::new(Semaphore::new(max_concurrent)),
            rate: rate_limit_per_sec
                .filter(|&r| r > 0)
                .map(|r| Mutex::new(TokenBucket::new(r as f64, Instant::now()))),
        }
    }

    /// Try to admit one new outbound call. On success returns a permit the
    /// caller holds for the call's lifetime; the concurrency slot is freed
    /// when it drops. The rate token (if any) is only consumed on admission.
    pub fn try_admit(&self) -> Result<OutboundPermit, OutboundRejection> {
        // Concurrency first — a rejected slot consumes nothing.
        let permit = Arc::clone(&self.concurrency)
            .try_acquire_owned()
            .map_err(|_| OutboundRejection::AtCapacity)?;
        if let Some(rate) = &self.rate {
            if !rate
                .lock()
                .expect("rate bucket mutex")
                .try_take(Instant::now())
            {
                drop(permit); // give the concurrency slot back
                return Err(OutboundRejection::RateLimited);
            }
        }
        Ok(OutboundPermit { _permit: permit })
    }

    /// Currently-available concurrency slots (for a metric / admin view).
    pub fn available(&self) -> usize {
        self.concurrency.available_permits()
    }
}

/// Held for the lifetime of an admitted outbound call; frees the
/// concurrency slot on drop.
#[derive(Debug)]
pub struct OutboundPermit {
    _permit: OwnedSemaphorePermit,
}

/// A simple token bucket: `refill_per_sec` tokens/sec, burst = the rate.
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(rate: f64, now: Instant) -> Self {
        Self {
            tokens: rate,
            capacity: rate,
            refill_per_sec: rate,
            last: now,
        }
    }

    fn try_take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_failure_maps_sip_codes() {
        assert_eq!(classify_failure(486, "Busy Here"), NotAnsweredCause::Busy);
        assert_eq!(
            classify_failure(600, "Busy Everywhere"),
            NotAnsweredCause::Busy
        );
        assert_eq!(classify_failure(603, "Decline"), NotAnsweredCause::Declined);
        assert_eq!(
            classify_failure(403, "Forbidden"),
            NotAnsweredCause::Declined
        );
        assert_eq!(
            classify_failure(408, "Request Timeout"),
            NotAnsweredCause::NoAnswer
        );
        assert_eq!(
            classify_failure(480, "Temporarily Unavailable"),
            NotAnsweredCause::NoAnswer
        );
        assert_eq!(
            classify_failure(487, "Request Terminated"),
            NotAnsweredCause::NoAnswer
        );
    }

    #[test]
    fn classify_failure_falls_back_to_rejected_with_code() {
        for (code, reason) in [
            (404, "Not Found"),
            (488, "Not Acceptable Here"),
            (503, "Service Unavailable"),
            (407, "Proxy Authentication Required"),
        ] {
            assert_eq!(
                classify_failure(code, reason),
                NotAnsweredCause::Rejected {
                    code,
                    reason: reason.to_string()
                },
                "code {code} should fall back to Rejected"
            );
        }
    }

    #[test]
    fn guard_caps_concurrency_and_releases_on_drop() {
        let guard = OutboundGuard::new(2, None);
        let p1 = guard.try_admit().expect("1st admit");
        let _p2 = guard.try_admit().expect("2nd admit");
        assert_eq!(guard.available(), 0);
        assert_eq!(
            guard.try_admit().unwrap_err(),
            OutboundRejection::AtCapacity
        );
        drop(p1); // call ended → slot freed
        assert_eq!(guard.available(), 1);
        let _p3 = guard.try_admit().expect("admit after a call ended");
    }

    #[test]
    fn guard_with_zero_capacity_always_rejects() {
        let guard = OutboundGuard::new(0, None);
        assert_eq!(
            guard.try_admit().unwrap_err(),
            OutboundRejection::AtCapacity
        );
    }

    #[test]
    fn token_bucket_limits_then_refills() {
        let base = Instant::now();
        let mut bucket = TokenBucket::new(2.0, base);
        assert!(bucket.try_take(base), "burst token 1");
        assert!(bucket.try_take(base), "burst token 2");
        assert!(
            !bucket.try_take(base),
            "3rd within the same instant is limited"
        );
        // One second later, ~2 tokens have refilled.
        let later = base + std::time::Duration::from_secs(1);
        assert!(bucket.try_take(later), "refilled token available");
    }

    #[test]
    fn guard_rate_limit_rejects_without_consuming_a_slot() {
        // rate 1/s, cap 5 — the 2nd immediate admit is rate-limited, and the
        // concurrency slot it briefly took is given back.
        let guard = OutboundGuard::new(5, Some(1));
        let _p1 = guard.try_admit().expect("1st admit");
        assert_eq!(
            guard.try_admit().unwrap_err(),
            OutboundRejection::RateLimited
        );
        assert_eq!(
            guard.available(),
            4,
            "rate-limited admit didn't keep a slot"
        );
    }

    #[tokio::test]
    async fn static_credentials_answers_any_realm() {
        let creds = StaticCredentials::new("alice", "s3cret");
        assert_eq!(
            creds.credentials("sip.example.com").await,
            Some(("alice".to_string(), "s3cret".to_string()))
        );
    }
}
