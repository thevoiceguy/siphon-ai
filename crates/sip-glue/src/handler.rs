//! `UasRequestHandler` impl that routes inbound INVITEs.
//!
//! Sits between siphon-rs's `IntegratedUAS` and the eventual
//! `core::CallController`. The flow:
//!
//! ```text
//!   IntegratedUAS::dispatch ─► RoutingHandler::on_invite
//!                                       │
//!                                       ▼
//!                              dispatch_invite (sync)
//!                              ├── RouteAction::SendFinal(404)  ─► handle.send_final
//!                              └── RouteAction::Accept ─► CallAcceptor::on_matched
//! ```
//!
//! `dispatch_invite` is intentionally synchronous so unit tests can
//! exercise the routing decision without standing up a transaction
//! manager. The async trait impl is a thin shim over it.
//!
//! ## Re-INVITE
//!
//! Routing only applies to *new* calls (`dialog: None`). Mid-dialog
//! re-INVITEs belong to the `CallController`'s acceptor — the
//! routing handler dispatches them via `CallAcceptor::on_reinvite`,
//! which validates the offer, mirrors the direction (hold / resume),
//! and answers 200 OK. The trait's default `on_reinvite` still
//! responds 501 for acceptors that didn't override it; production
//! impls (e.g., `BridgingAcceptor`) override and answer for real.
//! Mid-call codec / port renegotiation is rejected with 488 per
//! `BridgingAcceptor::on_reinvite` — that's a post-v1 feature.
//!
//! ## Contact / User-Agent on the 404
//!
//! `IntegratedUAS::auto_fill_headers` is only run for responses
//! IntegratedUAS itself synthesizes (100 Trying, 481, 405, 501). When
//! a `UasRequestHandler` returns a response via `handle.send_final`,
//! the header auto-fill is skipped. We don't add Contact to the 404
//! here because RFC 3261 §20.10 makes Contact optional on 4xx
//! responses; if a deployment needs it, the `RegisterSourceResolver`
//! seam is the right place to plug in a Contact-aware finalizer.

use std::sync::{Arc, Weak};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use sip_core::{Request, Response};
use sip_dialog::Dialog;
use sip_transaction::{ServerTransactionHandle, TransportContext};
use sip_uas::integrated::{IntegratedUAS, UasRequestHandler};
use sip_uas::UserAgentServer;
use siphon_ai_routes::{CompiledRoute, RouteSet};
// Metric names from the crate that declares them — see the note in
// media-glue's tap.rs (#474).
use siphon_ai_telemetry::metrics::{INVITE_ADMISSION_SOURCES, NOTIFY_TOTAL};
use tracing::{debug, info, instrument, warn};

use crate::dialog::{
    dispatch_bye, dispatch_cancel, DialogAction, DialogTerminatorHandle, NullDialogTerminator,
};

use crate::invite::InviteFacts;
use crate::route::{route_invite, RouteDecision};

/// Resolves the `register_source` value for an inbound request.
///
/// Returns the `name` of the matching `[[register]]` block, or
/// `"trunk"` for unregistered inbound. The default returns
/// `"trunk"` unconditionally (UAS-only / trunk-mode deployments);
/// register-mode plumbing in Week 4 will hand a richer resolver
/// that consults the daemon's registration registry.
pub type RegisterSourceResolver = Arc<dyn Fn(&Request, &TransportContext) -> String + Send + Sync>;

/// Allowlist gate consulted on every inbound INVITE (new dialogs
/// only — re-INVITEs use the previously-established register
/// source). Implementations identify the peer by source IP, From
/// URI host, or both. When configured and the peer does not match
/// any trunk, the routing handler rejects the INVITE with
/// `403 Forbidden` BEFORE any route matching or media setup runs.
///
/// `RoutingHandler::new` installs no gate (legacy "accept any
/// source" posture). The daemon's runtime installs a real impl
/// when `[[trunk]]` blocks are declared in the TOML config.
pub trait TrunkAllowlist: Send + Sync {
    /// Identify the inbound peer. `Some(register_source)` means
    /// the peer matched a trunk and the daemon should treat the
    /// call as originating from that trunk's name. `None` means
    /// no trunk matched and the routing handler should respond
    /// `403 Forbidden`.
    fn identify(&self, request: &Request, ctx: &TransportContext) -> Option<String>;
}

/// Convenience alias for the trait-object form.
pub type TrunkAllowlistHandle = Arc<dyn TrunkAllowlist>;

/// What [`dispatch_invite`] decided the daemon should do.
#[derive(Debug)]
pub enum RouteAction<'a> {
    /// Send this final response and stop. Used for 404 (no route
    /// matched) and 501 (re-INVITE, not yet implemented).
    SendFinal(Response),
    /// A route matched; hand off to the [`CallAcceptor`].
    Accept {
        facts: InviteFacts,
        route: &'a CompiledRoute,
    },
}

/// Build the drain-reject response for a *new* out-of-dialog INVITE
/// when the daemon is draining for shutdown (0.17.0), or `None` when it
/// isn't. `503 Service Unavailable` + `Retry-After` is the "this node
/// is going away, route elsewhere / retry" posture (RFC 3261 §21.5.4).
///
/// Pure (no UAS auto-fill / send) so it's unit-testable like
/// [`dispatch_invite`]; the caller applies `fill_response` and
/// `send_final`.
fn drain_reject_response(
    drain: &crate::drain::DrainFlag,
    retry_after_secs: u32,
    request: &Request,
) -> Option<Response> {
    if !drain.is_draining() {
        return None;
    }
    let mut response = UserAgentServer::create_response(request, 503, "Service Unavailable");
    let _ = response
        .headers_mut()
        .set_or_push("Retry-After", retry_after_secs.to_string());
    Some(response)
}

/// Decide what to do with an inbound INVITE.
///
/// Pure / synchronous. The async trait wrapper [`RoutingHandler`]
/// adapts this to the upstream [`UasRequestHandler`] surface.
pub fn dispatch_invite<'a>(
    routes: &'a RouteSet,
    register_source: &str,
    request: &Request,
) -> RouteAction<'a> {
    match route_invite(request, register_source, routes) {
        RouteDecision::Matched { facts, route } => {
            info!(
                route = route.name.as_str(),
                from_user = facts.from_user.as_str(),
                request_uri_user = facts.request_uri_user.as_str(),
                register_source,
                "INVITE routed"
            );
            RouteAction::Accept { facts, route }
        }
        RouteDecision::NoMatch { facts } => {
            warn!(
                from_user = facts.from_user.as_str(),
                request_uri_user = facts.request_uri_user.as_str(),
                register_source,
                "INVITE rejected: no route matched"
            );
            RouteAction::SendFinal(UserAgentServer::create_response(request, 404, "Not Found"))
        }
    }
}

/// Methods this UAS answers for real, advertised in `Allow` (405 and
/// OPTIONS responses). The upstream default plus NOTIFY, which
/// [`dispatch_notify`] answers since #357. RFC 3261 §20.5 defines
/// `Allow` as exactly the methods the UA supports — keep this in sync
/// with the `on_*` overrides below.
const SUPPORTED_METHODS: &[&str] = &["INVITE", "ACK", "BYE", "CANCEL", "OPTIONS", "NOTIFY"];

/// Decide the response to an inbound NOTIFY (#357).
///
/// SiphonAI's only NOTIFY producer is the implicit refer subscription
/// (RFC 3515 §2.4.4): after our outgoing REFER is accepted, the peer
/// reports transfer progress via `NOTIFY Event: refer`. Those are
/// accepted (`200 OK`) and dropped — v1 doesn't surface them over the
/// WS (`docs/PROTOCOL.md` §4.4), and the REFER+BYE teardown (RFC 5589
/// §6.1) means we never act on the progress they carry. Deliberately
/// dialog-blind: the refer NOTIFY normally arrives *after* our BYE
/// tore the dialog down, so a dialog-scoped responder would 481 the
/// very message this exists to accept.
///
/// `Event: message-summary` is absorbed with `200 OK` and no further
/// action (issue #486). A PBX we register to pushes unsolicited MWI
/// (RFC 3842) at the account after every REGISTER — FreeSWITCH and
/// Asterisk both do by default when the account has a mailbox — so on
/// a registered node this arrives once per registration refresh,
/// forever. Answering `489` was RFC-defensible but made
/// `siphon_ai_notify_total{result="bad_event"}` climb on a perfectly
/// healthy daemon, burying the genuinely unexpected package it exists
/// to reveal. A bridge has no mailbox to display, so the honest
/// handling is the one every hard phone on that PBX already performs:
/// take it and drop it. Counted `ignored`, not `accepted`, so absorbed
/// MWI stays distinguishable from post-REFER transfer progress.
///
/// Deliberately not conditioned on `Subscription-State: terminated`:
/// SiphonAI never sends a `message-summary` SUBSCRIBE, so *every* MWI
/// NOTIFY it can receive is unsolicited by construction, whichever
/// state flavour the PBX stamps on it. Gating on `terminated` would
/// re-open the same noise against a PBX that uses `active`.
///
/// Anything else follows RFC 6665: an event package we don't support
/// gets `489 Bad Event` (with `Allow-Events` naming what we do
/// support, §4.4.1), and a NOTIFY with no `Event` header at all is
/// malformed (§8.2.3) → `400`. Never 405 — NOTIFY is in our `Allow`
/// set now.
///
/// Pure / synchronous, like [`dispatch_invite`], so unit tests can
/// exercise the decision without a transaction manager.
pub fn dispatch_notify(request: &Request) -> NotifyDisposition {
    // "o" is the compact form of Event (RFC 6665 §8.1).
    let event = request
        .headers()
        .get("Event")
        .or_else(|| request.headers().get("o"));
    match event {
        Some(value) => {
            let package = value.split(';').next().unwrap_or("").trim();
            if package.eq_ignore_ascii_case("refer") {
                NotifyDisposition {
                    response: UserAgentServer::create_response(request, 200, "OK"),
                    result: "accepted",
                }
            } else if package.eq_ignore_ascii_case("message-summary") {
                NotifyDisposition {
                    response: UserAgentServer::create_response(request, 200, "OK"),
                    result: "ignored",
                }
            } else {
                let mut response = UserAgentServer::create_response(request, 489, "Bad Event");
                let _ = response.headers_mut().set_or_push("Allow-Events", "refer");
                NotifyDisposition {
                    response,
                    result: "bad_event",
                }
            }
        }
        None => NotifyDisposition {
            response: UserAgentServer::create_response(request, 400, "Bad Request"),
            result: "bad_request",
        },
    }
}

/// What [`dispatch_notify`] decided: the response to send, and the
/// `result` label to score it under.
///
/// The label travels with the response rather than being recovered
/// from its status code, because the code alone is now ambiguous —
/// refer progress and absorbed MWI are both `200`, and only the
/// decision site knows which is which.
pub struct NotifyDisposition {
    pub response: Response,
    /// `result` label for [`NOTIFY_TOTAL`]. One of `accepted`,
    /// `ignored`, `bad_event`, `bad_request`.
    pub result: &'static str,
}

/// One routed INVITE handed to the acceptor.
///
/// `handle` is owned by-value so the acceptor can move it into a
/// spawned controller task and respond at its leisure (200 OK after
/// SDP answer is built, 486 if the bridge refuses, etc.). The other
/// fields are borrowed for the duration of the on_matched call and
/// must be cloned/copied if the acceptor needs them past that point.
pub struct MatchedCall<'a> {
    pub request: &'a Request,
    pub handle: ServerTransactionHandle,
    pub transport: &'a TransportContext,
    pub facts: InviteFacts,
    pub route: &'a CompiledRoute,
}

/// Inputs to a re-INVITE handler. The routing handler dispatches
/// in-dialog INVITEs (the SIP UAS resolves the dialog before us)
/// here so the acceptor can answer with a new SDP — typically for
/// hold/resume, where only the `a=` direction attribute changes.
pub struct ReinviteCall<'a> {
    pub request: &'a Request,
    pub handle: ServerTransactionHandle,
    pub transport: &'a TransportContext,
    pub dialog: &'a Dialog,
    /// The SIP `Call-ID` header value. Cached here so the acceptor
    /// doesn't have to re-parse it to look up the cached answer
    /// SDP in its registry.
    pub sip_call_id: String,
}

/// Inputs to an ACK handler. An ACK has no response, so there is no
/// transaction handle — just the request (whose body may carry an SDP
/// answer, for the delayed-offer flow) and the resolved dialog.
///
/// Every in-dialog ACK is dispatched here, body or not (#425). The
/// acceptor matches the dialog against any half-negotiated
/// delayed-offer call it is holding: a body finalizes media from the
/// answer it carries, while a body-less ACK on a held dialog is the
/// peer failing to answer our offer (RFC 3261 §13.2.2.4) and reaps the
/// call as `missing_sdp_answer`. ACKs for dialogs the acceptor is not
/// holding — the entire early-offer population — are a no-op.
pub struct AckCall<'a> {
    pub request: &'a Request,
    pub dialog: &'a Dialog,
    /// The SIP `Call-ID` header value, cached so the acceptor doesn't
    /// re-parse it to look up its held delayed-offer state.
    pub sip_call_id: String,
}

/// Hook for the eventual `core::CallController`. SiphonAI's
/// per-call setup logic — answer with SDP, attach MediaTap, open
/// the WS bridge — implements this trait. Routing doesn't know
/// about media or bridges; it only knows "this call matched route
/// X, here's the handle, go do your thing."
#[async_trait]
pub trait CallAcceptor: Send + Sync {
    /// A matched INVITE arrived. The acceptor MUST send a final
    /// response (directly via `call.handle.send_final`, or by
    /// arranging for a spawned task to do so); otherwise the call
    /// stays in 100 Trying until the transaction times out.
    async fn on_matched(&self, call: MatchedCall<'_>) -> anyhow::Result<()>;

    /// A re-INVITE on an existing dialog arrived. The default impl
    /// returns 501 Not Implemented; consumers that handle
    /// hold/resume override this. Same contract as `on_matched`
    /// re sending the final response.
    async fn on_reinvite(&self, call: ReinviteCall<'_>) -> anyhow::Result<()> {
        let response = UserAgentServer::create_response(call.request, 501, "Not Implemented");
        call.handle.send_final(response).await;
        Ok(())
    }

    /// An in-dialog ACK arrived (with or without a body). The default
    /// impl ignores it (early-offer ACKs need no application
    /// handling). The delayed-offer acceptor overrides this to read
    /// the SDP answer from the ACK body and finalize the call — or,
    /// for a body-less ACK on a dialog it is holding, to reap the
    /// call as `missing_sdp_answer` (#425). There is no response to
    /// send — an ACK is the end of the INVITE transaction.
    async fn on_ack(&self, call: AckCall<'_>) -> anyhow::Result<()> {
        let _ = call;
        Ok(())
    }
}

/// `UasRequestHandler` that does INVITE routing, mid-dialog
/// teardown (BYE / CANCEL), and the refer-NOTIFY responder (#357).
/// Other methods fall through to the trait's default 405/501
/// responses.
pub struct RoutingHandler<A> {
    /// Hot-swappable route table. New INVITEs read the current value
    /// via [`ArcSwap::load`]; a SIGHUP config reload `store`s a fresh
    /// `RouteSet` and subsequent INVITEs pick it up. In-flight calls
    /// already matched are unaffected (they captured their route).
    routes: Arc<ArcSwap<RouteSet>>,
    acceptor: Arc<A>,
    resolver: RegisterSourceResolver,
    terminator: DialogTerminatorHandle,
    /// Trunk gate. `None` means "no `[[trunk]]` blocks declared"
    /// — accept INVITEs from any source (legacy posture). `Some`
    /// flips the daemon into strict-allowlist mode: an INVITE
    /// that doesn't match any trunk gets 403.
    trunk_gate: Option<TrunkAllowlistHandle>,
    /// Weak ref to the `IntegratedUAS` we feed. Used to apply
    /// `prepare_response` (rport / received / Contact / User-Agent
    /// auto-fill) to responses the handler builds directly — the
    /// trunk-rejection 403 and the route-no-match 404 / 488 paths
    /// otherwise bypass the auto-fill that the rest of the UAS
    /// applies via its dispatch loop. Weak avoids the cyclic
    /// `Arc<UAS>` ↔ `Arc<RoutingHandler>` reference. Injected by
    /// the daemon via `install_uas_filler` once the UAS exists;
    /// `OnceLock` because the install is one-shot at startup.
    uas_filler: std::sync::OnceLock<Weak<IntegratedUAS>>,
    /// Graceful-shutdown drain flag (0.17.0). Default = never
    /// draining (so `RoutingHandler::new` and tests behave exactly as
    /// before). When the runtime's drain phase flips it, *new*
    /// out-of-dialog INVITEs are answered `503 Service Unavailable`
    /// before any trunk/route work; in-dialog requests (re-INVITE,
    /// ACK, BYE) for calls already up still flow so they can drain.
    drain: crate::drain::DrainFlag,
    /// `Retry-After` delta-seconds put on the drain 503. Hints when
    /// the node will be gone — the runtime sets it from
    /// `[shutdown].drain_timeout_secs`. Ignored unless `drain` fires.
    drain_retry_after_secs: u32,
    /// Inbound digest authentication (0.19.0). `None` ⇒ off (no
    /// `[sip.auth]`). When set, a *new* INVITE whose source
    /// [`InboundDigestAuth::requires_auth`] is challenged with `401`
    /// unless it carries a valid `Authorization` — AND'd with the trunk
    /// allowlist, so it runs after the allowlist and before route
    /// dispatch.
    digest_auth: Option<Arc<crate::digest::InboundDigestAuth>>,
    /// Inbound INVITE admission control (0.19.0). `None` ⇒ off (no
    /// `[sip.admission]`). When set, it's the **first** gate on a new
    /// INVITE — per-source rate limit + global concurrency cap — so a
    /// flood is shed before any trunk/auth/route work.
    admission: Option<Arc<crate::admission::InviteAdmission>>,
}

impl<A> RoutingHandler<A> {
    /// Build a handler with the default register-source resolver
    /// (always returns `"trunk"` — fine for UAS-only deployments)
    /// and a no-op dialog terminator. Wire a real terminator with
    /// [`Self::with_dialog_terminator`] before deploying — without
    /// it, BYEs are 200 OK'd but the per-call controller doesn't
    /// learn the SIP leg ended.
    pub fn new(routes: Arc<ArcSwap<RouteSet>>, acceptor: Arc<A>) -> Self {
        Self {
            routes,
            acceptor,
            resolver: default_resolver(),
            terminator: Arc::new(NullDialogTerminator),
            trunk_gate: None,
            uas_filler: std::sync::OnceLock::new(),
            drain: crate::drain::DrainFlag::new(),
            drain_retry_after_secs: 5,
            digest_auth: None,
            admission: None,
        }
    }

    /// Inject a weak reference to the `IntegratedUAS` whose
    /// `prepare_response` (Contact / User-Agent / topmost-Via
    /// `rport` + `received`) should be applied to responses the
    /// handler builds directly. Set once at daemon startup once
    /// both the UAS and the handler exist (the cycle is broken
    /// by `Weak`). Calling again is a no-op.
    pub fn install_uas_filler(&self, uas: Weak<IntegratedUAS>) {
        let _ = self.uas_filler.set(uas);
    }

    /// Apply UAS auto-fill to a response the handler is about to
    /// send. No-op when the daemon hasn't injected a UAS reference
    /// (used in tests and as a fail-safe).
    async fn fill_response(&self, response: &mut Response, ctx: &TransportContext) {
        if let Some(uas) = self.uas_filler.get().and_then(Weak::upgrade) {
            uas.prepare_response(response, ctx).await;
        }
    }

    /// Override the register-source resolver. Used by the daemon in
    /// Week 4 to map an inbound request's transport peer / Contact
    /// to a `[[register]]` block name.
    pub fn with_register_source_resolver(mut self, resolver: RegisterSourceResolver) -> Self {
        self.resolver = resolver;
        self
    }

    /// Plug in the dialog terminator (typically
    /// `siphon-ai-core::CallRegistry`). Must match the registry the
    /// `CallAcceptor` registers handles into.
    pub fn with_dialog_terminator(mut self, terminator: DialogTerminatorHandle) -> Self {
        self.terminator = terminator;
        self
    }

    /// Install the trunk allowlist gate. Pass `None` (or simply
    /// don't call this method) to keep legacy "accept any source"
    /// behaviour. The daemon constructs an impl from the TOML
    /// `[[trunk]]` blocks.
    pub fn with_trunk_gate(mut self, gate: TrunkAllowlistHandle) -> Self {
        self.trunk_gate = Some(gate);
        self
    }

    /// Install the graceful-shutdown drain flag (0.17.0) and the
    /// `Retry-After` delta-seconds advertised on the drain 503. The
    /// runtime shares one flag between its `run()` drain phase (which
    /// flips it) and this handler (which reads it). Without this call
    /// the handler never drains — `RoutingHandler::new`'s default flag
    /// stays not-draining.
    pub fn with_drain(mut self, drain: crate::drain::DrainFlag, retry_after_secs: u32) -> Self {
        self.drain = drain;
        self.drain_retry_after_secs = retry_after_secs;
        self
    }

    /// Install inbound digest authentication (0.19.0). The daemon
    /// constructs the [`InboundDigestAuth`](crate::digest::InboundDigestAuth)
    /// from `[sip.auth]` + the per-trunk `auth_required` flags. Without
    /// this call, no INVITE is ever challenged.
    pub fn with_digest_auth(mut self, auth: Arc<crate::digest::InboundDigestAuth>) -> Self {
        self.digest_auth = Some(auth);
        self
    }

    /// Install inbound INVITE admission control (0.19.0). The daemon
    /// constructs the [`InviteAdmission`](crate::admission::InviteAdmission)
    /// from `[sip.admission]` (the active-call count comes from the
    /// `CallRegistry`). Without this call, no INVITE is rate-limited.
    pub fn with_admission(mut self, admission: Arc<crate::admission::InviteAdmission>) -> Self {
        self.admission = Some(admission);
        self
    }

    /// Snapshot of the current route table (after any SIGHUP reload).
    pub fn routes(&self) -> Arc<RouteSet> {
        self.routes.load_full()
    }
}

fn default_resolver() -> RegisterSourceResolver {
    Arc::new(|_req, _ctx| String::from("trunk"))
}

#[async_trait]
impl<A: CallAcceptor + 'static> UasRequestHandler for RoutingHandler<A> {
    fn supported_methods(&self) -> &'static [&'static str] {
        SUPPORTED_METHODS
    }

    /// Refer-progress NOTIFYs (post-REFER, RFC 3515 implicit
    /// subscription) are accepted and dropped, and a PBX's unsolicited
    /// MWI is absorbed; see [`dispatch_notify`] for the full decision
    /// table. No transport context on this hook, so no `fill_response`
    /// — same as the upstream default it replaces.
    #[instrument(skip_all, fields(method = "NOTIFY"))]
    async fn on_notify(
        &self,
        request: &Request,
        handle: ServerTransactionHandle,
    ) -> anyhow::Result<()> {
        let NotifyDisposition { response, result } = dispatch_notify(request);
        metrics::counter!(NOTIFY_TOTAL, "result" => result).increment(1);
        debug!(
            sip_call_id = request.headers().get("Call-ID").unwrap_or(""),
            event = request.headers().get("Event").unwrap_or(""),
            code = response.code(),
            result,
            "NOTIFY answered"
        );
        handle.send_final(response).await;
        Ok(())
    }

    #[instrument(skip_all, fields(method = "INVITE", peer = %ctx.peer()))]
    async fn on_invite(
        &self,
        request: &Request,
        handle: ServerTransactionHandle,
        ctx: &TransportContext,
        dialog: Option<&Dialog>,
    ) -> anyhow::Result<()> {
        if let Some(dialog) = dialog {
            // Re-INVITE on an existing dialog — hold / resume /
            // mid-call codec change. Routing doesn't dispatch on
            // route again; the acceptor knows the call's negotiated
            // state (RTP port, codec, last answer SDP) and answers
            // with a matching mid-dialog 200 OK.
            let sip_call_id = request
                .headers()
                .get_smol("Call-ID")
                .map(|s| s.to_string())
                .unwrap_or_default();
            debug!(sip_call_id = %sip_call_id, "re-INVITE → acceptor");
            return self
                .acceptor
                .on_reinvite(ReinviteCall {
                    request,
                    handle,
                    transport: ctx,
                    dialog,
                    sip_call_id,
                })
                .await;
        }

        // Admission control (0.19.0). The FIRST gate on a new INVITE —
        // per-source rate limit + global concurrency cap — so a flood is
        // shed before any drain/trunk/auth/route work. A rate trip is a
        // retryable 503; a source flooding past the drop threshold gets
        // no response at all (don't amplify).
        if let Some(admission) = self.admission.as_ref() {
            let decision = admission.check(ctx.peer().ip());
            metrics::counter!(
                "siphon_ai_invite_admission_total",
                "result" => decision.metric_result(),
            )
            .increment(1);
            metrics::gauge!(INVITE_ADMISSION_SOURCES).set(admission.source_count() as f64);
            match decision {
                crate::admission::AdmissionDecision::Accept => {}
                crate::admission::AdmissionDecision::Reject503 => {
                    warn!(
                        peer = %ctx.peer(),
                        "INVITE rejected: admission rate limit (503 Service Unavailable)"
                    );
                    if siphon_ai_audit::is_enabled() {
                        siphon_ai_audit::emit(siphon_ai_audit::AuditEvent::invite_rejected(
                            ctx.peer().to_string(),
                            "rate_limited",
                        ));
                    }
                    let mut response =
                        UserAgentServer::create_response(request, 503, "Service Unavailable");
                    let _ = response.headers_mut().set_or_push(
                        "Retry-After",
                        crate::admission::ADMISSION_RETRY_AFTER_SECS.to_string(),
                    );
                    self.fill_response(&mut response, ctx).await;
                    handle.send_final(response).await;
                    return Ok(());
                }
                crate::admission::AdmissionDecision::Drop => {
                    // Silently drop — no response. The retransmits a
                    // flooder sends will be dropped just as cheaply.
                    // Deliberately NOT audited: this is the flood-shedding
                    // fast path (fires per packet under attack), so emitting
                    // here would amplify the very DoS it defends against.
                    // The onset of shedding is captured by the Reject503
                    // `rate_limited` events above.
                    debug!(
                        peer = %ctx.peer(),
                        "INVITE dropped: admission flood threshold (no response)"
                    );
                    return Ok(());
                }
            }
        }

        // Drain gate (graceful shutdown, 0.17.0). We've already
        // returned above for in-dialog re-INVITEs, so reaching here
        // means this is a *new* out-of-dialog INVITE. While draining,
        // reject it with `503 Service Unavailable` + `Retry-After` so
        // an upstream proxy/LB routes elsewhere — complementing the
        // `/ready` flip (which a load balancer notices only on its
        // next poll). Runs BEFORE the trunk/route work so a node
        // that's going away does no per-call setup.
        if let Some(mut response) =
            drain_reject_response(&self.drain, self.drain_retry_after_secs, request)
        {
            warn!(
                peer = %ctx.peer(),
                "INVITE rejected: draining for shutdown (503 Service Unavailable)"
            );
            if siphon_ai_audit::is_enabled() {
                siphon_ai_audit::emit(siphon_ai_audit::AuditEvent::invite_rejected(
                    ctx.peer().to_string(),
                    "draining",
                ));
            }
            self.fill_response(&mut response, ctx).await;
            handle.send_final(response).await;
            return Ok(());
        }

        // Trunk allowlist gate, when configured. Runs BEFORE route
        // matching so a rejected peer never reaches media setup or
        // the per-call task. When no gate is installed (legacy
        // mode), fall back to the resolver — typically "trunk".
        let register_source = if let Some(gate) = self.trunk_gate.as_ref() {
            match gate.identify(request, ctx) {
                Some(name) => name,
                None => {
                    warn!(
                        peer = %ctx.peer(),
                        "INVITE rejected: no trunk matched (403 Forbidden)"
                    );
                    if siphon_ai_audit::is_enabled() {
                        siphon_ai_audit::emit(siphon_ai_audit::AuditEvent::invite_rejected(
                            ctx.peer().to_string(),
                            "no_trunk",
                        ));
                    }
                    let mut response = UserAgentServer::create_response(request, 403, "Forbidden");
                    self.fill_response(&mut response, ctx).await;
                    handle.send_final(response).await;
                    return Ok(());
                }
            }
        } else {
            (self.resolver)(request, ctx)
        };

        // Inbound digest auth gate (0.19.0). Runs AFTER the trunk
        // allowlist (so the source is already known) and BEFORE route
        // dispatch (so an unauthenticated INVITE does no per-call setup).
        // Only sources whose policy requires it are challenged — a
        // static-IP carrier trunk without `auth_required` stays
        // allowlist-only and is never asked for credentials.
        if let Some(digest) = self.digest_auth.as_ref() {
            if digest.requires_auth(&register_source) {
                let outcome = digest.evaluate(request);
                metrics::counter!(
                    "siphon_ai_sip_auth_total",
                    "result" => outcome.metric_result(),
                )
                .increment(1);
                if let crate::digest::DigestOutcome::Challenge { stale, .. } = &outcome {
                    let result = outcome.metric_result();
                    warn!(
                        peer = %ctx.peer(),
                        register_source = %register_source,
                        stale = *stale,
                        result,
                        "INVITE challenged: digest authentication required (401)"
                    );
                    // Audit `failed` (a bad credential — the attack
                    // signal) and `stale` (a nonce-freshness rejection:
                    // TTL expiry or the reuse window, #430 — kept in the
                    // stream for replay forensics, but NOT a credential
                    // anomaly; honest pre-emptively-authenticating peers
                    // land here routinely). The bare `challenged` (normal
                    // first-leg 401, before any credential) fires on every
                    // authenticated call, so auditing it would track call
                    // volume, not security.
                    if result == "failed" || result == "stale" {
                        siphon_ai_audit::emit(siphon_ai_audit::AuditEvent::sip_auth(
                            ctx.peer().to_string(),
                            Some(register_source.clone()),
                            result,
                        ));
                    }
                    let mut response = digest.challenge(request, *stale);
                    self.fill_response(&mut response, ctx).await;
                    handle.send_final(response).await;
                    return Ok(());
                }
            }
        }

        let routes = self.routes.load();
        match dispatch_invite(&routes, &register_source, request) {
            RouteAction::SendFinal(mut response) => {
                self.fill_response(&mut response, ctx).await;
                handle.send_final(response).await;
                Ok(())
            }
            RouteAction::Accept { facts, route } => {
                self.acceptor
                    .on_matched(MatchedCall {
                        request,
                        handle,
                        transport: ctx,
                        facts,
                        route,
                    })
                    .await
            }
        }
    }

    #[instrument(skip_all, fields(method = "BYE", peer = %ctx.peer()))]
    async fn on_bye(
        &self,
        request: &Request,
        handle: ServerTransactionHandle,
        ctx: &TransportContext,
        _dialog: &Dialog,
    ) -> anyhow::Result<()> {
        match dispatch_bye(self.terminator.as_ref(), request) {
            DialogAction::SendFinal(mut response) => {
                self.fill_response(&mut response, ctx).await;
                handle.send_final(response).await;
                Ok(())
            }
        }
    }

    #[instrument(skip_all, fields(method = "ACK"))]
    async fn on_ack(&self, request: &Request, dialog: &Dialog) -> anyhow::Result<()> {
        // Forward every in-dialog ACK, body-less ones included. An
        // empty ACK is the normal end of an early-offer INVITE
        // transaction (the acceptor's dialog-map probe makes it a
        // no-op), but on a pending delayed-offer dialog it means the
        // peer sent no SDP answer — the acceptor must see it to
        // classify the call as `missing_sdp_answer` instead of
        // letting Timer H expire 32 s later (#425).
        let sip_call_id = request
            .headers()
            .get_smol("Call-ID")
            .map(|s| s.to_string())
            .unwrap_or_default();
        debug!(sip_call_id = %sip_call_id, has_body = !request.body().is_empty(), "ACK → acceptor");
        self.acceptor
            .on_ack(AckCall {
                request,
                dialog,
                sip_call_id,
            })
            .await
    }

    #[instrument(skip_all, fields(method = "CANCEL", peer = %ctx.peer()))]
    async fn on_cancel(
        &self,
        request: &Request,
        handle: ServerTransactionHandle,
        ctx: &TransportContext,
    ) -> anyhow::Result<()> {
        match dispatch_cancel(self.terminator.as_ref(), request) {
            DialogAction::SendFinal(mut response) => {
                self.fill_response(&mut response, ctx).await;
                handle.send_final(response).await;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time check: the trait composes with the upstream
    /// `UasRequestHandler` and is object-safe enough to be held in
    /// an `Arc<dyn UasRequestHandler>` (which is what
    /// `IntegratedUAS::request_handler` takes).
    #[test]
    fn routing_handler_satisfies_uas_request_handler() {
        struct FakeAcceptor;

        #[async_trait]
        impl CallAcceptor for FakeAcceptor {
            async fn on_matched(&self, _call: MatchedCall<'_>) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let routes = Arc::new(ArcSwap::from_pointee(siphon_ai_routes::RouteSet::default()));
        let handler = RoutingHandler::new(routes, Arc::new(FakeAcceptor));
        let _: Arc<dyn UasRequestHandler> = Arc::new(handler);
    }

    fn invite() -> Request {
        use sip_core::{Headers, Method, RequestLine, SipUri};
        let uri = SipUri::parse("sip:5000@siphon.example.com").unwrap();
        let mut h = Headers::new();
        h.push("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1")
            .unwrap();
        h.push("From", "<sip:caller@carrier.example.net>;tag=abc")
            .unwrap();
        h.push("To", "<sip:5000@siphon.example.com>").unwrap();
        h.push("Call-ID", "drain-test@example.net").unwrap();
        h.push("CSeq", "1 INVITE").unwrap();
        Request::new(
            RequestLine::new(Method::Invite, uri),
            h,
            bytes::Bytes::new(),
        )
        .unwrap()
    }

    #[test]
    fn drain_reject_is_none_when_not_draining() {
        let drain = crate::drain::DrainFlag::new();
        assert!(drain_reject_response(&drain, 30, &invite()).is_none());
    }

    #[test]
    fn drain_reject_is_503_with_retry_after_when_draining() {
        let drain = crate::drain::DrainFlag::new();
        drain.begin();
        let resp = drain_reject_response(&drain, 30, &invite()).expect("drain → 503");
        assert_eq!(resp.code(), 503);
        assert_eq!(resp.headers().get("Retry-After"), Some("30"));
    }

    /// Build a NOTIFY as the post-REFER refer subscription sends it.
    /// `event` is the raw `Event` header value; `None` omits it.
    fn notify(event: Option<&str>) -> Request {
        use sip_core::{Headers, Method, RequestLine, SipUri};
        let uri = SipUri::parse("sip:siphon@10.0.0.2:5060").unwrap();
        let mut h = Headers::new();
        h.push("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-n1")
            .unwrap();
        h.push("From", "<sip:carrier@carrier.example.net>;tag=peer")
            .unwrap();
        h.push("To", "<sip:siphon@siphon.example.com>;tag=local")
            .unwrap();
        h.push("Call-ID", "refer-notify@example.net").unwrap();
        h.push("CSeq", "1 NOTIFY").unwrap();
        if let Some(event) = event {
            h.push("Event", event).unwrap();
        }
        Request::new(
            RequestLine::new(Method::Notify, uri),
            h,
            bytes::Bytes::new(),
        )
        .unwrap()
    }

    /// #357: the refer-progress NOTIFY the peer sends after our REFER
    /// is accepted, not 405'd (PROTOCOL.md §4.4, RFC 3515 §2.4.4).
    #[test]
    fn refer_notify_is_accepted_with_200() {
        let d = dispatch_notify(&notify(Some("refer")));
        assert_eq!(d.response.code(), 200);
        assert_eq!(d.result, "accepted");
    }

    /// The refer subscription may carry an `id` parameter (RFC 3515
    /// §2.4.6) — only the event package token decides.
    #[test]
    fn refer_notify_with_id_param_is_accepted() {
        let d = dispatch_notify(&notify(Some("refer;id=93809824")));
        assert_eq!(d.response.code(), 200);
        assert_eq!(d.result, "accepted");
    }

    /// #486: a PBX we register to pushes unsolicited MWI at the account
    /// after every REGISTER (RFC 3842). Absorb it — a bridge has no
    /// mailbox — rather than 489 once a minute forever.
    #[test]
    fn mwi_notify_is_absorbed_with_200() {
        let d = dispatch_notify(&notify(Some("message-summary")));
        assert_eq!(d.response.code(), 200);
        assert_eq!(d.result, "ignored");
    }

    /// Absorbed MWI must not read as refer progress: the two share a
    /// `200`, so only the `result` label separates "a transfer is
    /// progressing" from "the PBX said the mailbox is empty".
    #[test]
    fn mwi_is_scored_apart_from_refer_progress() {
        let mwi = dispatch_notify(&notify(Some("message-summary")));
        let refer = dispatch_notify(&notify(Some("refer")));
        assert_eq!(mwi.response.code(), refer.response.code());
        assert_ne!(mwi.result, refer.result);
    }

    /// Not gated on `Subscription-State: terminated` — we never send a
    /// `message-summary` SUBSCRIBE, so every flavour of MWI NOTIFY is
    /// unsolicited by construction. FreeSWITCH stamps
    /// `terminated;reason=noresource`; a PBX using `active` gets the
    /// same treatment instead of re-opening the #486 noise.
    #[test]
    fn mwi_is_absorbed_whatever_the_subscription_state() {
        for state in ["terminated;reason=noresource", "active;expires=3600"] {
            let mut req = notify(Some("message-summary"));
            req.headers_mut().push("Subscription-State", state).unwrap();
            let d = dispatch_notify(&req);
            assert_eq!(d.response.code(), 200, "state {state}");
            assert_eq!(d.result, "ignored", "state {state}");
        }
    }

    /// The package token decides, and it is case-insensitive (RFC 6665
    /// §8.2.1) — `Message-Summary` is the same package.
    #[test]
    fn mwi_package_match_is_case_insensitive() {
        assert_eq!(
            dispatch_notify(&notify(Some("Message-Summary"))).result,
            "ignored"
        );
    }

    /// An event package we don't support is a Bad Event, not a Method
    /// Not Allowed — NOTIFY itself is in our Allow set now. The 489
    /// names what we do support (RFC 6665 §4.4.1). Absorbing MWI must
    /// not soften this: an unknown package is still refused, which is
    /// the whole point of keeping `bad_event` meaningful.
    #[test]
    fn non_refer_notify_is_489_bad_event_with_allow_events() {
        for package in ["talk", "dialog", "presence"] {
            let d = dispatch_notify(&notify(Some(package)));
            assert_eq!(d.response.code(), 489, "package {package}");
            assert_eq!(d.response.headers().get("Allow-Events"), Some("refer"));
            assert_eq!(d.result, "bad_event", "package {package}");
        }
    }

    /// `Allow-Events` advertises what we would accept a SUBSCRIBE for.
    /// Absorbing an unsolicited push is a narrower claim than offering
    /// a subscription, so `message-summary` deliberately stays out of
    /// it — otherwise the PBX is invited to establish real MWI
    /// subscription state we have no machinery for.
    #[test]
    fn mwi_absorption_does_not_advertise_a_subscription() {
        let d = dispatch_notify(&notify(Some("dialog")));
        assert_eq!(d.response.headers().get("Allow-Events"), Some("refer"));
    }

    /// A NOTIFY with no Event header is malformed (RFC 6665 §8.2.3).
    #[test]
    fn notify_without_event_is_400() {
        let d = dispatch_notify(&notify(None));
        assert_eq!(d.response.code(), 400);
        assert_eq!(d.result, "bad_request");
    }

    /// NOTIFY must be advertised in `Allow` now that we answer it —
    /// RFC 3261 §20.5 says Allow is exactly what the UA supports.
    #[test]
    fn allow_header_advertises_notify() {
        struct FakeAcceptor;

        #[async_trait]
        impl CallAcceptor for FakeAcceptor {
            async fn on_matched(&self, _call: MatchedCall<'_>) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let routes = Arc::new(ArcSwap::from_pointee(siphon_ai_routes::RouteSet::default()));
        let handler = RoutingHandler::new(routes, Arc::new(FakeAcceptor));
        assert!(handler.allow_header().contains("NOTIFY"));
    }

    /// Acceptor that records every ACK it is handed, with body presence.
    #[derive(Default)]
    struct RecordingAckAcceptor {
        acks: parking_lot::Mutex<Vec<(String, bool)>>,
    }

    #[async_trait]
    impl CallAcceptor for RecordingAckAcceptor {
        async fn on_matched(&self, _call: MatchedCall<'_>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn on_ack(&self, call: AckCall<'_>) -> anyhow::Result<()> {
            self.acks
                .lock()
                .push((call.sip_call_id.clone(), !call.request.body().is_empty()));
            Ok(())
        }
    }

    /// A confirmed dialog to hang the test ACK on. The handler only
    /// threads the dialog through, so a UAC-built one is fine.
    fn test_dialog(call_id: &str) -> Dialog {
        let invite = format!(
            "INVITE sip:callee@pbx.example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP siphon.example.com;branch=z9hG4bK-ack-test\r\n\
             From: <sip:siphon@siphon.example.com>;tag=lt\r\n\
             To: <sip:callee@pbx.example.com>\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:siphon@siphon.example.com:5060>\r\n\
             Content-Length: 0\r\n\r\n"
        );
        let ok = format!(
            "SIP/2.0 200 OK\r\n\
             Via: SIP/2.0/UDP siphon.example.com;branch=z9hG4bK-ack-test\r\n\
             From: <sip:siphon@siphon.example.com>;tag=lt\r\n\
             To: <sip:callee@pbx.example.com>;tag=rt\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:callee@10.0.0.5:5060>\r\n\
             Content-Length: 0\r\n\r\n"
        );
        let req = sip_parse::parse_request(&bytes::Bytes::from(invite)).expect("parse INVITE");
        let resp = sip_parse::parse_response(&bytes::Bytes::from(ok)).expect("parse 200");
        Dialog::new_uac(
            &req,
            &resp,
            sip_core::SipUri::parse("sip:siphon@siphon.example.com").unwrap(),
            sip_core::SipUri::parse("sip:callee@pbx.example.com").unwrap(),
        )
        .expect("dialog")
    }

    fn ack(call_id: &str, body: &str) -> Request {
        use sip_core::{Headers, Method, RequestLine, SipUri};
        let uri = SipUri::parse("sip:siphon@siphon.example.com").unwrap();
        let mut h = Headers::new();
        h.push("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-a1")
            .unwrap();
        h.push("From", "<sip:caller@pbx.example.com>;tag=rt")
            .unwrap();
        h.push("To", "<sip:siphon@siphon.example.com>;tag=lt")
            .unwrap();
        h.push("Call-ID", call_id).unwrap();
        h.push("CSeq", "1 ACK").unwrap();
        h.push("Content-Length", body.len().to_string()).unwrap();
        Request::new(
            RequestLine::new(Method::Ack, uri),
            h,
            bytes::Bytes::from(body.to_string()),
        )
        .unwrap()
    }

    /// #425: a body-less ACK must reach the acceptor — it is how a
    /// pending delayed-offer call learns the peer sent no answer.
    /// (Before the fix the handler dropped empty ACKs, so the only
    /// exit for that call was the Timer-H `ack_timeout` 32 s later.)
    #[tokio::test]
    async fn body_less_ack_is_forwarded_to_acceptor() {
        let acceptor = Arc::new(RecordingAckAcceptor::default());
        let routes = Arc::new(ArcSwap::from_pointee(siphon_ai_routes::RouteSet::default()));
        let handler = RoutingHandler::new(routes, Arc::clone(&acceptor));
        let dialog = test_dialog("empty-ack@peer");
        handler
            .on_ack(&ack("empty-ack@peer", ""), &dialog)
            .await
            .expect("on_ack");
        assert_eq!(
            acceptor.acks.lock().as_slice(),
            &[("empty-ack@peer".to_string(), false)]
        );
    }

    /// The body-carrying path is unchanged: still forwarded, body seen.
    #[tokio::test]
    async fn ack_with_body_is_forwarded_to_acceptor() {
        let acceptor = Arc::new(RecordingAckAcceptor::default());
        let routes = Arc::new(ArcSwap::from_pointee(siphon_ai_routes::RouteSet::default()));
        let handler = RoutingHandler::new(routes, Arc::clone(&acceptor));
        let dialog = test_dialog("sdp-ack@peer");
        handler
            .on_ack(&ack("sdp-ack@peer", "v=0\r\n"), &dialog)
            .await
            .expect("on_ack");
        assert_eq!(
            acceptor.acks.lock().as_slice(),
            &[("sdp-ack@peer".to_string(), true)]
        );
    }
}
