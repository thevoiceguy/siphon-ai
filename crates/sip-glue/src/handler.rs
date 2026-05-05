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
//! re-INVITEs (hold/resume, codec change) belong to the
//! `CallController`, not the routing layer. Until core lands, this
//! handler responds 501 to re-INVITEs — see CLAUDE.md §8 for what's
//! deferred to Week 3+.
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

use std::sync::Arc;

use async_trait::async_trait;
use sip_core::{Request, Response};
use sip_dialog::Dialog;
use sip_transaction::{ServerTransactionHandle, TransportContext};
use sip_uas::integrated::UasRequestHandler;
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
        if dialog.is_some() {
            // Re-INVITE on an existing dialog. Routing has nothing
            // to say here; the dialog's controller should handle
            // it. Until core::CallController exists, refuse cleanly.
            debug!("re-INVITE received; routing layer cannot handle it yet");
            handle
                .send_final(UserAgentServer::create_response(
                    request,
                    501,
                    "Not Implemented",
                ))
                .await;
            return Ok(());
        }

        let register_source = (self.resolver)(request, ctx);
        match dispatch_invite(&self.routes, &register_source, request) {
            RouteAction::SendFinal(response) => {
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
