//! Outbound origination service — the daemon entry point for placing calls.
//!
//! Wires chunks 1-3 together: validate the request against the configured
//! gateways + guardrails, then place the call ([`OutboundOriginator::place`]),
//! run its audio bridge ([`CallController`]), and tear it down (BYE + stop the
//! media session). Implements [`OutboundOriginateHandle`] so the admin
//! `POST /admin/v1/calls` endpoint drives it.
//!
//! The originate call returns immediately with the bridge `call_id` (202) —
//! the call proceeds on a spawned task. Its progress surfaces out-of-band:
//! an `outbound_initiated` webhook when the INVITE goes out, then exactly one
//! of `outbound_answered` (followed by a `call_end` webhook + a CDR when the
//! bridge finishes) or `outbound_failed` (terminal — failed calls get the
//! webhook + the `siphon_ai_outbound_calls_total` metric, no CDR, mirroring
//! inbound where CDRs cover bridged calls only). The concurrency permit from
//! the guard is held for the spawned task's lifetime, so it's released
//! exactly when the call ends.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use forge_core::{CallId, ParticipantId};
use sip_core::SipUri;
use siphon_ai_bridge::{BridgeConfig, CallId as BridgeCallId};
use siphon_ai_cdr::{
    AudioInfo as CdrAudioInfo, CdrRecord, CdrSinkHandle, Direction as CdrDirection,
    TerminationInfo as CdrTerminationInfo, CDR_VERSION,
};
use siphon_ai_media_glue::{OutboundOfferRequest, TapOptions};
use siphon_ai_telemetry::{
    OriginateRejection, OriginateRequest, OutboundOriginateHandle, OUTBOUND_CALLS_ACTIVE,
    OUTBOUND_CALLS_TOTAL,
};
use siphon_ai_webhooks::{
    CallEndEvent, OutboundAnsweredEvent, OutboundFailedEvent, OutboundInitiatedEvent, WebhookEvent,
    WebhookSinkHandle, WEBHOOK_VERSION,
};
use tracing::{info, warn};

use crate::acceptor::{
    barge_in_to_tap_action, build_outbound_start_msg, termination_label, BridgeDefaults,
    CallIdFactory, CallTerminationView,
};
use crate::call::{CallController, CallControllerConfig};
use crate::conference::ConferenceRegistry;
use crate::outbound::{
    NotAnsweredCause, OutboundCall, OutboundError, OutboundGuard, OutboundOriginator,
    OutboundRejection,
};
use crate::park::ParkContext;
use crate::registry::{CallControlRegistry, ConsultRegistry};
use crate::transfer::{DialogSource, TransferContext};

/// One configured outbound gateway, ready to dial. Built by the daemon from a
/// compiled `[[gateway]]` (the `siphon-ai-config::Gateway`) plus a per-gateway
/// UAC-backed [`OutboundOriginator`]. Kept as plain fields so this crate
/// doesn't take a (cyclic) dependency on `siphon-ai-config`.
pub struct OutboundGateway {
    pub originator: Arc<OutboundOriginator>,
    pub proxy_host: String,
    pub proxy_port: u16,
    /// `;transport=…` Request-URI parameter for this trunk, empty
    /// for UDP (config's `SipTransport::uri_param()` — kept as a
    /// plain string for the same no-config-dep reason as above).
    pub transport_uri_param: &'static str,
    /// Default caller-ID `sip:` URI for calls through this gateway.
    pub from: String,
}

/// Daemon-wide outbound-origination service.
pub struct OutboundService {
    gateways: HashMap<String, OutboundGateway>,
    guard: OutboundGuard,
    defaults: BridgeDefaults,
    call_id_factory: CallIdFactory,
    cdr_sink: CdrSinkHandle,
    webhook_sink: WebhookSinkHandle,
    /// Attended-transfer lookup (DEV_PLAN_0.6.1 §2.1): answered calls
    /// register their dialog snapshot here so another call's transfer
    /// task can build a REFER-with-Replaces against this leg.
    consult_registry: ConsultRegistry,
    /// Conference registry (0.7.0). `Some` when `[conference].enabled`;
    /// an outbound bot can `conference_join` just like an inbound one.
    conference: Option<ConferenceRegistry>,
    /// Bridge-id → handle table so the admin conference API can reach
    /// answered outbound calls too (§9.1). Shared with the acceptor.
    control_registry: CallControlRegistry,
    /// Park context (0.7.0). `Some` when `[park].enabled`; outbound
    /// bots can park/retrieve just like inbound calls.
    park: Option<ParkContext>,
}

impl OutboundService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        gateways: HashMap<String, OutboundGateway>,
        guard: OutboundGuard,
        defaults: BridgeDefaults,
        call_id_factory: CallIdFactory,
        cdr_sink: CdrSinkHandle,
        webhook_sink: WebhookSinkHandle,
        consult_registry: ConsultRegistry,
    ) -> Self {
        Self {
            gateways,
            guard,
            defaults,
            call_id_factory,
            cdr_sink,
            webhook_sink,
            consult_registry,
            conference: None,
            control_registry: CallControlRegistry::new(),
            park: None,
        }
    }

    /// Share the daemon's conference registry so outbound calls can
    /// join rooms over the WS protocol. Unset → outbound joins are
    /// rejected with `conference_failed`, same as inbound.
    pub fn with_conference(mut self, conference: ConferenceRegistry) -> Self {
        self.conference = Some(conference);
        self
    }

    /// Share the daemon's bridge-id call-control registry so the admin
    /// conference API can reach answered outbound calls.
    pub fn with_control_registry(mut self, control_registry: CallControlRegistry) -> Self {
        self.control_registry = control_registry;
        self
    }

    /// Share the park context so outbound bots can park/retrieve.
    pub fn with_park(mut self, park: ParkContext) -> Self {
        self.park = Some(park);
        self
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

        let target_str = format!(
            "sip:{}@{}:{}{}",
            req.to, gw.proxy_host, gw.proxy_port, gw.transport_uri_param
        );
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
            // SRTP wiring from [[gateway]].srtp lands in the next chunk;
            // plaintext for now (unchanged 0.6.x behaviour).
            srtp: siphon_ai_media_glue::OutboundSrtp::Off,
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
        let gateway = req.gateway.clone();
        let originator = Arc::clone(&gw.originator);
        let cdr_sink = Arc::clone(&self.cdr_sink);
        let webhook_sink = Arc::clone(&self.webhook_sink);
        let consult_registry = self.consult_registry.clone();
        let conference = self.conference.clone();
        let control_registry = self.control_registry.clone();
        let park = self.park.clone();

        info!(call_id = %bridge_id_str, gateway = %gateway, "originating outbound call");
        let log_id = bridge_id_str.clone();
        tokio::spawn(async move {
            let _permit = permit; // held until the call ends, then released
            metrics::gauge!(OUTBOUND_CALLS_ACTIVE).increment(1.0);
            let started_at = Utc::now();
            webhook_sink
                .emit(WebhookEvent::OutboundInitiated(OutboundInitiatedEvent {
                    version: WEBHOOK_VERSION,
                    call_id: log_id.clone(),
                    timestamp: started_at,
                    to: to.clone(),
                    gateway: gateway.clone(),
                }))
                .await;
            let result = originator.place(target, offer_req, tap).await;
            let result_label = outbound_result_label(&result);
            metrics::counter!(OUTBOUND_CALLS_TOTAL, "result" => result_label).increment(1);
            match result {
                Ok(call) => {
                    let ctx = OutboundCallContext {
                        bridge_id,
                        started_at,
                        from,
                        to,
                        gateway,
                        cdr_sink,
                        webhook_sink,
                        consult_registry,
                        conference,
                        control_registry,
                        park,
                    };
                    run_call(originator, call, bridge, ctx).await;
                }
                Err(e) => {
                    warn!(call_id = %log_id, error = %e, "outbound call did not connect");
                    webhook_sink
                        .emit(WebhookEvent::OutboundFailed(OutboundFailedEvent {
                            version: WEBHOOK_VERSION,
                            call_id: log_id,
                            timestamp: Utc::now(),
                            cause: result_label.to_string(),
                        }))
                        .await;
                }
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

/// Everything the answered-call path needs at CDR/webhook-emission time
/// beyond the call itself — the outbound counterpart of the acceptor's
/// `CallStart` snapshot.
struct OutboundCallContext {
    bridge_id: BridgeCallId,
    /// When `place()` started (= the `outbound_initiated` timestamp), so
    /// the CDR's `duration_ms` covers ring time too; answer time is on
    /// the `outbound_answered` webhook.
    started_at: DateTime<Utc>,
    from: String,
    to: String,
    /// `[[gateway]].name` — fills the CDR `route` field for outbound.
    gateway: String,
    cdr_sink: CdrSinkHandle,
    webhook_sink: WebhookSinkHandle,
    /// Register/deregister this leg as an attended-transfer consult
    /// target for the call's lifetime.
    consult_registry: ConsultRegistry,
    /// Conference registry, shared with the controller so an outbound
    /// bot can `conference_join`. `None` when conferencing is off.
    conference: Option<ConferenceRegistry>,
    /// Bridge-id handle table — this leg registers in it for its
    /// lifetime so the admin conference API can reach it.
    control_registry: CallControlRegistry,
    /// Park context, shared with the controller so an outbound bot can
    /// park/retrieve. `None` when park is off.
    park: Option<ParkContext>,
}

/// Run an answered outbound call's audio bridge to completion, tear it
/// down (BYE the dialog + stop the media session), then emit the CDR and
/// the `call_end` webhook.
async fn run_call(
    originator: Arc<OutboundOriginator>,
    call: OutboundCall,
    bridge: BridgeConfig,
    ctx: OutboundCallContext,
) {
    let OutboundCall {
        accepted,
        dialog,
        call_handle,
        call_id,
    } = call;
    let sip_call_id = dialog.id().call_id().to_string();
    // Answered → this leg is a valid attended-transfer consult target
    // until it ends. Snapshot is enough: the transfer task only reads
    // the dialog's id and remote target (DEV_PLAN_0.6.1 §2.1).
    ctx.consult_registry
        .insert(ctx.bridge_id.as_str(), dialog.clone());
    ctx.webhook_sink
        .emit(WebhookEvent::OutboundAnswered(OutboundAnsweredEvent {
            version: WEBHOOK_VERSION,
            call_id: ctx.bridge_id.as_str().to_string(),
            sip_call_id: sip_call_id.clone(),
            timestamp: Utc::now(),
        }))
        .await;
    let audio = CdrAudioInfo {
        codec: accepted.answer.negotiated_codec.encoding_name().to_string(),
        payload_type: accepted.answer.negotiated_payload_type,
        sample_rate: accepted.answer.negotiated_audio_sample_rate,
    };
    let ws_url = bridge.ws_url.clone();
    let start = build_outbound_start_msg(
        ctx.bridge_id.clone(),
        &ctx.from,
        &ctx.to,
        &sip_call_id,
        &accepted.answer,
    );
    // Outbound legs are transferable too (DEV_PLAN_0.6.1 §2.4): the
    // bot can consult an agent and hand this callee off. The REFER
    // goes through this gateway's own UAC (digest credentials), on
    // the dialog we hold directly — each gateway UAC keeps a private
    // DialogManager, so the shared lookup the inbound path uses
    // can't see this dialog.
    let transfer = TransferContext {
        uac: originator.uac(),
        source: DialogSource::Direct(Box::new(dialog.clone())),
        consult_registry: ctx.consult_registry.clone(),
        // Outbound legs dialed out themselves, so the gateway UAC's
        // dispatcher can reach the peer without flow reuse.
        flow: None,
    };
    let cfg = CallControllerConfig {
        call_id: ctx.bridge_id.clone(),
        bridge,
        start,
        media_tap: accepted.tap,
        transfer: Some(transfer),
        recording: None,
        conference: ctx.conference.clone(),
        park: ctx.park.clone(),
    };
    let (controller, handle) = CallController::new(cfg);
    // Reachable by the admin conference API for this leg's lifetime.
    ctx.control_registry.insert(handle);
    let run_result = controller.run().await;
    ctx.control_registry.remove(ctx.bridge_id.as_str());
    match &run_result {
        Ok(o) => {
            info!(sip_call_id = %sip_call_id, termination = ?o.termination, "outbound call ended")
        }
        Err(e) => warn!(sip_call_id = %sip_call_id, error = %e, "outbound controller error"),
    }
    // The controller's done — this leg is no longer a consult target;
    // then BYE the dialog and release the media session.
    ctx.consult_registry.remove(ctx.bridge_id.as_str());
    originator.hangup(&dialog).await;
    originator.stop_session(&call_id).await;
    drop(call_handle); // stop keepalives / session-timer tasks

    let view = CallTerminationView::from_run_result(run_result);
    let ended_at = Utc::now();
    let record = build_outbound_record(&ctx, &sip_call_id, audio, &ws_url, ended_at, &view);
    let end_event = WebhookEvent::CallEnd(CallEndEvent {
        version: WEBHOOK_VERSION,
        call_id: ctx.bridge_id.as_str().to_string(),
        sip_call_id,
        timestamp: ended_at,
        from: ctx.from.clone(),
        to: ctx.to.clone(),
        route: ctx.gateway.clone(),
        ws_url,
        duration_ms: record.duration_ms,
        termination_cause: termination_label(view.cause).to_string(),
    });
    ctx.cdr_sink.emit(record).await;
    ctx.webhook_sink.emit(end_event).await;
}

/// Assemble the CDR for an answered outbound call — the outbound
/// counterpart of the acceptor's `CallStart::into_record`. Failed calls
/// don't get a CDR (the `outbound_failed` webhook + metric cover them),
/// mirroring inbound where CDRs cover bridged calls only.
fn build_outbound_record(
    ctx: &OutboundCallContext,
    sip_call_id: &str,
    audio: CdrAudioInfo,
    ws_url: &str,
    ended_at: DateTime<Utc>,
    view: &CallTerminationView,
) -> CdrRecord {
    let duration_ms = (ended_at - ctx.started_at).num_milliseconds().max(0) as u64;
    CdrRecord {
        version: CDR_VERSION,
        call_id: ctx.bridge_id.as_str().to_string(),
        sip_call_id: sip_call_id.to_string(),
        started_at: ctx.started_at,
        ended_at,
        duration_ms,
        from: ctx.from.clone(),
        to: ctx.to.clone(),
        direction: CdrDirection::Outbound,
        route: ctx.gateway.clone(),
        ws_url: ws_url.to_string(),
        audio,
        termination: CdrTerminationInfo {
            cause: view.cause,
            bridge_disconnect: view.bridge_detail.clone(),
            tap_disconnect: view.tap_detail.clone(),
        },
        // STIR/SHAKEN verstat is an inbound-verification concern; recording
        // isn't wired for outbound calls in this release (controller runs
        // with `recording: None`).
        verstat_attest: None,
        verstat_passed: None,
        recording_id: None,
        recording_path: None,
        // Outbound bots can park too (0.7.0); carry the accounting.
        park: view.park.map(|p| siphon_ai_cdr::ParkInfo {
            count: p.count,
            total_ms: p.total_ms,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use siphon_ai_cdr::TerminationCause as CdrTerminationCause;

    #[test]
    fn outbound_record_carries_direction_gateway_and_termination() {
        let ctx = OutboundCallContext {
            bridge_id: BridgeCallId::new("siphon-9b2c"),
            started_at: Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap(),
            from: "sip:bot@siphon.example.com".into(),
            to: "+13125550000".into(),
            gateway: "twilio_main".into(),
            cdr_sink: Arc::new(siphon_ai_cdr::NullSink),
            webhook_sink: Arc::new(siphon_ai_webhooks::NullSink),
            consult_registry: ConsultRegistry::new(),
            conference: None,
            control_registry: CallControlRegistry::new(),
            park: None,
        };
        let view = CallTerminationView {
            cause: CdrTerminationCause::ServerHangup,
            bridge_detail: "stop_sent".into(),
            tap_detail: "controller_hung_up".into(),
            recording: None,
            park: None,
        };
        let audio = CdrAudioInfo {
            codec: "PCMU".into(),
            payload_type: 0,
            sample_rate: 8000,
        };
        let ended_at = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 42).unwrap();
        let record = build_outbound_record(
            &ctx,
            "xyz-789@siphon",
            audio,
            "wss://agent.example.com/bridge",
            ended_at,
            &view,
        );
        assert_eq!(record.version, CDR_VERSION);
        assert_eq!(record.direction, CdrDirection::Outbound);
        assert_eq!(record.call_id, "siphon-9b2c");
        assert_eq!(record.sip_call_id, "xyz-789@siphon");
        assert_eq!(record.route, "twilio_main");
        assert_eq!(record.duration_ms, 42_000);
        assert_eq!(record.termination.cause, CdrTerminationCause::ServerHangup);
        assert_eq!(record.termination.bridge_disconnect, "stop_sent");
        assert_eq!(record.verstat_attest, None);
        assert_eq!(record.recording_id, None);
    }

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
