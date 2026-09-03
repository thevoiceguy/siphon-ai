//! Glue between an inbound INVITE and the route matcher.
//!
//! `route_invite` is the one entry point the SIP-side handler will
//! call. It bundles the steps of "extract facts → look up a
//! matching route → tell the caller what to do" so the controller
//! can stay agnostic of `sip-core` types.
//!
//! See `docs/DIALPLAN.md` for the matching grammar.

use sip_core::Request;
use siphon_ai_routes::{CompiledRoute, RouteSet};

use crate::invite::InviteFacts;

/// What the matcher concluded for an inbound call.
///
/// We borrow the matched `CompiledRoute` rather than cloning so the
/// caller can use the route's fields zero-copy until they decide
/// what to do (typically: open the bridge, then drop the route ref
/// because the bridge owns its own merged config).
#[derive(Debug)]
pub enum RouteDecision<'a> {
    /// A route matched. The caller should open the bridge to
    /// `route.bridge.ws_url` (post-merge with global defaults).
    Matched {
        facts: InviteFacts,
        route: &'a CompiledRoute,
    },
    /// No route matched. The caller should respond SIP 404 per
    /// `docs/DEV_PLAN.md` §6.3.
    NoMatch { facts: InviteFacts },
}

/// Decide which route — if any — handles `request`.
///
/// `register_source` is the name of the `[[register]]` block the
/// call arrived on, or `"trunk"` for unregistered inbound.
///
/// `peer_cert_names` is what the connection's verified TLS client
/// certificate asserts (see `InviteFacts::as_call_info`); pass an
/// empty slice for a call that presented none.
///
/// The returned `&CompiledRoute` lifetime is tied to `routes`
/// alone, *not* `register_source` or `peer_cert_names`. The matcher
/// needs all of them at call-evaluation time but the result only
/// references the route table — callers can pass short-lived
/// per-request strings and still hand the matched route off to a
/// longer-lived consumer.
pub fn route_invite<'r>(
    request: &Request,
    register_source: &str,
    peer_cert_names: &[String],
    routes: &'r RouteSet,
) -> RouteDecision<'r> {
    let facts = InviteFacts::extract(request);
    let info = facts.as_call_info(register_source, peer_cert_names);
    match routes.find_match(&info) {
        Some(route) => RouteDecision::Matched { facts, route },
        None => RouteDecision::NoMatch { facts },
    }
}
