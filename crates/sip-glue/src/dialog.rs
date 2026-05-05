//! Mid-dialog request handling: BYE and CANCEL.
//!
//! The routing layer (`crate::handler`) owns the SIP method seam.
//! When a BYE arrives the routing layer must:
//!
//! 1. Look up the matching call by SIP `Call-ID`.
//! 2. Ask that call to shut down (the controller drains, sends
//!    `stop` over the WS, and exits).
//! 3. Respond 200 OK to the BYE — RFC 3261 §15.1.2.
//!
//! sip-glue can't depend on `siphon-ai-core` (the dependency runs
//! the other way), so we abstract the call lookup behind a small
//! [`DialogTerminator`] trait. `siphon-ai-core::CallRegistry` is the
//! production impl; tests can supply their own.
//!
//! ## CANCEL semantics
//!
//! Per RFC 3261 §9.2 the UAS responds 200 OK to a CANCEL
//! unconditionally and 487 Request Terminated to the matched
//! INVITE transaction. siphon-rs's `IntegratedUAS` handles the
//! transaction-level pieces (200 + 487) automatically — we only
//! need to wake any controller we may have already spawned. In
//! practice that's a race: if the controller was spawned (i.e.,
//! the 200 OK to INVITE was sent), CANCEL is too late and the
//! peer should send BYE instead. If the call is still in early
//! state (no controller yet), there's nothing in the registry to
//! wake. Either way, signalling shutdown is best-effort and the
//! 200 OK reply is always correct.

use std::sync::Arc;

use sip_core::{Request, Response};
use sip_uas::UserAgentServer;
use tracing::{debug, info};

/// What to do with a mid-dialog request after the dispatcher has
/// consulted the registry. Returned by [`dispatch_bye`] and
/// [`dispatch_cancel`] so callers can compose response sending +
/// async glue around a synchronous decision.
#[derive(Debug)]
pub enum DialogAction {
    /// Send this final response and stop. The handler decides when
    /// to send (after waking the controller, typically).
    SendFinal(Response),
}

/// Look up calls by SIP `Call-ID` and signal them to shut down.
///
/// Implemented by `siphon-ai-core::CallRegistry`. Defined here so
/// `RoutingHandler` can hold a trait object without taking a
/// circular dep on `siphon-ai-core`.
pub trait DialogTerminator: Send + Sync {
    /// Ask the call identified by `sip_call_id` to shut down. Returns
    /// `true` if a call was found and signalled, `false` if no
    /// matching call exists. Implementations MUST NOT block.
    fn terminate(&self, sip_call_id: &str) -> bool;
}

/// Decide what to send back for a BYE.
///
/// `register_source` isn't used here (BYE is mid-dialog and the
/// dialog id is enough), but the parameter is kept symmetric with
/// [`crate::dispatch_invite`] so callers can pass the same
/// resolver output without conditionally choosing.
pub fn dispatch_bye(terminator: &dyn DialogTerminator, request: &Request) -> DialogAction {
    let sip_call_id = request
        .headers()
        .get_smol("Call-ID")
        .map(|s| s.to_string())
        .unwrap_or_default();
    let signalled = terminator.terminate(&sip_call_id);
    if signalled {
        info!(sip_call_id = %sip_call_id, "BYE → controller shutdown");
    } else {
        // BYE for an unknown dialog — the call may have already
        // ended on its own, or the peer is confused. RFC 3261
        // §15.1.2 says respond 200 OK regardless; logging at
        // debug because it isn't actionable.
        debug!(sip_call_id = %sip_call_id, "BYE for unknown call — responding 200 OK anyway");
    }
    DialogAction::SendFinal(UserAgentServer::create_response(request, 200, "OK"))
}

/// Decide what to send back for a CANCEL.
///
/// CANCEL semantics: best-effort wake of any spawned controller,
/// always 200 OK. The 487 to the cancelled INVITE transaction is
/// `IntegratedUAS`'s responsibility, not ours.
pub fn dispatch_cancel(terminator: &dyn DialogTerminator, request: &Request) -> DialogAction {
    let sip_call_id = request
        .headers()
        .get_smol("Call-ID")
        .map(|s| s.to_string())
        .unwrap_or_default();
    let signalled = terminator.terminate(&sip_call_id);
    if signalled {
        info!(sip_call_id = %sip_call_id, "CANCEL → controller shutdown (race: post-200 OK)");
    } else {
        debug!(
            sip_call_id = %sip_call_id,
            "CANCEL before any controller spawned (call still in early state); 200 OK"
        );
    }
    DialogAction::SendFinal(UserAgentServer::create_response(request, 200, "OK"))
}

/// Default no-op terminator. Used by [`RoutingHandler::new`] so
/// existing tests / deployments that don't yet wire a registry
/// keep working — BYE/CANCEL just respond 200 OK without waking
/// anything (which is the pre-registry behaviour).
#[derive(Debug, Default)]
pub struct NullDialogTerminator;

impl DialogTerminator for NullDialogTerminator {
    fn terminate(&self, _sip_call_id: &str) -> bool {
        false
    }
}

/// Convenience type alias for the trait-object form passed around
/// in [`RoutingHandler`].
pub type DialogTerminatorHandle = Arc<dyn DialogTerminator>;

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use sip_core::{Headers, Method, RequestLine, SipUri};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn req(method: Method, call_id: &str) -> Request {
        let uri = SipUri::parse("sip:5000@siphon.example.com").unwrap();
        let mut h = Headers::new();
        h.push("Via", "SIP/2.0/UDP h:5060;branch=z9hG4bK-1")
            .unwrap();
        h.push("From", "<sip:c@x>;tag=t").unwrap();
        h.push("To", "<sip:5000@y>;tag=u").unwrap();
        h.push("Call-ID", call_id).unwrap();
        h.push("CSeq", "2 BYE").unwrap();
        h.push("Content-Length", "0").unwrap();
        Request::new(RequestLine::new(method, uri), h, Bytes::new()).unwrap()
    }

    /// Test terminator that records its calls.
    #[derive(Default)]
    struct RecordingTerminator {
        terminated: parking_lot::Mutex<Vec<String>>,
        return_match: AtomicBool,
    }

    impl RecordingTerminator {
        fn matching() -> Self {
            let r = Self::default();
            r.return_match.store(true, Ordering::Relaxed);
            r
        }
    }

    impl DialogTerminator for RecordingTerminator {
        fn terminate(&self, sip_call_id: &str) -> bool {
            self.terminated.lock().push(sip_call_id.to_string());
            self.return_match.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn bye_sends_200_and_calls_terminator_with_call_id() {
        let term = RecordingTerminator::matching();
        let request = req(Method::Bye, "abc-123@pbx");
        match dispatch_bye(&term, &request) {
            DialogAction::SendFinal(resp) => {
                assert_eq!(resp.code(), 200);
                assert_eq!(resp.reason(), "OK");
            }
        }
        assert_eq!(*term.terminated.lock(), vec!["abc-123@pbx".to_string()]);
    }

    #[test]
    fn bye_for_unknown_call_still_200_ok() {
        // Default RecordingTerminator returns false (no match).
        let term = RecordingTerminator::default();
        let request = req(Method::Bye, "ghost@pbx");
        match dispatch_bye(&term, &request) {
            DialogAction::SendFinal(resp) => {
                assert_eq!(resp.code(), 200);
            }
        }
        assert_eq!(*term.terminated.lock(), vec!["ghost@pbx".to_string()]);
    }

    #[test]
    fn cancel_sends_200_and_signals_terminator() {
        let term = RecordingTerminator::matching();
        let request = req(Method::Cancel, "xyz@pbx");
        match dispatch_cancel(&term, &request) {
            DialogAction::SendFinal(resp) => assert_eq!(resp.code(), 200),
        }
        assert_eq!(*term.terminated.lock(), vec!["xyz@pbx".to_string()]);
    }

    #[test]
    fn null_terminator_never_matches() {
        let n = NullDialogTerminator;
        assert!(!n.terminate("anything"));
    }

    #[test]
    fn missing_call_id_header_yields_empty_lookup_key() {
        // Defensive: a malformed BYE without Call-ID still gets a
        // 200 OK and the terminator is called with "".
        let uri = SipUri::parse("sip:x@y").unwrap();
        let mut h = Headers::new();
        h.push("Via", "SIP/2.0/UDP h;branch=z").unwrap();
        h.push("From", "<sip:c@x>;tag=t").unwrap();
        h.push("To", "<sip:5000@y>;tag=u").unwrap();
        h.push("CSeq", "2 BYE").unwrap();
        h.push("Content-Length", "0").unwrap();
        let request = Request::new(RequestLine::new(Method::Bye, uri), h, Bytes::new()).unwrap();
        let term = RecordingTerminator::default();
        match dispatch_bye(&term, &request) {
            DialogAction::SendFinal(resp) => assert_eq!(resp.code(), 200),
        }
        assert_eq!(*term.terminated.lock(), vec!["".to_string()]);
    }
}
