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
//! Every name declared here is also listed in [`ALL_COUNTERS`] /
//! [`ALL_GAUGES`] / [`ALL_HISTOGRAMS`], and the tests assert both that
//! those lists are complete and that every listed metric renders a
//! `# HELP` line. A metric can't reach `/metrics` undescribed without
//! failing a test — the gap issue #431 found (eleven documented
//! families exporting a bare `# TYPE`) was silent because coverage was
//! a hand-written list of seven.
//!
//! ## Buckets
//!
//! **Every** histogram gets explicit buckets per the CLAUDE.md guidance
//! ("histograms get sensible buckets defined explicitly; don't rely on
//! defaults") — an unbucketed histogram renders as a *summary*
//! (quantiles), which can't be aggregated across instances. Each bucket
//! array documents the range it targets; for example:
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

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, Unit};
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

/// Calls that completed (controller exited), inbound and outbound
/// (#373 — outbound legs were previously not counted). Labeled by
/// `cause` — the CDR `termination.cause` set; see DEPLOY.md.
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

/// Recordings finished (or refused), when `[recording]` is on. Labeled by
/// `result`: `ok` (written cleanly), `degraded` (some 20 ms frames were
/// dropped under writer back-pressure — the file is short, not corrupt),
/// `failed` (an I/O error), `blocked` (a configured consent announcement
/// did not play to completion — unusable file (#440) or cut short by a
/// hold/park (#445) — so capture never started). Bounded
/// cardinality (four values); the per-call outcome lives on the CDR
/// (`recording_result`, `recording_path`).
pub const RECORDINGS_TOTAL: &str = "siphon_ai_recordings_total";

/// Recording uploads to object storage (`[recording.storage]`, 0.25.0)
/// by `result`: `ok` (durably uploaded), `failed` (attempt failed, will
/// retry), `dropped` (retry budget exhausted / local file gone /
/// unreadable job — the recording stays local-only). Emitted from the
/// upload worker in `siphon-ai-http`.
pub const RECORDING_UPLOADS_TOTAL: &str = "siphon_ai_recording_uploads_total";

/// Inbound SIP packets/frames dropped by siphon-rs's per-source-IP
/// ingress rate limit (`[sip].udp_rate_limit_pps` /
/// `stream_rate_limit_fps`, 0.48.11), labeled by `transport`
/// (`udp`, `tcp`, `tls`, `ws`, `wss`, `sctp`, `tls_sctp`).
///
/// This limiter sits **below** `[sip.admission]` and applies even when
/// admission control is disabled — the drop happens in the transport
/// before the packet is parsed or counted as received, so without this
/// counter the traffic is simply gone. It was invisible until 0.48.11
/// (#459): the cap was a hard-coded 200/sec that dropped SIP with no
/// metric at all.
///
/// The source IP is deliberately **not** a label — it is unbounded
/// cardinality (trivially so on UDP, where the source is spoofable).
/// The throttled `sip_transport` WARN names the peer when you need to
/// know which one.
pub const SIP_RATE_LIMITED_TOTAL: &str = "siphon_ai_sip_rate_limited_total";

/// HEP3 packets the sink has successfully written to the wire
/// (`[hep]`, 0.12.0). Mirrors `hep_rs::UdpHepSink::sent()`, which the
/// sampler in `crate::hep` republishes; the sink owns the count
/// because SIP and RTCP chunks are emitted by `sip-hep` and
/// `forge-hep` respectively and never pass through this crate.
///
/// **Wire-level success only, and a weak liveness signal.** A
/// collector that is up but silently discarding — a black-holing NAT,
/// a heplify-server not writing to storage — still counts here.
/// Unreachable-collector failures count *nowhere*: they are neither
/// `sent` nor [`HEP_PACKETS_DROPPED_TOTAL`], because the failure is
/// detected inside the upstream worker, which has no counter for it.
///
/// Do **not** read "stops climbing" as "collector down". The sink
/// writes to a *connected* UDP socket, so a refused datagram is
/// reported as `ECONNREFUSED` on the *following* send — the queued
/// ICMP error is consumed by one send and the next finds the queue
/// empty and succeeds. Measured against a dead local collector this
/// counter therefore keeps climbing at almost exactly **half** rate
/// (35 of ~70 attempted, 0.48.10), not zero. A remote collector that
/// black-holes without ICMP at all counts 100%. The signal for a dead
/// collector is the throttled `hep_rs::udp` WARN, not this metric.
/// See `docs/HEP.md`.
pub const HEP_PACKETS_SENT_TOTAL: &str = "siphon_ai_hep_packets_sent_total";

/// HEP3 packets dropped before reaching the wire (`[hep]`, 0.12.0),
/// labeled by `reason`. Currently one value — `queue_full`, from
/// `hep_rs::UdpHepSink::drops()`: the producer's `try_send` found the
/// bounded queue (`[hep].queue_capacity`, default 256) full and
/// discarded the packet rather than block, per CLAUDE.md §4.7.
///
/// Sustained movement here means the queue is too small for the call
/// rate, not that the collector is down. `reason` is a label rather
/// than a bare counter so a future `collector_down` value can join it
/// without a rename — that one needs a send-failure counter added to
/// `hep-rs` first (siphon-ai #460).
pub const HEP_PACKETS_DROPPED_TOTAL: &str = "siphon_ai_hep_packets_dropped_total";

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
/// `result` (`ok`, `unauthenticated`, `forbidden`, `not_found`, `error`).
/// `not_found` covers both an unknown route and a handler `404` (stale
/// call/room id); `error` covers every other handler failure (400 / 409 /
/// 429 / 501 / 503 / 5xx). One
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

/// `warn!`/`error!` events captured into the recent-errors ring
/// (`GET /admin/v1/errors`, 0.49.0). Labeled by `level`: `warn` |
/// `error`. A rate spike here is itself a health signal, independent
/// of anyone reading the ring. Literal must match the call site in
/// `siphon-ai-telemetry::error_ring`.
pub const ERROR_RING_CAPTURED_TOTAL: &str = "siphon_ai_error_ring_captured_total";

/// SIP messages offered to the per-call ladder ring
/// (DESIGN_SIP_LADDER.md), by `result`: `captured`,
/// `dropped_call_cap` (the per-call message cap evicted an older
/// message from the same trace), `dropped_trace_cap` (the pending
/// bound evicted a whole least-recently-touched trace — REGISTER /
/// OPTIONS / scanner noise pushing live calls out is what this
/// catches). A rising `dropped_call_cap` says the per-call cap is
/// wrong for this deployment, or something is retransmitting.
/// Literal must match the call site in `siphon-ai-telemetry::sip_ring`.
pub const SIP_RING_MESSAGES_TOTAL: &str = "siphon_ai_sip_ring_messages_total";

/// Traces currently held by the SIP ladder ring — live calls,
/// retained completed calls, and the non-call SIP dialogs (REGISTER,
/// OPTIONS, rejected INVITEs) that also carry a Call-ID. Named
/// `traces` rather than `calls` for exactly that reason.
pub const SIP_RING_TRACES: &str = "siphon_ai_sip_ring_traces";

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
/// `no_compatible_codec` (answer selected nothing we offered),
/// `invalid_remote_media` (answer's RTP address/port unusable or stream
/// rejected), or `caller_hangup` (peer BYE'd before answering, #425).
/// Bounded cardinality. Literal must match the call site in
/// `siphon-ai-core::acceptor`.
pub const DELAYED_OFFER_TOTAL: &str = "siphon_ai_delayed_offer_total";

/// Outbound delayed-offer (offerless INVITE we sent; the peer offers
/// in its 2xx and we answer in the ACK) negotiation outcomes (issue
/// #406 — previously this path emitted no delayed-offer metric at
/// all; both live failure modes in that issue would have been
/// one-line diagnoses with it). Labeled by `result`: `answered` (our
/// ACK answer built and media bridged), `srtp_policy` (the gateway's
/// `srtp` mode refused every audio alternative the peer offered),
/// `srtp_setup` (the selected secure alternative failed to negotiate
/// or install — bad crypto/fingerprint, post-process or DTLS enable
/// failure), `invalid_remote_media` (the peer's offer was unusable —
/// parse/codec/negotiation failure), or `missing_sdp_offer` (the 2xx
/// carried no usable SDP offer). Pre-2xx failures ride
/// `siphon_ai_outbound_calls_total`, not this. Bounded cardinality.
/// Literal must match the call sites in `siphon-ai-core::outbound`.
pub const OUTBOUND_DELAYED_OFFER_TOTAL: &str = "siphon_ai_outbound_delayed_offer_total";

/// Caller-leg 20 ms frames dropped because the negotiated direction
/// forbade our send — we answered a peer hold with `recvonly` /
/// `inactive` (RFC 3264 §6.1, #417). Counts every suppressed push
/// site (WS playout, barge-in re-queues, the room mix, parked MOH and
/// announcements); a sustained rate means the WS server keeps
/// streaming through peer holds instead of pausing on the §3.3 `hold`
/// event — harmless but wasted bandwidth. No labels. Literal must
/// match the const in `siphon-ai-media-glue::tap` (same pattern as
/// the room metrics above it in that crate).
pub const PEER_HOLD_TX_SUPPRESSED_FRAMES_TOTAL: &str =
    "siphon_ai_peer_hold_tx_suppressed_frames_total";

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

/// Inbound digest-auth outcomes per challenged INVITE (0.19.0),
/// counted only for sources whose policy requires `[sip.auth]`.
/// Labeled by `result`: `ok` (a presented `Authorization` verified),
/// `challenged` (no credentials → first `401`), `failed` (credentials
/// presented but not accepted), `stale` (a known-but-expired nonce →
/// `401` with `stale=true`). Bounded cardinality; `failed` and `stale`
/// also emit an audit event. Literal must match the call site in
/// `siphon-ai-sip-glue::handler`.
pub const SIP_AUTH_TOTAL: &str = "siphon_ai_sip_auth_total";

/// Inbound INVITE admission decisions (0.19.0), counted only when
/// `[sip.admission]` is on — the first gate on a new INVITE. Labeled by
/// `result`: `accepted`, `rate_limited` (per-source rate trip or the
/// global concurrency cap → `503`), `dropped` (source flooding past
/// `drop_after` → no response at all). Literal must match the call site
/// in `siphon-ai-sip-glue::handler`.
pub const INVITE_ADMISSION_TOTAL: &str = "siphon_ai_invite_admission_total";

/// Inbound NOTIFYs answered (#357). Labeled by `result`: `accepted`
/// (`Event: refer` post-REFER progress, RFC 3515 — 200 and dropped),
/// `ignored` (`Event: message-summary` — a registrar's unsolicited MWI
/// push, absorbed with 200 and no action, #486), `bad_event` (an event
/// package we don't implement → `489`), `bad_request` (no `Event`
/// header → `400`). Literal must match the call site in
/// `siphon-ai-sip-glue::handler`.
///
/// `bad_event` is the actionable one and is meant to stay at zero:
/// MWI was split out to `ignored` precisely so a registered node's
/// once-per-REGISTER MWI stops burying it.
pub const NOTIFY_TOTAL: &str = "siphon_ai_notify_total";

/// RFC 4028 session refreshes SiphonAI sent on an outbound leg it was
/// nominated to refresh (#484). Labeled by `result`: `ok` (2xx — the
/// armed expiry is pushed out), `rejected` (a non-2xx final response;
/// the peer answered and refused, e.g. `422`/`503`), `failed` (no usable
/// response at all — timeout or transport error).
///
/// Only outbound legs can increment this: SiphonAI never refreshes an
/// inbound leg, it nominates the caller (see `docs/CONFIG.md`). A leg
/// whose callee did not nominate us never refreshes either, so on most
/// deployments this metric stays absent rather than zero.
pub const SESSION_REFRESH_TOTAL: &str = "siphon_ai_session_refresh_total";

/// Outbound session-refresh loops that stopped while the call was still
/// up (#484). Labeled by `reason`: `dialog_gone` (the peer answered
/// `408`/`481` — the dialog does not exist and retrying cannot bring it
/// back, RFC 3261 §12.2.1.2, terminal on first occurrence), `exhausted`
/// (consecutive failures hit the give-up threshold), `unresolvable` (the
/// local dialog handle disappeared under us).
///
/// **This is the alertable one.** Every increment means nothing is
/// keeping that session alive any more, so the armed RFC 4028 expiry
/// will end the call at its deadline. The refresh loop deliberately does
/// not BYE the call itself (RFC 4028 §10 suggests it, but that is the
/// application's call) — it stops and says so.
pub const SESSION_REFRESH_STOPPED_TOTAL: &str = "siphon_ai_session_refresh_stopped_total";

/// Quality-history records emitted through the `[quality]` sinks
/// (0.31.0). Labeled by `kind`: `interval` / `final`. Records skipped
/// as empty don't count. Literal must match the call site in
/// `siphon-ai-quality::facade`.
pub const QUALITY_RECORDS_TOTAL: &str = "siphon_ai_quality_records_total";

/// Times the idle detector fired `silence_detected` on the WS bridge
/// (`[bridge].silence_threshold_ms` — no *caller* audio). No labels.
/// Literal must match the call site in `siphon-ai-media-glue::tap`.
pub const SILENCE_EVENTS_TOTAL: &str = "siphon_ai_silence_events_total";

/// Times the idle detector fired `dead_air_detected` on the WS bridge
/// (`[bridge].dead_air_threshold_ms` — no audio in *either* direction).
/// No labels. Literal must match the call site in
/// `siphon-ai-media-glue::tap`.
pub const DEAD_AIR_EVENTS_TOTAL: &str = "siphon_ai_dead_air_events_total";

/// Pause-mode barge-in arbitration resolutions (0.32.0). Labeled by
/// `outcome`: `confirmed` / `rejected` / `timeout`. The companion
/// histogram is [`BARGE_IN_DECISION_SECONDS`]. Literal must match the
/// call site in `siphon-ai-media-glue::tap`.
pub const BARGE_IN_DECISIONS_TOTAL: &str = "siphon_ai_barge_in_decisions_total";

/// `SIGHUP` cert-reload attempts for the SIP/TLS listener (0.3.0), one
/// tick per attempt. Labeled by `outcome`: `ok` / `failed` (a broken
/// cert/key on disk — the listener keeps serving the previous cert).
/// Emitted from the daemon binary's reloader.
pub const SIP_TLS_RELOAD_ATTEMPTS_TOTAL: &str = "siphon_ai_sip_tls_reload_attempts_total";

/// `SIGHUP` cert-reload attempts for the `[admin.tls]` listener
/// (0.18.0) — the [`SIP_TLS_RELOAD_ATTEMPTS_TOTAL`] counterpart, same
/// `outcome` labels and same keep-the-old-cert-on-failure behavior.
/// Only emitted when `[admin.tls]` is configured.
pub const ADMIN_TLS_RELOAD_ATTEMPTS_TOTAL: &str = "siphon_ai_admin_tls_reload_attempts_total";

// ─── Gauges ─────────────────────────────────────────────────────────

/// Currently-active bridged calls, inbound and outbound (#373 —
/// outbound legs were previously not counted). Inbound legs join at
/// accept, outbound legs at answer; both leave when the controller
/// exits. Setup-phase outbound legs count only on
/// `OUTBOUND_CALLS_ACTIVE`.
pub const CALLS_ACTIVE: &str = "siphon_ai_calls_active";

/// SIP dialogs currently held in the shared `DialogManager`
/// (0.48.13). Sampled by the dialog reaper each sweep, so it is
/// self-correcting rather than incremented/decremented in pairs.
///
/// This should track `siphon_ai_calls_active` closely, lagging it by
/// the reaper's grace window — a confirmed dialog outlives its call
/// briefly so a retransmitted BYE still matches. **A value that climbs
/// with cumulative calls instead of settling is the signature of
/// siphon-ai #458**, where dialogs were inserted per call and never
/// removed; unchecked it reaches `sip-dialog`'s hard
/// `MAX_CONFIRMED_DIALOGS = 10_000`, after which `DialogManager::insert`
/// fails and in-dialog requests stop matching.
pub const DIALOGS_ACTIVE: &str = "siphon_ai_dialogs_active";

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

/// Distinct source IPs currently tracked by per-source INVITE
/// admission (0.19.0). Bounded by `[sip.admission].max_sources`; idle
/// sources are evicted, so this reads as "peers seen recently", not
/// "peers ever seen". Literal must match the call site in
/// `siphon-ai-sip-glue::handler`.
pub const INVITE_ADMISSION_SOURCES: &str = "siphon_ai_invite_admission_sources";

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

/// Outbound WS-server audio frames evicted by the PROTOCOL.md §5.5
/// window (#366): the tap buffers at most 200 ms (10 frames) of
/// outbound audio ahead of realtime and drops the **oldest** beyond
/// that. A nonzero rate means a WS server is streaming faster than
/// realtime (unpaced TTS burst, or a hostile peer). Literal must match
/// the `counter!` call site in `siphon-ai-media-glue::tap`.
pub const OUTBOUND_AUDIO_FRAMES_DROPPED_TOTAL: &str =
    "siphon_ai_outbound_audio_frames_dropped_total";

/// Outbound webhook / CDR delivery latency in **seconds** (0.11.0):
/// accepted → 2xx, recorded only on success. Labeled by `sink`
/// (`lifecycle` / `cdr`). Captures retry/backoff dwell, so a slow
/// receiver shows up as a fat tail. Emitted from `siphon-ai-http`.
pub const WEBHOOK_DELIVERY_SECONDS: &str = "siphon_ai_webhook_delivery_seconds";

/// Remote-reported RTP jitter, in **milliseconds**, sampled on every
/// `rtp_stats` emission that carries a value (cadence:
/// `[bridge].rtp_stats_interval_ms`, default 5 s). The transmit-side
/// counterpart of [`RTP_RX_JITTER_MS`]. Literal must match the
/// `histogram!` call site in `siphon-ai-media-glue::tap`; the bucket
/// matcher keys on this string.
pub const RTP_JITTER_MS: &str = "siphon_ai_rtp_jitter_ms";

/// Locally-measured interarrival jitter (RFC 3550 §6.4.1) on the
/// caller→SiphonAI stream, in **milliseconds** (0.30.0) — the
/// receive-side counterpart of [`RTP_JITTER_MS`], which is what the
/// peer reports to us. Same sampling cadence.
pub const RTP_RX_JITTER_MS: &str = "siphon_ai_rtp_rx_jitter_ms";

/// Packet-loss ratio as a **0.0–1.0 fraction** (not a percentage),
/// sampled on every `rtp_stats` emission that carries a value.
pub const RTP_PACKET_LOSS_RATIO: &str = "siphon_ai_rtp_packet_loss_ratio";

/// Transport-only MOS-CQE estimate (**1.0–5.0**, higher is better;
/// 0.30.0): a simplified E-model over local RX jitter/loss + RTCP RTT,
/// the same math heplify-server applies to HEP QoS chunks. Sampled on
/// every `rtp_stats` emission once RX data exists.
pub const RTP_MOS_ESTIMATE: &str = "siphon_ai_rtp_mos_estimate";

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
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(RTP_JITTER_MS.to_string()),
                &RTP_JITTER_MS_BUCKETS,
            )
        })
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(RTP_RX_JITTER_MS.to_string()),
                &RTP_JITTER_MS_BUCKETS,
            )
        })
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(RTP_PACKET_LOSS_RATIO.to_string()),
                &RTP_PACKET_LOSS_RATIO_BUCKETS,
            )
        })
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(RTP_MOS_ESTIMATE.to_string()),
                &RTP_MOS_ESTIMATE_BUCKETS,
            )
        })
        // The embedded forge crates emit through this same recorder, and
        // bucket registration is exporter-side — forge can only *suggest*
        // buckets by exporting consts (forge-media #102). Without these
        // two matchers the forge histograms fall back to summaries
        // (quantiles), unaggregatable across instances (#437). These are
        // the only histogram families the forge crates we consume emit;
        // referencing forge-engine's consts keeps name and buckets in
        // lockstep with the pin by construction.
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(forge_engine::metrics::M_VAD_NEURAL_INFERENCE.to_string()),
                &forge_engine::metrics::VAD_NEURAL_INFERENCE_SECONDS_BUCKETS,
            )
        })
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(forge_engine::metrics::M_TRANSCODING_DURATION.to_string()),
                &forge_engine::metrics::TRANSCODING_DURATION_SECONDS_BUCKETS,
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
    publish_zero_baselines();
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
        "Completed calls by termination cause (caller_hangup, server_hangup, local_shutdown, drain_forced, bridge_ended, ws_disconnect, tap_ended, transfer)."
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
        "Call recordings by result (ok, degraded, failed, blocked)."
    );
    describe_counter!(
        RECORDING_UPLOADS_TOTAL,
        "Recording uploads to object storage by result (ok, failed, dropped)."
    );
    describe_counter!(
        SIP_RATE_LIMITED_TOTAL,
        "Inbound SIP packets/frames dropped by the per-source-IP ingress rate limit, by transport. Sits below [sip.admission] and applies even when admission control is off."
    );
    describe_counter!(
        HEP_PACKETS_SENT_TOTAL,
        "HEP3 packets written to the wire (wire-level success only; an unreachable collector is not counted here — see the throttled hep_rs::udp WARN)."
    );
    describe_counter!(
        HEP_PACKETS_DROPPED_TOTAL,
        "HEP3 packets dropped before the wire, by reason (queue_full)."
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
        "Authenticated admin API requests, by endpoint (route template), role, and result (ok, unauthenticated, forbidden, not_found, error)."
    );
    describe_counter!(
        ERROR_RING_CAPTURED_TOTAL,
        "warn/error tracing events captured into the recent-errors ring (GET /admin/v1/errors), by level (warn, error)."
    );
    describe_counter!(
        SIP_RING_MESSAGES_TOTAL,
        "SIP messages offered to the per-call ladder ring (GET /admin/v1/calls/{id}/sip), by result (captured, dropped_call_cap, dropped_trace_cap)."
    );
    describe_counter!(
        DELAYED_OFFER_TOTAL,
        "Inbound delayed-offer (offerless INVITE) outcomes, by result (answered, ack_timeout, missing_sdp_answer, invalid_sdp_answer, no_compatible_codec, invalid_remote_media, caller_hangup)."
    );
    describe_counter!(
        OUTBOUND_DELAYED_OFFER_TOTAL,
        "Outbound delayed-offer (offerless INVITE) negotiation outcomes, by result (answered, srtp_policy, srtp_setup, invalid_remote_media, media_activate, missing_sdp_offer)."
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
    describe_counter!(
        PEER_HOLD_TX_SUPPRESSED_FRAMES_TOTAL,
        "Caller-leg 20 ms frames dropped because the answered direction (recvonly/inactive, peer hold) forbade our send (#417)."
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
        DIALOGS_ACTIVE,
        "SIP dialogs held in the shared DialogManager. Tracks calls_active, lagging it by the reaper grace window; unbounded growth is issue #458."
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
        SIP_RING_TRACES,
        Unit::Count,
        "SIP dialogs currently held by the per-call ladder ring (live calls, retained completed calls, and non-call dialogs such as REGISTER)."
    );
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
        "Deliveries waiting in the durable spool, by sink (lifecycle, cdr, audit, quality)."
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
        BARGE_IN_DECISIONS_TOTAL,
        "Pause-mode barge-in arbitration resolutions by outcome (confirmed, rejected, timeout)."
    );
    describe_counter!(
        SIP_AUTH_TOTAL,
        "Inbound digest-auth outcomes per challenged INVITE, by result (ok, challenged, failed, stale)."
    );
    describe_counter!(
        INVITE_ADMISSION_TOTAL,
        "Inbound INVITE admission decisions, by result (accepted, rate_limited, dropped)."
    );
    describe_gauge!(
        INVITE_ADMISSION_SOURCES,
        Unit::Count,
        "Distinct source IPs currently tracked by per-source INVITE admission."
    );
    describe_counter!(
        NOTIFY_TOTAL,
        "Inbound NOTIFYs answered, by result (accepted, ignored, bad_event, bad_request)."
    );
    describe_counter!(
        SESSION_REFRESH_TOTAL,
        "RFC 4028 session refreshes sent on outbound legs, by result (ok, rejected, failed)."
    );
    describe_counter!(
        SESSION_REFRESH_STOPPED_TOTAL,
        "Outbound session-refresh loops that gave up while the call was still up, by reason (dialog_gone, exhausted, unresolvable)."
    );
    describe_counter!(
        QUALITY_RECORDS_TOTAL,
        "Quality-history records emitted through the [quality] sinks, by kind (interval, final)."
    );
    describe_counter!(
        SILENCE_EVENTS_TOTAL,
        "Times silence_detected fired on the WS bridge ([bridge].silence_threshold_ms)."
    );
    describe_counter!(
        DEAD_AIR_EVENTS_TOTAL,
        "Times dead_air_detected fired on the WS bridge ([bridge].dead_air_threshold_ms)."
    );
    describe_histogram!(
        RTP_JITTER_MS,
        Unit::Milliseconds,
        "Remote-reported RTP jitter (ms), sampled on every rtp_stats emission."
    );
    describe_histogram!(
        RTP_RX_JITTER_MS,
        Unit::Milliseconds,
        "Locally-measured interarrival jitter (ms, RFC 3550 6.4.1) on the caller-to-SiphonAI stream."
    );
    describe_histogram!(
        RTP_PACKET_LOSS_RATIO,
        "Packet-loss ratio (0.0-1.0), sampled on every rtp_stats emission."
    );
    describe_histogram!(
        RTP_MOS_ESTIMATE,
        "Transport-only MOS-CQE estimate (1.0-5.0) from local RX jitter/loss + RTCP RTT."
    );
    describe_counter!(
        SIP_TLS_RELOAD_ATTEMPTS_TOTAL,
        "SIGHUP SIP/TLS cert-reload attempts, by outcome (ok, failed — previous cert kept)."
    );
    describe_counter!(
        ADMIN_TLS_RELOAD_ATTEMPTS_TOTAL,
        "SIGHUP [admin.tls] cert-reload attempts, by outcome (ok, failed — previous cert kept)."
    );
    describe_counter!(
        OUTBOUND_AUDIO_FRAMES_DROPPED_TOTAL,
        "Outbound WS audio frames dropped (oldest-first) by the 200 ms playout window (PROTOCOL.md \u{a7}5.5)."
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
        "Outbound deliveries by sink (lifecycle, cdr, audit, quality) and result (delivered, rejected, dropped, spooled)."
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

/// Publish a zero for the counters whose *healthy* value is zero, so
/// that "nothing has gone wrong" is a fact you can read rather than an
/// absence you have to infer (siphon-ai #474).
///
/// `describe_*!` only registers `# HELP`; a series does not exist until
/// something increments it. For most counters that is fine — nobody
/// alerts on the absence of `transfers_total`, and `absent()` covers
/// it. It is not fine when zero is the good value and any movement is
/// the alert, because then "no data" and "healthy" render identically
/// and the transition you care about is the one you cannot see. That
/// is what made the load plan's Playout SLO unverifiable across two
/// tier-1 runs: `outbound_audio_frames_dropped_total` was simply
/// missing from `/metrics` on every clean run.
///
/// Same fix as [`crate::transport`] applies to
/// [`SIP_RATE_LIMITED_TOTAL`] (#464).
///
/// **Unlabeled counters only.** Publishing a labeled series means
/// choosing which label values exist before anything has happened —
/// `ROOM_FRAMES_DROPPED_TOTAL` would need a `stage` × `side` matrix —
/// and inventing series that may never apply is worse than the absence
/// this fixes. Labeled drop counters are published by the code that
/// owns their label space, the way the HEP sampler does.
///
/// Public so tests can call it inside a `with_local_recorder` scope.
pub fn publish_zero_baselines() {
    counter!(OUTBOUND_AUDIO_FRAMES_DROPPED_TOTAL).absolute(0);
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

/// Buckets for `rtp_jitter_ms` and `rtp_rx_jitter_ms`, in
/// **milliseconds** (both measure the same quantity from opposite
/// ends, so they share a scale). Healthy VoIP sits under one frame
/// time (20 ms); 30–100 ms is where the jitter buffer starts spending
/// its budget; the 500 ms top keeps a pathological path visible
/// instead of clipped into +Inf.
pub const RTP_JITTER_MS_BUCKETS: [f64; 10] =
    [1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 50.0, 100.0, 200.0, 500.0];

/// Buckets for `rtp_packet_loss_ratio`, a **0.0–1.0 fraction** (not a
/// percentage — 0.01 is 1 %). The bottom (0.1 %) is inaudible, callers
/// start noticing around 1–5 %, and 1.0 closes the scale so a fully
/// dead stream lands in a real bucket.
pub const RTP_PACKET_LOSS_RATIO_BUCKETS: [f64; 9] =
    [0.001, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0];

/// Buckets for `rtp_mos_estimate` (MOS-CQE, **1.0–5.0**, higher is
/// better). Cut on the conventional quality bands — below 2.6 poor,
/// 2.6–3.1 low, 3.1–3.6 medium, 3.6–4.0 high, above 4.0 best — so a
/// bucket boundary is a band boundary. The 5.0 top bounds the scale.
pub const RTP_MOS_ESTIMATE_BUCKETS: [f64; 8] = [1.5, 2.0, 2.6, 3.1, 3.6, 4.0, 4.4, 5.0];

/// Every counter this module declares. Exhaustive by test
/// (`every_metric_const_is_listed`), which is what keeps the
/// `# HELP` coverage test honest as metrics are added (#431).
pub const ALL_COUNTERS: &[&str] = &[
    INVITES_TOTAL,
    CALLS_TOTAL,
    CALLS_DRAIN_FORCED_TOTAL,
    ROUTE_MATCH_TOTAL,
    VERSTAT_TOTAL,
    RECORDINGS_TOTAL,
    RECORDING_UPLOADS_TOTAL,
    SIP_RATE_LIMITED_TOTAL,
    HEP_PACKETS_SENT_TOTAL,
    HEP_PACKETS_DROPPED_TOTAL,
    REGISTER_ATTEMPTS_TOTAL,
    REGISTER_ADMIN_TRIGGERS_TOTAL,
    OUTBOUND_CALLS_TOTAL,
    OUTBOUND_SRTP_TOTAL,
    ADMIN_REQUESTS_TOTAL,
    METRICS_REQUESTS_TOTAL,
    ERROR_RING_CAPTURED_TOTAL,
    SIP_RING_MESSAGES_TOTAL,
    WS_FAILURE_PROMPTS_TOTAL,
    DELAYED_OFFER_TOTAL,
    OUTBOUND_DELAYED_OFFER_TOTAL,
    PEER_HOLD_TX_SUPPRESSED_FRAMES_TOTAL,
    OUTBOUND_AUDIO_FRAMES_DROPPED_TOTAL,
    TRANSFERS_TOTAL,
    CONFERENCE_JOINS_TOTAL,
    ROOM_FRAMES_DROPPED_TOTAL,
    PARKS_TOTAL,
    RETRIEVES_TOTAL,
    HOLDS_TOTAL,
    WS_RECONNECTS_TOTAL,
    CONFIG_RELOADS_TOTAL,
    WEBHOOK_DELIVERIES_TOTAL,
    WEBHOOK_DELIVERY_ATTEMPTS_TOTAL,
    SIP_AUTH_TOTAL,
    INVITE_ADMISSION_TOTAL,
    NOTIFY_TOTAL,
    SESSION_REFRESH_TOTAL,
    SESSION_REFRESH_STOPPED_TOTAL,
    QUALITY_RECORDS_TOTAL,
    SILENCE_EVENTS_TOTAL,
    DEAD_AIR_EVENTS_TOTAL,
    BARGE_IN_DECISIONS_TOTAL,
    SIP_TLS_RELOAD_ATTEMPTS_TOTAL,
    ADMIN_TLS_RELOAD_ATTEMPTS_TOTAL,
];

/// Every gauge this module declares. See [`ALL_COUNTERS`].
pub const ALL_GAUGES: &[&str] = &[
    CALLS_ACTIVE,
    DIALOGS_ACTIVE,
    OUTBOUND_CALLS_ACTIVE,
    REGISTER_STATE,
    CONFERENCES_ACTIVE,
    SIP_RING_TRACES,
    CONFERENCE_PARTICIPANTS,
    PARKED_CALLS_ACTIVE,
    DRAINING,
    WEBHOOK_SPOOL_DEPTH,
    RECORDING_UPLOAD_SPOOL_DEPTH,
    INVITE_ADMISSION_SOURCES,
];

/// Every histogram this module declares. See [`ALL_COUNTERS`]. Each of
/// these also has explicit buckets registered in
/// [`prometheus_builder`] — pinned by
/// `every_histogram_renders_with_buckets_not_a_summary`.
pub const ALL_HISTOGRAMS: &[&str] = &[
    WS_CONNECT_SECONDS,
    SDP_NEGOTIATE_SECONDS,
    CALL_DURATION_SECONDS,
    RECORDING_UPLOAD_SECONDS,
    RTP_RTT_MS,
    RTP_JITTER_MS,
    RTP_RX_JITTER_MS,
    RTP_PACKET_LOSS_RATIO,
    RTP_MOS_ESTIMATE,
    ROOM_TICK_LAG_SECONDS,
    DRAIN_SECONDS,
    BARGE_IN_DECISION_SECONDS,
    WEBHOOK_DELIVERY_SECONDS,
];

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

    /// #474: a counter whose healthy value is zero has to be *present*
    /// at zero, or a clean run and an uninstrumented build render the
    /// same and the SLO built on it can never be asserted.
    #[test]
    fn zero_is_good_counters_are_published_before_anything_goes_wrong() {
        // Guard: describing a metric must not be enough to create the
        // series. If this ever stops holding, the assertion below would
        // pass for free and prove nothing.
        let bare = with_recorder(|| {});
        assert!(
            !bare.contains(OUTBOUND_AUDIO_FRAMES_DROPPED_TOTAL),
            "describe_* alone created {OUTBOUND_AUDIO_FRAMES_DROPPED_TOTAL}; \
             this test no longer proves anything:\n{bare}"
        );

        let out = with_recorder(publish_zero_baselines);
        assert!(
            out.contains(&format!("{OUTBOUND_AUDIO_FRAMES_DROPPED_TOTAL} 0")),
            "expected a zero baseline for {OUTBOUND_AUDIO_FRAMES_DROPPED_TOTAL} in:\n{out}"
        );
    }

    /// The baseline must not clobber a real count — `absolute(0)` runs
    /// at install, before any call exists, but a later increment wins.
    #[test]
    fn zero_baseline_does_not_suppress_real_drops() {
        let out = with_recorder(|| {
            publish_zero_baselines();
            counter!(OUTBOUND_AUDIO_FRAMES_DROPPED_TOTAL).increment(7);
        });
        assert!(
            out.contains(&format!("{OUTBOUND_AUDIO_FRAMES_DROPPED_TOTAL} 7")),
            "expected 7 after the baseline in:\n{out}"
        );
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
        for &name in ALL_COUNTERS
            .iter()
            .chain(ALL_GAUGES.iter())
            .chain(ALL_HISTOGRAMS.iter())
        {
            assert!(name.starts_with("siphon_ai_"), "{name} missing prefix");
        }
    }

    #[test]
    fn every_declared_metric_renders_a_help_line() {
        // The #431 regression: eleven metrics emitted from other crates
        // were never described here, so they exported a bare `# TYPE`
        // with no description while DEPLOY.md documented them. Touching
        // every declared name and demanding HELP makes that unshippable.
        let out = with_recorder(|| {
            for &name in ALL_COUNTERS {
                counter!(name).increment(1);
            }
            for &name in ALL_GAUGES {
                gauge!(name).set(1.0);
            }
            for &name in ALL_HISTOGRAMS {
                histogram!(name).record(1.0);
            }
        });
        for &name in ALL_COUNTERS
            .iter()
            .chain(ALL_GAUGES.iter())
            .chain(ALL_HISTOGRAMS.iter())
        {
            assert!(
                out.contains(&format!("# HELP {name} ")),
                "missing HELP for {name} in:\n{out}"
            );
        }
    }

    #[test]
    fn every_metric_const_is_listed() {
        // The coverage test above only sees what the ALL_* lists name,
        // so the lists themselves need pinning: scan this module's own
        // source (everything above the test module, where every
        // `"siphon_ai_…"` literal is a const declaration) and require
        // the two sets to agree in both directions.
        let src = include_str!("metrics.rs");
        let decls = src.split("#[cfg(test)]").next().expect("module source");
        let mut declared: Vec<&str> = Vec::new();
        let mut rest = decls;
        while let Some(idx) = rest.find("\"siphon_ai_") {
            let after = &rest[idx + 1..];
            let end = after.find('"').expect("unterminated metric name literal");
            declared.push(&after[..end]);
            rest = &after[end..];
        }
        assert!(
            declared.len() > 40,
            "source scan found only {} names — did the declaration style change?",
            declared.len()
        );

        let listed: Vec<&str> = ALL_COUNTERS
            .iter()
            .chain(ALL_GAUGES.iter())
            .chain(ALL_HISTOGRAMS.iter())
            .copied()
            .collect();
        for name in &declared {
            assert!(
                listed.contains(name),
                "{name} is declared in this module but missing from ALL_COUNTERS / ALL_GAUGES / ALL_HISTOGRAMS"
            );
        }
        for name in &listed {
            assert!(
                declared.contains(name),
                "{name} is listed in an ALL_* array but not declared as a const here"
            );
        }
    }

    #[test]
    fn every_histogram_renders_with_buckets_not_a_summary() {
        // CLAUDE.md §7.4 and DEPLOY.md both promise explicit buckets on
        // every histogram. Four rtp_* histograms didn't have them and
        // rendered as summaries — unaggregatable across instances
        // (#431) — while their sibling rtp_rtt_ms, recorded three lines
        // away, did.
        for &name in ALL_HISTOGRAMS {
            let out = with_recorder(|| {
                histogram!(name).record(1.0);
            });
            assert!(
                out.contains(&format!("{name}_bucket")),
                "{name} has no buckets registered in prometheus_builder():\n{out}"
            );
            assert!(
                !out.contains(&format!("{name}{{quantile")),
                "{name} rendered as a summary:\n{out}"
            );
        }
    }

    #[test]
    fn forge_histograms_render_with_buckets_not_summaries() {
        // Same promise as above, for the forge families that emit
        // through our recorder (#437): forge-media #102 exports
        // suggested buckets, but only prometheus_builder() can apply
        // them. Kept out of ALL_HISTOGRAMS — that list is pinned to the
        // `siphon_ai_` namespace by the source-scan test.
        for &name in &[
            forge_engine::metrics::M_VAD_NEURAL_INFERENCE,
            forge_engine::metrics::M_TRANSCODING_DURATION,
        ] {
            let out = with_recorder(|| {
                histogram!(name).record(0.001);
            });
            assert!(
                out.contains(&format!("{name}_bucket")),
                "{name} has no buckets registered in prometheus_builder():\n{out}"
            );
            assert!(
                !out.contains(&format!("{name}{{quantile")),
                "{name} rendered as a summary:\n{out}"
            );
        }
    }
}
