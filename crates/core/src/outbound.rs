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

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use forge_core::CallId;
use sip_core::SipUri;
use sip_dialog::Dialog;
use sip_uac::integrated::{CallHandle, IntegratedUAC, RequestTarget};
use sip_uac::CredentialProvider;
use siphon_ai_media_glue::{
    MediaSetup, OutboundAccepted, OutboundOfferRequest, SetupError, TapOptions,
};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
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
/// built-in auth — see `docs/DEV_PLAN_0.6.0.md` §9.5/§9.6). A
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
