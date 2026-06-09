//! Outbound origination service — the daemon entry point for placing calls.
//!
//! Wires chunks 1-3 together: validate the request against the configured
//! gateways + guardrails, then place the call ([`OutboundOriginator::place`]),
//! run its audio bridge ([`CallController`]), and tear it down (BYE + stop the
//! media session). Implements [`OutboundOriginateHandle`] so the admin
//! `POST /admin/v1/calls` endpoint drives it.
//!
//! The originate call returns immediately with the bridge `call_id` (202) —
//! the call proceeds on a spawned task. Its progress (ringing / answered /
//! failed) surfaces via webhooks + the CDR in chunk 5. The concurrency permit
//! from the guard is held for the spawned task's lifetime, so it's released
//! exactly when the call ends.

use std::collections::HashMap;
use std::sync::Arc;

use forge_core::{CallId, ParticipantId};
use sip_core::SipUri;
use siphon_ai_bridge::{BridgeConfig, CallId as BridgeCallId};
use siphon_ai_media_glue::{OutboundOfferRequest, TapOptions};
use siphon_ai_telemetry::{
    OriginateRejection, OriginateRequest, OutboundOriginateHandle, OUTBOUND_CALLS_ACTIVE,
    OUTBOUND_CALLS_TOTAL,
};
use tracing::{info, warn};

use crate::acceptor::{
    barge_in_to_tap_action, build_outbound_start_msg, BridgeDefaults, CallIdFactory,
};
use crate::call::{CallController, CallControllerConfig};
use crate::outbound::{
    NotAnsweredCause, OutboundCall, OutboundError, OutboundGuard, OutboundOriginator,
    OutboundRejection,
};

/// One configured outbound gateway, ready to dial. Built by the daemon from a
/// compiled `[[gateway]]` (the `siphon-ai-config::Gateway`) plus a per-gateway
/// UAC-backed [`OutboundOriginator`]. Kept as plain fields so this crate
/// doesn't take a (cyclic) dependency on `siphon-ai-config`.
pub struct OutboundGateway {
    pub originator: Arc<OutboundOriginator>,
    pub proxy_host: String,
    pub proxy_port: u16,
    /// Default caller-ID `sip:` URI for calls through this gateway.
    pub from: String,
}

/// Daemon-wide outbound-origination service.
pub struct OutboundService {
    gateways: HashMap<String, OutboundGateway>,
    guard: OutboundGuard,
    defaults: BridgeDefaults,
    call_id_factory: CallIdFactory,
}

impl OutboundService {
    pub fn new(
        gateways: HashMap<String, OutboundGateway>,
        guard: OutboundGuard,
        defaults: BridgeDefaults,
        call_id_factory: CallIdFactory,
    ) -> Self {
        Self {
            gateways,
            guard,
            defaults,
            call_id_factory,
        }
    }
}

impl OutboundOriginateHandle for OutboundService {
    fn originate(&self, req: OriginateRequest) -> Result<String, OriginateRejection> {
        let gw = self
            .gateways
            .get(&req.gateway)
            .ok_or_else(|| OriginateRejection::UnknownGateway(req.gateway.clone()))?;

        // Cheap validation before we consume a concurrency permit.
        let ws_url = req
            .ws_url
            .clone()
            .or_else(|| self.defaults.ws_url.clone())
            .filter(|s| !s.is_empty())
            .ok_or(OriginateRejection::NoWsUrl)?;

        let target_str = format!("sip:{}@{}:{}", req.to, gw.proxy_host, gw.proxy_port);
        let target = SipUri::parse(&target_str)
            .map_err(|e| OriginateRejection::BadTarget(format!("{target_str}: {e}")))?;

        // Admit — the permit lives for the spawned call's whole lifetime.
        let permit = self.guard.try_admit().map_err(|r| match r {
            OutboundRejection::AtCapacity => OriginateRejection::AtCapacity,
            OutboundRejection::RateLimited => OriginateRejection::RateLimited,
        })?;

        let bridge_id = (self.call_id_factory)();
        let bridge_id_str = bridge_id.as_str().to_string();
        let forge_id = CallId::new(bridge_id.as_str());

        let offer_req = OutboundOfferRequest {
            call_id: forge_id,
            codecs: self.defaults.codecs.clone(),
            dtmf_payload_type: self.defaults.dtmf_payload_type,
            participant_a: ParticipantId::generate(),
            participant_b: ParticipantId::generate(),
            from_tag: None,
            to_tag: None,
        };
        let tap = TapOptions {
            barge_in_action: barge_in_to_tap_action(&self.defaults.barge_in),
            inactivity_timeout: self.defaults.inactivity_timeout,
            silence_threshold: self.defaults.silence_threshold,
            dead_air_threshold: self.defaults.dead_air_threshold,
            rtp_stats_interval: self.defaults.rtp_stats_interval,
        };
        let bridge = BridgeConfig {
            ws_url,
            auth_header: self.defaults.auth_header.clone(),
            connect_timeout: self.defaults.connect_timeout,
            tls: self.defaults.bridge_tls.clone(),
        };
        let from = req.from.clone().unwrap_or_else(|| gw.from.clone());
        let to = req.to.clone();
        let originator = Arc::clone(&gw.originator);

        info!(call_id = %bridge_id_str, gateway = %req.gateway, "originating outbound call");
        let log_id = bridge_id_str.clone();
        tokio::spawn(async move {
            let _permit = permit; // held until the call ends, then released
            metrics::gauge!(OUTBOUND_CALLS_ACTIVE).increment(1.0);
            let result = originator.place(target, offer_req, tap).await;
            metrics::counter!(OUTBOUND_CALLS_TOTAL, "result" => outbound_result_label(&result))
                .increment(1);
            match result {
                Ok(call) => run_call(originator, call, bridge_id, bridge, from, to).await,
                Err(e) => warn!(call_id = %log_id, error = %e, "outbound call did not connect"),
            }
            metrics::gauge!(OUTBOUND_CALLS_ACTIVE).decrement(1.0);
        });

        Ok(bridge_id_str)
    }
}

/// The `result` label for `siphon_ai_outbound_calls_total`, from the
/// `place()` outcome.
fn outbound_result_label(result: &Result<OutboundCall, OutboundError>) -> &'static str {
    match result {
        Ok(_) => "answered",
        Err(OutboundError::NotAnswered(cause)) => match cause {
            NotAnsweredCause::Busy => "busy",
            NotAnsweredCause::Declined => "declined",
            NotAnsweredCause::NoAnswer => "no_answer",
            NotAnsweredCause::Rejected { .. } => "rejected",
        },
        // No usable final response — DNS / transport / transaction timeout.
        Err(OutboundError::Transport(_)) => "unreachable",
        // Local media (offer/answer) setup failure.
        Err(OutboundError::Setup(_)) => "failed",
    }
}

/// Run an answered outbound call's audio bridge to completion, then tear it
/// down (BYE the dialog + stop the media session).
async fn run_call(
    originator: Arc<OutboundOriginator>,
    call: OutboundCall,
    bridge_id: BridgeCallId,
    bridge: BridgeConfig,
    from: String,
    to: String,
) {
    let OutboundCall {
        accepted,
        dialog,
        call_handle,
        call_id,
    } = call;
    let sip_call_id = dialog.id().call_id().to_string();
    let start = build_outbound_start_msg(
        bridge_id.clone(),
        &from,
        &to,
        &sip_call_id,
        &accepted.answer,
    );
    let cfg = CallControllerConfig {
        call_id: bridge_id,
        bridge,
        start,
        media_tap: accepted.tap,
        transfer: None,
        recording: None,
    };
    let (controller, _handle) = CallController::new(cfg);
    match controller.run().await {
        Ok(o) => {
            info!(sip_call_id = %sip_call_id, termination = ?o.termination, "outbound call ended")
        }
        Err(e) => warn!(sip_call_id = %sip_call_id, error = %e, "outbound controller error"),
    }
    // The controller's done — BYE the dialog and release the media session.
    originator.hangup(&dialog).await;
    originator.stop_session(&call_id).await;
    drop(call_handle); // stop keepalives / session-timer tasks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_labels_map_failure_outcomes() {
        let na = |c| Err::<OutboundCall, _>(OutboundError::NotAnswered(c));
        assert_eq!(outbound_result_label(&na(NotAnsweredCause::Busy)), "busy");
        assert_eq!(
            outbound_result_label(&na(NotAnsweredCause::Declined)),
            "declined"
        );
        assert_eq!(
            outbound_result_label(&na(NotAnsweredCause::NoAnswer)),
            "no_answer"
        );
        assert_eq!(
            outbound_result_label(&na(NotAnsweredCause::Rejected {
                code: 500,
                reason: "x".into()
            })),
            "rejected"
        );
        assert_eq!(
            outbound_result_label(&Err(OutboundError::Transport("dns".into()))),
            "unreachable"
        );
    }
}
