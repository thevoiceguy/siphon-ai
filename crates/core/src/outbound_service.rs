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

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use forge_core::{CallId, ParticipantId};
use sip_core::SipUri;
use siphon_ai_bridge::{BridgeConfig, CallId as BridgeCallId, Direction};
use siphon_ai_cdr::{
    AudioInfo as CdrAudioInfo, CdrRecord, CdrSinkHandle, Direction as CdrDirection,
    TerminationInfo as CdrTerminationInfo, CDR_VERSION,
};
use siphon_ai_media_glue::{
    rewrite_sdp_direction, MediaDirection, OutboundOfferRequest, OutboundSrtp, TapOptions,
};
use siphon_ai_telemetry::{
    OriginateRejection, OriginateRequest, OutboundOriginateHandle, OUTBOUND_CALLS_ACTIVE,
    OUTBOUND_CALLS_TOTAL, OUTBOUND_SRTP_TOTAL, RECORDINGS_TOTAL,
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
use crate::hold::HoldContext;
use crate::outbound::{
    NotAnsweredCause, OutboundCall, OutboundError, OutboundGuard, OutboundOriginator,
    OutboundRejection,
};
use crate::park::ParkContext;
use crate::registry::{CallControlRegistry, CallEntry, CallRegistry, ConsultRegistry};
use crate::transfer::{DialogControl, DialogSource, TransferContext};
use siphon_ai_recording::{RecordingConfig, RecordingMode, RecordingSetup};

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
    /// SRTP policy for media on this trunk (0.7.x). The daemon maps the
    /// config gateway's `SrtpMode` onto this when building the service.
    pub srtp: OutboundSrtp,
    /// Default recording mode for calls through this gateway (0.26.0).
    /// A per-originate `recording` override wins.
    pub recording: RecordingMode,
}

/// Daemon-wide outbound-origination service.
pub struct OutboundService {
    /// Gateway table, behind an `ArcSwap` so a SIGHUP reload can swap
    /// the set (add / remove / modify gateways) for new originations.
    /// In-flight outbound calls already hold their gateway's
    /// `Arc<OutboundOriginator>`, so they keep running on it.
    gateways: ArcSwap<HashMap<String, OutboundGateway>>,
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
    /// SIP-Call-ID keyed registry that UAS dispatch consults for an
    /// inbound in-dialog BYE/CANCEL. Outbound legs join it for their
    /// lifetime so a far-end hangup reaches this leg's controller —
    /// without it `terminate_from_bye` misses and the call runs on to
    /// the media watchdog (#324). Distinct from `control_registry`,
    /// which is keyed by *bridge* id and serves the admin API.
    call_registry: CallRegistry,
    /// Park context (0.7.0). `Some` when `[park].enabled`; outbound
    /// bots can park/retrieve just like inbound calls.
    park: Option<ParkContext>,
    /// `[media].moh_file` — hold music for the WS-reconnect gap (0.7.3).
    /// Shared with the inbound side (the acceptor's `hold_moh_file`).
    /// `None` → comfort silence.
    moh_file: Option<std::path::PathBuf>,
    /// `[recording]` config (0.26.0) — outbound legs record with the
    /// same dir/encryption/format as inbound.
    recording: RecordingConfig,
    /// `[recording.storage]` upload settings, shared with the acceptor's
    /// teardown enqueue.
    recording_upload: Option<std::sync::Arc<siphon_ai_http::upload::UploadSettings>>,
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
            gateways: ArcSwap::from_pointee(gateways),
            guard,
            defaults,
            call_id_factory,
            cdr_sink,
            webhook_sink,
            consult_registry,
            conference: None,
            control_registry: CallControlRegistry::new(),
            call_registry: CallRegistry::new(),
            park: None,
            moh_file: None,
            recording: RecordingConfig::default(),
            recording_upload: None,
        }
    }

    /// Install the `[recording]` config so outbound legs can record
    /// (0.26.0). Without this every originated call runs unrecorded.
    pub fn with_recording(mut self, recording: RecordingConfig) -> Self {
        self.recording = recording;
        self
    }

    /// Install `[recording.storage]` upload settings (0.25.0/0.26.0) so
    /// finalized outbound recordings are spooled for upload too.
    pub fn with_recording_upload(
        mut self,
        upload: Option<std::sync::Arc<siphon_ai_http::upload::UploadSettings>>,
    ) -> Self {
        self.recording_upload = upload;
        self
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

    /// Share the daemon's SIP-Call-ID call registry — the one
    /// `dispatch_bye` / `dispatch_cancel` resolve against — so a far-end
    /// BYE on an outbound call reaches its controller (#324).
    ///
    /// Without this an answered outbound leg exists only in the
    /// bridge-id `control_registry`, so `terminate_from_bye` misses, the
    /// peer's BYE is answered `200` but changes nothing, and the call
    /// survives to the 60 s media-inactivity watchdog.
    ///
    /// Note this registry is also the daemon's active-call count for
    /// graceful shutdown (`/admin/v1/drain`'s `active_calls`, and what
    /// `drain_wait` blocks on), so joining it means a drain now waits
    /// for in-flight outbound calls as it always has for inbound.
    pub fn with_call_registry(mut self, call_registry: CallRegistry) -> Self {
        self.call_registry = call_registry;
        self
    }

    /// Share the park context so outbound bots can park/retrieve.
    pub fn with_park(mut self, park: ParkContext) -> Self {
        self.park = Some(park);
        self
    }

    /// Set the hold-music file (`[media].moh_file`) used during the
    /// WS-reconnect gap on outbound legs (0.7.3). `None` → comfort
    /// silence. Reconnect itself is gated by `[bridge].ws_reconnect_enabled`
    /// (from the daemon defaults).
    pub fn with_moh_file(mut self, moh_file: Option<std::path::PathBuf>) -> Self {
        self.moh_file = moh_file;
        self
    }

    /// Swap the gateway table (SIGHUP reload). New originations use the
    /// new set; in-flight calls keep the originator they captured. The
    /// concurrency guard (`max_concurrent` / `rate_limit`) is *not*
    /// touched — resizing a live semaphore isn't safe, so those changes
    /// remain restart-required.
    pub fn reload_gateways(&self, gateways: HashMap<String, OutboundGateway>) {
        self.gateways.store(Arc::new(gateways));
    }
}

impl OutboundOriginateHandle for OutboundService {
    fn originate(&self, req: OriginateRequest) -> Result<String, OriginateRejection> {
        // Snapshot the gateway table for this origination. The guard
        // lives to the end of this (synchronous) method, past every
        // `gw` read; the spawned call captures only the cloned
        // `Arc<OutboundOriginator>`, so a concurrent reload is safe.
        let gateways = self.gateways.load();
        let gw = gateways
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

        // Caller-ID for the INVITE From header (issue #316). Per-request
        // `from` overrides the gateway default; the gateway's `from` is
        // validated at config load, so only a bad per-request override
        // reaches `BadFrom` here. Validated before a permit is consumed.
        // The parsed URI drives the SIP From; the string still feeds the
        // WS `start` message + CDR via `ctx.from`.
        let from = req.from.clone().unwrap_or_else(|| gw.from.clone());
        let from_uri = SipUri::parse(&from)
            .map_err(|e| OriginateRejection::BadFrom(format!("{from}: {e}")))?;

        // Recording for this leg (0.26.0): per-request override beats the
        // gateway default. Same vocabulary as [recording].mode; anything
        // else — or recording without a configured [recording].dir — is a
        // 400, before a concurrency permit is consumed.
        let recording_mode = match req.recording.as_deref() {
            None => gw.recording,
            Some("off") => RecordingMode::Off,
            Some("always") => RecordingMode::Always,
            Some("on_demand") => RecordingMode::OnDemand,
            Some(other) => {
                return Err(OriginateRejection::BadRecording(format!(
                    "{other:?} (expected \"off\", \"always\", or \"on_demand\")"
                )))
            }
        };
        if recording_mode != RecordingMode::Off && self.recording.dir.as_os_str().is_empty() {
            return Err(OriginateRejection::BadRecording(
                "recording requested but [recording].dir is not configured".into(),
            ));
        }

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
            // Offer SRTP per the gateway's policy.
            srtp: gw.srtp,
            vad: self.defaults.vad,
        };
        let tap = TapOptions {
            barge_in_action: barge_in_to_tap_action(&self.defaults.barge_in),
            barge_in_debounce: self.defaults.barge_in.debounce,
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
            // WS liveness (PROTOCOL.md §5.6 / §3.1) applies to outbound
            // legs too — a slow/hung WS server wedges an outbound call the
            // same way it would an inbound one.
            ping_interval: self.defaults.ws_ping_interval,
            pong_timeout: self.defaults.ws_pong_timeout,
            start_deadline: self.defaults.server_start_deadline,
        };
        let to = req.to.clone();
        let gateway = req.gateway.clone();
        let delayed_offer = req.delayed_offer;
        // SRTP is requested whenever the gateway asks for it. For early
        // offer we offer SDES; for delayed offer we can't offer (no SDP in
        // the INVITE) but we answer the peer's SDES offer per the same
        // policy — either way the negotiated suite rides `accepted.srtp_profile`.
        let srtp_requested = gw.srtp != OutboundSrtp::Off;
        let originator = Arc::clone(&gw.originator);
        let cdr_sink = Arc::clone(&self.cdr_sink);
        let webhook_sink = Arc::clone(&self.webhook_sink);
        let consult_registry = self.consult_registry.clone();
        let conference = self.conference.clone();
        let control_registry = self.control_registry.clone();
        let call_registry = self.call_registry.clone();
        let park = self.park.clone();
        // WS reconnect (0.7.3) — outbound legs reconnect on the same daemon
        // defaults as inbound; extracted before the spawn (no `self` inside).
        let ws_reconnect_enabled = self.defaults.ws_reconnect_enabled;
        let ws_reconnect_max = self.defaults.ws_reconnect_max;
        let ws_reconnect_moh_file = self.moh_file.clone();
        // Recording setup mirrors the inbound acceptor's resolution: the
        // path keys on the bridge id, `always` auto-starts, `on_demand`
        // wires the writer idle for a WS `start_recording`.
        let recording_setup = match recording_mode {
            RecordingMode::Off => None,
            mode => Some(RecordingSetup {
                path: self.recording.path_for(bridge_id.as_str()),
                auto_start: mode == RecordingMode::Always,
                encryption: self.recording.encryption.clone(),
                format: self.recording.format,
                announcement: self.recording.announcement.clone(),
            }),
        };
        let recording_upload = self.recording_upload.clone();
        // Announced on `start.barge_in_mode` (0.32.0); extracted before
        // the spawn like the reconnect defaults (no `self` inside).
        let barge_in_mode = crate::acceptor::barge_in_mode_info(&self.defaults.barge_in);
        // Outbound legs resolve the WS-failure prompt from the global
        // defaults (no route). Extracted before the spawn like the rest.
        let ws_failure_prompt = (self.defaults.ws_failure_action
            == crate::acceptor::WsFailureAction::PlayPrompt)
            .then(|| self.defaults.ws_failure_prompt_file.clone())
            .flatten();

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
            let result = if delayed_offer {
                originator
                    .place_delayed(target, from_uri, offer_req, tap)
                    .await
            } else {
                originator.place(target, from_uri, offer_req, tap).await
            };
            let result_label = outbound_result_label(&result);
            metrics::counter!(OUTBOUND_CALLS_TOTAL, "result" => result_label).increment(1);
            match result {
                Ok(call) => {
                    // `place`/`place_delayed` return on the 2xx, so this is
                    // the answer instant — the only point on the outbound
                    // path where connected time starts. `started_at` above
                    // was stamped when the originate request was accepted,
                    // before the INVITE went out (issue #331).
                    let answered_at = Utc::now();
                    let ctx = OutboundCallContext {
                        bridge_id,
                        started_at,
                        answered_at,
                        from,
                        to,
                        gateway,
                        cdr_sink,
                        webhook_sink,
                        consult_registry,
                        conference,
                        control_registry,
                        call_registry,
                        park,
                        srtp_requested,
                        ws_reconnect_enabled,
                        ws_reconnect_max,
                        ws_reconnect_moh_file,
                        recording_setup,
                        recording_upload,
                        barge_in_mode,
                        ws_failure_prompt,
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
    /// When the 2xx arrived — connected time starts here, not at
    /// `started_at`. Feeds the CDR's `answered_at` (issue #331).
    answered_at: DateTime<Utc>,
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
    /// SIP-Call-ID registry the UAS dispatch resolves a far-end BYE
    /// against. This leg joins for its lifetime (#324).
    call_registry: CallRegistry,
    /// Park context, shared with the controller so an outbound bot can
    /// park/retrieve. `None` when park is off.
    park: Option<ParkContext>,
    /// Whether this gateway offered SRTP (`[[gateway]].srtp != off`), so the
    /// answered-call path can record `encrypted` vs `downgraded`.
    srtp_requested: bool,
    /// WS reconnect (0.7.3), from the daemon `[bridge]` defaults — outbound
    /// legs reconnect the same way inbound does (the drive is bridge-generic).
    ws_reconnect_enabled: bool,
    ws_reconnect_max: std::time::Duration,
    ws_reconnect_moh_file: Option<std::path::PathBuf>,
    /// Recording for this leg (0.26.0), resolved at originate time
    /// (request override > gateway default). `None` = unrecorded.
    recording_setup: Option<RecordingSetup>,
    /// `[recording.storage]` upload settings for the teardown enqueue.
    recording_upload: Option<std::sync::Arc<siphon_ai_http::upload::UploadSettings>>,
    /// Resolved barge-in announcement for `start.barge_in_mode` (0.32.0)
    /// — outbound legs use the daemon `[bridge]` defaults (no route).
    barge_in_mode: siphon_ai_bridge::BargeInModeInfo,
    /// WS-failure prompt (0.34.0), from the daemon `[bridge]` defaults
    /// (no route on originated calls). `None` = plain hangup.
    ws_failure_prompt: Option<std::path::PathBuf>,
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
    // Surface negotiated outbound SRTP on `start.srtp` (exchange is always
    // SDES on the originate path). `None` for a plaintext call or a
    // `preferred` downgrade.
    let srtp_info =
        accepted
            .srtp_profile
            .as_ref()
            .map(|profile| siphon_ai_bridge::protocol::SrtpInfo {
                // SDES on early offer; the delayed-offer answer path may
                // have negotiated DTLS-SRTP (0.9.3) — use what it recorded.
                exchange: accepted.srtp_exchange,
                profile: profile.clone(),
            });
    if ctx.srtp_requested {
        // Encrypted when the peer accepted SRTP; downgraded when the gateway
        // is `preferred` and the peer answered plaintext. (`required`
        // downgrades never reach here — they fail in apply_answer.)
        let result = if accepted.srtp_profile.is_some() {
            "encrypted"
        } else {
            "downgraded"
        };
        metrics::counter!(OUTBOUND_SRTP_TOTAL, "result" => result).increment(1);
    }
    let start = build_outbound_start_msg(
        ctx.bridge_id.clone(),
        &ctx.from,
        &ctx.to,
        &sip_call_id,
        &accepted.answer,
        srtp_info,
        ctx.barge_in_mode,
    );
    // Outbound legs are transferable too (DEV_PLAN_0.6.1 §2.4): the
    // bot can consult an agent and hand this callee off. The REFER
    // goes through this gateway's own UAC (digest credentials), on
    // the dialog we hold directly — each gateway UAC keeps a private
    // DialogManager, so the shared lookup the inbound path uses
    // can't see this dialog.
    let transfer = TransferContext {
        control: DialogControl {
            uac: originator.uac(),
            source: DialogSource::Direct(Box::new(dialog.clone())),
            // Outbound legs dialed out themselves, so the gateway UAC's
            // dispatcher can reach the peer without flow reuse.
            flow: None,
        },
        consult_registry: ctx.consult_registry.clone(),
    };
    // Bot-initiated hold on outbound legs (0.7.5). Same DialogControl as
    // transfer (Direct dialog, no flow — the gateway UAC reaches the peer).
    // The hold/resume offers are our original offer SDP with the direction
    // flipped (the outbound analogue of inbound's cached answer); the gap
    // MOH reuses the shared `[media].moh_file`.
    let hold = Some(HoldContext {
        control: DialogControl {
            uac: originator.uac(),
            source: DialogSource::Direct(Box::new(dialog.clone())),
            flow: None,
        },
        hold_offer_sdp: rewrite_sdp_direction(&accepted.offer_sdp, MediaDirection::SendOnly),
        resume_offer_sdp: rewrite_sdp_direction(&accepted.offer_sdp, MediaDirection::SendRecv),
        moh_file: ctx.ws_reconnect_moh_file.clone(),
    });
    let cfg = CallControllerConfig {
        call_id: ctx.bridge_id.clone(),
        bridge,
        start,
        // Reconnect-enabled legs put the tap in survive-WS-drop mode so an
        // unexpected drop doesn't tear it down before the controller redials
        // (0.7.3) — same as the acceptor does for inbound.
        media_tap: accepted
            .tap
            .with_survive_ws_drop(ctx.ws_reconnect_enabled || ctx.ws_failure_prompt.is_some()),
        transfer: Some(transfer),
        recording: ctx.recording_setup.clone(),
        conference: ctx.conference.clone(),
        park: ctx.park.clone(),
        hold,
        // WS reconnect (0.7.3) — outbound legs reconnect on the daemon
        // `[bridge]` defaults, same drive as inbound.
        ws_reconnect_enabled: ctx.ws_reconnect_enabled,
        ws_reconnect_max: ctx.ws_reconnect_max,
        ws_reconnect_moh_file: ctx.ws_reconnect_moh_file.clone(),
        ws_failure_prompt: ctx.ws_failure_prompt.clone(),
    };
    let (controller, handle) = CallController::new(cfg);
    // Clone BEFORE the registry takes it — teardown below consults
    // `remote_bye_received()` to decide whether we still owe the peer a
    // BYE. Same reason and same shape as the inbound acceptor's
    // `cleanup_handle`; `CallHandle` is cheap (Arc-of-Notify +
    // Arc-of-AtomicBool).
    let cleanup_handle = handle.clone();
    // Reachable by the admin conference API for this leg's lifetime.
    // Carries the SIP Call-ID + direction for the `GET /admin/calls`
    // listing (issue #311).
    ctx.control_registry
        .insert(handle, sip_call_id.clone(), Direction::Outbound);
    // …and by SIP Call-ID, which is what UAS dispatch resolves an
    // inbound in-dialog BYE/CANCEL against. The two registries are
    // separate namespaces (bridge id vs SIP Call-ID) and outbound legs
    // previously joined only the first, so a far-end BYE reached
    // `on_bye`, missed the lookup, and terminated nothing (#324).
    //
    // `answer_text` is `None`: it holds the SDP *we* answered an inbound
    // INVITE with, which has no outbound analogue — we sent the offer.
    // A peer re-INVITE on this leg is therefore refused `501` (the
    // documented "no stored answer" path) rather than the `481` an
    // unregistered dialog used to produce. Renegotiating an outbound leg
    // is genuinely unimplemented, so 501 is the honest answer.
    ctx.call_registry.insert(
        sip_call_id.clone(),
        CallEntry::new(cleanup_handle.clone(), None),
    );

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
    // Only BYE if the far end didn't already. When the callee hangs up,
    // `terminate_from_bye` has marked the handle and we've answered
    // their BYE with 200 — the dialog is over, and a BYE of our own
    // goes to a dialog the peer has discarded, dying on Timer F ~32 s
    // later (issue #324's last impact row). The inbound acceptor has
    // always gated its teardown BYE this way; the outbound path never
    // did. It went unnoticed because until the dialog-store fix above,
    // a far-end BYE was answered 481 and never reached the controller
    // at all, so this branch was unreachable in the case that needs it.
    if !cleanup_handle.remote_bye_received() {
        originator.hangup(&dialog).await;
    }
    originator.stop_session(&call_id).await;
    drop(call_handle); // stop keepalives / session-timer tasks

    let view = CallTerminationView::from_run_result(run_result);
    if let Some(rec) = view.recording.as_ref() {
        metrics::counter!(RECORDINGS_TOTAL, "result" => rec.result.as_str()).increment(1);
    }
    let ended_at = Utc::now();
    let mut record = build_outbound_record(&ctx, &sip_call_id, audio, &ws_url, ended_at, &view);
    // Spool the finalized recording for object-storage upload (0.25.0
    // machinery, outbound wiring 0.26.0) — mirrors the inbound acceptor.
    if let (Some(upload), Some(rec)) = (ctx.recording_upload.as_ref(), view.recording.as_ref()) {
        if rec.result != crate::call::RecordingResult::Failed {
            let key = upload.render_key(&record.call_id, &record.route, "outbound", &rec.path);
            let job = upload.job(&record.call_id, key.clone(), rec.path.clone());
            match upload.enqueue(&job) {
                Ok(()) => record.recording_url = Some(upload.planned_uri(&key)),
                Err(err) => warn!(
                    call_id = %record.call_id,
                    error = %err,
                    "could not spool recording upload; kept local only"
                ),
            }
        }
    }
    let end_event = WebhookEvent::CallEnd(CallEndEvent {
        version: WEBHOOK_VERSION,
        call_id: ctx.bridge_id.as_str().to_string(),
        sip_call_id: sip_call_id.clone(),
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

    // Deregister LAST — after the CDR is durably written, not before.
    // `drain_wait` polls this registry's length to decide the graceful
    // shutdown is done; removing the entry earlier (as this used to,
    // right after the BYE) let a drain-forced call's slot empty while
    // its CDR emit was still in flight, so teardown proceeded and the
    // detached cleanup task was cancelled mid-write — the call's
    // billing-grade record silently lost (issue #344). Holding the
    // entry until the `FileSink::emit` above has flushed makes the drain
    // wait for it. A BYE retransmit racing this window still finds the
    // entry and gets a 200, same as before — the slot just lives a few
    // milliseconds longer.
    ctx.call_registry.remove(&sip_call_id);
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
        answered_at: Some(ctx.answered_at),
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
        // STIR/SHAKEN verstat is an inbound-verification concern.
        verstat_attest: None,
        verstat_passed: None,
        // Recording works on outbound legs since 0.26.0; recording_id ==
        // call_id, same as inbound. `recording_url` is stamped post-hoc
        // by the upload-enqueue block.
        recording_id: view
            .recording
            .as_ref()
            .map(|_| ctx.bridge_id.as_str().to_string()),
        recording_path: view
            .recording
            .as_ref()
            .map(|r| r.path.display().to_string()),
        recording_encrypted: view.recording.as_ref().map(|r| r.encrypted),
        recording_url: None,
        consent: view.consent.as_ref().map(|c| siphon_ai_cdr::ConsentInfo {
            announced: c.announced,
            announcement_ms: c.announcement_ms,
            server: c.server.clone(),
        }),
        // Outbound bots can park too (0.7.0); carry the accounting.
        park: view.park.map(|p| siphon_ai_cdr::ParkInfo {
            count: p.count,
            total_ms: p.total_ms,
        }),
        // Bot-hold accounting (0.7.2). Always `None` for outbound legs in
        // this release (outbound bot-hold is a follow-up), but mapped for
        // forward-compatibility once it's wired.
        hold: view.hold.map(|h| siphon_ai_cdr::HoldInfo {
            count: h.count,
            total_ms: h.total_ms,
        }),
        // WS-reconnect accounting (0.7.3). `None` for outbound legs (the
        // drive is inbound-only this release), mapped for forward-compat.
        reconnect: view.reconnect.map(|r| siphon_ai_cdr::ReconnectInfo {
            count: r.count,
            total_gap_ms: r.total_gap_ms,
        }),
        // Per-call quality summary (0.30.0) — same mapping as inbound.
        quality: view.quality.map(crate::acceptor::quality_info),
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
            // Rang for 12 s before the callee picked up — the gap this
            // field exists to make visible (#331).
            answered_at: Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 12).unwrap(),
            from: "sip:bot@siphon.example.com".into(),
            to: "+13125550000".into(),
            gateway: "twilio_main".into(),
            cdr_sink: Arc::new(siphon_ai_cdr::NullSink),
            webhook_sink: Arc::new(siphon_ai_webhooks::NullSink),
            consult_registry: ConsultRegistry::new(),
            conference: None,
            control_registry: CallControlRegistry::new(),
            call_registry: CallRegistry::new(),
            park: None,
            srtp_requested: false,
            ws_reconnect_enabled: false,
            ws_reconnect_max: std::time::Duration::from_secs(30),
            ws_reconnect_moh_file: None,
            recording_setup: None,
            recording_upload: None,
            barge_in_mode: siphon_ai_bridge::BargeInModeInfo::AutoClear,
            ws_failure_prompt: None,
        };
        let view = CallTerminationView {
            cause: CdrTerminationCause::ServerHangup,
            bridge_detail: "stop_sent".into(),
            tap_detail: "controller_hung_up".into(),
            recording: None,
            park: None,
            hold: None,
            reconnect: None,
            consent: None,
            quality: None,
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
