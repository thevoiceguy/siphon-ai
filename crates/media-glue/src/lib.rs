//! Bidirectional audio tap on top of forge-engine.
//!
//! See `crates/media-glue/src/tap.rs` for the implementation and
//! `docs/design/SPIKE_MEDIA_TAP.md` for the design notes that justify it.
//!
//! Per CLAUDE.md §4.3 the audio hot path is sacred: no allocations in
//! the steady-state frame loop beyond what the codec/wire format
//! mandate, no `unwrap`/`panic`, no `std::sync::Mutex`, no blocking I/O.

pub(crate) mod idle;
pub mod moh;
pub mod room;
pub(crate) mod rtp_stats;
pub mod sdp;
pub mod setup;
pub mod tap;

pub use moh::{AnnounceSource, MohSource};
pub use room::{
    spawn_room, RoomConfig, RoomEvent, RoomHandle, RoomJoinError, RoomLifecycle, RoomMembership,
    RoomObserver,
};
pub use sdp::{
    audio_remote_addr, build_answer, generate_offer, negotiate_answer, negotiate_offer_answer,
    parse_offer, rewrite_sdp_direction, AnswerOutcome, Codec, LocalCapabilities, MediaDirection,
    SdpError,
};
pub use setup::{
    InboundAccepted, InboundCall, MediaSetup, OutboundAccepted, OutboundOffer,
    OutboundOfferRequest, OutboundSrtp, SetupError, TapOptions,
};
pub use tap::{BargeInAction, MediaTap, MediaTapError, TapCommand, TapDisconnect};
