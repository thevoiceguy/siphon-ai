//! Per-call orchestration: [`CallController`], the call state machine,
//! and the glue that ties SIP, media, bridge, CDR, telemetry, and
//! webhooks together.
//!
//! Each call is one owned [`CallController`] task. There is NO global
//! mutable call state and NO calls registry — see CLAUDE.md §4.4.

pub mod acceptor;
pub mod call;
pub mod outbound;
pub mod outbound_service;
pub mod registry;
pub mod transfer;

pub use acceptor::{
    build_bridge_config, build_outbound_start_msg, build_start_msg, default_call_id_factory,
    extract_offer_sdp, extract_sip_call_id, resolve_barge_in, resolve_codecs, resolve_dtmf_pt,
    AcceptError, AcceptSecurityPolicy, BargeInConfig, BargeInMode, BridgeBuildError,
    BridgeDefaults, BridgingAcceptor, CallIdFactory, CallProgressMode, OfferError, PreparedCall,
    SrtpMode,
};
pub use call::{
    CallController, CallControllerConfig, CallError, CallHandle, CallOutcome, CallState,
    CallTermination,
};
pub use outbound::{
    NotAnsweredCause, OutboundCall, OutboundError, OutboundGuard, OutboundOriginator,
    OutboundPermit, OutboundRejection, StaticCredentials,
};
pub use outbound_service::{OutboundGateway, OutboundService};
pub use registry::{CallEntry, CallRegistry};
pub use transfer::{TransferContext, TransferOutcome};
