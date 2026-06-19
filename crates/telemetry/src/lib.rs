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

pub mod admin;
pub mod auth;
pub mod hep;
pub mod http;
pub mod log_filter;
pub mod metrics;
pub mod readiness;

pub use admin::{
    AddParticipantRequest, AdminCallRegistry, AdminConference, AdminOutbound, AdminPark,
    AdminState, CallRegistryHandle, ConferenceAdminError, ConferenceAdminHandle, ConferenceRow,
    CreateConferenceRequest, OriginateRejection, OriginateRequest, OutboundOriginateHandle,
    ParkAdminError, ParkAdminHandle, ParkRequest, ParkedRow, RegistrationRow, RetrieveRequest,
};
pub use auth::{AdminAuth, AdminToken, AuthReject, Role};
pub use hep::{HepBuildError, HepTelemetry, HepTelemetryBuild, HepWorkerHandle};
pub use http::{AdminServer, ObservabilityServer};
pub use log_filter::{LogFilterError, LogFilterHandle};

// Re-exports for the daemon binary so it doesn't need a second
// direct dep on `metrics-exporter-prometheus`.
pub use metrics::{
    install_recorder, prometheus_builder, register_descriptions, InitError, ADMIN_REQUESTS_TOTAL,
    CALLS_ACTIVE, CALLS_TOTAL, CALL_DURATION_BUCKETS, CALL_DURATION_SECONDS, CONFIG_RELOADS_TOTAL,
    DELAYED_OFFER_TOTAL, HOLDS_TOTAL, INVITES_TOTAL, OUTBOUND_CALLS_ACTIVE, OUTBOUND_CALLS_TOTAL,
    OUTBOUND_SRTP_TOTAL, PARKED_CALLS_ACTIVE, PARKS_TOTAL, RECORDINGS_TOTAL,
    REGISTER_ATTEMPTS_TOTAL, REGISTER_STATE, RETRIEVES_TOTAL, ROUTE_MATCH_TOTAL,
    SDP_NEGOTIATE_BUCKETS, SDP_NEGOTIATE_SECONDS, TRANSFERS_TOTAL, VERSTAT_TOTAL,
    WEBHOOK_DELIVERIES_TOTAL, WEBHOOK_DELIVERY_ATTEMPTS_TOTAL, WEBHOOK_DELIVERY_SECONDS,
    WEBHOOK_SPOOL_DEPTH, WS_CONNECT_BUCKETS, WS_CONNECT_SECONDS, WS_RECONNECTS_TOTAL,
};
pub use metrics_exporter_prometheus::PrometheusHandle;
pub use readiness::ReadinessFlag;
