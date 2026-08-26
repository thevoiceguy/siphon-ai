//! `siphon-ai-webrtc-glue` — the browser's media leg.
//!
//! The adapter between siphon-ai's call machinery and forge-webrtc's
//! `PeerConnection`, mirroring the `sip-glue` / `media-glue` split:
//! `sip-glue` adapts siphon-rs events, `media-glue` adapts
//! forge-engine's RTP sessions, and this crate adapts forge-webrtc's
//! peer connection for calls that arrive from a browser
//! (`docs/design/DEV_PLAN_WebRTC.md` Phase 2).
//!
//! # Where this sits
//!
//! ```text
//!   INVITE over WS/WSS ──► sip-glue ──► acceptor
//!                                         │
//!                    offer is WebRTC-shaped? (detect)
//!                          ├── no  ──► media-glue  (classic RTP leg)
//!                          └── yes ──► webrtc-glue (PeerConnection)
//! ```
//!
//! Same dialog machinery either way — only the media backend differs.
//!
//! # What is here today
//!
//! [`detect`] (the plan's §4.1 leg-selection rule) and [`settings`]
//! (`[webrtc]` config → forge-webrtc's `PeerConfig`). The live leg —
//! offer/answer, ICE/DTLS lifecycle, and the Opus↔PCM16LE audio path
//! of §4.2–4.4 — lands on top of these.
//!
//! Nothing here is reachable from a default build: the daemon depends
//! on this crate behind its `webrtc` feature.

pub mod audio;
pub mod detect;
pub mod leg;
pub mod settings;

pub use audio::{InboundAudio, OutboundAudio};
pub use detect::{inspect, is_webrtc_offer, OfferShape};
/// forge-webrtc's peer/transport event stream, re-exported so
/// consumers need not depend on forge-webrtc directly.
pub use forge_webrtc::PeerEvent;
pub use leg::{Answered, WebRtcLeg};
pub use settings::{SettingsError, WebRtcSettings};

/// What can go wrong building or running a browser media leg.
#[derive(Debug, thiserror::Error)]
pub enum WebRtcGlueError {
    /// The peer connection could not be created (certificate, socket).
    #[error("WebRTC setup failed: {0}")]
    Setup(String),
    /// The browser's offer was not applicable — malformed, or missing
    /// the ICE/DTLS material a leg needs.
    #[error("WebRTC offer rejected: {0}")]
    Offer(String),
    /// No answer could be produced (typically no codec in common).
    #[error("WebRTC answer failed: {0}")]
    Answer(String),
    /// ICE or DTLS did not complete within the setup budget.
    #[error("WebRTC connect failed: {0}")]
    Connect(String),
    /// Codec construction, decode, or an unsupported bridge rate.
    #[error("WebRTC codec error: {0}")]
    Codec(String),
    /// The transport refused an outbound frame.
    #[error("WebRTC send failed: {0}")]
    Send(String),
}

pub type Result<T> = std::result::Result<T, WebRtcGlueError>;
