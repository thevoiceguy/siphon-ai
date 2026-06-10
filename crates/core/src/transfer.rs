//! Call transfer support — `BridgeIn::Transfer` → SIP REFER, blind
//! (RFC 3515) or attended (RFC 5589, `replaces_call_id`).
//!
//! The controller owns the per-call audio path and SHOULD NOT block
//! it waiting for a multi-RTT REFER round-trip. Instead, the Transfer
//! arm spawns a one-shot task that:
//!
//! 1. Resolves the message into a [`ReferPlan`] ([`plan_refer`]):
//!    blind = parse `target`; attended = look the consult call up in
//!    the [`ConsultRegistry`] and derive the Refer-To from its
//!    Contact unless `target` overrides. Invalid input fails the
//!    transfer locally without ever hitting the wire.
//! 2. Resolves the SIP dialog by Call-ID (the same id the SIP-side
//!    BYE handler uses), via the shared [`DialogManager`] the
//!    `IntegratedUAS` owns.
//! 3. Issues `IntegratedUAC::send_refer(&mut dialog, &refer_to,
//!    consult)` — upstream builds the plain `Refer-To` or the
//!    `Replaces`-carrying one from the consult dialog's identifiers.
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

use sip_core::SipUri;
use sip_dialog::{Dialog, DialogManager};
use sip_uac::integrated::IntegratedUAC;

use crate::registry::ConsultRegistry;

/// Everything the controller needs to fire a REFER on the dialog
/// established by the inbound INVITE.
///
/// One [`TransferContext`] per call: it pins this call's SIP Call-ID
/// so the per-call dialog lookup is local to the controller, not a
/// process-wide search. `uac` and `dialog_manager` are `Arc` clones
/// of the daemon-wide pair installed at startup; `consult_registry`
/// is the daemon-wide consult-leg lookup for attended transfer
/// (empty unless `[outbound]` calls are live).
#[derive(Clone)]
pub struct TransferContext {
    pub sip_call_id: String,
    pub uac: Arc<IntegratedUAC>,
    pub dialog_manager: Arc<DialogManager>,
    pub consult_registry: ConsultRegistry,
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

/// What kind of REFER one `BridgeIn::Transfer` resolves to, with
/// everything the send needs. Pure resolution (no I/O beyond the
/// registry read), factored out of the send path so the
/// blind/attended/derivation/error matrix is unit-testable without
/// an `IntegratedUAC`.
#[derive(Debug)]
pub(crate) enum ReferPlan {
    /// RFC 3515 blind transfer: `Refer-To: <target>`.
    Blind { refer_to: SipUri },
    /// RFC 5589 attended transfer: `Refer-To` carries a `Replaces`
    /// parameter built from the consult dialog's identifiers.
    Attended {
        refer_to: SipUri,
        consult: Box<Dialog>,
    },
}

/// Resolve a `transfer` message's fields into a [`ReferPlan`]
/// (DEV_PLAN_0.6.1 §2.2). Rules:
///
/// - `replaces_call_id` set → attended. The consult call must be a
///   currently-answered outbound call; `target` defaults to the
///   consult dialog's remote target (its Contact) and overrides when
///   sent.
/// - no `replaces_call_id` → blind; `target` is required.
///
/// Errors become `TransferOutcome::LocalError` — the call keeps
/// running and the WS server gets `BridgeOut::Error{TransferFailed}`.
pub(crate) fn plan_refer(
    consult_registry: &ConsultRegistry,
    target: Option<&str>,
    replaces_call_id: Option<&str>,
) -> Result<ReferPlan, String> {
    let parse =
        |t: &str| SipUri::parse(t).map_err(|e| format!("invalid transfer target {t:?}: {e}"));
    match replaces_call_id {
        Some(consult_id) => {
            let Some(consult) = consult_registry.lookup(consult_id) else {
                return Err(format!(
                    "replaces_call_id {consult_id:?} is not an answered outbound call \
                     (unknown id, not yet answered, or already ended)"
                ));
            };
            let refer_to = match target {
                Some(t) => parse(t)?,
                None => consult.remote_target().clone(),
            };
            Ok(ReferPlan::Attended {
                refer_to,
                consult: Box::new(consult),
            })
        }
        None => {
            let t = target.ok_or("transfer requires `target` or `replaces_call_id`")?;
            Ok(ReferPlan::Blind {
                refer_to: parse(t)?,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::test_support::consult_dialog;

    fn registry_with(id: &str) -> ConsultRegistry {
        let reg = ConsultRegistry::new();
        reg.insert(id, consult_dialog("consult@siphon", "ltag", "rtag"));
        reg
    }

    #[test]
    fn blind_requires_and_parses_target() {
        let reg = ConsultRegistry::new();
        let plan = plan_refer(&reg, Some("sip:agent@example.com"), None).expect("plan");
        let ReferPlan::Blind { refer_to } = plan else {
            panic!("expected blind plan");
        };
        assert_eq!(refer_to.as_str(), "sip:agent@example.com");

        // No target and no replaces → local error, not a panic.
        let err = plan_refer(&reg, None, None).unwrap_err();
        assert!(err.contains("requires"), "got: {err}");

        // Garbage target → parse error surfaced with the input.
        let err = plan_refer(&reg, Some("not-a-uri"), None).unwrap_err();
        assert!(err.contains("not-a-uri"), "got: {err}");
    }

    #[test]
    fn attended_derives_refer_to_from_consult_contact() {
        // §9.2 (locked): with replaces_call_id and no target, the
        // Refer-To is the consult dialog's remote target — the
        // Contact from its 200 OK.
        let reg = registry_with("siphon-C");
        let plan = plan_refer(&reg, None, Some("siphon-C")).expect("plan");
        let ReferPlan::Attended { refer_to, consult } = plan else {
            panic!("expected attended plan");
        };
        assert_eq!(refer_to.as_str(), "sip:agent@10.0.0.5:5080");
        // The Replaces identifiers come from this dialog in
        // send_refer → create_refer_with_replaces.
        assert_eq!(consult.id().call_id(), "consult@siphon");
        assert_eq!(consult.id().local_tag(), "ltag");
        assert_eq!(consult.id().remote_tag(), "rtag");
    }

    #[test]
    fn attended_explicit_target_overrides_derived() {
        let reg = registry_with("siphon-C");
        let plan =
            plan_refer(&reg, Some("sip:agent@sbc.example.com"), Some("siphon-C")).expect("plan");
        let ReferPlan::Attended { refer_to, .. } = plan else {
            panic!("expected attended plan");
        };
        assert_eq!(refer_to.as_str(), "sip:agent@sbc.example.com");
    }

    #[test]
    fn attended_unknown_consult_is_a_local_error() {
        // Unknown id, not-yet-answered, or already-ended consult call
        // all look the same to the registry: a miss. The transfer
        // fails locally and call A keeps running.
        let reg = ConsultRegistry::new();
        let err = plan_refer(&reg, None, Some("siphon-gone")).unwrap_err();
        assert!(err.contains("siphon-gone"), "got: {err}");

        // An explicit target does NOT rescue an unknown consult call —
        // the Replaces identifiers are the point of attended mode.
        let err = plan_refer(&reg, Some("sip:agent@example.com"), Some("siphon-gone")).unwrap_err();
        assert!(err.contains("siphon-gone"), "got: {err}");
    }

    #[test]
    fn attended_bad_explicit_target_is_a_local_error() {
        let reg = registry_with("siphon-C");
        let err = plan_refer(&reg, Some("nope"), Some("siphon-C")).unwrap_err();
        assert!(err.contains("nope"), "got: {err}");
    }
}
