//! Metric names, descriptions, and recorder installation.
//!
//! ## Naming
//!
//! Every metric is prefixed `siphon_ai_` per CLAUDE.md §4.5 + §7.4.
//! Names live in this module as `pub const &str` so consumers
//! reference them by symbol — a typo in the acceptor would otherwise
//! produce a silent metric-not-found.
//!
//! ## Descriptions
//!
//! Registered via `metrics::describe_*!` at recorder install time so
//! `# HELP` lines appear in Prometheus output. The describe call is
//! a no-op when no recorder is installed — perfectly fine for tests.
//!
//! ## Buckets
//!
//! Four histograms get explicit buckets per the CLAUDE.md guidance
//! ("histograms get sensible buckets defined explicitly; don't rely
//! on defaults"). Buckets target the latencies operators care about:
//!
//! - `ws_connect_seconds`: 25ms → 30s. Most healthy connects land
//!   under 200ms; the long tail is for hung TLS handshakes.
//! - `sdp_negotiate_seconds`: 100us → 200ms. Pure CPU work; runs in
//!   tens of microseconds normally.
//! - `call_duration_seconds`: 1s → 4h. Captures everything from a
//!   barge-in cancel to a long support call.
//!
//! ## Cardinality
//!
//! Per CLAUDE.md §4.5 we never label by `call_id`. `route` IS a
//! label — it has bounded cardinality (operators have tens of
//! routes, not millions). Termination cause is a small enum.

use std::sync::{Mutex, OnceLock};

use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use thiserror::Error;

// ─── Counters ───────────────────────────────────────────────────────

/// Total INVITEs the daemon has seen. Labeled by `result`:
/// `accepted`, `rejected`, `rejected_attestation`, `no_match`. `rejected`
/// covers every 4xx/5xx final response from the routing/media layer (see
/// `siphon_ai_core::AcceptError::sip_status`); `rejected_attestation` is
/// carved out for STIR/SHAKEN policy rejections (`min_attestation` gate or
/// `require_identity`) so fraud-control alerts don't bury in routing noise.
pub const INVITES_TOTAL: &str = "siphon_ai_invites_total";

/// Calls that completed (controller exited). Labeled by `cause`:
/// `server_hangup` / `local_shutdown` / `bridge_ended` / `tap_ended`.
pub const CALLS_TOTAL: &str = "siphon_ai_calls_total";

/// Per-route call counter. Labeled by `route` (the matched
/// `[[route]].name`). Useful for "which route is hot" dashboards.
pub const ROUTE_MATCH_TOTAL: &str = "siphon_ai_route_match_total";

/// STIR/SHAKEN verification outcomes on inbound INVITEs, counted only
/// when `[security.stir_shaken].enabled = true`. Labeled by `result`:
/// `passed` (every check held — attestation is trustworthy),
/// `failed` (an `Identity` header was present but verification did not
/// fully pass), `unsigned` (no `Identity` header on the INVITE).
/// Bounded cardinality (three values); per-call detail lives on the CDR
/// (`verstat_attest`/`verstat_passed`) and in traces.
pub const VERSTAT_TOTAL: &str = "siphon_ai_verstat_total";

/// Recordings finished, when `[recording]` is on. Labeled by `result`:
/// `ok` (written cleanly), `degraded` (some 20 ms frames were dropped under
/// writer back-pressure — the file is short, not corrupt), `failed` (an I/O
/// error). Bounded cardinality (three values); the per-call recording path
/// lives on the CDR (`recording_path`).
pub const RECORDINGS_TOTAL: &str = "siphon_ai_recordings_total";

/// Recording uploads to object storage (`[recording.storage]`, 0.25.0)
/// by `result`: `ok` (durably uploaded), `failed` (attempt failed, will
/// retry), `dropped` (retry budget exhausted / local file gone /
/// unreadable job — the recording stays local-only). Emitted from the
/// upload worker in `siphon-ai-http`.
pub const RECORDING_UPLOADS_TOTAL: &str = "siphon_ai_recording_uploads_total";

/// REGISTER attempts the daemon has driven. Labeled by `name`
/// (the `[[register]].name`) and `outcome`:
/// `registered` / `auth_failed` / `transport_error` / `timeout` /
/// `rejected` (any other 4xx/5xx/6xx final response).
/// Counts the FINAL outcome of each REGISTER transaction — the
/// upstream IntegratedUAC handles 401/407 retry internally, so
/// challenges aren't counted here.
pub const REGISTER_ATTEMPTS_TOTAL: &str = "siphon_ai_register_attempts_total";

/// Outbound calls placed (0.6.0). Labeled by `result`: `answered`,
/// `busy` (486/600), `declined` (403/603), `no_answer` (408/480/487),
/// `rejected` (other non-2xx), `unreachable` (DNS/transport/timeout, no
/// response), `failed` (local media setup error). Bounded cardinality.
pub const OUTBOUND_CALLS_TOTAL: &str = "siphon_ai_outbound_calls_total";

/// Outbound SRTP (SDES) negotiation outcomes for answered calls placed
/// through a gateway with `[[gateway]].srtp` set (0.7.x). Labeled by
/// `result`: `encrypted` (peer accepted SRTP; media is SRTP) or
/// `downgraded` (gateway is `preferred` and the peer answered plaintext —
/// the call continued unencrypted). A `required` trunk that refuses SRTP
/// fails the call, counting as `failed` on `siphon_ai_outbound_calls_total`
/// instead. Bounded cardinality. Literal must match the call site in
/// `siphon-ai-core::outbound_service`.
pub const OUTBOUND_SRTP_TOTAL: &str = "siphon_ai_outbound_srtp_total";

/// Authenticated admin API requests (0.10.0). Labeled by `endpoint`
/// (the route template, ids collapsed — bounded), `role` (the
/// authenticated token's role, or `none` when auth failed), and
/// `result` (`ok`, `unauthenticated`, `forbidden`, `not_found`). One
/// counter per admin call on the `[admin]` listener; pairs with the
/// structured audit log. Literal must match the call site in
/// `siphon-ai-telemetry::http`.
pub const ADMIN_REQUESTS_TOTAL: &str = "siphon_ai_admin_requests_total";

/// `/metrics` scrape outcomes when — and only when — the optional
/// bearer gate is configured (`[observability].metrics_token`,
/// 0.35.0). Labeled by `result`: `ok` | `unauthenticated`. An open
/// (default) endpoint counts nothing, so this series existing at all
/// means the gate is on. Literal must match the call site in
/// `siphon-ai-telemetry::http`.
pub const METRICS_REQUESTS_TOTAL: &str = "siphon_ai_metrics_requests_total";

/// WS-failure prompt playbacks (0.34.0,
/// `[bridge].on_ws_failure = "play_prompt"`). Labeled by `result`:
/// `played` (EOF reached), `cut_short` (caller hung up / teardown
/// preempted it), `unusable` (prompt file failed to load at call time
/// — rate mismatch or unreadable; the call fell open to a plain
/// hangup), `timeout` (the 30 s playback safety cap fired). Literal
/// must match the call site in `siphon-ai-core::call`.
pub const WS_FAILURE_PROMPTS_TOTAL: &str = "siphon_ai_ws_failure_prompts_total";

/// Operator-triggered registration actions accepted by the admin API
/// (0.33.0): `POST /admin/v1/registrations/{name}/refresh|restart`.
/// Labeled by `name` (the `[[register]].name` — operator-chosen,
/// bounded like `register_attempts_total`) and `action`
/// (`refresh` | `restart`). Counts *accepted* triggers; the resulting
/// REGISTER's outcome lands on `siphon_ai_register_attempts_total`.
/// Literal must match the call site in `siphon-ai-telemetry::admin`.
pub const REGISTER_ADMIN_TRIGGERS_TOTAL: &str = "siphon_ai_register_admin_triggers_total";

/// Inbound delayed-offer (offerless INVITE) outcomes (0.9.0). Labeled by
/// `result`: `answered` (peer's ACK answer negotiated and the call
/// bridged), `ack_timeout` (no ACK before Timer H), `missing_sdp_answer`
/// (ACK had no body), `invalid_sdp_answer` (ACK body unparseable),
/// `no_compatible_codec` (answer selected nothing we offered), or
/// `invalid_remote_media` (answer's RTP address/port unusable or stream
/// rejected). Bounded cardinality. Literal must match the call site in
/// `siphon-ai-core::acceptor`.
pub const DELAYED_OFFER_TOTAL: &str = "siphon_ai_delayed_offer_total";

/// REFER transfers attempted (0.6.1; back-fills blind-transfer
/// counting, which previously had no metric). Labeled by `mode`
/// (`blind` / `attended`) and `result`: `accepted` (202, call torn
/// down), `rejected` (peer non-2xx final), `local_error` (bad target,
/// unknown consult call, dialog not found, send failure). Bounded
/// cardinality.
pub const TRANSFERS_TOTAL: &str = "siphon_ai_transfers_total";

/// Conference joins attempted (0.7.0). Labeled by `result`: `joined`,
/// `disabled`, `too_many_rooms`, `room_full`, `rate_mismatch`,
/// `already_joined`, `error`. Bounded cardinality; the literal must
/// match the call site in `siphon-ai-core::conference`.
pub const CONFERENCE_JOINS_TOTAL: &str = "siphon_ai_conference_joins_total";

/// 20 ms frames a conference room dropped instead of blocking the
/// audio path (0.7.0). Labeled by `stage` (`input` — the tap→room
/// channel was full; `sink` — a member's output channel was full)
/// and `side` (`sip` / `ws`). A healthy room sits at zero; sustained
/// `sink` drops mean a stalled consumer. Literal must match the call
/// sites in `siphon-ai-media-glue::room`.
pub const ROOM_FRAMES_DROPPED_TOTAL: &str = "siphon_ai_room_frames_dropped_total";

/// Calls parked (0.7.0). Labeled by `result`: `ok` / `rejected` (park
/// disabled or `[park].max_parked` reached). Literal must match the
/// call site in `siphon-ai-core::call`.
pub const PARKS_TOTAL: &str = "siphon_ai_parks_total";

/// Parked calls retrieved (0.7.0). Labeled by `result`: `ok` /
/// `not_parked`. Literal must match the call site in
/// `siphon-ai-core::call`.
pub const RETRIEVES_TOTAL: &str = "siphon_ai_retrieves_total";

/// Bot-initiated hold/resume re-INVITE attempts (0.7.2). Labeled by
/// `result`: `ok` / `failed`. Covers both directions (hold and resume);
/// a failed attempt leaves the call in its prior media state. Literal
/// must match the call site in `siphon-ai-core::call`.
pub const HOLDS_TOTAL: &str = "siphon_ai_holds_total";

/// WS reconnect episodes mid-call (0.7.3, `[bridge].ws_reconnect_enabled`).
/// Labeled by `result`: `recovered` (re-dialed within the window) /
/// `exhausted` (hit `ws_reconnect_max_secs` and tore the call down). One
/// increment per reconnect episode (an unexpected drop that entered the
/// reconnect path). Literal must match the call site in
/// `siphon-ai-core::call`.
pub const WS_RECONNECTS_TOTAL: &str = "siphon_ai_ws_reconnects_total";

/// Config reloads triggered by `SIGHUP` (0.12.0). Labeled by `result`:
/// `applied` (the new config loaded and the hot-reloadable sections were
/// swapped), `no_change` (loaded fine, nothing reloadable differed), or
/// `failed` (the new config didn't load/compile — the running config was
/// kept). One increment per `SIGHUP`. Emitted from the daemon binary.
pub const CONFIG_RELOADS_TOTAL: &str = "siphon_ai_config_reloads_total";

/// Calls force-terminated at the graceful-shutdown drain deadline
/// (0.17.0): they were still active when `[shutdown].drain_timeout_secs`
/// elapsed, so the drain ended them with a real BYE + WS hangup instead
/// of leaving them to finish. `0` after a clean rolling deploy (all
/// calls drained naturally); a non-zero value means the drain window
/// was too short for the call mix. Emitted once per straggler from the
/// runtime's drain phase. Unlabeled — these also appear on
/// `siphon_ai_calls_total{cause="drain_forced"}` and per-call on the CDR.
pub const CALLS_DRAIN_FORCED_TOTAL: &str = "siphon_ai_calls_drain_forced_total";

/// Outbound webhook / CDR deliveries by terminal outcome (0.11.0).
/// Labeled by `sink` (`lifecycle` / `cdr`) and `result`: `delivered`
/// (2xx), `rejected` (non-retryable 4xx), `dropped` (retry budget
/// exhausted, or the payload couldn't be serialized). One increment
/// per logical delivery. Emitted from `siphon-ai-http`; the literal
/// must match the call site there. Bounded cardinality.
pub const WEBHOOK_DELIVERIES_TOTAL: &str = "siphon_ai_webhook_deliveries_total";

/// Individual outbound HTTP delivery *attempts* (0.11.0) — one per
/// POST, so a retried delivery ticks this several times. Labeled by
/// `sink` and `outcome`: `ok` (2xx), `transient` (retryable 5xx/408/
/// 429), `error` (connect/timeout), `rejected` (non-retryable 4xx).
/// Divide by `siphon_ai_webhook_deliveries_total` for an
/// attempts-per-delivery ratio. Emitted from `siphon-ai-http`.
pub const WEBHOOK_DELIVERY_ATTEMPTS_TOTAL: &str = "siphon_ai_webhook_delivery_attempts_total";

// ─── Gauges ─────────────────────────────────────────────────────────

/// Currently-active calls. Incremented when the controller spawns,
/// decremented when it exits.
pub const CALLS_ACTIVE: &str = "siphon_ai_calls_active";

/// Currently in-flight outbound calls (0.6.0) — incremented when an
/// originate is admitted, decremented when the call settles (answered+ended,
/// or failed to connect). Compare with `[outbound].max_concurrent`.
pub const OUTBOUND_CALLS_ACTIVE: &str = "siphon_ai_outbound_calls_active";

/// Per-`[[register]]` registration status. Labeled by `name` and
/// `state` (`pending`/`registered`/`failed`/`disabled`); the gauge
/// is `1` for the row matching the current state and `0` for the
/// other rows of the same `name`. Lets dashboards page on
/// `siphon_ai_register_state{state="failed"} == 1` without
/// stringly-typed comparisons.
pub const REGISTER_STATE: &str = "siphon_ai_register_state";

/// Live conference rooms (0.7.0). Incremented when a room task
/// spawns, decremented when it exits (last member left). Literal
/// must match `siphon-ai-media-glue::room`.
pub const CONFERENCES_ACTIVE: &str = "siphon_ai_conferences_active";

/// Mixer participants across all rooms (0.7.0). Each member call
/// contributes 2 (its SIP leg + its WS session) — two calls in one
/// room read 4. Literal must match `siphon-ai-media-glue::room`.
pub const CONFERENCE_PARTICIPANTS: &str = "siphon_ai_conference_participants";

/// Currently-parked calls (0.7.0). Incremented on park, decremented on
/// retrieve / teardown. Literal must match `siphon-ai-core::call`.
pub const PARKED_CALLS_ACTIVE: &str = "siphon_ai_parked_calls_active";

/// Whether the daemon is currently draining for shutdown (0.17.0):
/// `1` from the moment a SIGTERM/SIGINT drain begins until the process
/// exits, `0` otherwise. A scraper seeing `1` knows new INVITEs are
/// being 503'd and `/ready` has flipped. Emitted from the runtime's
/// drain phase.
pub const DRAINING: &str = "siphon_ai_draining";

/// Webhook/CDR deliveries currently waiting in the durable spool
/// (0.11.0, `[webhooks].spool_dir` / `[cdr.webhook].spool_dir`).
/// Labeled by `sink` (`lifecycle` / `cdr`). Sampled by the drain
/// worker each pass (self-correcting across restarts). A healthy
/// receiver keeps this at 0; a rising value means deliveries are
/// failing and backing up on disk. Emitted from `siphon-ai-http`.
pub const WEBHOOK_SPOOL_DEPTH: &str = "siphon_ai_webhook_spool_depth";

/// Recording uploads waiting in the durable spool
/// (`[recording.storage].spool_dir`, 0.25.0). Sampled by the upload
/// worker each pass. Healthy = 0; rising = the object store is
/// unreachable and uploads are backing up on disk.
pub const RECORDING_UPLOAD_SPOOL_DEPTH: &str = "siphon_ai_recording_upload_spool_depth";

// ─── Histograms ─────────────────────────────────────────────────────

/// Time from "spawned WS bridge task" to "WS handshake completed
/// AND `start` sent." Labeled by `result`: `ok` / `error`.
pub const WS_CONNECT_SECONDS: &str = "siphon_ai_ws_connect_seconds";

/// Wall-time of one successful recording upload (0.25.0). No labels
/// (failures don't record a duration).
pub const RECORDING_UPLOAD_SECONDS: &str = "siphon_ai_recording_upload_seconds";

/// Time spent inside `MediaSetup::accept_inbound` — SDP parse +
/// forge port allocation + answer build + tap attach. Labeled by
/// `result`: `ok` / `error`.
pub const SDP_NEGOTIATE_SECONDS: &str = "siphon_ai_sdp_negotiate_seconds";

/// End-to-end call duration (started_at → ended_at on the CDR).
pub const CALL_DURATION_SECONDS: &str = "siphon_ai_call_duration_seconds";

/// RTCP-derived round-trip time, in **milliseconds** (the `_ms` suffix
/// carries the unit — unlike the `_seconds` histograms above). Recorded
/// per received Receiver Report from `media-glue` (RFC 3550 §A.7). The
/// literal name must match the `histogram!` call site in
/// `siphon-ai-media-glue`; the bucket matcher keys on this string.
pub const RTP_RTT_MS: &str = "siphon_ai_rtp_rtt_ms";

/// How far past its 20 ms cadence a conference room's mix tick fired
/// (0.7.0), in seconds — the mixer-health signal for the known
/// upstream per-tick allocation (DEV_PLAN_0.7.0.md §6). A healthy
/// room sits in the lowest bucket. Literal must match
/// `siphon-ai-media-glue::room`.
pub const ROOM_TICK_LAG_SECONDS: &str = "siphon_ai_room_tick_lag_seconds";

/// How long the shutdown drain took, in **seconds** (0.17.0): from the
/// moment draining began until the call registry emptied or the
/// `[shutdown].drain_timeout_secs` deadline fired. Observed exactly
/// once per process lifetime (so it's only useful via a scrape that
/// catches the dying pod, or via push). Emitted from the runtime's
/// drain phase.
pub const DRAIN_SECONDS: &str = "siphon_ai_drain_seconds";

/// Time from a pause-mode barge-in arbitration arming to its
/// resolution, in **seconds** (0.32.0,
/// `[bridge.barge_in].mode = "pause"`). Includes timeout resolutions,
/// so the distribution's ceiling is the configured `decision_ms`.
/// Literal must match the `histogram!` call site in
/// `siphon-ai-media-glue::tap`. The companion counter
/// `siphon_ai_barge_in_decisions_total{outcome}` carries the verdict
/// split.
pub const BARGE_IN_DECISION_SECONDS: &str = "siphon_ai_barge_in_decision_seconds";

/// Outbound webhook / CDR delivery latency in **seconds** (0.11.0):
/// accepted → 2xx, recorded only on success. Labeled by `sink`
/// (`lifecycle` / `cdr`). Captures retry/backoff dwell, so a slow
/// receiver shows up as a fat tail. Emitted from `siphon-ai-http`.
pub const WEBHOOK_DELIVERY_SECONDS: &str = "siphon_ai_webhook_delivery_seconds";

#[derive(Debug, Error)]
pub enum InitError {
    #[error("metrics recorder install failed: {0}")]
    Install(String),
}

/// Build a `PrometheusBuilder` with our histogram buckets pre-set.
/// Exposed so the daemon can install it as the global recorder, and
/// tests can call `.build_recorder()` for a per-test isolated one.
pub fn prometheus_builder() -> Result<PrometheusBuilder, InitError> {
    PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full(WS_CONNECT_SECONDS.to_string()),
            &WS_CONNECT_BUCKETS,
        )
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(SDP_NEGOTIATE_SECONDS.to_string()),
                &SDP_NEGOTIATE_BUCKETS,
            )
        })
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(CALL_DURATION_SECONDS.to_string()),
                &CALL_DURATION_BUCKETS,
            )
        })
        .and_then(|b| {
            b.set_buckets_for_metric(Matcher::Full(RTP_RTT_MS.to_string()), &RTP_RTT_MS_BUCKETS)
        })
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(ROOM_TICK_LAG_SECONDS.to_string()),
                &ROOM_TICK_LAG_BUCKETS,
            )
        })
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(WEBHOOK_DELIVERY_SECONDS.to_string()),
                &WEBHOOK_DELIVERY_BUCKETS,
            )
        })
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(RECORDING_UPLOAD_SECONDS.to_string()),
                &RECORDING_UPLOAD_BUCKETS,
            )
        })
        .and_then(|b| {
            b.set_buckets_for_metric(Matcher::Full(DRAIN_SECONDS.to_string()), &DRAIN_BUCKETS)
        })
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(BARGE_IN_DECISION_SECONDS.to_string()),
                &BARGE_IN_DECISION_BUCKETS,
            )
        })
        .map_err(|e| InitError::Install(e.to_string()))
}

/// Install the Prometheus recorder as the process-wide `metrics`
/// recorder. Idempotent — subsequent calls return a clone of the
/// originally-installed handle. Tests that build multiple
/// `Runtime` instances in one process rely on this; the
/// `metrics::set_global_recorder` call underneath happens exactly
/// once.
///
/// Returns the handle so the HTTP server can call `handle.render()`
/// to produce `/metrics` text.
pub fn install_recorder() -> Result<PrometheusHandle, InitError> {
    // OnceLock<Mutex<Option<_>>> rather than `OnceLock<_>` because
    // installing returns Result. The Mutex is held only while we
    // commit the handle — install errors don't poison subsequent
    // attempts.
    static HANDLE: OnceLock<Mutex<Option<PrometheusHandle>>> = OnceLock::new();
    let cell = HANDLE.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().expect("telemetry handle mutex poisoned");
    if let Some(h) = guard.as_ref() {
        return Ok(h.clone());
    }
    let builder = prometheus_builder()?;
    let handle = builder
        .install_recorder()
        .map_err(|e| InitError::Install(e.to_string()))?;
    register_descriptions();
    *guard = Some(handle.clone());
    Ok(handle)
}

/// Register the `# HELP` text. Safe to call when no recorder is
/// installed (the `describe_*!` macros become no-ops). Public so
/// tests using a per-test recorder can register descriptions inside
/// their `with_local_recorder` scope.
pub fn register_descriptions() {
    describe_counter!(
        INVITES_TOTAL,
        "Inbound INVITEs by result (accepted, rejected, rejected_attestation, no_match)."
    );
    describe_counter!(
        CALLS_TOTAL,
        "Completed calls by termination cause (server_hangup, local_shutdown, drain_forced, bridge_ended, tap_ended)."
    );
    describe_counter!(
        CALLS_DRAIN_FORCED_TOTAL,
        "Calls force-terminated (BYE + WS hangup) at the graceful-shutdown drain deadline."
    );
    describe_counter!(ROUTE_MATCH_TOTAL, "Calls accepted by matched route name.");
    describe_counter!(
        VERSTAT_TOTAL,
        "STIR/SHAKEN verification outcomes by result (passed, failed, unsigned)."
    );
    describe_counter!(
        RECORDINGS_TOTAL,
        "Call recordings finished by result (ok, degraded, failed)."
    );
    describe_counter!(
        RECORDING_UPLOADS_TOTAL,
        "Recording uploads to object storage by result (ok, failed, dropped)."
    );
    describe_counter!(
        REGISTER_ATTEMPTS_TOTAL,
        "REGISTER attempts by [[register]].name and outcome."
    );
    describe_counter!(
        OUTBOUND_CALLS_TOTAL,
        "Outbound calls placed, by result (answered, busy, declined, no_answer, rejected, unreachable, failed)."
    );
    describe_counter!(
        OUTBOUND_SRTP_TOTAL,
        "Outbound SRTP (SDES) outcomes for answered calls, by result (encrypted, downgraded)."
    );
    describe_counter!(
        ADMIN_REQUESTS_TOTAL,
        "Authenticated admin API requests, by endpoint (route template), role, and result (ok, unauthenticated, forbidden, not_found)."
    );
    describe_counter!(
        DELAYED_OFFER_TOTAL,
        "Inbound delayed-offer (offerless INVITE) outcomes, by result (answered, ack_timeout, missing_sdp_answer, invalid_sdp_answer, no_compatible_codec, invalid_remote_media)."
    );
    describe_counter!(
        TRANSFERS_TOTAL,
        "REFER transfers attempted, by mode (blind, attended) and result (accepted, rejected, local_error)."
    );
    describe_counter!(
        CONFERENCE_JOINS_TOTAL,
        "Conference joins attempted, by result (joined, disabled, too_many_rooms, room_full, rate_mismatch, already_joined, error)."
    );
    describe_counter!(
        ROOM_FRAMES_DROPPED_TOTAL,
        "20 ms frames a conference room dropped instead of blocking, by stage (input, sink) and side (sip, ws)."
    );
    describe_counter!(PARKS_TOTAL, "Calls parked, by result (ok, rejected).");
    describe_counter!(
        RETRIEVES_TOTAL,
        "Parked calls retrieved, by result (ok, not_parked)."
    );
    describe_counter!(
        HOLDS_TOTAL,
        "Bot-initiated hold/resume re-INVITEs, by result (ok, failed)."
    );
    describe_counter!(
        WS_RECONNECTS_TOTAL,
        "WS reconnect episodes mid-call, by result (recovered, exhausted)."
    );
    describe_gauge!(
        CALLS_ACTIVE,
        Unit::Count,
        "Currently-running per-call controllers."
    );
    describe_gauge!(
        OUTBOUND_CALLS_ACTIVE,
        Unit::Count,
        "In-flight outbound calls (admitted but not yet settled)."
    );
    describe_gauge!(
        REGISTER_STATE,
        Unit::Count,
        "Per-[[register]] status. 1 = current state for that name; 0 = other states."
    );
    describe_gauge!(CONFERENCES_ACTIVE, Unit::Count, "Live conference rooms.");
    describe_gauge!(
        CONFERENCE_PARTICIPANTS,
        Unit::Count,
        "Mixer participants across all rooms (2 per member call: SIP leg + WS session)."
    );
    describe_gauge!(PARKED_CALLS_ACTIVE, Unit::Count, "Currently-parked calls.");
    describe_gauge!(
        DRAINING,
        Unit::Count,
        "1 while the daemon is draining for shutdown (new INVITEs 503'd, /ready false); 0 otherwise."
    );
    describe_gauge!(
        WEBHOOK_SPOOL_DEPTH,
        Unit::Count,
        "Webhook/CDR deliveries waiting in the durable spool, by sink (lifecycle, cdr)."
    );
    describe_gauge!(
        RECORDING_UPLOAD_SPOOL_DEPTH,
        Unit::Count,
        "Recording uploads waiting in the durable spool."
    );
    describe_histogram!(
        RECORDING_UPLOAD_SECONDS,
        Unit::Seconds,
        "Wall-time of one successful recording upload to object storage."
    );
    describe_histogram!(
        WS_CONNECT_SECONDS,
        Unit::Seconds,
        "Time to complete the WS bridge handshake and send `start`."
    );
    describe_histogram!(
        SDP_NEGOTIATE_SECONDS,
        Unit::Seconds,
        "Time inside MediaSetup::accept_inbound (SDP + port + tap)."
    );
    describe_histogram!(
        CALL_DURATION_SECONDS,
        Unit::Seconds,
        "End-to-end call duration."
    );
    describe_histogram!(
        RTP_RTT_MS,
        Unit::Milliseconds,
        "RTCP-derived round-trip time (ms) per received Receiver Report (RFC 3550 §A.7)."
    );
    describe_histogram!(
        ROOM_TICK_LAG_SECONDS,
        Unit::Seconds,
        "How far past its 20 ms cadence a conference room's mix tick fired."
    );
    describe_histogram!(
        BARGE_IN_DECISION_SECONDS,
        Unit::Seconds,
        "Pause-mode barge-in arbitration latency: armed on speech_started, resolved by verdict/timeout/preemption."
    );
    describe_counter!(
        "siphon_ai_barge_in_decisions_total",
        "Pause-mode barge-in arbitration resolutions by outcome (confirmed, rejected, timeout)."
    );
    describe_counter!(
        REGISTER_ADMIN_TRIGGERS_TOTAL,
        "Operator registration triggers accepted by the admin API, by name and action (refresh, restart)."
    );
    describe_counter!(
        WS_FAILURE_PROMPTS_TOTAL,
        "WS-failure prompt playbacks by result (played, cut_short, unusable, timeout)."
    );
    describe_counter!(
        METRICS_REQUESTS_TOTAL,
        "/metrics scrape outcomes when the bearer gate is configured (ok, unauthenticated)."
    );
    describe_counter!(
        CONFIG_RELOADS_TOTAL,
        "SIGHUP config reloads by result (applied, no_change, failed)."
    );
    describe_counter!(
        WEBHOOK_DELIVERIES_TOTAL,
        "Outbound webhook/CDR deliveries by sink (lifecycle, cdr) and result (delivered, rejected, dropped)."
    );
    describe_counter!(
        WEBHOOK_DELIVERY_ATTEMPTS_TOTAL,
        "Individual outbound delivery attempts by sink and outcome (ok, transient, error, rejected)."
    );
    describe_histogram!(
        WEBHOOK_DELIVERY_SECONDS,
        Unit::Seconds,
        "Outbound webhook/CDR delivery latency (accepted to 2xx), by sink."
    );
    describe_histogram!(
        DRAIN_SECONDS,
        Unit::Seconds,
        "Time the shutdown drain took (drain start to registry empty or deadline)."
    );
}

/// Buckets for `ws_connect_seconds`. The first bucket (25ms) catches
/// the typical healthy local-network handshake; the last (30s)
/// captures pathological hangs that would otherwise make our
/// connect_timeout invisible in summaries.
pub const WS_CONNECT_BUCKETS: [f64; 9] = [0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 30.0];

/// Buckets for `sdp_negotiate_seconds`. Pure CPU; healthy ranges in
/// the tens-of-microseconds, but we keep enough headroom that a
/// large dialplan with many regex re-evals stays bounded.
pub const SDP_NEGOTIATE_BUCKETS: [f64; 8] = [0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.2];

/// Buckets for `call_duration_seconds`. The bottom (1s) catches
/// barge-in / immediate hangup; the top (4h = 14400s) catches the
/// stuck-call long-tail that operators want page-able.
pub const CALL_DURATION_BUCKETS: [f64; 10] = [
    1.0, 5.0, 15.0, 30.0, 60.0, 180.0, 600.0, 1800.0, 3600.0, 14400.0,
];

/// Buckets for `rtp_rtt_ms`, in **milliseconds**. Span healthy regional
/// VoIP (10–100 ms — a Twilio leg measured ~67 ms), elevated
/// transcontinental / congested paths (100–300 ms), and the pathological
/// tail (≥500 ms) operators page on.
pub const RTP_RTT_MS_BUCKETS: [f64; 11] = [
    10.0, 20.0, 30.0, 50.0, 75.0, 100.0, 150.0, 200.0, 300.0, 500.0, 1000.0,
];

/// `room_tick_lag_seconds`: healthy ticks land at ~0 (the interval
/// fired on schedule); one full missed frame is 0.02. Buckets stretch
/// to 0.25 s so a starved runtime is visible, not just clipped into
/// +Inf.
pub const ROOM_TICK_LAG_BUCKETS: [f64; 9] =
    [0.0005, 0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.25];

/// Buckets for `webhook_delivery_seconds`. A healthy receiver answers
/// in tens of ms (bottom buckets); the top (30s) catches deliveries
/// that only succeeded after several backoff rounds against a flaky
/// receiver — visible as a fat tail rather than clipped into +Inf.
pub const WEBHOOK_DELIVERY_BUCKETS: [f64; 10] =
    [0.005, 0.025, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];

/// Buckets for `drain_seconds`, in **seconds**. A clean rolling deploy
/// drains in well under a second when no calls are up (bottom buckets);
/// the spread up to 120 s covers full-window drains against the
/// common k8s grace periods (30/60/120 s) so a drain that ran to its
/// deadline is visible rather than clipped into +Inf.
pub const DRAIN_BUCKETS: [f64; 9] = [0.1, 0.5, 1.0, 5.0, 15.0, 30.0, 60.0, 90.0, 120.0];

/// Recording-upload duration buckets: a small local MinIO PUT lands in
/// tens of ms; a multi-hundred-MB WAV to a remote region can take tens
/// of seconds.
pub const RECORDING_UPLOAD_BUCKETS: [f64; 9] = [0.05, 0.1, 0.25, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0];

/// Verdict latency clusters around STT-partial turnaround (50–500 ms);
/// the tail is bounded by `decision_ms` (default 0.5 s, operators may
/// raise it to a few seconds), so resolution past 5 s means a
/// misconfigured window rather than a slow server.
pub const BARGE_IN_DECISION_BUCKETS: [f64; 9] = [0.05, 0.1, 0.2, 0.3, 0.5, 0.75, 1.0, 2.5, 5.0];

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::{counter, gauge, histogram};

    /// Install a per-test recorder, run the closure, return the
    /// rendered `/metrics` text. `metrics::with_local_recorder`
    /// scopes the recorder to the closure so tests don't leak into
    /// each other's globals.
    fn with_recorder<F: FnOnce()>(f: F) -> String {
        let recorder = prometheus_builder().expect("builder").build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_descriptions();
            f();
        });
        handle.render()
    }

    #[test]
    fn descriptions_emit_help_lines() {
        let out = with_recorder(|| {
            // Touch each metric so it appears in the output.
            counter!(INVITES_TOTAL, "result" => "accepted").increment(1);
            counter!(CALLS_TOTAL, "cause" => "server_hangup").increment(1);
            counter!(ROUTE_MATCH_TOTAL, "route" => "default").increment(1);
            gauge!(CALLS_ACTIVE).set(1.0);
            histogram!(WS_CONNECT_SECONDS, "result" => "ok").record(0.05);
            histogram!(SDP_NEGOTIATE_SECONDS, "result" => "ok").record(0.001);
            histogram!(CALL_DURATION_SECONDS).record(42.0);
        });

        for name in [
            INVITES_TOTAL,
            CALLS_TOTAL,
            ROUTE_MATCH_TOTAL,
            CALLS_ACTIVE,
            WS_CONNECT_SECONDS,
            SDP_NEGOTIATE_SECONDS,
            CALL_DURATION_SECONDS,
        ] {
            assert!(
                out.contains(&format!("# HELP {name} ")),
                "missing HELP for {name} in:\n{out}"
            );
        }
    }

    #[test]
    fn ws_connect_seconds_renders_with_explicit_buckets_not_summary() {
        let out = with_recorder(|| {
            histogram!(WS_CONNECT_SECONDS, "result" => "ok").record(0.05);
        });
        // Histograms with set_buckets_for_metric render as
        // `_bucket{le="..."}` lines, not `quantile="..."` summaries.
        assert!(
            out.contains(&format!("{WS_CONNECT_SECONDS}_bucket")),
            "expected buckets in:\n{out}"
        );
        assert!(
            !out.contains(&format!("{WS_CONNECT_SECONDS}{{quantile")),
            "histogram unexpectedly rendered as summary"
        );
    }

    #[test]
    fn rtp_rtt_ms_renders_with_explicit_buckets_not_summary() {
        // Regression for the cosmetic 0.3.2 follow-up: rtcp_rtt_ms was
        // rendering as a summary (quantiles) because no buckets were set.
        let out = with_recorder(|| {
            histogram!(RTP_RTT_MS).record(67.1);
        });
        assert!(
            out.contains(&format!("{RTP_RTT_MS}_bucket")),
            "expected buckets in:\n{out}"
        );
        assert!(
            !out.contains(&format!("{RTP_RTT_MS}{{quantile")),
            "rtt histogram unexpectedly rendered as summary"
        );
    }

    #[test]
    fn counters_render_with_labels_intact() {
        let out = with_recorder(|| {
            counter!(INVITES_TOTAL, "result" => "accepted").increment(2);
            counter!(INVITES_TOTAL, "result" => "no_match").increment(1);
        });
        assert!(out.contains(&format!("{INVITES_TOTAL}{{result=\"accepted\"}} 2")));
        assert!(out.contains(&format!("{INVITES_TOTAL}{{result=\"no_match\"}} 1")));
    }

    #[test]
    fn gauges_render_current_value() {
        let out = with_recorder(|| {
            gauge!(CALLS_ACTIVE).increment(3.0);
            gauge!(CALLS_ACTIVE).decrement(1.0);
        });
        assert!(
            out.contains(&format!("{CALLS_ACTIVE} 2")),
            "expected gauge value 2 in:\n{out}"
        );
    }

    #[test]
    fn metric_names_have_siphon_ai_prefix() {
        // Pin the convention so a typo doesn't drift the namespace.
        for name in [
            INVITES_TOTAL,
            CALLS_TOTAL,
            ROUTE_MATCH_TOTAL,
            CALLS_ACTIVE,
            WS_CONNECT_SECONDS,
            SDP_NEGOTIATE_SECONDS,
            CALL_DURATION_SECONDS,
        ] {
            assert!(name.starts_with("siphon_ai_"), "{name} missing prefix");
        }
    }
}
