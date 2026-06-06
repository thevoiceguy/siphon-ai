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
/// `accepted`, `rejected`, `no_match`. `rejected` covers every 4xx/
/// 5xx final response from the routing layer (see
/// `siphon_ai_core::AcceptError::sip_status`).
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

/// REGISTER attempts the daemon has driven. Labeled by `name`
/// (the `[[register]].name`) and `outcome`:
/// `registered` / `auth_failed` / `transport_error` / `timeout` /
/// `rejected` (any other 4xx/5xx/6xx final response).
/// Counts the FINAL outcome of each REGISTER transaction — the
/// upstream IntegratedUAC handles 401/407 retry internally, so
/// challenges aren't counted here.
pub const REGISTER_ATTEMPTS_TOTAL: &str = "siphon_ai_register_attempts_total";

// ─── Gauges ─────────────────────────────────────────────────────────

/// Currently-active calls. Incremented when the controller spawns,
/// decremented when it exits.
pub const CALLS_ACTIVE: &str = "siphon_ai_calls_active";

/// Per-`[[register]]` registration status. Labeled by `name` and
/// `state` (`pending`/`registered`/`failed`/`disabled`); the gauge
/// is `1` for the row matching the current state and `0` for the
/// other rows of the same `name`. Lets dashboards page on
/// `siphon_ai_register_state{state="failed"} == 1` without
/// stringly-typed comparisons.
pub const REGISTER_STATE: &str = "siphon_ai_register_state";

// ─── Histograms ─────────────────────────────────────────────────────

/// Time from "spawned WS bridge task" to "WS handshake completed
/// AND `start` sent." Labeled by `result`: `ok` / `error`.
pub const WS_CONNECT_SECONDS: &str = "siphon_ai_ws_connect_seconds";

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
        "Inbound INVITEs by routing result (accepted, rejected, no_match)."
    );
    describe_counter!(
        CALLS_TOTAL,
        "Completed calls by termination cause (server_hangup, local_shutdown, bridge_ended, tap_ended)."
    );
    describe_counter!(ROUTE_MATCH_TOTAL, "Calls accepted by matched route name.");
    describe_counter!(
        VERSTAT_TOTAL,
        "STIR/SHAKEN verification outcomes by result (passed, failed, unsigned)."
    );
    describe_counter!(
        REGISTER_ATTEMPTS_TOTAL,
        "REGISTER attempts by [[register]].name and outcome."
    );
    describe_gauge!(
        CALLS_ACTIVE,
        Unit::Count,
        "Currently-running per-call controllers."
    );
    describe_gauge!(
        REGISTER_STATE,
        Unit::Count,
        "Per-[[register]] status. 1 = current state for that name; 0 = other states."
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
