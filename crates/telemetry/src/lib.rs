//! Telemetry: Prometheus metrics + observability HTTP endpoints.
//!
//! Per CLAUDE.md §4.5 observability ships in the same PR as a
//! feature, never later. This crate owns the operator-facing surface:
//!
//! - [`metrics`] — names, descriptions, histogram buckets, recorder
//!   install. Other crates increment via the `metrics` facade
//!   (`metrics::counter!`/`gauge!`/`histogram!`) using the agreed
//!   `siphon_ai_*` names exported from this module.
//! - [`readiness`] — process-wide readiness flag the daemon flips on
//!   once the SIP transport is bound.
//! - [`http`] — `/health`, `/ready`, `/metrics` over hyper.
//!
//! ## What's NOT here yet
//!
//! - HEP/Homer wiring (see CLAUDE.md §3.5; depends on the upstream
//!   `hep-rs` crate).
//! - OpenTelemetry / OTLP traces.
//! - Dynamic log-level admin endpoint.
//! - Per-call HEP correlation chunks.

pub mod http;
pub mod metrics;
pub mod readiness;

pub use http::ObservabilityServer;
pub use metrics::{
    install_recorder, prometheus_builder, register_descriptions, InitError, CALLS_ACTIVE,
    CALLS_TOTAL, CALL_DURATION_BUCKETS, CALL_DURATION_SECONDS, INVITES_TOTAL, ROUTE_MATCH_TOTAL,
    SDP_NEGOTIATE_BUCKETS, SDP_NEGOTIATE_SECONDS, WS_CONNECT_BUCKETS, WS_CONNECT_SECONDS,
};
pub use readiness::ReadinessFlag;
