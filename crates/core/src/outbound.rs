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

use std::sync::Arc;

use forge_core::CallId;
use sip_core::SipUri;
use sip_dialog::Dialog;
use sip_uac::integrated::{CallHandle, IntegratedUAC, RequestTarget};
use siphon_ai_media_glue::{
    MediaSetup, OutboundAccepted, OutboundOfferRequest, SetupError, TapOptions,
};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

/// Places outbound calls through a shared UAC, allocating + binding media
/// via [`MediaSetup`]. Daemon-wide; cheap to construct.
pub struct OutboundOriginator {
    media: MediaSetup,
    uac: Arc<IntegratedUAC>,
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
        Self { media, uac }
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
                self.rollback(&call_id).await;
                return Err(OutboundError::Transport(e.to_string()));
            }
        };

        // (3) Await the final response (the UAC drains provisionals + sends
        //     ACK on 2xx itself).
        let response = match handle.await_final().await {
            Ok(r) => r,
            Err(e) => {
                self.rollback(&call_id).await;
                return Err(OutboundError::Transport(e.to_string()));
            }
        };

        let code = response.code();
        if !(200..300).contains(&code) {
            let cause = classify_failure(code, response.reason());
            debug!(code, ?cause, "outbound INVITE not answered");
            self.rollback(&call_id).await;
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
        })
    }

    /// End an established outbound call by sending BYE on its dialog. The
    /// daemon calls this after the `CallController` returns. Best-effort —
    /// a BYE failure is logged, not propagated (the call is over either way).
    pub async fn hangup(&self, call: &OutboundCall) {
        if let Err(e) = self.uac.bye(&call.dialog).await {
            warn!(error = %e, "outbound BYE failed");
        }
    }

    async fn rollback(&self, call_id: &CallId) {
        if let Err(e) = self.media.session_manager().stop_session(call_id).await {
            warn!(call_id = %call_id, error = %e, "outbound session rollback failed");
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
}
