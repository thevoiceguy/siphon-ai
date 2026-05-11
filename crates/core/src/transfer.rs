//! Blind transfer support — `BridgeIn::Transfer` → SIP REFER.
//!
//! The controller owns the per-call audio path and SHOULD NOT block
//! it waiting for a multi-RTT REFER round-trip. Instead, the Transfer
//! arm spawns a one-shot task that:
//!
//! 1. Resolves the SIP dialog by Call-ID (the same id the SIP-side
//!    BYE handler uses), via the shared [`DialogManager`] the
//!    `IntegratedUAS` owns.
//! 2. Parses the WS server's `target` as a [`SipUri`] — invalid URIs
//!    fail the transfer locally without ever hitting the wire.
//! 3. Issues `IntegratedUAC::send_refer(&mut dialog, &target, None)`
//!    for an RFC 3515 blind transfer.
//! 4. Forwards the outcome ([`TransferOutcome`]) back to the
//!    controller over a bounded channel, which then either tears the
//!    call down with `StopReason::Transfer` (on 202 Accepted) or
//!    emits `BridgeOut::Error { code: TransferFailed }` and keeps the
//!    call running (on any non-2xx / network failure).
//!
//! The CSeq divergence between the UAS-owned dialog and the
//! UAC-owned dialog clone is acceptable here: after a successful
//! REFER the PBX initiates BYE, and `IntegratedUAS::dispatch` matches
//! BYEs by tag+call-id, not by local CSeq. See CLAUDE.md §4.4
//! (no shared per-call state) and §4.8 (use upstream primitives).

use std::sync::Arc;

use sip_dialog::DialogManager;
use sip_uac::integrated::IntegratedUAC;

/// Everything the controller needs to fire a REFER on the dialog
/// established by the inbound INVITE.
///
/// One [`TransferContext`] per call: it pins this call's SIP Call-ID
/// so the per-call dialog lookup is local to the controller, not a
/// process-wide search. `uac` and `dialog_manager` are `Arc` clones
/// of the daemon-wide pair installed at startup.
#[derive(Clone)]
pub struct TransferContext {
    pub sip_call_id: String,
    pub uac: Arc<IntegratedUAC>,
    pub dialog_manager: Arc<DialogManager>,
}

impl std::fmt::Debug for TransferContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferContext")
            .field("sip_call_id", &self.sip_call_id)
            .finish_non_exhaustive()
    }
}

/// Outcome of one `BridgeIn::Transfer` round-trip. Used by the spawn
/// task to report back to the controller without re-entering the
/// audio path.
#[derive(Debug)]
pub enum TransferOutcome {
    /// The PBX accepted the REFER (202). The controller tears the
    /// call down with `StopReason::Transfer`.
    Accepted,
    /// REFER failed before reaching the PBX (bad target URI, dialog
    /// not found, in-process error). The call keeps running.
    LocalError(String),
    /// PBX returned a non-2xx final response to the REFER. Call
    /// keeps running; the WS server gets `BridgeOut::Error`.
    RemoteRejected { status: u16, reason: String },
}
