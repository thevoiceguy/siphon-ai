//! Per-call orchestration: [`CallController`], the call state machine,
//! and the glue that ties SIP, media, bridge, CDR, telemetry, and
//! webhooks together.
//!
//! Each call is one owned [`CallController`] task. There is NO global
//! mutable call state and NO calls registry — see CLAUDE.md §4.4.

pub mod call;

pub use call::{
    CallController, CallControllerConfig, CallError, CallHandle, CallOutcome, CallState,
    CallTermination,
};
