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

use async_trait::async_trait;
use sip_core::{Request, Response};
use sip_dialog::Dialog;
use sip_transaction::{ServerTransactionHandle, TransportContext};
use sip_uas::integrated::{IntegratedUAS, UasRequestHandler};
use sip_uas::UserAgentServer;
use siphon_ai_routes::{CompiledRoute, RouteSet};
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
}

/// `UasRequestHandler` that does INVITE routing and mid-dialog
/// teardown (BYE / CANCEL). Other methods fall through to the
/// trait's default 405/501 responses; REFER (transfer) lands in a
/// follow-up.
pub struct RoutingHandler<A> {
    routes: Arc<RouteSet>,
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
}

impl<A> RoutingHandler<A> {
    /// Build a handler with the default register-source resolver
    /// (always returns `"trunk"` — fine for UAS-only deployments)
    /// and a no-op dialog terminator. Wire a real terminator with
    /// [`Self::with_dialog_terminator`] before deploying — without
    /// it, BYEs are 200 OK'd but the per-call controller doesn't
    /// learn the SIP leg ended.
    pub fn new(routes: Arc<RouteSet>, acceptor: Arc<A>) -> Self {
        Self {
            routes,
            acceptor,
            resolver: default_resolver(),
            terminator: Arc::new(NullDialogTerminator),
            trunk_gate: None,
            uas_filler: std::sync::OnceLock::new(),
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

    pub fn routes(&self) -> &RouteSet {
        &self.routes
    }
}

fn default_resolver() -> RegisterSourceResolver {
    Arc::new(|_req, _ctx| String::from("trunk"))
}

#[async_trait]
impl<A: CallAcceptor + 'static> UasRequestHandler for RoutingHandler<A> {
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
                    let mut response =
                        UserAgentServer::create_response(request, 403, "Forbidden");
                    self.fill_response(&mut response, ctx).await;
                    handle.send_final(response).await;
                    return Ok(());
                }
            }
        } else {
            (self.resolver)(request, ctx)
        };

        match dispatch_invite(&self.routes, &register_source, request) {
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

    #[instrument(skip_all, fields(method = "BYE"))]
    async fn on_bye(
        &self,
        request: &Request,
        handle: ServerTransactionHandle,
        _dialog: &Dialog,
    ) -> anyhow::Result<()> {
        match dispatch_bye(self.terminator.as_ref(), request) {
            DialogAction::SendFinal(response) => {
                handle.send_final(response).await;
                Ok(())
            }
        }
    }

    #[instrument(skip_all, fields(method = "CANCEL"))]
    async fn on_cancel(
        &self,
        request: &Request,
        handle: ServerTransactionHandle,
    ) -> anyhow::Result<()> {
        match dispatch_cancel(self.terminator.as_ref(), request) {
            DialogAction::SendFinal(response) => {
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

        let routes = Arc::new(siphon_ai_routes::RouteSet::default());
        let handler = RoutingHandler::new(routes, Arc::new(FakeAcceptor));
        let _: Arc<dyn UasRequestHandler> = Arc::new(handler);
    }
}
