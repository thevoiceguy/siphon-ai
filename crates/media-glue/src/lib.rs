//! Bidirectional audio tap on top of forge-engine.
//!
//! See `crates/media-glue/src/tap.rs` for the implementation and
//! `docs/SPIKE_MEDIA_TAP.md` for the design notes that justify it.
//!
//! Per CLAUDE.md §4.3 the audio hot path is sacred: no allocations in
//! the steady-state frame loop beyond what the codec/wire format
//! mandate, no `unwrap`/`panic`, no `std::sync::Mutex`, no blocking I/O.

pub(crate) mod idle;
pub mod sdp;
pub mod setup;
pub mod tap;

pub use sdp::{
    audio_remote_addr, build_answer, negotiate_answer, parse_offer, AnswerOutcome, Codec,
    LocalCapabilities, MediaDirection, SdpError,
};
pub use setup::{InboundAccepted, InboundCall, MediaSetup, SetupError};
pub use tap::{BargeInAction, MediaTap, MediaTapError, TapCommand, TapDisconnect};
