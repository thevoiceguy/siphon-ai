//! `BridgingAcceptor` — turn a routed INVITE into a running call.
//!
//! Sits at the seam between sip-glue's [`CallAcceptor`] trait and the
//! per-call machinery in this crate. Given a [`MatchedCall`] from the
//! routing layer, it:
//!
//! 1. Pulls the offer SDP out of the INVITE body.
//! 2. Resolves daemon-wide media defaults against the matched route's
//!    `[route.media]` and `[route.bridge]` overrides.
//! 3. Asks [`MediaSetup`] to allocate the forge session, negotiate
//!    the answer, and attach a [`MediaTap`].
//! 4. Sends the 200 OK with the negotiated answer.
//! 5. Builds the bridge [`StartMsg`] from the inbound facts and
//!    spawns a [`CallController`] task.
//!
//! ## Design
//!
//! The deterministic pieces — building [`BridgeConfig`], building
//! [`StartMsg`], extracting the offer body, resolving codec lists —
//! are pure functions in this module so they can be unit-tested
//! without `ServerTransactionHandle` (which has no public test
//! constructor; see `sip-glue/tests/handler_dispatch.rs`). The async
//! [`CallAcceptor`] impl is a thin shim over them.
//!
//! ## Failure → SIP response
//!
//! | Cause                                       | Response                  |
//! |---------------------------------------------|---------------------------|
//! | INVITE has no body or wrong Content-Type    | 415 Unsupported Media Type|
//! | Offer parse / no common codec               | 488 Not Acceptable Here   |
//! | Forge port pool exhausted, internal error   | 500 Server Internal Error |
//! | Route's `ws_url` unset and no global default| 503 Service Unavailable   |
//!
//! Per CLAUDE.md §4.6 the last case should fail at config-load time;
//! we still surface a runtime 503 because the validation step isn't
//! wired yet — defensive and removable once the config crate lands.
//!
//! ## What's deferred
//!
//! - **BYE / CANCEL plumbing.** The spawned controller has a
//!   [`CallHandle`] but nothing calls `handle.shutdown()` on a SIP
//!   BYE yet. Tracked as the next layer; until then, the call ends
//!   when the WS server hangs up or the tap sees forge tear down.
//! - **CDR / lifecycle webhooks.** The controller's `CallOutcome`
//!   carries everything needed to emit them; the wiring belongs
//!   alongside BYE plumbing so a single "call ended" event drives
//!   both.
//! - **Forwarded headers.** `forward_headers` is honored if the
//!   caller passes a list, but the daemon doesn't read it from
//!   config yet.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use sip_core::{Request, Response};
use sip_dialog::session_timer_manager::{SessionTimerEvent, SessionTimerManager};
use sip_dialog::{DialogId, DialogManager};
use sip_uac::integrated::IntegratedUAC;
use sip_uas::integrated::{AcceptInviteAsyncOutcome, IntegratedUAS};
use sip_uas::{NegotiatedSessionTimer, SessionTimerPolicy, UserAgentServer};
use siphon_ai_bridge::{
    normalize_auth_header, AudioEncoding, AudioFormat, BridgeConfig, CallId as BridgeCallId,
    Direction, DisconnectReason, OutgoingEvent, SipMeta, StartMsg, PROTOCOL_VERSION,
};
use siphon_ai_cdr::{
    AudioInfo as CdrAudioInfo, CdrRecord, CdrSinkHandle, Direction as CdrDirection, NullSink,
    TerminationCause as CdrTerminationCause, TerminationInfo as CdrTerminationInfo, CDR_VERSION,
};
use siphon_ai_media_glue::{
    AnswerOutcome, Codec, InboundAccepted, InboundCall, MediaSetup, MediaTapError, SdpError,
    SetupError, TapDisconnect,
};
use siphon_ai_routes::CompiledRoute;
use siphon_ai_sip_glue::{CallAcceptor, InviteFacts, MatchedCall};
use siphon_ai_telemetry::{
    CALLS_ACTIVE, CALLS_TOTAL, CALL_DURATION_SECONDS, INVITES_TOTAL, ROUTE_MATCH_TOTAL,
    SDP_NEGOTIATE_SECONDS,
};
use siphon_ai_webhooks::{
    CallEndEvent, CallStartEvent, NullSink as WebhookNullSink, WebhookEvent, WebhookSinkHandle,
    WEBHOOK_VERSION,
};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

use crate::call::{CallController, CallControllerConfig, CallOutcome, CallTermination};
use crate::registry::CallRegistry;
use crate::transfer::TransferContext;

/// Daemon-wide bridge & media defaults. Routes' `[route.bridge]`
/// and `[route.media]` blocks override individual fields.
///
/// Owned by the acceptor; the daemon constructs one from parsed
/// TOML config and hands it in at startup.
#[derive(Debug, Clone)]
pub struct BridgeDefaults {
    /// Default WebSocket URL. May be empty — in that case every
    /// matched route MUST set its own `ws_url` or the call is
    /// rejected with 503 (see module-level docs).
    pub ws_url: Option<String>,
    /// Full `Authorization` header value (with scheme) to set on
    /// every WS upgrade by default. `None` ⇒ no header. Per-route
    /// `[route.bridge].ws_auth_header` overrides if set.
    pub auth_header: Option<String>,
    pub connect_timeout: Duration,
    /// Codecs to advertise, in priority order.
    pub codecs: Vec<Codec>,
    /// RFC-2833 telephone-event payload type, or `None` to disable.
    pub dtmf_payload_type: Option<u8>,
    /// SIP header names to forward verbatim onto the bridge
    /// `start.sip.headers` map. Names are matched case-insensitively
    /// against the INVITE.
    pub forward_headers: Vec<String>,
    /// Barge-in policy. [`BargeInMode::AutoClear`] (the default)
    /// drops pending outbound playout the moment forge-vad reports
    /// the caller started speaking — caller interruption acks
    /// without a server round-trip. [`BargeInMode::NotifyOnly`]
    /// just forwards `speech_started` and leaves the decision to
    /// the WS server.
    pub barge_in: BargeInConfig,
    /// Tear the call down after this many seconds of no inbound RTP.
    /// `None` disables the watchdog entirely (the per-route
    /// `[route.media].inactivity_timeout_secs = 0` opt-out resolves
    /// to `None` here). Default in `Default::default()` is 60 s —
    /// enough to weather a flap, short enough that an abandoned call
    /// after PBX network failure releases its forge session quickly.
    pub inactivity_timeout: Option<Duration>,
    /// Default one-sided silence threshold: emit `silence_detected`
    /// when the caller has been silent for this long (forge-vad
    /// drives the underlying "speech" signal). `None` disables.
    /// Default `Some(3000ms)` per `docs/DEV_PLAN_0.2.0.md` §9.2.
    pub silence_threshold: Option<Duration>,
    /// Default two-sided dead-air threshold: emit `dead_air_detected`
    /// when neither caller speech nor outbound WS audio has been
    /// observed for this long. `None` disables. Default
    /// `Some(10000ms)` per `docs/DEV_PLAN_0.2.0.md` §9.2.
    pub dead_air_threshold: Option<Duration>,
    /// Default cadence for `rtp_stats` events. `None` disables.
    /// Default `Some(5000ms)` per `docs/DEV_PLAN_0.2.0.md` §9.3,
    /// mirroring RTCP §6.2's compound-report cadence.
    pub rtp_stats_interval: Option<Duration>,
}

/// What the daemon does when forge-vad reports speech-started.
///
/// Public so the acceptor / media-glue / config layers can refer to
/// the type by symbol rather than threading a `String`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BargeInMode {
    /// Just forward `speech_started` / `speech_stopped` to the WS.
    /// The server decides whether to send `clear`.
    NotifyOnly,
    /// Forward the event AND drop pending outbound playout the
    /// moment speech-started fires.
    AutoClear,
}

/// Resolved barge-in plan after merging globals + route overrides.
#[derive(Debug, Clone)]
pub struct BargeInConfig {
    /// Master enable. When `false`, VAD events still flow to the WS
    /// (the tap subscribes regardless), but `mode` is treated as if
    /// it were `NotifyOnly` and never drives a server-side flush.
    pub enabled: bool,
    pub mode: BargeInMode,
}

impl Default for BargeInConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: BargeInMode::AutoClear,
        }
    }
}

/// How the UAS responds to inbound INVITEs before the 200 OK
/// (per `docs/DEV_PLAN_0.2.0.md` §4.1). All three modes still emit
/// `100 Trying` from `IntegratedUAS`; this enum picks what — if
/// anything — siphon-ai layers on top before the 2xx.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CallProgressMode {
    /// Skip any extra provisional and let `IntegratedUAS` go
    /// straight from 100 Trying to the 2xx. Default; matches v0.1.0
    /// behaviour.
    #[default]
    InstantAnswer,
    /// Send `180 Ringing` (no body) before the 2xx. Useful when an
    /// upstream PBX wants a ringback-style progress signal.
    Ringing,
    /// Send `183 Session Progress` carrying the negotiated answer
    /// SDP before the 2xx, so a peer that needs the answer for
    /// carrier-style early-media routing / billing has it before
    /// the call is technically answered.
    ///
    /// "Flavour B" per the §9.1 decision: the provisional is
    /// best-effort (no `100rel`). Peers that include `Require:
    /// 100rel` in the INVITE fall back to `InstantAnswer` with a
    /// `warn!` — the reliable / Flavour-C path is deferred until
    /// `on_prack` wiring lands.
    SessionProgress,
}

impl Default for BridgeDefaults {
    fn default() -> Self {
        Self {
            ws_url: None,
            auth_header: None,
            connect_timeout: Duration::from_secs(5),
            codecs: vec![Codec::Pcmu, Codec::Pcma],
            dtmf_payload_type: Some(101),
            forward_headers: Vec::new(),
            barge_in: BargeInConfig::default(),
            inactivity_timeout: Some(Duration::from_secs(60)),
            silence_threshold: Some(Duration::from_millis(3000)),
            dead_air_threshold: Some(Duration::from_millis(10000)),
            rtp_stats_interval: Some(Duration::from_millis(5000)),
        }
    }
}

/// What [`extract_offer_sdp`] / pre-flight checks return when the
/// INVITE is unfit to negotiate against.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum OfferError {
    /// `Content-Type` was missing or not `application/sdp`.
    #[error("INVITE Content-Type is not application/sdp (got {0:?})")]
    UnsupportedMediaType(Option<String>),

    /// Body was empty (`Content-Length: 0`).
    #[error("INVITE has no body")]
    NoBody,

    /// Body bytes weren't valid UTF-8 — SDP is text per RFC 4566.
    #[error("INVITE body is not valid UTF-8")]
    InvalidUtf8,
}

/// What can go wrong while building [`BridgeConfig`] from the daemon
/// defaults plus a route's override.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BridgeBuildError {
    /// Neither the daemon default nor the matched route specifies a
    /// `ws_url`. CLAUDE.md §4.6 says config-load should catch this;
    /// we still error at runtime so a stale config can't 200 OK a
    /// call we have nowhere to bridge to.
    #[error("no ws_url configured (no global default and no route override)")]
    NoWsUrl,
}

/// Errors the [`BridgingAcceptor`] surfaces internally before
/// translating them to SIP responses. The async wrapper consumes
/// these to choose a status code.
#[derive(Debug, Error)]
pub enum AcceptError {
    #[error(transparent)]
    Offer(#[from] OfferError),

    #[error(transparent)]
    Bridge(#[from] BridgeBuildError),

    #[error(transparent)]
    Setup(#[from] SetupError),

    /// Forge created the session and we negotiated the answer, but
    /// failed to spawn the controller (e.g., `tokio::spawn` from a
    /// non-tokio context — exceedingly rare; mostly defensive).
    #[error("controller setup failed: {0}")]
    Controller(String),
}

impl AcceptError {
    /// Status / reason pair to return in a SIP final response.
    /// Centralised so the async wrapper has one source of truth and
    /// tests can assert it without rebuilding the table.
    pub fn sip_status(&self) -> (u16, &'static str) {
        match self {
            AcceptError::Offer(OfferError::UnsupportedMediaType(_)) => {
                (415, "Unsupported Media Type")
            }
            AcceptError::Offer(OfferError::NoBody | OfferError::InvalidUtf8) => {
                (400, "Bad Request")
            }
            AcceptError::Bridge(BridgeBuildError::NoWsUrl) => (503, "Service Unavailable"),
            AcceptError::Setup(SetupError::Sdp(SdpError::Parse(_))) => (400, "Bad Request"),
            AcceptError::Setup(SetupError::Sdp(SdpError::NoAudio))
            | AcceptError::Setup(SetupError::Sdp(SdpError::NoCommonCodec))
            | AcceptError::Setup(SetupError::Sdp(SdpError::AudioRejected)) => {
                (488, "Not Acceptable Here")
            }
            AcceptError::Setup(SetupError::Sdp(SdpError::Negotiate(_))) => {
                (488, "Not Acceptable Here")
            }
            AcceptError::Setup(SetupError::Session(_))
            | AcceptError::Setup(SetupError::Tap(_))
            | AcceptError::Controller(_) => (500, "Server Internal Error"),
        }
    }
}

// ─── Pure helpers (unit-tested below) ───────────────────────────────

/// Pull the offer SDP body out of `request`. Verifies `Content-Type`
/// is `application/sdp` (case-insensitive on the type/subtype, and we
/// tolerate parameters like `; charset=utf-8`).
pub fn extract_offer_sdp(request: &Request) -> Result<&str, OfferError> {
    match request.headers().get_smol("Content-Type") {
        Some(value) => {
            let mime = value.split(';').next().unwrap_or("").trim();
            if !mime.eq_ignore_ascii_case("application/sdp") {
                return Err(OfferError::UnsupportedMediaType(Some(value.to_string())));
            }
        }
        None => {
            // Some gateways elide Content-Type when Content-Length is
            // 0; that's still no-body, treat as such.
            if !request.has_body() {
                return Err(OfferError::NoBody);
            }
            return Err(OfferError::UnsupportedMediaType(None));
        }
    }
    if !request.has_body() {
        return Err(OfferError::NoBody);
    }
    std::str::from_utf8(request.body()).map_err(|_| OfferError::InvalidUtf8)
}

/// Pull the SIP `Call-ID` header off `request`. Returns `""` if
/// absent — the matcher already routed, so we don't re-validate
/// here; the empty string just means the bridge `start.sip.call_id`
/// is empty for a malformed peer.
pub fn extract_sip_call_id(request: &Request) -> String {
    request
        .headers()
        .get_smol("Call-ID")
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Build a [`BridgeConfig`] by merging the daemon's [`BridgeDefaults`]
/// with the matched route's override block.
///
/// Rules per CLAUDE.md §4.6 ("Per-route overrides only override.
/// Anything not specified inherits from globals"):
/// - `ws_url`: route override > daemon default; one MUST be set.
/// - `auth_header`: derived from `ws_auth_header` (route override
///   only, since the daemon default is global) when it's a `Bearer`
///   token; other auth schemes pass through to the WS handshake but
///   we don't crack them open here.
/// - `connect_timeout`: route override (`ws_connect_timeout_ms`) >
///   daemon default.
pub fn build_bridge_config(
    defaults: &BridgeDefaults,
    route: &CompiledRoute,
) -> Result<BridgeConfig, BridgeBuildError> {
    let ws_url = route
        .bridge
        .ws_url
        .clone()
        .or_else(|| defaults.ws_url.clone())
        .ok_or(BridgeBuildError::NoWsUrl)?;
    if ws_url.is_empty() {
        return Err(BridgeBuildError::NoWsUrl);
    }

    let auth_header = match route.bridge.ws_auth_header.as_deref() {
        Some("") | None => defaults.auth_header.clone(),
        Some(header) => Some(normalize_auth_header(header)),
    };

    let connect_timeout = route
        .bridge
        .ws_connect_timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(defaults.connect_timeout);

    Ok(BridgeConfig {
        ws_url,
        auth_header,
        connect_timeout,
    })
}

/// Resolve the codec list for a matched route. The route's
/// `[route.media].codecs` (when set) replaces the daemon default;
/// individual codecs are parsed via [`Codec::from_encoding_name`].
/// Unrecognised names are dropped with a warning — the call still
/// proceeds with whatever the matcher could parse.
pub fn resolve_codecs(defaults: &BridgeDefaults, route: &CompiledRoute) -> Vec<Codec> {
    match route.media.codecs.as_ref() {
        None => defaults.codecs.clone(),
        Some(names) => {
            let mut out = Vec::with_capacity(names.len());
            for name in names {
                match Codec::from_encoding_name(name) {
                    Some(c) => out.push(c),
                    None => warn!(
                        codec = %name,
                        route = %route.name,
                        "unknown codec in route override; dropped"
                    ),
                }
            }
            if out.is_empty() {
                warn!(
                    route = %route.name,
                    "route media.codecs resolved to empty list; falling back to daemon defaults"
                );
                defaults.codecs.clone()
            } else {
                out
            }
        }
    }
}

/// Pick the RFC-2833 PT for this call. v1 has no per-route override
/// for it, but the seam is here so a future `[route.media].dtmf`
/// (`"rfc2833" | "off" | "inband"`) merge can land without changing
/// callers.
pub fn resolve_dtmf_pt(defaults: &BridgeDefaults, route: &CompiledRoute) -> Option<u8> {
    match route.media.dtmf.as_deref() {
        Some(v) if v.eq_ignore_ascii_case("off") => None,
        // "rfc2833" / "inband" / unset — keep the global PT. Inband
        // doesn't need a PT but advertising one costs nothing and
        // lets a peer that prefers RFC-2833 pick it.
        _ => defaults.dtmf_payload_type,
    }
}

/// Resolve the barge-in plan for one call by merging
/// `[bridge.barge_in]` (global) with `[route.bridge.barge_in]`. Same
/// shape as the other `resolve_*` helpers — unset route fields
/// inherit the default. An unrecognised `mode` string on a route
/// silently falls back to the global mode rather than failing the
/// call, matching the config crate's existing tolerance for partial
/// overrides.
pub fn resolve_barge_in(defaults: &BridgeDefaults, route: &CompiledRoute) -> BargeInConfig {
    let mut out = defaults.barge_in.clone();
    if let Some(enabled) = route.bridge.barge_in.enabled {
        out.enabled = enabled;
    }
    if let Some(mode) = route.bridge.barge_in.mode.as_deref() {
        if let Some(parsed) = parse_barge_in_mode_route(mode) {
            out.mode = parsed;
        }
    }
    out
}

fn parse_barge_in_mode_route(s: &str) -> Option<BargeInMode> {
    match s {
        "auto_clear" => Some(BargeInMode::AutoClear),
        "notify_only" => Some(BargeInMode::NotifyOnly),
        _ => None,
    }
}

/// Resolve the inactivity watchdog for one call. Route value wins
/// when set, with `Some(0)` meaning "disabled for this route";
/// otherwise the daemon default applies.
pub fn resolve_inactivity_timeout(
    defaults: &BridgeDefaults,
    route: &CompiledRoute,
) -> Option<Duration> {
    match route.media.inactivity_timeout_secs {
        None => defaults.inactivity_timeout,
        Some(0) => None,
        Some(n) => Some(Duration::from_secs(n)),
    }
}

/// Resolve the per-call silence threshold by merging the daemon
/// default with the per-route override (`[route.bridge].silence_threshold_ms`).
/// `None` returned = the event is disabled for this call.
pub fn resolve_silence_threshold(
    defaults: &BridgeDefaults,
    route: &CompiledRoute,
) -> Option<Duration> {
    match route.bridge.silence_threshold_ms {
        None => defaults.silence_threshold,
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
    }
}

/// Resolve the per-call `rtp_stats` emission cadence (same shape as
/// [`resolve_silence_threshold`]).
pub fn resolve_rtp_stats_interval(
    defaults: &BridgeDefaults,
    route: &CompiledRoute,
) -> Option<Duration> {
    match route.bridge.rtp_stats_interval_ms {
        None => defaults.rtp_stats_interval,
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
    }
}

/// Resolve the per-call dead-air threshold (same shape as
/// [`resolve_silence_threshold`]).
pub fn resolve_dead_air_threshold(
    defaults: &BridgeDefaults,
    route: &CompiledRoute,
) -> Option<Duration> {
    match route.bridge.dead_air_threshold_ms {
        None => defaults.dead_air_threshold,
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
    }
}

/// Translate the public [`BargeInConfig`] into the media-glue tap's
/// own enum. Kept as a single chokepoint so a future
/// `BargeInConfig.enabled = false` flip — which today degrades
/// `AutoClear` to `Notify` — can grow knobs without leaking into
/// every call site.
/// True when the INVITE's `Require` header lists `100rel`
/// (case-insensitive per RFC 3261 §27.1). siphon-ai 0.2.0 ships
/// best-effort provisionals only — peers that *require* reliable
/// provisionals fall back to `InstantAnswer` mode rather than risk
/// sending a non-compliant unreliable 1xx.
fn requires_100rel(request: &Request) -> bool {
    request
        .headers()
        .get("Require")
        .map(|v| {
            v.split(',')
                .any(|tok| tok.trim().eq_ignore_ascii_case("100rel"))
        })
        .unwrap_or(false)
}

/// Attach an SDP body to a `Response` (typically a freshly-built
/// `183 Session Progress`), updating `Content-Type` and
/// `Content-Length` to match. Returns a new `Response` — the
/// underlying type sets the body at construction, not by mutation.
fn attach_sdp_body(response: Response, sdp: &str) -> Response {
    let bytes = sdp.as_bytes().to_vec();
    let content_length = bytes.len().to_string();
    let mut response = response;
    response
        .headers_mut()
        .set_or_push("Content-Type", "application/sdp")
        .expect("content-type header valid");
    response
        .headers_mut()
        .set_or_push("Content-Length", &content_length)
        .expect("content-length header valid");
    let (start, headers, _) = response.into_parts();
    Response::new(start, headers, bytes.into()).expect("valid response with SDP body")
}

fn barge_in_to_tap_action(cfg: &BargeInConfig) -> siphon_ai_media_glue::BargeInAction {
    if !cfg.enabled {
        return siphon_ai_media_glue::BargeInAction::Notify;
    }
    match cfg.mode {
        BargeInMode::AutoClear => siphon_ai_media_glue::BargeInAction::AutoClear,
        BargeInMode::NotifyOnly => siphon_ai_media_glue::BargeInAction::Notify,
    }
}

/// Compose the bridge `start` message from the inbound INVITE facts,
/// the negotiation outcome, and the daemon's forward-header list.
///
/// `bridge_call_id` is the SiphonAI-internal id (distinct from the
/// SIP Call-ID per PROTOCOL.md §1) the caller has chosen. `seq` is
/// always 0 here — the bridge connection task overwrites it with 0
/// anyway, but we keep the field truthful.
pub fn build_start_msg(
    bridge_call_id: BridgeCallId,
    facts: &InviteFacts,
    sip_call_id: &str,
    answer: &AnswerOutcome,
    forward_headers: &[String],
) -> StartMsg {
    let mut headers = std::collections::HashMap::with_capacity(forward_headers.len());
    for name in forward_headers {
        if let Some(value) = facts.headers.get(&name.to_ascii_lowercase()) {
            headers.insert(canonical_header_name(name), value.to_string());
        }
    }

    StartMsg {
        version: PROTOCOL_VERSION.to_string(),
        call_id: bridge_call_id,
        seq: 0,
        from: facts.from_user.clone(),
        to: facts.request_uri_user.clone(),
        direction: Direction::Inbound,
        audio: AudioFormat {
            encoding: AudioEncoding::Pcm16le,
            sample_rate: answer.negotiated_audio_sample_rate,
            channels: 1,
            frame_ms: 20,
        },
        sip: SipMeta {
            call_id: sip_call_id.to_string(),
            headers,
        },
    }
}

/// Title-case a hyphen-separated SIP header name (`x-foo-bar` →
/// `X-Foo-Bar`). The bridge protocol doesn't care, but emitting
/// canonical names keeps WS server logs readable.
fn canonical_header_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut start_of_word = true;
    for ch in name.chars() {
        if start_of_word {
            out.extend(ch.to_uppercase());
        } else {
            out.extend(ch.to_lowercase());
        }
        start_of_word = ch == '-';
    }
    out
}

// ─── Async acceptor ─────────────────────────────────────────────────

/// Per-call ID generator hook — exposed so tests can pin it. Default
/// uses a v4 UUID prefixed with `siphon-`.
pub type CallIdFactory = Arc<dyn Fn() -> BridgeCallId + Send + Sync>;

fn default_call_id_factory() -> CallIdFactory {
    Arc::new(|| BridgeCallId::new(format!("siphon-{}", Uuid::new_v4().simple())))
}

/// `CallAcceptor` impl that drives every step from "matched route"
/// to "running [`CallController`]". Constructed once at daemon
/// startup; cheap to clone (everything inside is `Arc` or `Clone`).
///
/// `uas` is the [`IntegratedUAS`] the daemon already builds for the
/// dispatcher. The acceptor uses [`IntegratedUAS::accept_invite`] to
/// send the 200 OK so the confirmed dialog lands in the SAME dialog
/// manager that dispatch consults on the follow-up BYE/REFER/INFO.
/// Holding a parallel `UserAgentServer` here would silently produce
/// an independent `dialog_manager`, making BYE come back as
/// "Received BYE for unknown dialog" — see siphon-rs PR #35 for the
/// upstream fix that exposes the canonical `accept_invite` helper.
///
/// `registry` is the shared [`CallRegistry`] the SIP-side BYE /
/// CANCEL handlers consult. The acceptor inserts a [`CallHandle`]
/// keyed by the inbound INVITE's `Call-ID` on the happy path and
/// removes the entry from the spawned task when the controller
/// exits, so a follow-up BYE/CANCEL has someone to wake.
pub struct BridgingAcceptor {
    media: Arc<MediaSetup>,
    defaults: BridgeDefaults,
    /// Late-bound: the daemon builds `BridgingAcceptor` first (so the
    /// routing handler can hold an `Arc` to it), then builds the
    /// `IntegratedUAS` that owns the routing handler, then calls
    /// [`Self::install_uas`] to close the cycle. Tests that don't
    /// drive `on_matched` (the only consumer) can leave it unset.
    uas: OnceLock<Arc<IntegratedUAS>>,
    /// Late-bound too — same reason as `uas`. The daemon-wide
    /// `IntegratedUAC` is built once after the UAS so it can share
    /// the UAS's [`DialogManager`]. Without it, `BridgeIn::Transfer`
    /// returns `TransferFailed` to the WS server.
    transfer: OnceLock<InstalledTransfer>,
    registry: CallRegistry,
    cdr_sink: CdrSinkHandle,
    webhook_sink: WebhookSinkHandle,
    call_id_factory: CallIdFactory,
    /// RFC 4028 negotiation policy used on every inbound INVITE.
    /// Defaults to the upstream `SessionTimerPolicy::default()` (90 s
    /// floor, no preference, no force-refresher) until the daemon's
    /// `[sip]` config calls `with_session_timer_policy`.
    session_timer_policy: SessionTimerPolicy,
    /// Authoritative timer for every dialog we accepted with session
    /// timers. The fan-out task subscribed in `new()` reads its event
    /// stream and dispatches `SessionExpired` to the per-dialog
    /// handle in `dialog_handles` — turning a timer event into a
    /// controller shutdown that, via PR #19's path, sends an outbound
    /// BYE to the peer.
    session_timer_manager: Arc<SessionTimerManager>,
    /// `DialogId → CallHandle` map populated when a call is accepted
    /// with timers and drained when it ends. Read by the fan-out
    /// task; written by `on_matched` and `run_call`'s cleanup arm.
    dialog_handles: Arc<RwLock<HashMap<DialogId, crate::call::CallHandle>>>,
    /// What — if any — provisional response `on_matched` sends
    /// before the 2xx. Defaults to [`CallProgressMode::InstantAnswer`]
    /// (v0.1.0 behaviour); operators opt in to `Ringing` or
    /// `SessionProgress` via `[sip.call_progress]`.
    call_progress: CallProgressMode,
}

/// Daemon-wide REFER plumbing (shared across every accepted call).
struct InstalledTransfer {
    uac: Arc<IntegratedUAC>,
    dialog_manager: Arc<DialogManager>,
}

/// Pair handed to [`BridgingAcceptor::run_call`] when a call was
/// accepted via the session-timer-aware path. `dialog` keys the
/// session-timer registry; `timer` is `Some` iff RFC 4028
/// negotiated successfully on this INVITE. Tests that drive
/// `run_call` directly (without the full acceptor flow) pass
/// `None` for the outer Option and skip session-timer wiring.
pub struct AcceptedSession {
    pub dialog: sip_dialog::Dialog,
    pub timer: Option<NegotiatedSessionTimer>,
}

/// Carried into the post-controller cleanup task. Same Arc'd UAC +
/// DialogManager as REFER uses; we send the closing BYE through this
/// when a call ends without the peer having sent BYE first. `None`
/// when `install_transfer` was never called — see `run_call`.
struct TeardownContext {
    uac: Arc<IntegratedUAC>,
    dialog_manager: Arc<DialogManager>,
}

/// Send an outbound BYE on the confirmed dialog for `sip_call_id`,
/// if we have the plumbing for it. Logs the outcome and returns —
/// the cleanup task can't recover from a BYE failure, so this is
/// best-effort. `bridge_call_id` is only used for log correlation.
async fn send_outbound_bye(
    teardown: Option<&TeardownContext>,
    sip_call_id: &str,
    bridge_call_id: &str,
) {
    let Some(ctx) = teardown else {
        warn!(
            call_id = %bridge_call_id,
            sip_call_id = %sip_call_id,
            "controller exited without remote BYE but no UAC is installed; \
             SIP dialog will linger until session-expires"
        );
        return;
    };
    let Some(dialog) = ctx.dialog_manager.find_by_call_id(sip_call_id) else {
        debug!(
            call_id = %bridge_call_id,
            sip_call_id = %sip_call_id,
            "no dialog to BYE — already torn down at the dialog layer"
        );
        return;
    };
    match ctx.uac.bye(&dialog).await {
        Ok(resp) => debug!(
            call_id = %bridge_call_id,
            sip_call_id = %sip_call_id,
            status = resp.code(),
            "outbound BYE sent"
        ),
        Err(e) => warn!(
            call_id = %bridge_call_id,
            sip_call_id = %sip_call_id,
            error = %e,
            "outbound BYE failed; SIP dialog may linger"
        ),
    }
}

impl BridgingAcceptor {
    pub fn new(media: Arc<MediaSetup>, defaults: BridgeDefaults, registry: CallRegistry) -> Self {
        let session_timer_manager = Arc::new(SessionTimerManager::new());
        let dialog_handles: Arc<RwLock<HashMap<DialogId, crate::call::CallHandle>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Fan-out: drain the manager's event stream and dispatch
        // `SessionExpired` to the per-dialog handle. The subscribe()
        // call is one-shot upstream (last subscriber wins), so doing
        // it here at construction guarantees nobody else can race us
        // for the receiver. Spawning in sync code is fine because
        // every callsite (daemon main, integration tests) runs inside
        // a tokio runtime — if a caller forgets to set one up the
        // panic surfaces immediately.
        let mgr_for_fanout = Arc::clone(&session_timer_manager);
        let map_for_fanout = Arc::clone(&dialog_handles);
        tokio::spawn(async move {
            let mut events = mgr_for_fanout.subscribe().await;
            while let Some(ev) = events.recv().await {
                match ev {
                    SessionTimerEvent::SessionExpired(dialog_id) => {
                        let handle = map_for_fanout.read().get(&dialog_id).cloned();
                        if let Some(handle) = handle {
                            info!(
                                sip_call_id = %dialog_id.call_id(),
                                "RFC 4028 session expired; tearing down call"
                            );
                            handle.shutdown();
                        } else {
                            debug!(
                                sip_call_id = %dialog_id.call_id(),
                                "session expired for unknown dialog — already torn down"
                            );
                        }
                    }
                    SessionTimerEvent::RefreshNeeded(dialog_id) => {
                        // We default to `refresher=uac`, so this only
                        // fires when an operator forced the UAS to be
                        // the refresher AND the half-deadline elapsed
                        // without a refresh re-INVITE going out. v1
                        // doesn't initiate refreshes, so log and let
                        // the same dialog's `SessionExpired` fire at
                        // the full deadline.
                        warn!(
                            sip_call_id = %dialog_id.call_id(),
                            "RFC 4028 refresh due but UAS-initiated refresh \
                             is not implemented in v1 — call will tear down \
                             at session-expires"
                        );
                    }
                }
            }
        });

        Self {
            media,
            defaults,
            uas: OnceLock::new(),
            transfer: OnceLock::new(),
            registry,
            cdr_sink: Arc::new(NullSink),
            webhook_sink: Arc::new(WebhookNullSink),
            call_id_factory: default_call_id_factory(),
            session_timer_policy: SessionTimerPolicy::default(),
            session_timer_manager,
            dialog_handles,
            call_progress: CallProgressMode::default(),
        }
    }

    /// Override the call-progress mode used by [`Self::on_matched`]
    /// when responding to inbound INVITEs. Defaults to
    /// [`CallProgressMode::InstantAnswer`] (the v0.1.0 behaviour).
    pub fn with_call_progress(mut self, mode: CallProgressMode) -> Self {
        self.call_progress = mode;
        self
    }

    /// Install the [`IntegratedUAS`] handle the acceptor uses to send
    /// the 2xx via [`IntegratedUAS::accept_invite`]. Must be called
    /// once after both this acceptor and its enclosing `IntegratedUAS`
    /// are built; calling twice panics.
    pub fn install_uas(&self, uas: Arc<IntegratedUAS>) {
        self.uas
            .set(uas)
            .map_err(|_| ())
            .expect("install_uas called twice on BridgingAcceptor");
    }

    /// Install the daemon-wide REFER plumbing. `uac` issues the
    /// REFER; `dialog_manager` MUST be the same instance the UAS
    /// dispatches against (`IntegratedUAS::dialog_manager()`), or
    /// the controller's per-call dialog lookup will miss.
    ///
    /// Optional: callers that don't enable transfer never call this
    /// and `BridgeIn::Transfer` is rejected at the controller with
    /// `TransferFailed`. Calling twice panics — there is exactly one
    /// transfer UAC per daemon.
    pub fn install_transfer(&self, uac: Arc<IntegratedUAC>, dialog_manager: Arc<DialogManager>) {
        self.transfer
            .set(InstalledTransfer {
                uac,
                dialog_manager,
            })
            .map_err(|_| ())
            .expect("install_transfer called twice on BridgingAcceptor");
    }

    /// Override the bridge call-id factory. Useful in tests where
    /// you want a deterministic id; production should keep the
    /// default.
    pub fn with_call_id_factory(mut self, factory: CallIdFactory) -> Self {
        self.call_id_factory = factory;
        self
    }

    /// Plug in a CDR sink. Defaults to a no-op when not set; the
    /// daemon binary swaps in a file or webhook sink based on
    /// `[cdr]` config.
    pub fn with_cdr_sink(mut self, sink: CdrSinkHandle) -> Self {
        self.cdr_sink = sink;
        self
    }

    /// Plug in a lifecycle webhook sink (call_start / call_end
    /// events). Defaults to a no-op; the daemon binary swaps in
    /// an HTTP sink based on `[webhooks]` config.
    pub fn with_webhook_sink(mut self, sink: WebhookSinkHandle) -> Self {
        self.webhook_sink = sink;
        self
    }

    /// Replace the RFC 4028 negotiation policy. The daemon builds
    /// one from `[sip].min_session_expires_secs` and
    /// `[sip].preferred_session_expires_secs` at startup; tests
    /// override it for focused coverage.
    pub fn with_session_timer_policy(mut self, policy: SessionTimerPolicy) -> Self {
        self.session_timer_policy = policy;
        self
    }

    /// The registry this acceptor populates. Cheap to clone — share
    /// it with the SIP-side BYE/CANCEL handler.
    pub fn registry(&self) -> &CallRegistry {
        &self.registry
    }
}

/// Snapshot of "everything we know at call-start that we'll need at
/// CDR-emission time". Built inside the spawned task so the
/// controller's exit handler doesn't have to re-derive it.
#[derive(Debug, Clone)]
struct CallStart {
    bridge_call_id: BridgeCallId,
    sip_call_id: String,
    started_at: DateTime<Utc>,
    from: String,
    to: String,
    route: String,
    ws_url: String,
    audio: CdrAudioInfo,
}

impl CallStart {
    fn into_record(self, ended_at: DateTime<Utc>, outcome: &CallTerminationView) -> CdrRecord {
        let duration_ms = (ended_at - self.started_at).num_milliseconds().max(0) as u64;
        CdrRecord {
            version: CDR_VERSION,
            call_id: self.bridge_call_id.as_str().to_string(),
            sip_call_id: self.sip_call_id,
            started_at: self.started_at,
            ended_at,
            duration_ms,
            from: self.from,
            to: self.to,
            direction: CdrDirection::Inbound,
            route: self.route,
            ws_url: self.ws_url,
            audio: self.audio,
            termination: CdrTerminationInfo {
                cause: outcome.cause,
                bridge_disconnect: outcome.bridge_detail.clone(),
                tap_disconnect: outcome.tap_detail.clone(),
            },
        }
    }
}

/// Flat view of `Result<CallOutcome, CallError>` for the CDR layer:
/// just the cause + the human strings from the sub-task results.
struct CallTerminationView {
    cause: CdrTerminationCause,
    bridge_detail: String,
    tap_detail: String,
}

impl CallTerminationView {
    fn from_run_result(result: Result<CallOutcome, crate::call::CallError>) -> Self {
        match result {
            Ok(o) => Self {
                cause: map_cause(o.termination),
                bridge_detail: bridge_detail(o.bridge),
                tap_detail: tap_detail(o.tap),
            },
            Err(e) => Self {
                // Treat a panic / join error as "bridge ended" —
                // the call did end, and the cause string surfaces
                // the underlying error for diagnostics.
                cause: CdrTerminationCause::BridgeEnded,
                bridge_detail: format!("controller error: {e}"),
                tap_detail: String::new(),
            },
        }
    }
}

/// Record `siphon_ai_sdp_negotiate_seconds` when prepare_call exits.
/// "Prepare" is the umbrella for SDP negotiate + forge port alloc +
/// tap attach — all happening inside `MediaSetup::accept_inbound`,
/// which is what operators actually want to time.
fn record_prepare_outcome(elapsed: std::time::Duration, ok: bool) {
    let result = if ok { "ok" } else { "error" };
    metrics::histogram!(SDP_NEGOTIATE_SECONDS, "result" => result).record(elapsed.as_secs_f64());
}

/// Map a CDR termination cause to a stable wire string for the
/// `siphon_ai_calls_total` counter label. Mirrors
/// [`CdrTerminationCause`]'s snake_case serialization so dashboards
/// can correlate the two without re-mapping.
fn termination_label(cause: CdrTerminationCause) -> &'static str {
    match cause {
        CdrTerminationCause::ServerHangup => "server_hangup",
        CdrTerminationCause::LocalShutdown => "local_shutdown",
        CdrTerminationCause::BridgeEnded => "bridge_ended",
        CdrTerminationCause::TapEnded => "tap_ended",
    }
}

fn map_cause(t: CallTermination) -> CdrTerminationCause {
    match t {
        CallTermination::ServerHangup => CdrTerminationCause::ServerHangup,
        CallTermination::LocalShutdown => CdrTerminationCause::LocalShutdown,
        CallTermination::BridgeEnded => CdrTerminationCause::BridgeEnded,
        CallTermination::TapEnded => CdrTerminationCause::TapEnded,
    }
}

fn bridge_detail(res: Option<Result<DisconnectReason, siphon_ai_bridge::BridgeError>>) -> String {
    match res {
        None => String::new(),
        Some(Ok(reason)) => match reason {
            DisconnectReason::StopSent => "stop_sent".into(),
            DisconnectReason::ServerClosed => "server_closed".into(),
            DisconnectReason::ControllerHungUp => "controller_hung_up".into(),
        },
        Some(Err(e)) => format!("error: {e}"),
    }
}

fn tap_detail(res: Option<Result<TapDisconnect, MediaTapError>>) -> String {
    match res {
        None => String::new(),
        Some(Ok(TapDisconnect::CallEnded)) => "call_ended".into(),
        Some(Ok(TapDisconnect::ControllerHungUp)) => "controller_hung_up".into(),
        Some(Ok(TapDisconnect::InactivityTimeout)) => "inactivity_timeout".into(),
        Some(Err(e)) => format!("error: {e}"),
    }
}

/// Read the audio media's `a=` direction from a parsed offer.
/// Returns `None` for offers without an audio media or with no
/// explicit direction attribute (the caller maps that to the RFC
/// 4566 §6 default of sendrecv).
fn sdp_audio_direction(
    session: &forge_sdp::SessionDescription,
) -> Option<siphon_ai_media_glue::MediaDirection> {
    use forge_sdp::MediaType;
    let audio = session.find_media(MediaType::Audio)?;
    audio
        .direction()
        .as_ref()
        .map(|d| d.as_token())
        .and_then(siphon_ai_media_glue::MediaDirection::from_attr)
}

/// Audio payload-type list from a parsed SDP. Used by re-INVITE
/// handling to confirm the peer's new offer still proposes a codec
/// we accepted on the initial INVITE. Empty when there's no audio
/// media.
fn sdp_audio_payload_types(session: &forge_sdp::SessionDescription) -> Vec<String> {
    use forge_sdp::MediaType;
    session
        .find_media(MediaType::Audio)
        .map(|m| m.formats.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default()
}

/// Resolve the peer's RTP endpoint from an offer SDP. Media-level
/// `c=` overrides the session-level `c=` per RFC 4566 §5.7. Returns
/// `None` when neither carries a valid IP address or when audio is
/// absent. Used by `on_reinvite` to push a `remote_addr` update to
/// forge when the peer changes its RTP endpoint mid-call. The initial
/// INVITE path uses the same helper through `media-glue`, applied at
/// `accept_inbound` time so forge has the address before the answer
/// goes out.
fn sdp_audio_remote_addr(session: &forge_sdp::SessionDescription) -> Option<std::net::SocketAddr> {
    siphon_ai_media_glue::audio_remote_addr(session)
}

/// Rejection signal returned by [`prepare_reinvite_answer`] when
/// the re-INVITE offer can't be safely answered with the cached
/// SDP — the caller sends the carried response and exits.
#[derive(Debug)]
struct ReinviteRejection {
    code: u16,
    reason: &'static str,
}

/// Inputs to the canonical accept_invite_with_session_timer call
/// for a re-INVITE. Built by [`prepare_reinvite_answer`].
struct ReinviteAnswer {
    /// SDP body to put in the 2xx (a direction-flipped version of
    /// the cached initial answer, or the cached answer verbatim on
    /// a body-less re-INVITE).
    answer_sdp: String,
    /// `None` for body-less re-INVITEs. Only used for the closing
    /// debug log; otherwise the new SDP carries the same media as
    /// the cached one by construction.
    offer_direction: Option<siphon_ai_media_glue::MediaDirection>,
    answer_direction: Option<siphon_ai_media_glue::MediaDirection>,
    /// Peer's audio RTP endpoint advertised in the offer's media
    /// `c=` / `m=` lines. `None` for body-less re-INVITEs. The
    /// acceptor pushes this to forge so RTP follows the peer to
    /// the new address instead of relying on symmetric-RTP
    /// latching.
    remote_addr: Option<std::net::SocketAddr>,
}

/// Validate an inbound re-INVITE against the cached initial answer
/// and produce the SDP for the 200 OK. Rejects (instead of producing
/// a 200 with stale SDP) when:
///
/// - The body is present but malformed → 400 Bad Request.
/// - The offer parses but lists payload types we never accepted →
///   488 Not Acceptable Here (mid-call codec change is post-v1).
///
/// A body-less re-INVITE is treated as a session-timer refresh per
/// RFC 3261 §14.2 — the cached answer stands.
fn prepare_reinvite_answer(
    request: &Request,
    prev_answer: &str,
    sip_call_id: &str,
) -> Result<ReinviteAnswer, ReinviteRejection> {
    let offer_text = match extract_offer_sdp(request) {
        Ok(t) => t,
        Err(OfferError::NoBody) => {
            // RFC 3261 §14.2: body-less re-INVITE refreshes the
            // session without renegotiating media. Echo the cached
            // answer so the peer keeps the same media path.
            debug!(
                sip_call_id = %sip_call_id,
                "re-INVITE has no SDP body — treating as session-timer refresh"
            );
            return Ok(ReinviteAnswer {
                answer_sdp: prev_answer.to_string(),
                offer_direction: None,
                answer_direction: None,
                remote_addr: None,
            });
        }
        Err(e) => {
            warn!(
                sip_call_id = %sip_call_id,
                error = %e,
                "re-INVITE rejected 400: malformed body"
            );
            return Err(ReinviteRejection {
                code: 400,
                reason: "Bad Request",
            });
        }
    };

    let offer_session = siphon_ai_media_glue::parse_offer(offer_text).map_err(|e| {
        warn!(
            sip_call_id = %sip_call_id,
            error = %e,
            "re-INVITE rejected 488: offer parse failed"
        );
        ReinviteRejection {
            code: 488,
            reason: "Not Acceptable Here",
        }
    })?;

    // The previously-sent answer parses through the same routine
    // since it's a valid SDP we generated. Failure here would be
    // a daemon bug.
    let cached_session = siphon_ai_media_glue::parse_offer(prev_answer).map_err(|e| {
        warn!(
            sip_call_id = %sip_call_id,
            error = %e,
            "cached answer failed to re-parse — rejecting 500"
        );
        ReinviteRejection {
            code: 500,
            reason: "Server Internal Error",
        }
    })?;

    let offer_pts = sdp_audio_payload_types(&offer_session);
    let cached_pts = sdp_audio_payload_types(&cached_session);
    let has_common_pt =
        !cached_pts.is_empty() && offer_pts.iter().any(|pt| cached_pts.contains(pt));
    if !has_common_pt {
        warn!(
            sip_call_id = %sip_call_id,
            offer_pts = ?offer_pts,
            cached_pts = ?cached_pts,
            "re-INVITE rejected 488: payload types diverge from accepted call \
             (mid-call codec change unsupported in v1)"
        );
        return Err(ReinviteRejection {
            code: 488,
            reason: "Not Acceptable Here",
        });
    }

    // RFC 3264 §6.1 direction mirror.
    let offer_direction = sdp_audio_direction(&offer_session).unwrap_or_default();
    let answer_direction = mirror_direction(offer_direction);
    let answer_sdp = rewrite_sdp_direction(prev_answer, answer_direction);
    let remote_addr = sdp_audio_remote_addr(&offer_session);

    Ok(ReinviteAnswer {
        answer_sdp,
        offer_direction: Some(offer_direction),
        answer_direction: Some(answer_direction),
        remote_addr,
    })
}

/// RFC 3264 §6.1 direction mirror. Hold/resume re-INVITE answering
/// depends on this — see `BridgingAcceptor::on_reinvite`.
fn mirror_direction(
    offer: siphon_ai_media_glue::MediaDirection,
) -> siphon_ai_media_glue::MediaDirection {
    use siphon_ai_media_glue::MediaDirection::*;
    match offer {
        SendRecv => SendRecv,
        SendOnly => RecvOnly,
        RecvOnly => SendOnly,
        Inactive => Inactive,
    }
}

/// Replace the `a=sendrecv|sendonly|recvonly|inactive` line in an
/// SDP body with the requested direction. Linear scan over lines.
/// The first match wins and no other lines are touched — re-using
/// the original port / codec / rtpmap / fmtp values verbatim.
///
/// If no direction line exists in the input, one is appended after
/// the audio media's `m=` line. This is rare in practice (the
/// initial answer we cache always emits `a=sendrecv`) but keeps
/// the helper total.
fn rewrite_sdp_direction(sdp: &str, new_dir: siphon_ai_media_glue::MediaDirection) -> String {
    let mut out = String::with_capacity(sdp.len());
    let mut replaced = false;
    for line in sdp.split_inclusive('\n') {
        let trimmed = line.trim_end();
        let is_direction = matches!(
            trimmed,
            "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive"
        );
        if is_direction && !replaced {
            // Preserve CRLF vs LF: take the trailing newline bytes
            // off the original line and re-attach them.
            let nl = &line[trimmed.len()..];
            out.push_str("a=");
            out.push_str(new_dir.as_attr());
            out.push_str(nl);
            replaced = true;
        } else {
            out.push_str(line);
        }
    }
    if !replaced {
        // Append. Caller's responsibility to ensure the audio
        // media section is the last thing — true for our cached
        // answers (built by `build_answer`).
        out.push_str("a=");
        out.push_str(new_dir.as_attr());
        out.push_str("\r\n");
    }
    out
}

#[async_trait]
impl CallAcceptor for BridgingAcceptor {
    #[instrument(skip(self, call), fields(sip_call_id = %call.sip_call_id))]
    async fn on_reinvite(&self, call: siphon_ai_sip_glue::ReinviteCall<'_>) -> anyhow::Result<()> {
        // Look up the call's cached answer SDP. Without it we have
        // no record of the original codec / port / direction and
        // can't safely build a re-INVITE answer; surface that as
        // 481 (the dialog effectively isn't ours).
        let Some(entry) = self.registry.entry(&call.sip_call_id) else {
            let response = UserAgentServer::create_response(
                call.request,
                481,
                "Call/Transaction Does Not Exist",
            );
            call.handle.send_final(response).await;
            return Ok(());
        };
        let Some(prev_answer) = entry.answer_text else {
            // Legacy path: call was registered without an answer
            // cache (currently only happens in tests). Refuse
            // cleanly with 501.
            let response = UserAgentServer::create_response(call.request, 501, "Not Implemented");
            call.handle.send_final(response).await;
            return Ok(());
        };

        // Strict offer validation. The cached `prev_answer` only
        // carries direction; mirroring the offer's `a=` line is
        // safe ONLY when the offer still proposes the same codec /
        // PT we negotiated on the initial INVITE. v1 doesn't
        // implement mid-call codec / port renegotiation, so anything
        // beyond a direction change gets a clean 488 instead of a
        // 200 OK with an SDP that doesn't correspond to the offer.
        //
        // RFC 3261 §14.2 permits a body-less re-INVITE as a session-
        // timer refresh; the previous answer stands unchanged on
        // that path.
        let reinvite = match prepare_reinvite_answer(call.request, &prev_answer, &call.sip_call_id)
        {
            Ok(r) => r,
            Err(ReinviteRejection { code, reason }) => {
                let response = UserAgentServer::create_response(call.request, code, reason);
                call.handle.send_final(response).await;
                return Ok(());
            }
        };
        let new_answer = reinvite.answer_sdp;
        let offer_direction = reinvite.offer_direction;
        let answer_direction = reinvite.answer_direction;
        let new_remote_addr = reinvite.remote_addr;

        // Send 200 OK via the canonical helper so the dialog
        // manager gets the updated CSeq / route-set.
        let uas = self.uas.get().ok_or_else(|| {
            anyhow::anyhow!(
                "BridgingAcceptor::install_uas was not called; on_reinvite cannot accept INVITE"
            )
        })?;
        let outcome = uas
            .accept_invite_with_session_timer(
                call.request,
                &call.handle,
                call.transport,
                Some(&new_answer),
                &self.session_timer_policy,
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to accept re-INVITE: {e}"))?;

        // If the refresh re-INVITE carried Session-Expires (whether
        // hold/resume or a pure timer refresh), reset the per-dialog
        // timer to the freshly-negotiated value. A 422-too-small
        // outcome here means the peer's refresh asks for a shorter
        // session than our Min-SE; the helper already sent the 422,
        // and the original timer keeps running — the peer is
        // expected to retry with a larger value.
        match outcome {
            AcceptInviteAsyncOutcome::Accepted {
                dialog,
                session_timer: Some(timer),
            } => {
                self.session_timer_manager.refresh_timer(
                    dialog.id().clone(),
                    timer.session_expires,
                    matches!(timer.refresher, sip_core::RefresherRole::Uas),
                );
                debug!(
                    sip_call_id = %call.sip_call_id,
                    session_expires_secs = timer.session_expires.as_secs(),
                    "re-INVITE refreshed RFC 4028 timer"
                );
            }
            AcceptInviteAsyncOutcome::Accepted { .. } => { /* no timer renegotiated */ }
            AcceptInviteAsyncOutcome::SessionIntervalTooSmall { required_min_se } => {
                warn!(
                    sip_call_id = %call.sip_call_id,
                    required_min_se_secs = required_min_se.as_secs(),
                    "re-INVITE rejected 422: refresh Session-Expires below Min-SE"
                );
                // Keep the existing dialog + timer running; the peer
                // can resend with a larger value.
                return Ok(());
            }
        }

        // Push the peer's new RTP endpoint to forge if the offer
        // advertised one. Forge would otherwise fall back to
        // symmetric-RTP latching — adequate when the peer keeps
        // sending from the latched address, but brittle when the
        // peer switches port (a soft-phone reconfigure, NAT pinhole
        // shift) and pauses sending. The explicit update closes
        // that gap. Best-effort: a session that just ended races
        // with the cleanup task, so we log + ignore on miss.
        if let (Some(addr), Some(forge_call_id)) = (new_remote_addr, entry.forge_call_id.as_ref()) {
            if let Some(session) = self.media.session_manager().get_session(forge_call_id) {
                let update = forge_engine::ParticipantMediaUpdate {
                    remote_addr: Some(Some(addr)),
                    ..Default::default()
                };
                match session
                    .update_participant_media(forge_engine::ParticipantLabel::A, update)
                    .await
                {
                    Ok(_) => debug!(
                        sip_call_id = %call.sip_call_id,
                        remote_addr = %addr,
                        "pushed peer RTP endpoint to forge"
                    ),
                    Err(e) => warn!(
                        sip_call_id = %call.sip_call_id,
                        error = %e,
                        "failed to push peer RTP endpoint to forge; \
                         relying on symmetric-RTP latching"
                    ),
                }
            }
        }

        // Hold / Resume emission. The cached `entry.current_direction`
        // started at SendRecv on the initial INVITE; on each accepted
        // re-INVITE we update it and emit a transition event when
        // we cross the SendRecv ↔ non-SendRecv boundary. Body-less
        // re-INVITEs (session-timer refresh) don't carry a direction
        // and don't generate a transition.
        if let Some(new_direction) = offer_direction {
            let mut current = entry.current_direction.write();
            let prev = *current;
            *current = new_direction;
            drop(current);
            let was_held = prev.is_held();
            let now_held = new_direction.is_held();
            match (was_held, now_held) {
                (false, true) => {
                    debug!(
                        sip_call_id = %call.sip_call_id,
                        direction = new_direction.as_attr(),
                        "emitting Hold on WS bridge"
                    );
                    entry.handle.push_bridge_event(OutgoingEvent::Hold {
                        direction: new_direction.as_attr().to_string(),
                    });
                }
                (true, false) => {
                    debug!(
                        sip_call_id = %call.sip_call_id,
                        "emitting Resume on WS bridge"
                    );
                    entry.handle.push_bridge_event(OutgoingEvent::Resume);
                }
                _ => {
                    // No transition (held→held with different flavor,
                    // or sendrecv→sendrecv). No event needed.
                }
            }
        }

        debug!(
            sip_call_id = %call.sip_call_id,
            offer = ?offer_direction,
            answer = ?answer_direction,
            "re-INVITE answered"
        );
        Ok(())
    }

    #[instrument(skip(self, call), fields(
        route = %call.route.name,
        from = %call.facts.from_user,
        to = %call.facts.request_uri_user,
    ))]
    async fn on_matched(&self, call: MatchedCall<'_>) -> anyhow::Result<()> {
        match self
            .prepare_call(call.request, call.route, &call.facts)
            .await
        {
            Ok(prepared) => {
                let uas = self
                    .uas
                    .get()
                    .ok_or_else(|| anyhow::anyhow!(
                        "BridgingAcceptor::install_uas was not called; on_matched cannot accept INVITE without the IntegratedUAS handle"
                    ))?;

                // Start forge's RTP forwarding loop BEFORE the 200 OK.
                // Decoding inbound audio, the RFC-2833 detector, and
                // ForgeEvent::DtmfDigitDetected all depend on this
                // step running. If it fails AFTER we accept the
                // INVITE we have a confirmed SIP dialog with no media
                // — RFC-3261's worst silent failure mode. Doing it
                // first means a failure rejects the INVITE with 500
                // and rolls back the forge session, no zombie call.
                // forge's state machine requires Initializing →
                // Active exactly once; we own that single call here.
                if let Err(e) = self
                    .media
                    .session_manager()
                    .start_session(&prepared.forge_call_id)
                    .await
                {
                    warn!(
                        call_id = %prepared.bridge_call_id,
                        error = %e,
                        "forge start_session failed; rejecting INVITE 500",
                    );
                    self.rollback_forge_session(
                        &prepared.bridge_call_id,
                        &prepared.forge_call_id,
                        "start_session",
                    )
                    .await;
                    let response = UserAgentServer::create_response(
                        call.request,
                        500,
                        "Server Internal Error",
                    );
                    call.handle.send_final(response).await;
                    metrics::counter!(INVITES_TOTAL, "result" => "rejected").increment(1);
                    return Ok(());
                }

                // ─── Configurable call progress (§4 / §9.1) ──────
                // Send the operator-selected provisional response —
                // 180 Ringing or 183 Session Progress with the
                // negotiated answer SDP — between the forge-active
                // step above and the 2xx below. `InstantAnswer`
                // (default, v0.1.0 behaviour) skips this entirely
                // and lets `accept_invite_with_session_timer` send
                // the 2xx straight after `100 Trying`.
                match self.call_progress {
                    CallProgressMode::InstantAnswer => {}
                    CallProgressMode::Ringing => {
                        let r180 = UserAgentServer::create_response(call.request, 180, "Ringing");
                        call.handle.send_provisional(r180).await;
                    }
                    CallProgressMode::SessionProgress => {
                        // Flavour B (§9.1): best-effort 183 with the
                        // negotiated answer SDP. A peer that requires
                        // 100rel needs reliable provisionals, which
                        // siphon-ai 0.2.0 does not ship — sending an
                        // unreliable 183 to such a peer is
                        // non-compliant. Fall through to InstantAnswer
                        // with a warn so the call still completes.
                        if requires_100rel(call.request) {
                            warn!(
                                call_id = %prepared.bridge_call_id,
                                "INVITE has `Require: 100rel` but reliable \
                                 provisionals are not supported yet \
                                 (deferred to 0.2.1 / 0.3.0); falling back \
                                 to instant_answer for this call"
                            );
                        } else {
                            let r183 = attach_sdp_body(
                                UserAgentServer::create_response(
                                    call.request,
                                    183,
                                    "Session Progress",
                                ),
                                &prepared.answer.answer_text,
                            );
                            call.handle.send_provisional(r183).await;
                        }
                    }
                }

                // 200 OK with the negotiated SDP via the canonical
                // session-timer-aware upstream helper (siphon-rs
                // PR #40). This builds the response, parses any
                // `Session-Expires` header for RFC 4028 negotiation,
                // appends `Session-Expires` + `Require: timer` to
                // the 2xx when timers are in play, auto-fills
                // Via/Contact/User-Agent, sends through the
                // transaction handle, AND registers the confirmed
                // dialog with the dialog manager that `dispatch`
                // consults.
                let accept_outcome = match uas
                    .accept_invite_with_session_timer(
                        call.request,
                        &call.handle,
                        call.transport,
                        Some(&prepared.answer.answer_text),
                        &self.session_timer_policy,
                    )
                    .await
                {
                    Ok(o) => o,
                    Err(e) => {
                        // The 200 OK never reached the peer — roll back
                        // the forge session we just activated so the
                        // RTP port doesn't leak.
                        self.rollback_forge_session(
                            &prepared.bridge_call_id,
                            &prepared.forge_call_id,
                            "accept_invite",
                        )
                        .await;
                        return Err(anyhow::anyhow!("failed to accept INVITE: {e}"));
                    }
                };

                let (dialog, session_timer) = match accept_outcome {
                    AcceptInviteAsyncOutcome::Accepted {
                        dialog,
                        session_timer,
                    } => (dialog, session_timer),
                    AcceptInviteAsyncOutcome::SessionIntervalTooSmall { required_min_se } => {
                        // The helper already sent the 422; we just need
                        // to release the forge session that prepare_call
                        // / start_session set up. No dialog, no call.
                        warn!(
                            call_id = %prepared.bridge_call_id,
                            required_min_se_secs = required_min_se.as_secs(),
                            "rejecting INVITE 422: Session-Expires below configured Min-SE"
                        );
                        self.rollback_forge_session(
                            &prepared.bridge_call_id,
                            &prepared.forge_call_id,
                            "session_interval_too_small",
                        )
                        .await;
                        metrics::counter!(INVITES_TOTAL, "result" => "rejected").increment(1);
                        return Ok(());
                    }
                };

                if let Some(ref timer) = session_timer {
                    debug!(
                        call_id = %prepared.bridge_call_id,
                        sip_call_id = %dialog.id().call_id(),
                        session_expires_secs = timer.session_expires.as_secs(),
                        refresher = ?timer.refresher,
                        "RFC 4028 session timer negotiated"
                    );
                }

                metrics::counter!(INVITES_TOTAL, "result" => "accepted").increment(1);
                self.run_call(
                    prepared,
                    call.route.name.as_str(),
                    Some(AcceptedSession {
                        dialog,
                        timer: session_timer,
                    }),
                );
                Ok(())
            }
            Err(e) => {
                let (code, reason) = e.sip_status();
                warn!(error = %e, code, reason, "rejecting INVITE");
                metrics::counter!(INVITES_TOTAL, "result" => "rejected").increment(1);
                let response = UserAgentServer::create_response(call.request, code, reason);
                call.handle.send_final(response).await;
                // The acceptor's contract with the routing layer is
                // "MUST send a final response" — we did, so this is
                // not an error from the trait's perspective.
                Ok(())
            }
        }
    }
}

impl BridgingAcceptor {
    /// Drive a [`PreparedCall`] to completion on a spawned task.
    ///
    /// Registers the handle in the [`CallRegistry`] so an inbound
    /// BYE can wake it, runs the controller, deregisters on exit,
    /// stops the forge session, and emits the CDR. Returns the
    /// `JoinHandle` of the spawned task — production callers
    /// (`on_matched`) drop it; tests `await` it.
    /// Best-effort `stop_session` cleanup. Called from every error
    /// path between `prepare_call` (which allocates the forge
    /// session) and a successful controller spawn — start_session
    /// failure, accept_invite failure, and 422-too-small. Logs a
    /// warning if `stop_session` itself errors and otherwise stays
    /// quiet.
    async fn rollback_forge_session(
        &self,
        bridge_call_id: &BridgeCallId,
        forge_call_id: &forge_core::CallId,
        reason: &'static str,
    ) {
        if let Err(stop_err) = self
            .media
            .session_manager()
            .stop_session(forge_call_id)
            .await
        {
            warn!(
                call_id = %bridge_call_id,
                error = %stop_err,
                reason,
                "stop_session after {reason} failure also failed"
            );
        }
    }

    pub fn run_call(
        &self,
        prepared: PreparedCall,
        route_name: &str,
        accepted: Option<AcceptedSession>,
    ) -> tokio::task::JoinHandle<()> {
        let call_start = CallStart {
            bridge_call_id: prepared.bridge_call_id.clone(),
            sip_call_id: prepared.sip_call_id.clone(),
            started_at: Utc::now(),
            from: prepared.start.from.clone(),
            to: prepared.start.to.clone(),
            route: route_name.to_string(),
            ws_url: prepared.bridge_config.ws_url.clone(),
            audio: CdrAudioInfo {
                codec: prepared.answer.negotiated_codec.encoding_name().to_string(),
                payload_type: prepared.answer.negotiated_payload_type,
                sample_rate: prepared.answer.negotiated_audio_sample_rate,
            },
        };

        // Clone the handle BEFORE moving it into the registry — the
        // cleanup task needs it to consult `remote_bye_received()`
        // so it knows whether to send an outbound BYE. `CallHandle`
        // is cheap (Arc-of-Notify + Arc-of-AtomicBool inside).
        let cleanup_handle = prepared.handle.clone();

        // Wire the RFC 4028 timer if negotiation produced one. The
        // fan-out task spawned in `new()` reads `SessionExpired`
        // events from the manager and looks the handle up in
        // `dialog_handles` to drive teardown. Stopping the timer is
        // the cleanup task's job below.
        let session_timer_key = match accepted.as_ref() {
            Some(AcceptedSession {
                dialog,
                timer: Some(timer),
            }) => {
                let id = dialog.id().clone();
                self.dialog_handles
                    .write()
                    .insert(id.clone(), cleanup_handle.clone());
                self.session_timer_manager.start_timer(
                    id.clone(),
                    timer.session_expires,
                    matches!(timer.refresher, sip_core::RefresherRole::Uas),
                );
                Some(id)
            }
            _ => None,
        };

        // Register before spawning so a BYE arriving on the very
        // next packet finds an entry to wake. Cache the answer SDP
        // we sent so a future re-INVITE (hold/resume) can rebuild a
        // matching answer with just the direction flipped.
        self.registry.insert(
            prepared.sip_call_id.clone(),
            crate::registry::CallEntry::new(
                prepared.handle,
                Some(prepared.answer.answer_text.clone()),
            )
            .with_forge_call_id(prepared.forge_call_id.clone()),
        );

        // Per-route counter is owned-by-route — bounded cardinality
        // by config (operators have tens of routes, not millions).
        metrics::counter!(ROUTE_MATCH_TOTAL, "route" => route_name.to_string()).increment(1);
        metrics::gauge!(CALLS_ACTIVE).increment(1.0);

        // Fire call_start before the controller spawn so an immediate
        // call_end (e.g. WS bridge connect failure) follows the
        // expected start→end ordering on the receiving end.
        let start_event = WebhookEvent::CallStart(CallStartEvent {
            version: WEBHOOK_VERSION,
            call_id: call_start.bridge_call_id.as_str().to_string(),
            sip_call_id: call_start.sip_call_id.clone(),
            timestamp: call_start.started_at,
            from: call_start.from.clone(),
            to: call_start.to.clone(),
            route: call_start.route.clone(),
            ws_url: call_start.ws_url.clone(),
        });
        let webhook_for_start = Arc::clone(&self.webhook_sink);
        tokio::spawn(async move {
            webhook_for_start.emit(start_event).await;
        });

        let bridge_call_id = prepared.bridge_call_id.clone();
        let forge_call_id = prepared.forge_call_id.clone();
        let sip_call_id = prepared.sip_call_id;
        let controller = prepared.controller;
        let media = Arc::clone(&self.media);
        let registry = self.registry.clone();
        let cdr_sink = Arc::clone(&self.cdr_sink);
        let webhook_sink = Arc::clone(&self.webhook_sink);
        // Daemon-wide UAC + DialogManager Arc clones so the cleanup
        // task can send an outbound BYE when teardown was driven
        // locally (WS `hangup`, admin force-hangup, bridge ended).
        // When `install_transfer` was never called these are `None`
        // and we log + skip the BYE — the SIP leg lingers until the
        // peer's own session-expires fires, which is the previous
        // behaviour, but at least the registry no longer claims the
        // call is dead while the dialog stays up.
        let teardown = self.transfer.get().map(|t| TeardownContext {
            uac: Arc::clone(&t.uac),
            dialog_manager: Arc::clone(&t.dialog_manager),
        });
        let session_timer_manager = Arc::clone(&self.session_timer_manager);
        let dialog_handles = Arc::clone(&self.dialog_handles);
        let cleanup_session_timer_key = session_timer_key;

        tokio::spawn(async move {
            let run_result = controller.run().await;
            let view = CallTerminationView::from_run_result(run_result);
            info!(
                call_id = %bridge_call_id,
                cause = ?view.cause,
                "call ended"
            );
            // Stop the RFC 4028 timer and drop the handle map entry
            // first — otherwise a `SessionExpired` racing the
            // controller exit would try to shutdown an already-gone
            // controller. Cheap when no timer was negotiated.
            if let Some(dialog_id) = cleanup_session_timer_key.as_ref() {
                session_timer_manager.stop_timer(dialog_id);
                dialog_handles.write().remove(dialog_id);
            }
            // Send outbound BYE if the peer didn't already drive it.
            // Order: BYE first, registry remove second, forge stop
            // third. That way a follow-up BYE retransmit from the
            // peer (which would be racing our outbound BYE in the
            // wild) still finds the entry and gets a 200 OK instead
            // of "unknown dialog".
            if !cleanup_handle.remote_bye_received() {
                send_outbound_bye(teardown.as_ref(), &sip_call_id, bridge_call_id.as_str()).await;
            }
            registry.remove(&sip_call_id);
            if let Err(e) = media.session_manager().stop_session(&forge_call_id).await {
                warn!(
                    call_id = %bridge_call_id,
                    error = %e,
                    "forge session teardown failed"
                );
            }

            let ended_at = Utc::now();
            let duration_ms = (ended_at - call_start.started_at).num_milliseconds().max(0) as u64;
            let duration_secs = duration_ms as f64 / 1000.0;
            metrics::gauge!(CALLS_ACTIVE).decrement(1.0);
            metrics::counter!(
                CALLS_TOTAL,
                "cause" => termination_label(view.cause),
            )
            .increment(1);
            metrics::histogram!(CALL_DURATION_SECONDS).record(duration_secs);

            let end_event = WebhookEvent::CallEnd(CallEndEvent {
                version: WEBHOOK_VERSION,
                call_id: call_start.bridge_call_id.as_str().to_string(),
                sip_call_id: call_start.sip_call_id.clone(),
                timestamp: ended_at,
                from: call_start.from.clone(),
                to: call_start.to.clone(),
                route: call_start.route.clone(),
                ws_url: call_start.ws_url.clone(),
                duration_ms,
                termination_cause: termination_label(view.cause).to_string(),
            });

            let record = call_start.into_record(ended_at, &view);
            cdr_sink.emit(record).await;
            webhook_sink.emit(end_event).await;
        })
    }
}

/// Output of [`BridgingAcceptor::prepare_call`] — the deterministic
/// preparation step before the SIP 200 OK and the controller spawn.
///
/// Exposed so integration tests can exercise the media + bridge wire-
/// up without needing to fabricate a [`sip_transaction::ServerTransactionHandle`].
///
/// `handle` is the controller's shutdown hook — the same one
/// `on_matched` registers in the [`CallRegistry`] before spawning
/// the task. Tests that drive `prepare_call` directly use it to
/// observe registry behaviour.
pub struct PreparedCall {
    pub bridge_call_id: BridgeCallId,
    pub forge_call_id: forge_core::CallId,
    pub sip_call_id: String,
    pub answer: AnswerOutcome,
    pub bridge_config: BridgeConfig,
    pub start: StartMsg,
    pub controller: CallController,
    pub handle: crate::call::CallHandle,
}

impl std::fmt::Debug for PreparedCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // CallController owns a `MediaTap` that wraps non-Debug forge
        // types; redact it instead of cascading the constraint.
        f.debug_struct("PreparedCall")
            .field("bridge_call_id", &self.bridge_call_id)
            .field("forge_call_id", &self.forge_call_id)
            .field("sip_call_id", &self.sip_call_id)
            .field("answer", &self.answer)
            .field("bridge_config", &self.bridge_config)
            .field("start", &self.start)
            .finish_non_exhaustive()
    }
}

impl BridgingAcceptor {
    /// Run every step from "matched route" up to "ready-to-run
    /// `CallController`," but stop short of sending the 200 OK or
    /// spawning the controller. The caller composes those steps —
    /// in production, [`CallAcceptor::on_matched`] does it; in tests,
    /// callers can inspect the [`PreparedCall`] directly.
    pub async fn prepare_call(
        &self,
        request: &Request,
        route: &CompiledRoute,
        facts: &InviteFacts,
    ) -> Result<PreparedCall, AcceptError> {
        let prepare_started = std::time::Instant::now();
        let result = self.prepare_call_inner(request, route, facts).await;
        record_prepare_outcome(prepare_started.elapsed(), result.is_ok());
        result
    }

    async fn prepare_call_inner(
        &self,
        request: &Request,
        route: &CompiledRoute,
        facts: &InviteFacts,
    ) -> Result<PreparedCall, AcceptError> {
        let offer_sdp = extract_offer_sdp(request)?;
        let sip_call_id = extract_sip_call_id(request);
        let bridge_config = build_bridge_config(&self.defaults, route)?;
        let codecs = resolve_codecs(&self.defaults, route);
        let dtmf_pt = resolve_dtmf_pt(&self.defaults, route);

        let bridge_call_id = (self.call_id_factory)();
        let forge_call_id = forge_core::CallId::new(bridge_call_id.as_str());

        debug!(
            ws_url = %bridge_config.ws_url,
            codec_count = codecs.len(),
            "media setup starting"
        );

        let InboundAccepted {
            answer,
            session: _session,
            tap,
        } = self
            .media
            .accept_inbound(InboundCall {
                call_id: forge_call_id.clone(),
                offer_sdp,
                codecs,
                dtmf_payload_type: dtmf_pt,
                participant_a: forge_core::ParticipantId::new(format!("sip-{}", forge_call_id.0)),
                participant_b: forge_core::ParticipantId::new(format!("ws-{}", forge_call_id.0)),
                from_tag: None,
                to_tag: None,
                barge_in_action: barge_in_to_tap_action(&resolve_barge_in(&self.defaults, route)),
                inactivity_timeout: resolve_inactivity_timeout(&self.defaults, route),
                silence_threshold: resolve_silence_threshold(&self.defaults, route),
                dead_air_threshold: resolve_dead_air_threshold(&self.defaults, route),
                rtp_stats_interval: resolve_rtp_stats_interval(&self.defaults, route),
            })
            .await?;

        let start = build_start_msg(
            bridge_call_id.clone(),
            facts,
            &sip_call_id,
            &answer,
            &self.defaults.forward_headers,
        );

        let transfer = self.transfer.get().map(|installed| TransferContext {
            sip_call_id: sip_call_id.clone(),
            uac: Arc::clone(&installed.uac),
            dialog_manager: Arc::clone(&installed.dialog_manager),
        });

        let cfg = CallControllerConfig {
            call_id: bridge_call_id.clone(),
            bridge: bridge_config.clone(),
            start: start.clone(),
            media_tap: tap,
            transfer,
        };
        let (controller, handle) = CallController::new(cfg);

        Ok(PreparedCall {
            bridge_call_id,
            forge_call_id,
            sip_call_id,
            answer,
            bridge_config,
            start,
            controller,
            handle,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use sip_core::{Headers as SipHeaders, Method, Request, RequestLine, SipUri};
    use siphon_ai_media_glue::{AnswerOutcome, Codec};
    use siphon_ai_routes::load_from_toml;

    fn invite_with(content_type: Option<&str>, body: &str) -> Request {
        let uri = SipUri::parse("sip:5000@siphon.example.com").expect("uri");
        let line = RequestLine::new(Method::Invite, uri);
        let mut headers = SipHeaders::new();
        headers
            .push("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z")
            .unwrap();
        headers
            .push("From", "<sip:caller@example.net>;tag=abc")
            .unwrap();
        headers.push("To", "<sip:5000@siphon.example.com>").unwrap();
        headers.push("Call-ID", "abc-123@pbx").unwrap();
        headers.push("CSeq", "1 INVITE").unwrap();
        if let Some(ct) = content_type {
            headers.push("Content-Type", ct).unwrap();
        }
        headers
            .push("Content-Length", body.len().to_string())
            .unwrap();
        Request::new(line, headers, Bytes::from(body.as_bytes().to_vec())).unwrap()
    }

    fn fake_answer() -> AnswerOutcome {
        // We don't go through the real negotiator here; we just need
        // an AnswerOutcome shape with the audio sample rate filled
        // in. Build it via a real round-trip so any field rename
        // upstream breaks this test loudly rather than silently.
        let offer = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.5\r\n\
s=t\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=audio 7000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=sendrecv\r\n";
        let caps = siphon_ai_media_glue::LocalCapabilities {
            local_ip: "192.168.1.10".into(),
            local_port: 20100,
            codecs: vec![Codec::Pcmu],
            dtmf_payload_type: None,
        };
        siphon_ai_media_glue::build_answer(offer, &caps).expect("answer")
    }

    fn first_route(toml: &str) -> siphon_ai_routes::RouteSet {
        load_from_toml(toml).expect("compile routes")
    }

    // ─── extract_offer_sdp ─────────────────────────────────────────

    #[test]
    fn extract_offer_accepts_application_sdp() {
        let req = invite_with(Some("application/sdp"), "v=0\r\n");
        assert_eq!(extract_offer_sdp(&req).unwrap(), "v=0\r\n");
    }

    #[test]
    fn extract_offer_accepts_application_sdp_with_charset() {
        let req = invite_with(Some("application/sdp; charset=utf-8"), "v=0\r\n");
        assert_eq!(extract_offer_sdp(&req).unwrap(), "v=0\r\n");
    }

    #[test]
    fn extract_offer_is_case_insensitive_on_mime() {
        let req = invite_with(Some("Application/SDP"), "v=0\r\n");
        assert!(extract_offer_sdp(&req).is_ok());
    }

    #[test]
    fn extract_offer_rejects_other_content_types() {
        let req = invite_with(Some("text/plain"), "hello");
        assert!(matches!(
            extract_offer_sdp(&req),
            Err(OfferError::UnsupportedMediaType(_))
        ));
    }

    #[test]
    fn extract_offer_rejects_empty_body() {
        let req = invite_with(Some("application/sdp"), "");
        assert_eq!(extract_offer_sdp(&req), Err(OfferError::NoBody));
    }

    #[test]
    fn extract_offer_rejects_missing_content_type_when_body_present() {
        // Some peers still send a body without Content-Type; we
        // refuse to guess.
        let req = invite_with(None, "v=0\r\n");
        assert!(matches!(
            extract_offer_sdp(&req),
            Err(OfferError::UnsupportedMediaType(None))
        ));
    }

    #[test]
    fn extract_offer_rejects_missing_content_type_no_body() {
        let req = invite_with(None, "");
        assert_eq!(extract_offer_sdp(&req), Err(OfferError::NoBody));
    }

    #[test]
    fn extract_sip_call_id_returns_header_value() {
        let req = invite_with(Some("application/sdp"), "v=0\r\n");
        assert_eq!(extract_sip_call_id(&req), "abc-123@pbx");
    }

    // ─── build_bridge_config ───────────────────────────────────────

    #[test]
    fn route_ws_url_overrides_default() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://route.example/ws"
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let defaults = BridgeDefaults {
            ws_url: Some("wss://default.example/ws".into()),
            ..BridgeDefaults::default()
        };
        let cfg = build_bridge_config(&defaults, route).unwrap();
        assert_eq!(cfg.ws_url, "wss://route.example/ws");
    }

    #[test]
    fn defaults_ws_url_used_when_route_omits_it() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let defaults = BridgeDefaults {
            ws_url: Some("wss://default.example/ws".into()),
            ..BridgeDefaults::default()
        };
        let cfg = build_bridge_config(&defaults, route).unwrap();
        assert_eq!(cfg.ws_url, "wss://default.example/ws");
    }

    #[test]
    fn missing_ws_url_anywhere_errors() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let defaults = BridgeDefaults::default();
        assert_eq!(
            build_bridge_config(&defaults, route).unwrap_err(),
            BridgeBuildError::NoWsUrl
        );
    }

    #[test]
    fn route_connect_timeout_overrides_default() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            ws_connect_timeout_ms = 12345
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let cfg = build_bridge_config(&BridgeDefaults::default(), route).unwrap();
        assert_eq!(cfg.connect_timeout, Duration::from_millis(12345));
    }

    #[test]
    fn route_bearer_auth_passes_through_verbatim() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            ws_auth_header = "Bearer abc123"
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let cfg = build_bridge_config(&BridgeDefaults::default(), route).unwrap();
        // Stored verbatim — the bridge emits the full value as
        // `Authorization:` on the WS upgrade.
        assert_eq!(cfg.auth_header.as_deref(), Some("Bearer abc123"));
    }

    #[test]
    fn route_basic_auth_passes_through_verbatim() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            ws_auth_header = "Basic dXNlcjpwYXNz"
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let cfg = build_bridge_config(&BridgeDefaults::default(), route).unwrap();
        // Non-Bearer scheme survives. Previous behaviour would
        // double-prefix this into `Bearer Basic dXNlcjpwYXNz` on
        // the wire.
        assert_eq!(cfg.auth_header.as_deref(), Some("Basic dXNlcjpwYXNz"));
    }

    #[test]
    fn route_bare_token_normalized_to_bearer() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            ws_auth_header = "abc123"
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let cfg = build_bridge_config(&BridgeDefaults::default(), route).unwrap();
        // Bare tokens (no scheme keyword) get the historic
        // Bearer-by-default treatment.
        assert_eq!(cfg.auth_header.as_deref(), Some("Bearer abc123"));
    }

    // ─── resolve_codecs ────────────────────────────────────────────

    #[test]
    fn defaults_used_when_route_omits_codecs() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let defaults = BridgeDefaults {
            codecs: vec![Codec::Pcma, Codec::G722],
            ..BridgeDefaults::default()
        };
        assert_eq!(
            resolve_codecs(&defaults, route),
            vec![Codec::Pcma, Codec::G722]
        );
    }

    #[test]
    fn route_codecs_replace_defaults_in_order() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            [route.media]
            codecs = ["opus", "pcmu"]
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        assert_eq!(
            resolve_codecs(&BridgeDefaults::default(), route),
            vec![Codec::Opus, Codec::Pcmu]
        );
    }

    #[test]
    fn unknown_codecs_drop_with_warning_and_keep_known() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            [route.media]
            codecs = ["amr", "pcmu"]
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        assert_eq!(
            resolve_codecs(&BridgeDefaults::default(), route),
            vec![Codec::Pcmu]
        );
    }

    #[test]
    fn empty_resolved_codecs_falls_back_to_defaults() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            [route.media]
            codecs = ["g729", "amr"]
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let defaults = BridgeDefaults {
            codecs: vec![Codec::Pcmu],
            ..BridgeDefaults::default()
        };
        assert_eq!(resolve_codecs(&defaults, route), vec![Codec::Pcmu]);
    }

    // ─── resolve_inactivity_timeout ────────────────────────────────

    #[test]
    fn inactivity_timeout_route_overrides_default() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            [route.media]
            inactivity_timeout_secs = 30
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let defaults = BridgeDefaults {
            inactivity_timeout: Some(Duration::from_secs(60)),
            ..BridgeDefaults::default()
        };
        assert_eq!(
            resolve_inactivity_timeout(&defaults, route),
            Some(Duration::from_secs(30)),
        );
    }

    #[test]
    fn inactivity_timeout_zero_on_route_disables_watchdog() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            [route.media]
            inactivity_timeout_secs = 0
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let defaults = BridgeDefaults {
            inactivity_timeout: Some(Duration::from_secs(60)),
            ..BridgeDefaults::default()
        };
        assert_eq!(resolve_inactivity_timeout(&defaults, route), None);
    }

    #[test]
    fn inactivity_timeout_unset_route_inherits_default() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let defaults = BridgeDefaults {
            inactivity_timeout: Some(Duration::from_secs(45)),
            ..BridgeDefaults::default()
        };
        assert_eq!(
            resolve_inactivity_timeout(&defaults, route),
            Some(Duration::from_secs(45)),
        );
    }

    // ─── resolve_dtmf_pt ──────────────────────────────────────────

    #[test]
    fn dtmf_off_disables_telephone_event_pt() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            [route.media]
            dtmf = "off"
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let defaults = BridgeDefaults {
            dtmf_payload_type: Some(101),
            ..BridgeDefaults::default()
        };
        assert_eq!(resolve_dtmf_pt(&defaults, route), None);
    }

    #[test]
    fn dtmf_unset_keeps_default_pt() {
        let routes = first_route(
            r#"
            [[route]]
            name = "r"
            [route.match]
            any = true
            [route.bridge]
            ws_url = "wss://x/y"
            "#,
        );
        let route = routes.find_match(&dummy_call_info()).unwrap();
        let defaults = BridgeDefaults {
            dtmf_payload_type: Some(101),
            ..BridgeDefaults::default()
        };
        assert_eq!(resolve_dtmf_pt(&defaults, route), Some(101));
    }

    // ─── build_start_msg ──────────────────────────────────────────

    #[test]
    fn start_msg_pulls_facts_and_answer_into_protocol_shape() {
        let req = invite_with(Some("application/sdp"), "v=0\r\n");
        let facts = InviteFacts::extract(&req);
        let answer = fake_answer();
        let start = build_start_msg(
            BridgeCallId::new("siphon-1"),
            &facts,
            "abc-123@pbx",
            &answer,
            &[],
        );
        assert_eq!(start.version, PROTOCOL_VERSION);
        assert_eq!(start.call_id.as_str(), "siphon-1");
        assert_eq!(start.seq, 0);
        assert_eq!(start.from, facts.from_user);
        assert_eq!(start.to, facts.request_uri_user);
        assert_eq!(start.direction, Direction::Inbound);
        assert_eq!(start.audio.encoding, AudioEncoding::Pcm16le);
        assert_eq!(start.audio.sample_rate, 8000);
        assert_eq!(start.audio.channels, 1);
        assert_eq!(start.audio.frame_ms, 20);
        assert_eq!(start.sip.call_id, "abc-123@pbx");
        assert!(start.sip.headers.is_empty());
    }

    #[test]
    fn start_msg_forwards_configured_headers_only() {
        let uri = SipUri::parse("sip:5000@siphon.example.com").expect("uri");
        let line = RequestLine::new(Method::Invite, uri);
        let mut headers = SipHeaders::new();
        headers.push("Via", "SIP/2.0/UDP h:5060;branch=z").unwrap();
        headers
            .push("From", "<sip:caller@example.net>;tag=t")
            .unwrap();
        headers.push("To", "<sip:5000@siphon.example.com>").unwrap();
        headers.push("Call-ID", "x@y").unwrap();
        headers.push("CSeq", "1 INVITE").unwrap();
        headers.push("User-Agent", "Cisco-CP8841").unwrap();
        headers.push("X-Tenant-Id", "acme").unwrap();
        headers.push("X-Secret", "hush").unwrap();
        headers.push("Content-Length", "0").unwrap();
        let req = Request::new(line, headers, Bytes::new()).unwrap();
        let facts = InviteFacts::extract(&req);
        let answer = fake_answer();

        let start = build_start_msg(
            BridgeCallId::new("c"),
            &facts,
            "x@y",
            &answer,
            &["User-Agent".into(), "X-Tenant-Id".into()],
        );

        // Forwarded headers come back canonical-cased.
        assert_eq!(
            start.sip.headers.get("User-Agent").map(String::as_str),
            Some("Cisco-CP8841")
        );
        assert_eq!(
            start.sip.headers.get("X-Tenant-Id").map(String::as_str),
            Some("acme")
        );
        // Anything not in the allowlist stays out.
        assert!(!start.sip.headers.contains_key("X-Secret"));
    }

    #[test]
    fn forward_header_lookup_is_case_insensitive() {
        let uri = SipUri::parse("sip:5000@siphon.example.com").expect("uri");
        let line = RequestLine::new(Method::Invite, uri);
        let mut headers = SipHeaders::new();
        headers.push("Via", "SIP/2.0/UDP h;branch=z").unwrap();
        headers.push("From", "<sip:c@x>;tag=t").unwrap();
        headers.push("To", "<sip:5000@y>").unwrap();
        headers.push("Call-ID", "x@y").unwrap();
        headers.push("CSeq", "1 INVITE").unwrap();
        headers.push("user-agent", "Linphone").unwrap();
        headers.push("Content-Length", "0").unwrap();
        let req = Request::new(line, headers, Bytes::new()).unwrap();
        let facts = InviteFacts::extract(&req);
        let answer = fake_answer();

        let start = build_start_msg(
            BridgeCallId::new("c"),
            &facts,
            "x@y",
            &answer,
            &["USER-AGENT".into()],
        );
        assert_eq!(
            start.sip.headers.get("User-Agent").map(String::as_str),
            Some("Linphone"),
            "headers map: {:?}",
            start.sip.headers
        );
    }

    // ─── AcceptError → SIP status mapping ─────────────────────────

    #[test]
    fn accept_error_status_table() {
        let cases: &[(AcceptError, (u16, &'static str))] = &[
            (
                AcceptError::Offer(OfferError::UnsupportedMediaType(Some("text/plain".into()))),
                (415, "Unsupported Media Type"),
            ),
            (AcceptError::Offer(OfferError::NoBody), (400, "Bad Request")),
            (
                AcceptError::Offer(OfferError::InvalidUtf8),
                (400, "Bad Request"),
            ),
            (
                AcceptError::Bridge(BridgeBuildError::NoWsUrl),
                (503, "Service Unavailable"),
            ),
            (
                AcceptError::Setup(SetupError::Sdp(SdpError::Parse("bad".into()))),
                (400, "Bad Request"),
            ),
            (
                AcceptError::Setup(SetupError::Sdp(SdpError::NoCommonCodec)),
                (488, "Not Acceptable Here"),
            ),
            (
                AcceptError::Setup(SetupError::Sdp(SdpError::NoAudio)),
                (488, "Not Acceptable Here"),
            ),
            (
                AcceptError::Setup(SetupError::Sdp(SdpError::AudioRejected)),
                (488, "Not Acceptable Here"),
            ),
            (
                AcceptError::Setup(SetupError::Sdp(SdpError::Negotiate("oops".into()))),
                (488, "Not Acceptable Here"),
            ),
            (
                AcceptError::Setup(SetupError::Session("port pool empty".into())),
                (500, "Server Internal Error"),
            ),
            (
                AcceptError::Controller("spawn refused".into()),
                (500, "Server Internal Error"),
            ),
        ];
        for (err, (code, reason)) in cases {
            assert_eq!(err.sip_status(), (*code, *reason), "for {err:?}");
        }
    }

    /// Compile-time check: `BridgingAcceptor` actually satisfies the
    /// `CallAcceptor` trait the routing layer expects. Mirror of
    /// `RoutingHandler` / `UasRequestHandler` in handler.rs.
    #[allow(dead_code)]
    fn _bridging_acceptor_is_a_call_acceptor(b: BridgingAcceptor) {
        let _: Arc<dyn CallAcceptor> = Arc::new(b);
    }

    fn dummy_call_info<'a>() -> siphon_ai_routes::CallInfo<'a> {
        siphon_ai_routes::CallInfo {
            request_uri_user: "5000",
            request_uri_host: "siphon.example.com",
            to_user: "5000",
            to_host: "siphon.example.com",
            from_user: "caller",
            from_host: "example.net",
            register_source: "trunk",
            headers: leak_empty_headers(),
        }
    }

    fn leak_empty_headers() -> &'static siphon_ai_routes::Headers {
        // CallInfo borrows headers; the test tolerates the leak.
        use std::sync::OnceLock;
        static EMPTY: OnceLock<siphon_ai_routes::Headers> = OnceLock::new();
        EMPTY.get_or_init(siphon_ai_routes::Headers::new)
    }

    // ─── Hold / resume helpers ──────────────────────────────────

    use siphon_ai_media_glue::MediaDirection;

    #[test]
    fn mirror_direction_follows_rfc_3264_section_6_1() {
        assert_eq!(
            mirror_direction(MediaDirection::SendRecv),
            MediaDirection::SendRecv
        );
        assert_eq!(
            mirror_direction(MediaDirection::SendOnly),
            MediaDirection::RecvOnly
        );
        assert_eq!(
            mirror_direction(MediaDirection::RecvOnly),
            MediaDirection::SendOnly
        );
        assert_eq!(
            mirror_direction(MediaDirection::Inactive),
            MediaDirection::Inactive
        );
    }

    #[test]
    fn rewrite_sdp_direction_swaps_sendrecv_to_recvonly() {
        let sdp = "v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\n\
                   t=0 0\r\nm=audio 30000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n\
                   a=sendrecv\r\n";
        let out = rewrite_sdp_direction(sdp, MediaDirection::RecvOnly);
        assert!(out.contains("a=recvonly\r\n"));
        assert!(!out.contains("a=sendrecv"));
        // Everything else preserved verbatim — port, codec, rtpmap.
        assert!(out.contains("m=audio 30000 RTP/AVP 0"));
        assert!(out.contains("a=rtpmap:0 PCMU/8000"));
    }

    #[test]
    fn rewrite_sdp_direction_swaps_recvonly_back_to_sendrecv() {
        // The resume case: previous answer was recvonly (we were
        // held); new offer is sendrecv; we mirror to sendrecv.
        let sdp = "v=0\r\nm=audio 30000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=recvonly\r\n";
        let out = rewrite_sdp_direction(sdp, MediaDirection::SendRecv);
        assert!(out.contains("a=sendrecv\r\n"));
        assert!(!out.contains("a=recvonly"));
    }

    #[test]
    fn rewrite_sdp_direction_appends_when_missing() {
        // RFC 4566 §6 lets the direction attribute be implicit. If
        // it's absent we append the explicit attribute rather than
        // silently leaving the direction unspecified.
        let sdp = "v=0\r\nm=audio 30000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        let out = rewrite_sdp_direction(sdp, MediaDirection::Inactive);
        assert!(out.ends_with("a=inactive\r\n"));
    }

    #[test]
    fn rewrite_sdp_direction_preserves_lf_only_endings() {
        let sdp = "v=0\nm=audio 30000 RTP/AVP 0\na=sendrecv\n";
        let out = rewrite_sdp_direction(sdp, MediaDirection::SendOnly);
        assert!(out.contains("a=sendonly\n"));
        // No spurious CR added.
        assert!(!out.contains("\r"));
    }

    // ─── prepare_reinvite_answer ───────────────────────────────────

    /// SDP we'd cache after accepting an initial INVITE that offered
    /// PCMU. Matches what `build_answer` would produce shape-wise.
    fn cached_answer_pcmu() -> &'static str {
        "v=0\r\n\
o=siphon 1 1 IN IP4 192.168.1.10\r\n\
s=-\r\n\
c=IN IP4 192.168.1.10\r\n\
t=0 0\r\n\
m=audio 20100 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=sendrecv\r\n"
    }

    #[test]
    fn reinvite_matching_pt_with_hold_returns_mirrored_answer() {
        // Peer puts the call on hold by sending a re-INVITE with
        // sendonly + same PT (PCMU). We should answer recvonly and
        // keep the same media line otherwise.
        let req = invite_with(
            Some("application/sdp"),
            "v=0\r\n\
o=alice 2 2 IN IP4 10.0.0.5\r\n\
s=t\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=audio 7000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=sendonly\r\n",
        );
        let outcome = prepare_reinvite_answer(&req, cached_answer_pcmu(), "abc-123@pbx")
            .expect("re-INVITE should be accepted");
        assert_eq!(
            outcome.offer_direction,
            Some(siphon_ai_media_glue::MediaDirection::SendOnly)
        );
        assert_eq!(
            outcome.answer_direction,
            Some(siphon_ai_media_glue::MediaDirection::RecvOnly)
        );
        assert!(outcome.answer_sdp.contains("a=recvonly"));
        // Media line preserved.
        assert!(outcome.answer_sdp.contains("m=audio 20100 RTP/AVP 0"));
    }

    #[test]
    fn reinvite_with_unsupported_pt_rejected_488() {
        // Original call negotiated PCMU (PT 0). Peer's re-INVITE
        // proposes only G.722 (PT 9). v1 doesn't renegotiate
        // codecs mid-call — must be 488, not a stale 200.
        let req = invite_with(
            Some("application/sdp"),
            "v=0\r\n\
o=alice 2 2 IN IP4 10.0.0.5\r\n\
s=t\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=audio 7000 RTP/AVP 9\r\n\
a=rtpmap:9 G722/8000\r\n\
a=sendrecv\r\n",
        );
        match prepare_reinvite_answer(&req, cached_answer_pcmu(), "abc-123@pbx") {
            Err(ReinviteRejection { code, reason }) => {
                assert_eq!(code, 488);
                assert_eq!(reason, "Not Acceptable Here");
            }
            Ok(_) => panic!("expected 488 rejection on codec divergence"),
        }
    }

    #[test]
    fn reinvite_with_malformed_body_rejected_488() {
        // Body is non-SDP — parse fails. RFC 3261 §13.3 maps a
        // failed offer to 488 (we choose 488 over 400 because the
        // peer SENT a body and we just can't accept it).
        let req = invite_with(Some("application/sdp"), "not actually sdp\r\n");
        match prepare_reinvite_answer(&req, cached_answer_pcmu(), "abc-123@pbx") {
            Err(ReinviteRejection { code, .. }) => assert_eq!(code, 488),
            Ok(_) => panic!("expected rejection on parse failure"),
        }
    }

    #[test]
    fn reinvite_without_content_type_but_with_body_rejected_400() {
        // No Content-Type header but a body is present — that's
        // malformed SIP (RFC 3261 §20.15). 400 Bad Request, not 488.
        let req = invite_with(None, "v=0\r\n");
        match prepare_reinvite_answer(&req, cached_answer_pcmu(), "abc-123@pbx") {
            Err(ReinviteRejection { code, .. }) => assert_eq!(code, 400),
            Ok(_) => panic!("expected 400 on missing Content-Type"),
        }
    }

    #[test]
    fn reinvite_without_body_treated_as_session_refresh() {
        // RFC 3261 §14.2: body-less re-INVITE is permitted for
        // session refresh. We answer with the unchanged cached SDP
        // and no direction info (nothing to mirror).
        let req = invite_with(None, "");
        let outcome = prepare_reinvite_answer(&req, cached_answer_pcmu(), "abc-123@pbx")
            .expect("body-less re-INVITE should be accepted");
        assert_eq!(outcome.offer_direction, None);
        assert_eq!(outcome.answer_direction, None);
        assert_eq!(outcome.answer_sdp, cached_answer_pcmu());
        assert_eq!(outcome.remote_addr, None);
    }

    // ─── requires_100rel ───────────────────────────────────────────

    #[test]
    fn requires_100rel_false_when_no_require_header() {
        let req = invite_with(Some("application/sdp"), "v=0\r\n");
        assert!(!super::requires_100rel(&req));
    }

    #[test]
    fn requires_100rel_true_when_present_alone() {
        let req = invite_with_require("100rel");
        assert!(super::requires_100rel(&req));
    }

    #[test]
    fn requires_100rel_true_when_in_token_list() {
        // RFC 3261 §27.1 — `Require` is a comma-separated option-tag list.
        let req = invite_with_require("timer, 100rel, replaces");
        assert!(super::requires_100rel(&req));
    }

    #[test]
    fn requires_100rel_is_case_insensitive() {
        let req = invite_with_require("100REL");
        assert!(super::requires_100rel(&req));
    }

    #[test]
    fn requires_100rel_false_for_unrelated_tokens() {
        let req = invite_with_require("timer, replaces");
        assert!(!super::requires_100rel(&req));
    }

    fn invite_with_require(value: &str) -> Request {
        let mut req = invite_with(Some("application/sdp"), "v=0\r\n");
        req.headers_mut().push("Require", value).unwrap();
        req
    }

    // ─── sdp_audio_remote_addr ─────────────────────────────────────

    #[test]
    fn reinvite_remote_addr_extracted_from_offer() {
        // Peer changed RTP endpoint mid-call (port + connection
        // address). prepare_reinvite_answer must surface the new
        // address so the acceptor can push it to forge.
        let req = invite_with(
            Some("application/sdp"),
            "v=0\r\n\
o=alice 2 2 IN IP4 10.0.0.99\r\n\
s=t\r\n\
c=IN IP4 10.0.0.99\r\n\
t=0 0\r\n\
m=audio 19999 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=sendrecv\r\n",
        );
        let outcome = prepare_reinvite_answer(&req, cached_answer_pcmu(), "abc-123@pbx")
            .expect("re-INVITE should be accepted");
        let addr = outcome.remote_addr.expect("remote_addr present");
        assert_eq!(addr.to_string(), "10.0.0.99:19999");
    }

    #[test]
    fn reinvite_remote_addr_media_level_connection_wins() {
        // RFC 4566 §5.7: media-level `c=` overrides session-level.
        let req = invite_with(
            Some("application/sdp"),
            "v=0\r\n\
o=alice 2 2 IN IP4 10.0.0.5\r\n\
s=t\r\n\
c=IN IP4 10.0.0.5\r\n\
t=0 0\r\n\
m=audio 7000 RTP/AVP 0\r\n\
c=IN IP4 192.168.42.5\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=sendrecv\r\n",
        );
        let outcome = prepare_reinvite_answer(&req, cached_answer_pcmu(), "abc-123@pbx")
            .expect("re-INVITE should be accepted");
        let addr = outcome.remote_addr.expect("remote_addr present");
        assert_eq!(addr.to_string(), "192.168.42.5:7000");
    }
}
