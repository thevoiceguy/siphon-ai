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
//! ## DTMF events
//!
//! The tap subscribes to the daemon-wide [`EventBus`] forge publishes
//! to. When a [`ForgeEvent::DtmfDigitDetected`] for *this* call's
//! `call_id` arrives — and only on the `End` of the press, so each
//! press maps to one WS event with a final `duration_ms` — we forward
//! it through the supplied [`OutgoingEvent`] sender as
//! `OutgoingEvent::Dtmf`. The bridge crate stamps `call_id` and `seq`
//! and writes the JSON `dtmf` text frame on the WS.
//!
//! ## Not yet wired
//!
//! - VAD `speech_started` / `speech_stopped`. Forge has the detector
//!   surface; we just don't have a `ForgeEvent` variant for it yet.

use std::sync::Arc;

use forge_core::{CallId, DtmfDetectionMethod, DtmfEventKind, EventBus, ForgeError, ForgeEvent};
use forge_dtmf::DtmfDigit;
use forge_engine::{
    MediaBridgeHandle, MediaBridgeManager, MediaTarget, OutboundDtmfRequest, OutboundMediaFrame,
    PlayoutMode,
};
use siphon_ai_bridge::{
    pack_pcm16_le, unpack_pcm16_le, AudioError, DtmfMethod, OutgoingEvent, Reframer,
};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
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

/// Out-of-band commands the controller can drive the tap with —
/// distinct from the audio path because they're rare control-plane
/// events, not per-frame data.
///
/// Each variant maps to a forge `MediaBridgeHandle` action. The tap
/// translates the WS server's intent (`BridgeIn::SendDtmf`, future
/// `Clear`, future `Mark`) into the right forge call.
///
/// New variants are additive — older controllers / tests building
/// only `SendDtmf` keep working without any match-arm churn here.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TapCommand {
    /// Server-driven outbound DTMF press. The tap turns it into an
    /// [`OutboundDtmfRequest`] targeting [`MediaTarget::A`] (the SIP
    /// caller); forge's encoder writes the RFC 2833 packets onto the
    /// session's RTP socket.
    ///
    /// Invalid `digit` chars (anything outside `0-9 * # A-D`) are
    /// dropped with a warning rather than tearing the call down — a
    /// misbehaving WS server shouldn't kill a call.
    SendDtmf { digit: char, duration_ms: u32 },

    /// Drop every byte of pending outbound playout — both the bytes
    /// queued in the controller→tap audio channel that haven't yet
    /// reached forge, and forge's own encoder queue. Drives the WS
    /// protocol's `Clear` message: the server's typical use is
    /// "stop talking immediately" in response to caller barge-in,
    /// so any tail audio left in the pipe must not reach the
    /// caller.
    Clear,
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
    /// Subscription to the daemon-wide forge [`EventBus`]. The tap
    /// filters for events matching its own `call_id`. Subscribing at
    /// `attach` time (rather than inside `run`) means events emitted
    /// between attach and run are still buffered for us — the
    /// broadcast channel's per-receiver queue holds up to the bus's
    /// configured capacity.
    events_rx: broadcast::Receiver<ForgeEvent>,
    /// Held alive across calls to [`Self::run`] so that, if the
    /// daemon-wide bus's `Sender` is dropped (test fixtures that don't
    /// keep the `Arc<EventBus>` alive, or a future shutdown sequence),
    /// the swapped-in fallback receiver doesn't immediately re-close.
    /// Without this anchor the `events_rx` `select!` arm would fire
    /// `Closed` every poll under `biased;`, starving the other arms.
    /// `None` while the original bus is still feeding us; `Some`
    /// after we've swapped in a fallback channel.
    events_keepalive: Option<broadcast::Sender<ForgeEvent>>,
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
    /// `MediaBridgeManager` and subscribe to the supplied `EventBus`
    /// (the same bus forge's session manager publishes on).
    ///
    /// Fails with [`MediaTapError::AttachFailed`] if a tap is already
    /// attached for the same call (forge enforces 1 handle per call).
    /// Also fails on an unsupported `sample_rate` per the bridge
    /// audio module's rules (8 kHz or 16 kHz only in v1).
    pub fn attach(
        manager: &Arc<MediaBridgeManager>,
        event_bus: &Arc<EventBus>,
        call_id: CallId,
        sample_rate: u32,
    ) -> Result<Self, MediaTapError> {
        // Validate sample rate up front so we don't attach a handle
        // we'd immediately have to detach.
        let _ = siphon_ai_bridge::samples_per_frame(sample_rate)?;
        let handle = manager
            .attach_call(call_id.clone())
            .map_err(forge_attach_err)?;
        let events_rx = event_bus.subscribe();
        Ok(Self {
            handle,
            sample_rate,
            call_id,
            events_rx,
            events_keepalive: None,
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
    /// - `events_tx`: out-of-band [`OutgoingEvent`]s the tap derives
    ///   from forge's event bus (currently only DTMF). The bridge
    ///   crate stamps `call_id` + `seq` and writes the JSON text
    ///   frame on the WS.
    /// - `commands_rx`: control-plane requests from the controller
    ///   (currently [`TapCommand::SendDtmf`]). The tap translates
    ///   each into the right forge `MediaBridgeHandle` call.
    #[instrument(skip_all, fields(call_id = %self.call_id, sample_rate = self.sample_rate))]
    pub async fn run(
        mut self,
        caller_audio_tx: mpsc::Sender<Vec<u8>>,
        mut playout_audio_rx: mpsc::Receiver<Vec<u8>>,
        events_tx: mpsc::Sender<OutgoingEvent>,
        mut commands_rx: mpsc::Receiver<TapCommand>,
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

                // Forge events (currently only DTMF). Filter to this
                // call_id; map End-of-press to a single OutgoingEvent.
                event = self.events_rx.recv() => {
                    match event {
                        Ok(ev) => {
                            if let Some(out) = derive_outgoing_event(&self.call_id, ev) {
                                if events_tx.send(out).await.is_err() {
                                    debug!("events_tx closed; ending tap");
                                    return Ok(TapDisconnect::ControllerHungUp);
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // The broadcast queue overflowed for this
                            // subscriber. Per CLAUDE.md §4.5 we never
                            // panic on the audio path; just log and
                            // resume — events from the lag window are
                            // best-effort by definition.
                            warn!(skipped = n, "tap event subscriber lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            debug!("forge event bus closed; tap continues without events");
                            // Without holding a keepalive sender, the
                            // fresh receiver would also be Closed, the
                            // arm would fire on every poll, and under
                            // `biased;` the cmd / audio arms would be
                            // starved. Stash the sender on `self` so
                            // it lives as long as the tap.
                            let (dummy_tx, dummy_rx) = broadcast::channel(1);
                            self.events_keepalive = Some(dummy_tx);
                            self.events_rx = dummy_rx;
                        }
                    }
                }

                // Controller-driven commands (currently only SendDtmf).
                cmd = commands_rx.recv() => {
                    let Some(cmd) = cmd else {
                        // Controller dropped its sender. The audio
                        // arms still drive the call; treat command
                        // closure as a non-fatal "no more commands"
                        // and stop polling this arm by recreating the
                        // receiver against a dropped sender.
                        debug!("commands_rx closed; tap continues without commands");
                        let (_drop_tx, replacement) = mpsc::channel::<TapCommand>(1);
                        commands_rx = replacement;
                        continue;
                    };
                    match cmd {
                        TapCommand::Clear => {
                            // Drain any bytes queued in the
                            // controller→tap audio channel before
                            // they can reach forge. The mpsc bound
                            // is small (10 frames = 200 ms) but
                            // letting that tail slip through a
                            // barge-in event would be perceptible.
                            let mut drained = 0usize;
                            while let Ok(_bytes) = playout_audio_rx.try_recv() {
                                drained += 1;
                            }
                            if let Err(e) = self
                                .handle
                                .flush(Some(MediaTarget::A), None)
                                .await
                            {
                                warn!(
                                    call_id = %self.call_id,
                                    error = %e,
                                    "forge flush failed during Clear",
                                );
                            } else {
                                debug!(
                                    call_id = %self.call_id,
                                    drained,
                                    "cleared pending outbound playout",
                                );
                            }
                        }
                        TapCommand::SendDtmf { digit, duration_ms } => {
                            handle_send_dtmf(&self.call_id, &self.handle, digit, duration_ms).await;
                        }
                    }
                }
            }
        }
    }
}

async fn handle_send_dtmf(
    call_id: &CallId,
    handle: &MediaBridgeHandle,
    digit: char,
    duration_ms: u32,
) {
    let parsed = match DtmfDigit::from_char(digit) {
        Ok(d) => d,
        Err(e) => {
            warn!(
                call_id = %call_id,
                digit = %digit, error = %e,
                "ignoring SendDtmf with unsupported digit char",
            );
            return;
        }
    };
    let req = OutboundDtmfRequest {
        target: MediaTarget::A,
        digit: parsed,
        duration_ms,
        playback_id: None,
        mode: PlayoutMode::Append,
    };
    if let Err(e) = handle.send_dtmf(req).await {
        warn!(call_id = %call_id, error = %e, "forge send_dtmf failed");
    } else {
        debug!(call_id = %call_id, digit = %digit, duration_ms, "queued outbound DTMF");
    }
}

/// Map a forge `ForgeEvent` to an `OutgoingEvent` the bridge can ship.
///
/// Returns `None` for events that aren't this call's, that aren't part
/// of the v1 WS protocol surface, or that don't carry the final
/// duration we need (DTMF `Start`/`Continue`).
fn derive_outgoing_event(call_id: &CallId, event: ForgeEvent) -> Option<OutgoingEvent> {
    match event {
        ForgeEvent::DtmfDigitDetected {
            call_id: ev_call,
            digit,
            duration_ms,
            method,
            event_type: DtmfEventKind::End,
            ..
        } if &ev_call == call_id => Some(OutgoingEvent::Dtmf {
            digit,
            // RFC 2833 detection always reports duration, but the
            // ForgeEvent type is Option to model SIP INFO. Fall back
            // to 0 — the bridge protocol's `duration_ms` is u32 and
            // a missing duration on End is forge giving up; we still
            // emit the press so the WS server isn't left guessing.
            duration_ms: duration_ms.unwrap_or(0),
            method: map_dtmf_method(method),
        }),
        _ => None,
    }
}

fn map_dtmf_method(method: DtmfDetectionMethod) -> DtmfMethod {
    match method {
        DtmfDetectionMethod::Rfc2833 => DtmfMethod::Rfc2833,
        DtmfDetectionMethod::Inband => DtmfMethod::Inband,
        // SIP INFO DTMF rides on the SIP layer (not RTP), so the
        // bridge's RTP-derived `method` enum has no slot for it. v1's
        // SIP layer doesn't surface INFO DTMF anyway; map it to
        // Inband so a future change here doesn't break wire shape if
        // forge ever publishes one.
        DtmfDetectionMethod::SipInfo => DtmfMethod::Inband,
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
        let err = MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), CallId::new("c"), 48000).unwrap_err();
        assert!(matches!(err, MediaTapError::Audio(_)));
    }

    #[test]
    fn attach_succeeds_at_supported_rate_and_drops_clean() {
        let manager = Arc::new(MediaBridgeManager::new());
        let call = CallId::new("c");
        {
            let tap = MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), call.clone(), 8000).expect("attach");
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
        let _t1 = MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), call.clone(), 8000).expect("first attach");
        let err = MediaTap::attach(&manager, &::std::sync::Arc::new(forge_core::EventBus::new()), call, 8000).unwrap_err();
        assert!(matches!(err, MediaTapError::AttachFailed(_)));
    }
}
