//! Lifecycle webhooks: out-of-band HTTP POST events for ops
//! automation.
//!
//! Distinct from the per-call WebSocket bridge — these are
//! fire-and-forget notifications operators wire up to Slack,
//! billing, on-call paging, etc. Per CLAUDE.md §4.7 sinks are
//! best-effort and never block the call path.
//!
//! ## Pipeline
//!
//! ```text
//!   BridgingAcceptor ─► WebhookEvent ─► WebhookSink ─► …
//! ```
//!
//! - [`event::WebhookEvent`] — the on-the-wire JSON shape. Schema
//!   bumps follow CLAUDE.md §7.9.
//! - [`sink::WebhookSink`] — async trait every emitter implements;
//!   [`sink::NullSink`] is the default-when-not-configured;
//!   [`sink::FilteredSink`] enforces an event-name allowlist.
//! - [`http::HttpSink`] — POST + retry with exponential backoff.

pub mod event;
pub mod http;
pub mod sink;

pub use event::{
    CallEndEvent, CallStartEvent, OutboundAnsweredEvent, OutboundFailedEvent,
    OutboundInitiatedEvent, RegistrationStateChangedEvent, WebhookEvent, WEBHOOK_VERSION,
};
pub use http::{HttpSink, HttpSinkConfig};
pub use sink::{FilteredSink, NullSink, WebhookSink, WebhookSinkHandle};
