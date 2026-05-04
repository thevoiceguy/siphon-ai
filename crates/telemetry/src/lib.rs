//! Telemetry: tracing init, Prometheus metrics, HEP/Homer wiring, admin endpoints.
//!
//! Per CLAUDE.md §4.5 observability ships in the same PR as a feature, never
//! later. HEP emission is best-effort and never blocks the call path (§4.7).
