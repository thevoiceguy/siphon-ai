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

pub mod detect;
pub mod settings;

pub use detect::{inspect, is_webrtc_offer, OfferShape};
pub use settings::{SettingsError, WebRtcSettings};
