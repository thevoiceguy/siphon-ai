//! Signed audit-event stream: a tamper-evident trail of admin and
//! security decisions, shipped to a SIEM.
//!
//! Distinct from lifecycle webhooks (ops automation) and CDRs (billing
//! / call detail): audit events answer *who did what* on the `[admin]`
//! surface and *what the daemon refused* on the SIP surface — the
//! events a security team needs for an incident review. Per CLAUDE.md
//! §4.7 the sinks are best-effort and never block the call path.
//!
//! ## Pipeline
//!
//! ```text
//!   admin / SIP / reload call site ─► audit::emit(AuditEvent)
//!        │  (process-global facade)
//!        ▼
//!   AuditSink ─┬─► FileSink   (append-only JSONL, on-box)
//!              └─► HttpSink   (HMAC-signed POST to a SIEM webhook)
//! ```
//!
//! - [`event::AuditEvent`] — the on-the-wire JSON shape. Schema bumps
//!   follow the lifecycle-webhook rules (CLAUDE.md §7.9): additive
//!   variants are safe, changing an existing field bumps
//!   [`event::AUDIT_VERSION`].
//! - [`sink::AuditSink`] — async trait; [`sink::NullSink`] is the
//!   default-when-not-configured, [`sink::FilteredSink`] enforces an
//!   event-name allowlist, [`sink::FanoutSink`] tees to file + webhook.
//! - [`http::HttpSink`] — HMAC-signed POST with retry + durable spool,
//!   over the shared `siphon-ai-http` transport.
//! - [`file::FileSink`] — append-only JSONL for a log shipper.
//! - [`facade`] — the process-wide `install` / `emit` entry points.

pub mod event;
pub mod facade;
pub mod file;
pub mod http;
pub mod sink;

pub use event::{
    AdminRequestEvent, AttestationRejectedEvent, AuditEvent, CertReloadEvent, ConfigReloadEvent,
    InviteRejectedEvent, SipAuthEvent, AUDIT_VERSION,
};
pub use facade::{emit, install, is_enabled};
pub use file::{FileSink, FileSinkError};
pub use http::{HttpSink, HttpSinkConfig};
pub use sink::{AuditSink, AuditSinkHandle, FanoutSink, FilteredSink, NullSink};
