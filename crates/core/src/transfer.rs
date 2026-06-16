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

use crate::acceptor::DialogFlow;
use crate::registry::ConsultRegistry;

/// Where the REFER's dialog comes from — the inbound and outbound
/// legs hold their dialogs differently (DEV_PLAN_0.6.1 §2.4).
#[derive(Clone)]
pub enum DialogSource {
    /// Inbound leg: the UAS owns the canonical dialog; resolve a
    /// per-task clone from the shared [`DialogManager`] by SIP
    /// Call-ID at REFER time.
    Managed {
        sip_call_id: String,
        dialog_manager: Arc<DialogManager>,
    },
    /// Outbound leg: the originate path holds the confirmed dialog
    /// directly (each gateway UAC has its own private DialogManager,
    /// so the shared one can't resolve it). Snapshot taken at answer;
    /// the CSeq-divergence note below applies the same way.
    Direct(Box<Dialog>),
}

impl DialogSource {
    /// Per-task dialog clone for the REFER. `None` = the dialog is
    /// gone (inbound leg already torn down) → `LocalError`.
    pub(crate) fn resolve(&self) -> Option<Dialog> {
        match self {
            DialogSource::Managed {
                sip_call_id,
                dialog_manager,
            } => dialog_manager.find_by_call_id(sip_call_id),
            DialogSource::Direct(dialog) => Some(*dialog.clone()),
        }
    }

    /// Whether the transfer task should BYE the leg itself after a
    /// 202 ("REFER + BYE", RFC 5589 §6.1). True for inbound legs.
    /// False for outbound legs — their `run_call` teardown already
    /// sends the BYE when the controller exits, and a second one
    /// here would just draw a 481 from the peer.
    pub(crate) fn bye_after_refer(&self) -> bool {
        matches!(self, DialogSource::Managed { .. })
    }

    /// The leg's SIP Call-ID, for logging.
    fn sip_call_id(&self) -> &str {
        match self {
            DialogSource::Managed { sip_call_id, .. } => sip_call_id,
            DialogSource::Direct(dialog) => dialog.id().call_id(),
        }
    }
}

/// The shared handle for driving an **in-dialog request** (REFER for
/// transfer, re-INVITE for hold/resume) on this call's leg, factored
/// out of [`TransferContext`] so hold (0.7.2) reuses the same dialog
/// resolution and the 0.6.x `*_via_flow` TCP/TLS connection-reuse
/// without duplicating it.
///
/// `source` pins this call's dialog — inbound (resolved via the shared
/// [`DialogManager`]) or outbound (held directly, sent through the
/// gateway's own UAC so its digest credentials answer any challenge).
/// `flow` is `Some` when the dialog arrived over an inbound TCP/TLS
/// connection: the request (and any follow-up BYE) must reuse it via
/// the `*_via_flow` UAC methods, because the dispatcher is inbound-only
/// and the peer's Contact names an ephemeral port nothing listens on
/// (same plumbing as `TeardownContext.flow`, #157).
#[derive(Clone)]
pub struct DialogControl {
    pub uac: Arc<IntegratedUAC>,
    pub source: DialogSource,
    pub flow: Option<DialogFlow>,
}

impl DialogControl {
    /// Per-task dialog clone, or `None` if the dialog is gone.
    pub(crate) fn resolve(&self) -> Option<sip_dialog::Dialog> {
        self.source.resolve()
    }

    /// Drive a re-INVITE with `sdp` as the offer (e.g. `a=sendonly` to
    /// hold, `a=sendrecv` to resume), reusing the inbound TCP/TLS
    /// connection when `flow` is set. The 2xx is auto-ACKed by the
    /// stack. Used by bot-initiated hold/resume (0.7.2).
    pub(crate) async fn send_reinvite(
        &self,
        dialog: &mut sip_dialog::Dialog,
        sdp: &str,
    ) -> anyhow::Result<sip_core::Response> {
        match &self.flow {
            Some(flow) => {
                self.uac
                    .send_reinvite_via_flow(dialog, Some(sdp), flow.to_uac_flow())
                    .await
            }
            None => self.uac.send_reinvite(dialog, Some(sdp)).await,
        }
    }

    /// The leg's SIP Call-ID, for logging.
    pub(crate) fn sip_call_id(&self) -> &str {
        self.source.sip_call_id()
    }
}

impl std::fmt::Debug for DialogControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialogControl")
            .field("sip_call_id", &self.source.sip_call_id())
            .field("via_flow", &self.flow.is_some())
            .finish_non_exhaustive()
    }
}

/// Everything the controller needs to fire a REFER on this call's
/// dialog. Wraps a [`DialogControl`] (the dialog + flow drive, shared
/// with hold) plus the daemon-wide `consult_registry` for attended
/// transfer (empty unless `[outbound]` calls are live).
#[derive(Clone)]
pub struct TransferContext {
    pub control: DialogControl,
    pub consult_registry: ConsultRegistry,
}

impl std::fmt::Debug for TransferContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferContext")
            .field("sip_call_id", &self.control.sip_call_id())
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
    fn direct_source_resolves_the_held_dialog_and_skips_bye() {
        // Outbound legs: the dialog is held directly; run_call's
        // teardown owns the BYE, so the transfer task must not send
        // a second one.
        let src = DialogSource::Direct(Box::new(consult_dialog("out@siphon", "l", "r")));
        let dialog = src.resolve().expect("direct always resolves");
        assert_eq!(dialog.id().call_id(), "out@siphon");
        assert!(!src.bye_after_refer());
    }

    #[test]
    fn managed_source_misses_when_dialog_is_gone_and_byes() {
        // Inbound legs: resolution goes through the shared manager;
        // an empty manager (call torn down) is a clean miss. The
        // post-REFER BYE stays on (RFC 5589 §6.1 pattern).
        let src = DialogSource::Managed {
            sip_call_id: "gone@pbx".into(),
            dialog_manager: Arc::new(DialogManager::new()),
        };
        assert!(src.resolve().is_none());
        assert!(src.bye_after_refer());
    }

    #[test]
    fn attended_bad_explicit_target_is_a_local_error() {
        let reg = registry_with("siphon-C");
        let err = plan_refer(&reg, Some("nope"), Some("siphon-C")).unwrap_err();
        assert!(err.contains("nope"), "got: {err}");
    }
}
