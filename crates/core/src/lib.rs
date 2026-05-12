//! Per-call orchestration: [`CallController`], the call state machine,
//! and the glue that ties SIP, media, bridge, CDR, telemetry, and
//! webhooks together.
//!
//! Each call is one owned [`CallController`] task. There is NO global
//! mutable call state and NO calls registry — see CLAUDE.md §4.4.

pub mod acceptor;
pub mod call;
pub mod registry;
pub mod transfer;

pub use acceptor::{
    build_bridge_config, build_start_msg, extract_offer_sdp, extract_sip_call_id, resolve_barge_in,
    resolve_codecs, resolve_dtmf_pt, AcceptError, BargeInConfig, BargeInMode, BridgeBuildError,
    BridgeDefaults, BridgingAcceptor, CallIdFactory, OfferError, PreparedCall,
};
pub use call::{
    CallController, CallControllerConfig, CallError, CallHandle, CallOutcome, CallState,
    CallTermination,
};
pub use registry::{CallEntry, CallRegistry};
pub use transfer::{TransferContext, TransferOutcome};
