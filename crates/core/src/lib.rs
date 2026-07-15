//! Per-call orchestration: [`CallController`], the call state machine,
//! and the glue that ties SIP, media, bridge, CDR, telemetry, and
//! webhooks together.
//!
//! Each call is one owned [`CallController`] task. There is NO global
//! mutable call state and NO calls registry — see CLAUDE.md §4.4.

pub mod acceptor;
pub mod call;
pub mod conference;
pub mod conference_admin;
pub mod hold;
pub mod outbound;
pub mod outbound_service;
pub mod park;
pub mod park_admin;
pub mod quality_live;
pub mod registry;
pub mod transfer;

pub use acceptor::{
    build_bridge_config, build_outbound_start_msg, build_start_msg, default_call_id_factory,
    extract_offer_sdp, extract_sip_call_id, resolve_barge_in, resolve_codecs, resolve_dtmf_pt,
    resolve_ws_failure_prompt, AcceptError, AcceptSecurityPolicy, BargeInConfig, BargeInMode,
    BargeInTimeout, BridgeBuildError, BridgeDefaults, BridgingAcceptor, CallIdFactory,
    CallProgressMode, OfferError, PreparedCall, SrtpMode, WsFailureAction,
};
pub use call::{
    CallController, CallControllerConfig, CallError, CallHandle, CallOutcome, CallState,
    CallTermination, ConferenceCommand, ParkCommand, ParkSummary,
};
pub use conference::{ConferenceError, ConferenceLimits, ConferenceRegistry, ConferenceSnapshot};
pub use conference_admin::ConferenceAdmin;
pub use hold::HoldContext;
pub use outbound::{
    DelayedOfferAnswerer, DelayedOfferRegistry, NotAnsweredCause, OutboundCall, OutboundError,
    OutboundGuard, OutboundOriginator, OutboundPermit, OutboundRejection, StaticCredentials,
};
pub use outbound_service::{OutboundGateway, OutboundService};
pub use park::{
    ParkContext, ParkError, ParkRegistry, ParkSettings, ParkSnapshot, ParkTimeoutAction,
};
pub use park_admin::ParkAdmin;
pub use registry::{CallControlRegistry, CallEntry, CallRegistry, ConsultRegistry};
pub use transfer::{DialogControl, DialogSource, TransferContext, TransferOutcome};
