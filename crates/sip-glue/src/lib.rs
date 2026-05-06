//! Adapter layer: siphon-rs SIP events → SiphonAI core call state.
//!
//! Per CLAUDE.md §4.8, this crate does *not* reimplement SIP. It
//! converts what siphon-rs hands us (`sip_core::Request`,
//! `sip_dialog::Dialog`, etc.) into the typed events the
//! controller and the route matcher want to see.
//!
//! ## Module map
//!
//! - [`invite`] — pull route-matchable facts out of an INVITE
//!   ([`InviteFacts`]).
//! - [`route`] — combine facts + a [`siphon_ai_routes::RouteSet`]
//!   into a [`RouteDecision`].
//! - [`handler`] — `UasRequestHandler` impl that plugs into
//!   siphon-rs's `IntegratedUAS` and dispatches matched INVITEs to
//!   a [`CallAcceptor`].
//!
//! Future modules (one per concern, per CLAUDE.md §6.2):
//! - dialog: BYE / re-INVITE / CANCEL handling
//! - refer: REFER (transfer) → controller event
//! - register: UAC REGISTER lifecycle (Week 4)

pub mod dialog;
pub mod handler;
pub mod invite;
pub mod register;
pub mod route;

pub use dialog::{
    dispatch_bye, dispatch_cancel, DialogAction, DialogTerminator, DialogTerminatorHandle,
    NullDialogTerminator,
};
pub use handler::{
    dispatch_invite, CallAcceptor, MatchedCall, RegisterSourceResolver, RouteAction, RoutingHandler,
};
pub use invite::InviteFacts;
pub use register::{
    refresh_delay, spawn_disabled_task, RegistrationEntry, RegistrationManager, RegistrationState,
    RegistrationStatus, ShutdownSignal,
};
pub use route::{route_invite, RouteDecision};
