//! Call Detail Records: schema, file sink, webhook sink.
//!
//! ## Pipeline
//!
//! ```text
//!   CallController → CallOutcome ─► (acceptor stitches in start
//!                                     context) ─► CdrRecord ─► CdrSink ─► …
//! ```
//!
//! - [`schema::CdrRecord`] is the JSON shape consumers parse. Schema
//!   changes follow CLAUDE.md §7.7: bump [`schema::CDR_VERSION`] for
//!   anything that could break a parser, additive optional fields
//!   are fine without a bump.
//! - [`sink::CdrSink`] is the trait every emitter implements;
//!   [`sink::NullSink`] is the default-when-not-configured, and
//!   [`sink::MultiSink`] fans out (panic-isolated) to multiple
//!   sinks.
//! - [`file::FileSink`] writes JSONL with append-on-startup
//!   semantics.
//! - [`webhook::WebhookSink`] POSTs JSON to a URL with retry.
//!
//! Per CLAUDE.md §4.7, sinks are best-effort — failures log and
//! drop the record rather than blocking the per-call task or
//! propagating panics.

pub mod file;
pub mod hep;
pub mod schema;
pub mod sink;
pub mod webhook;

pub use file::{FileSink, FileSinkError};
pub use hep::HepCdrSink;
pub use schema::{
    AudioInfo, CdrRecord, Direction, HoldInfo, ParkInfo, TerminationCause, TerminationInfo,
    CDR_VERSION,
};
pub use sink::{CdrSink, CdrSinkHandle, MultiSink, NullSink};
pub use webhook::{WebhookSink, WebhookSinkConfig};
