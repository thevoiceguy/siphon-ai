//! Which media backend a call is running on.
//!
//! The [`CallController`](crate::call::CallController) does not care how
//! audio reaches the wire — it exchanges 20 ms PCM16LE frames over
//! plain channels and issues [`TapCommand`]s. That seam has always been
//! backend-agnostic (park-retrieve and WS-reconnect already tear it
//! down and re-plumb it at runtime); what was concrete was the *type*
//! sitting behind it.
//!
//! [`MediaLeg`] names that choice. A classic call gets
//! [`MediaLeg::Classic`] — forge-engine's RTP session, exactly as
//! before. A browser call over WS/WSS gets [`MediaLeg::WebRtc`] —
//! forge-webrtc's peer connection (`DEV_PLAN_WebRTC.md` §4.1).
//!
//! An enum rather than a trait on purpose: there are exactly two
//! backends, both known here, and the compiler then forces every new
//! controller feature to say what it means for a browser leg instead of
//! silently inheriting a default. Dynamic dispatch would buy nothing —
//! nobody outside this workspace implements a media backend.

use tokio::sync::mpsc;

use siphon_ai_media_glue::{MediaTap, MediaTapError, QualityReport, TapCommand, TapDisconnect};
use siphon_ai_recording::RecFrame;

use siphon_ai_bridge::OutgoingEvent;

/// The media backend behind one call.
pub enum MediaLeg {
    /// forge-engine RTP: every SIP call, and the only backend before
    /// the WebRTC plan.
    ///
    /// Boxed — see the note on [`MediaLeg::WebRtc`].
    Classic(Box<MediaTap>),
    /// forge-webrtc: a browser that called in over WS/WSS.
    ///
    /// Both variants are boxed: a tap is 512+ bytes of forge handles
    /// and a peer connection is ~776, so an unboxed enum would make
    /// every call carry the larger of the two backends whichever one
    /// it actually uses.
    #[cfg(feature = "webrtc")]
    WebRtc(Box<crate::webrtc_leg::WebRtcTap>),
}

impl std::fmt::Debug for MediaLeg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaLeg::Classic(_) => f.write_str("MediaLeg::Classic"),
            #[cfg(feature = "webrtc")]
            MediaLeg::WebRtc(_) => f.write_str("MediaLeg::WebRtc"),
        }
    }
}

impl From<MediaTap> for MediaLeg {
    fn from(tap: MediaTap) -> Self {
        MediaLeg::Classic(Box::new(tap))
    }
}

/// Reads a leg's live phase as a short stable string.
///
/// A closure rather than a concrete handle so the two admin registries
/// (`CallControlRegistry`, `quality_live`) can report a browser leg's
/// ICE/DTLS phase without core's non-WebRTC build depending on the
/// type that produces it — the same indirection the admin state uses
/// for every other cross-crate probe.
pub type LegPhaseProbe = std::sync::Arc<dyn Fn() -> &'static str + Send + Sync>;

impl MediaLeg {
    /// A probe for this leg's live ICE/DTLS phase, or `None` for a
    /// classic leg — which has no such phase, rather than an unknown
    /// one (`DEV_PLAN_WebRTC.md` §4.6).
    pub fn phase_probe(&self) -> Option<LegPhaseProbe> {
        match self {
            MediaLeg::Classic(_) => None,
            #[cfg(feature = "webrtc")]
            MediaLeg::WebRtc(t) => {
                let state = t.state();
                Some(std::sync::Arc::new(move || state.phase().as_str()))
            }
        }
    }

    /// The PCM rate the WS bridge sees for this call — 8 kHz or
    /// 16 kHz, whichever the negotiated codec decodes to. Both
    /// backends land inside the bridge's fixed contract.
    pub fn sample_rate(&self) -> u32 {
        match self {
            MediaLeg::Classic(t) => t.sample_rate(),
            #[cfg(feature = "webrtc")]
            MediaLeg::WebRtc(t) => t.sample_rate(),
        }
    }

    /// Per-call quality reports for the CDR (receive-side jitter/loss/
    /// MOS). The browser leg has no equivalent source yet, so it
    /// simply never publishes — the CDR's quality block stays absent
    /// rather than reporting zeros that look like a perfect call.
    pub fn with_quality_watch(self, tx: tokio::sync::watch::Sender<QualityReport>) -> Self {
        match self {
            MediaLeg::Classic(t) => MediaLeg::Classic(Box::new(t.with_quality_watch(tx))),
            #[cfg(feature = "webrtc")]
            other @ MediaLeg::WebRtc(_) => other,
        }
    }

    /// Flag the acceptor flips when the far end puts *us* on hold, so
    /// the inactivity watchdog does not fire on a legitimately silent
    /// stream (#402).
    pub fn with_peer_held(self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        match self {
            MediaLeg::Classic(t) => MediaLeg::Classic(Box::new(t.with_peer_held(flag))),
            #[cfg(feature = "webrtc")]
            other @ MediaLeg::WebRtc(_) => other,
        }
    }

    /// Flag that suppresses our outbound audio while the answered
    /// direction is `recvonly`/`inactive` (#417).
    pub fn with_tx_suppressed(self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        match self {
            MediaLeg::Classic(t) => MediaLeg::Classic(Box::new(t.with_tx_suppressed(flag))),
            #[cfg(feature = "webrtc")]
            MediaLeg::WebRtc(t) => MediaLeg::WebRtc(Box::new(t.with_tx_suppressed(flag))),
        }
    }

    /// Fork of both directions into the recorder.
    pub fn with_recording(
        self,
        fork: Option<(
            mpsc::Sender<RecFrame>,
            std::sync::Arc<std::sync::atomic::AtomicU64>,
        )>,
    ) -> Self {
        match self {
            MediaLeg::Classic(t) => MediaLeg::Classic(Box::new(t.with_recording(fork))),
            #[cfg(feature = "webrtc")]
            MediaLeg::WebRtc(t) => MediaLeg::WebRtc(Box::new(t.with_recording(fork))),
        }
    }

    /// Keep the leg alive across a WS drop (mid-call reconnect, or the
    /// failure prompt). A browser leg has no equivalent today — a
    /// browser whose WS died has lost its signalling channel too, so
    /// there is nothing to reconnect *to* — and the flag is ignored.
    pub fn with_survive_ws_drop(self, enabled: bool) -> Self {
        match self {
            MediaLeg::Classic(t) => MediaLeg::Classic(Box::new(t.with_survive_ws_drop(enabled))),
            #[cfg(feature = "webrtc")]
            other @ MediaLeg::WebRtc(_) => other,
        }
    }

    /// Run the leg to completion.
    ///
    /// The four channels are the seam: PCM16LE frames out to the WS
    /// bridge, PCM16LE frames in from it, events up to the controller,
    /// and commands down from it.
    pub async fn run(
        self,
        caller_audio_tx: mpsc::Sender<Vec<u8>>,
        playout_audio_rx: mpsc::Receiver<Vec<u8>>,
        events_tx: mpsc::Sender<OutgoingEvent>,
        cmd_rx: mpsc::Receiver<TapCommand>,
    ) -> Result<TapDisconnect, MediaTapError> {
        match self {
            MediaLeg::Classic(t) => {
                t.run(caller_audio_tx, playout_audio_rx, events_tx, cmd_rx)
                    .await
            }
            #[cfg(feature = "webrtc")]
            MediaLeg::WebRtc(t) => {
                t.run(caller_audio_tx, playout_audio_rx, events_tx, cmd_rx)
                    .await
            }
        }
    }

    /// Whether this is a browser leg — for logs, CDR, and the
    /// features that cannot apply to one.
    pub fn is_webrtc(&self) -> bool {
        match self {
            MediaLeg::Classic(_) => false,
            #[cfg(feature = "webrtc")]
            MediaLeg::WebRtc(_) => true,
        }
    }
}
