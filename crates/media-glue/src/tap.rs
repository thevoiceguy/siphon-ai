//! Bidirectional audio tap on top of forge-engine's `MediaBridgeManager`.
//!
//! This is the integration point identified by the Week-1 spike
//! (`docs/SPIKE_MEDIA_TAP.md`): one tap per call, attached to forge's
//! `MediaBridgeManager`, exposing PCM16-LE byte channels the
//! `CallController` wires straight into the bridge crate.
//!
//! ## What flows where
//!
//! ```text
//!  caller's voice                                        WS server
//!       │                                                    ▲
//!       ▼                                                    │
//!   forge RTP recv ─► InboundMediaFrame ─► Reframer (20 ms) ─┘
//!                                          ─► pack_pcm16_le ─► caller_audio_tx
//!
//!  WS server                                          caller's ear
//!       │                                                    ▲
//!       ▼                                                    │
//!  playout_audio_rx ─► unpack_pcm16_le ─► OutboundMediaFrame ─► forge RTP send
//!                                         (target = MediaTarget::A)
//! ```
//!
//! ## Single-leg model
//!
//! Per the spike (§ "Single-leg vs two-leg"), forge models calls as
//! 2-legged. SiphonAI is single-leg — there's the SIP caller and a WS
//! server with no second SIP participant. We always inject playout to
//! `MediaTarget::A` (the SIP caller); a synthetic-or-quiet `B` stays
//! silent and never produces inbound frames.
//!
//! ## Hot path
//!
//! Per CLAUDE.md §4.3, the steady-state pump:
//! - Holds no `std::sync::Mutex` on the audio path.
//! - Does no blocking I/O — `select!` yields to the reactor.
//! - Allocates one `Vec<i16>` and one `Vec<u8>` per outbound frame
//!   (the `Reframer` pop and the PCM16 pack); see `siphon-ai-bridge`'s
//!   `audio` module for the budget breakdown.
//! - Validates that forge's `sample_rate` matches what the controller
//!   committed to in `start.audio.sample_rate`. A mid-call codec
//!   switch that changes the rate yields a fatal `SampleRateMismatch`.
//!
//! ## Not yet wired
//!
//! - DTMF / VAD events (forge `ForgeEvent` broadcast bus → channel to
//!   the controller). Tracked as a follow-up; the audio path here is
//!   independent.

use std::sync::Arc;

use forge_core::{CallId, ForgeError};
use forge_engine::{
    MediaBridgeHandle, MediaBridgeManager, MediaTarget, OutboundMediaFrame, PlayoutMode,
};
use siphon_ai_bridge::{pack_pcm16_le, unpack_pcm16_le, AudioError, Reframer};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, instrument, warn};

#[derive(Debug, Error)]
pub enum MediaTapError {
    /// `MediaBridgeManager::attach_call` rejected (already attached).
    #[error("forge attach failed: {0}")]
    AttachFailed(String),

    /// forge's `send_audio` returned an error (bridge channel closed,
    /// resource limit, etc.).
    #[error("forge playout failed: {0}")]
    PlayoutFailed(String),

    /// forge handed us a frame whose sample rate doesn't match what the
    /// tap was configured for.
    #[error("sample rate mismatch: expected {expected} Hz, got {got} Hz")]
    SampleRateMismatch { expected: u32, got: u32 },

    /// The controller-supplied `playout_audio_rx` carried bytes that
    /// don't form a valid PCM16-LE buffer.
    #[error(transparent)]
    Audio(#[from] AudioError),
}

/// Why the tap pump exited cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapDisconnect {
    /// forge's inbound stream returned `None` — the SIP-side media
    /// session ended (call hung up).
    CallEnded,
    /// The controller dropped at least one of the audio channels.
    /// Equivalent to "the bridge is gone, so there's no point reading
    /// or writing audio anymore".
    ControllerHungUp,
}

/// Bidirectional audio tap attached to one call's `MediaBridgeHandle`.
///
/// Construct via [`MediaTap::attach`], then drive with [`MediaTap::run`].
pub struct MediaTap {
    handle: MediaBridgeHandle,
    sample_rate: u32,
    call_id: CallId,
}

impl std::fmt::Debug for MediaTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // forge's `MediaBridgeHandle` doesn't impl Debug; redact it.
        f.debug_struct("MediaTap")
            .field("call_id", &self.call_id)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl MediaTap {
    /// Attach a new tap for `call_id` against the process-wide
    /// `MediaBridgeManager`.
    ///
    /// Fails with [`MediaTapError::AttachFailed`] if a tap is already
    /// attached for the same call (forge enforces 1 handle per call).
    /// Also fails on an unsupported `sample_rate` per the bridge
    /// audio module's rules (8 kHz or 16 kHz only in v1).
    pub fn attach(
        manager: &Arc<MediaBridgeManager>,
        call_id: CallId,
        sample_rate: u32,
    ) -> Result<Self, MediaTapError> {
        // Validate sample rate up front so we don't attach a handle
        // we'd immediately have to detach.
        let _ = siphon_ai_bridge::samples_per_frame(sample_rate)?;
        let handle = manager
            .attach_call(call_id.clone())
            .map_err(forge_attach_err)?;
        Ok(Self {
            handle,
            sample_rate,
            call_id,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Drive the bidirectional pump until the call ends or the
    /// controller drops its channels.
    ///
    /// Channels:
    /// - `caller_audio_tx`: each completed 20 ms PCM16-LE frame from
    ///   the SIP caller is sent here (the bridge crate forwards into
    ///   the WS as a binary frame).
    /// - `playout_audio_rx`: bytes the bridge crate received from the
    ///   WS server arrive here; the tap unpacks and hands them to
    ///   forge for playout into the SIP leg.
    #[instrument(skip_all, fields(call_id = %self.call_id, sample_rate = self.sample_rate))]
    pub async fn run(
        mut self,
        caller_audio_tx: mpsc::Sender<Vec<u8>>,
        mut playout_audio_rx: mpsc::Receiver<Vec<u8>>,
    ) -> Result<TapDisconnect, MediaTapError> {
        let mut reframer = Reframer::new(self.sample_rate)?;

        loop {
            tokio::select! {
                biased;

                // Controller dropped the caller_audio_tx receiver →
                // tear down immediately, even if no inbound frame is
                // pending.
                _ = caller_audio_tx.closed() => {
                    debug!("caller_audio_tx receiver dropped; ending tap");
                    return Ok(TapDisconnect::ControllerHungUp);
                }

                // Server → caller: bytes from the WS into forge's playout.
                playout = playout_audio_rx.recv() => {
                    let Some(bytes) = playout else {
                        debug!("playout_audio_rx closed; ending tap");
                        return Ok(TapDisconnect::ControllerHungUp);
                    };
                    let samples = unpack_pcm16_le(&bytes)?;
                    let frame = OutboundMediaFrame {
                        target: MediaTarget::A,
                        sample_rate: self.sample_rate,
                        samples,
                        playback_id: None,
                        mode: PlayoutMode::Append,
                    };
                    self.handle
                        .send_audio(frame)
                        .await
                        .map_err(|e| MediaTapError::PlayoutFailed(e.to_string()))?;
                }

                // Caller → server: PCM16 samples from forge, reframed and packed.
                inbound = self.handle.recv_frame() => {
                    let Some(frame) = inbound else {
                        debug!("forge handle stream ended");
                        return Ok(TapDisconnect::CallEnded);
                    };
                    if frame.sample_rate != self.sample_rate {
                        warn!(
                            expected = self.sample_rate,
                            got = frame.sample_rate,
                            "sample rate mismatch on inbound frame; tearing down tap"
                        );
                        return Err(MediaTapError::SampleRateMismatch {
                            expected: self.sample_rate,
                            got: frame.sample_rate,
                        });
                    }
                    reframer.push(&frame.samples);
                    while let Some(samples) = reframer.pop_frame() {
                        let bytes = pack_pcm16_le(&samples);
                        if caller_audio_tx.send(bytes).await.is_err() {
                            debug!("caller_audio_tx closed; ending tap");
                            return Ok(TapDisconnect::ControllerHungUp);
                        }
                    }
                }
            }
        }
    }
}

fn forge_attach_err(e: ForgeError) -> MediaTapError {
    MediaTapError::AttachFailed(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_validates_sample_rate_first() {
        // We don't need a manager for the failure path: validation
        // happens before attach_call is invoked.
        let manager = Arc::new(MediaBridgeManager::new());
        let err = MediaTap::attach(&manager, CallId::new("c"), 48000).unwrap_err();
        assert!(matches!(err, MediaTapError::Audio(_)));
    }

    #[test]
    fn attach_succeeds_at_supported_rate_and_drops_clean() {
        let manager = Arc::new(MediaBridgeManager::new());
        let call = CallId::new("c");
        {
            let tap = MediaTap::attach(&manager, call.clone(), 8000).expect("attach");
            assert_eq!(tap.sample_rate(), 8000);
            assert_eq!(tap.call_id(), &call);
            assert!(manager.has_bridge(&call));
        }
        // Tap dropped → handle dropped → forge auto-detaches.
        assert!(!manager.has_bridge(&call));
    }

    #[test]
    fn attach_twice_for_same_call_fails() {
        let manager = Arc::new(MediaBridgeManager::new());
        let call = CallId::new("c");
        let _t1 = MediaTap::attach(&manager, call.clone(), 8000).expect("first attach");
        let err = MediaTap::attach(&manager, call, 8000).unwrap_err();
        assert!(matches!(err, MediaTapError::AttachFailed(_)));
    }
}
