//! The browser leg, in the shape the controller already speaks.
//!
//! [`WebRtcTap`] is to a `PeerConnection` what `MediaTap` is to a forge
//! RTP session: it owns the media, exchanges 20 ms PCM16LE frames over
//! the controller's four channels, and honours the [`TapCommand`]s that
//! mean something for a browser (`DEV_PLAN_WebRTC.md` §4.3).
//!
//! # What a browser leg cannot do, and why that is fine
//!
//! Several controller features are shaped around a *SIP* peer with a
//! forge session behind it. Rather than pretend, each is refused
//! explicitly and the controller's existing failure path reports it to
//! the WS server:
//!
//! - **DTMF out** — RFC 2833 into a browser is meaningless; browsers
//!   surface DTMF as data, not audio. Counted and dropped.
//! - **Hold / Park** — both are SIP re-INVITE dances on the classic
//!   path. `Hold` answers its `accepted` oneshot with `false`, which
//!   is exactly the "refused, tell the server `hold_failed`" path the
//!   conference-hold conflict already uses (#403), so no new protocol
//!   surface is needed.
//! - **Conference rooms** — the room mixer is fed by the classic tap's
//!   frame loop; joining is refused for now.
//!
//! Everything that is really about *bytes to the caller* — `Clear`,
//! `Mute`/`Unmute`, `Mark`, barge-in verdicts — works, because those
//! operate on this leg's own playout, not on forge.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use siphon_ai_media_glue::{MediaTapError, TapCommand, TapDisconnect};
use siphon_ai_recording::RecFrame;
use siphon_ai_webrtc_glue::{InboundAudio, OutboundAudio, WebRtcLeg};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use siphon_ai_bridge::OutgoingEvent;

/// How often the jitter buffer is drained. The buffer releases a
/// packet only once it has been held for its target depth, so this
/// tick is what turns arrival jitter into the steady 20 ms cadence the
/// bridge expects — the same role the classic tap's pacing tick plays.
const DRAIN_TICK: Duration = Duration::from_millis(20);

/// A browser call's media, driven like a tap.
pub struct WebRtcTap {
    leg: WebRtcLeg,
    events: mpsc::Receiver<siphon_ai_webrtc_glue::PeerEvent>,
    /// `[webrtc].setup_timeout` — ICE + DTLS must complete inside it or
    /// the call gives the slot back (§4.4).
    setup_timeout: Duration,
    /// Set while the answered direction forbids our send (#417).
    tx_suppressed: Option<Arc<AtomicBool>>,
    /// Recording fork, if the call is being recorded.
    recording: Option<(
        mpsc::Sender<RecFrame>,
        std::sync::Arc<std::sync::atomic::AtomicU64>,
    )>,
}

impl WebRtcTap {
    pub fn new(
        leg: WebRtcLeg,
        events: mpsc::Receiver<siphon_ai_webrtc_glue::PeerEvent>,
        setup_timeout: Duration,
    ) -> Self {
        Self {
            leg,
            events,
            setup_timeout,
            tx_suppressed: None,
            recording: None,
        }
    }

    /// The PCM rate the bridge sees — 16 kHz for Opus, 8 kHz for
    /// G.711, matching what a classic call with the same codec gives.
    pub fn sample_rate(&self) -> u32 {
        self.leg.bridge_sample_rate()
    }

    /// Wrap in the boxed enum variant the controller stores.
    pub fn into_leg(self) -> crate::media_leg::MediaLeg {
        crate::media_leg::MediaLeg::WebRtc(Box::new(self))
    }

    pub fn with_tx_suppressed(mut self, flag: Arc<AtomicBool>) -> Self {
        self.tx_suppressed = Some(flag);
        self
    }

    pub fn with_recording(
        mut self,
        fork: Option<(
            mpsc::Sender<RecFrame>,
            std::sync::Arc<std::sync::atomic::AtomicU64>,
        )>,
    ) -> Self {
        self.recording = fork;
        self
    }

    /// Drive the leg until the browser goes away or the controller
    /// does.
    pub async fn run(
        mut self,
        caller_audio_tx: mpsc::Sender<Vec<u8>>,
        mut playout_audio_rx: mpsc::Receiver<Vec<u8>>,
        events_tx: mpsc::Sender<OutgoingEvent>,
        mut cmd_rx: mpsc::Receiver<TapCommand>,
    ) -> Result<TapDisconnect, MediaTapError> {
        let _ = &events_tx;

        // ICE + DTLS. Until this completes there is no SRTP context, so
        // there is nothing to send and nothing will arrive.
        if let Err(e) = self.leg.wait_connected(self.setup_timeout).await {
            warn!(
                connection_id = self.leg.connection_id(),
                error = %e,
                timeout_secs = self.setup_timeout.as_secs(),
                "browser media never connected; giving the call slot back"
            );
            self.leg.close();
            // The far end signalled but never completed media — the
            // same shape as a stream that stalls, and the same cause
            // the controller already knows how to report.
            return Ok(TapDisconnect::InactivityTimeout);
        }

        let (codec, payload_type) = self.leg.codec();
        let bridge_rate = self.leg.bridge_sample_rate();
        info!(
            connection_id = self.leg.connection_id(),
            ?codec,
            payload_type,
            bridge_rate,
            "browser media connected"
        );

        let mut inbound = InboundAudio::new(codec, payload_type, bridge_rate)
            .map_err(|e| MediaTapError::AttachFailed(e.to_string()))?;
        let sender = self
            .leg
            .peer()
            .sender()
            .map_err(|e| MediaTapError::AttachFailed(e.to_string()))?;
        let mut outbound = OutboundAudio::new(codec, sender, bridge_rate)
            .map_err(|e| MediaTapError::AttachFailed(e.to_string()))?;

        let mut muted = false;
        let mut ticker = tokio::time::interval(DRAIN_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let disconnect = loop {
            tokio::select! {
                // Inbound SRTP → jitter buffer.
                event = self.events.recv() => {
                    match event {
                        Some(siphon_ai_webrtc_glue::PeerEvent::Rtp(pkt)) => inbound.push(&pkt),
                        Some(siphon_ai_webrtc_glue::PeerEvent::Failed(why)) => {
                            warn!(connection_id = self.leg.connection_id(), %why,
                                  "browser media transport failed");
                            break TapDisconnect::CallEnded;
                        }
                        Some(siphon_ai_webrtc_glue::PeerEvent::Closed) | None => {
                            break TapDisconnect::CallEnded;
                        }
                        // RTCP and late ICE/DTLS notices: nothing to do
                        // here, forge-webrtc has already acted on them.
                        Some(_) => {}
                    }
                }

                // Steady cadence out to the WS bridge.
                _ = ticker.tick() => {
                    for frame in inbound.drain() {
                        if let Some((rec, _)) = &self.recording {
                            let _ = rec.try_send(RecFrame::Caller(frame.clone()));
                        }
                        if caller_audio_tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                }

                // Bridge → browser.
                playout = playout_audio_rx.recv() => {
                    let Some(frame) = playout else {
                        break TapDisconnect::ControllerHungUp;
                    };
                    let suppressed = self
                        .tx_suppressed
                        .as_ref()
                        .is_some_and(|f| f.load(Ordering::Relaxed));
                    if muted || suppressed {
                        continue;
                    }
                    if let Some((rec, _)) = &self.recording {
                        let _ = rec.try_send(RecFrame::Bot(frame.clone()));
                    }
                    if let Err(e) = outbound.send_frame(&frame).await {
                        warn!(connection_id = self.leg.connection_id(), error = %e,
                              "browser media send failed");
                        break TapDisconnect::CallEnded;
                    }
                }

                // Controller commands.
                cmd = cmd_rx.recv() => {
                    let Some(cmd) = cmd else {
                        break TapDisconnect::ControllerHungUp;
                    };
                    match cmd {
                        TapCommand::Mute => muted = true,
                        TapCommand::Unmute => muted = false,
                        // Everything queued for the browser is already
                        // on the wire — this leg holds no playout queue
                        // of its own — so a flush is what the caller
                        // stops hearing next, which is immediate.
                        TapCommand::Clear => {
                            while playout_audio_rx.try_recv().is_ok() {}
                        }
                        // Hold and Park are SIP re-INVITE dances. Refuse
                        // rather than half-do them; the controller turns
                        // this into `hold_failed` for the WS server via
                        // the same path a conference conflict uses.
                        TapCommand::Hold { accepted, .. } => {
                            let _ = accepted.send(false);
                            debug!("hold refused: not supported on a browser leg");
                        }
                        TapCommand::SendDtmf { digit, .. } => {
                            debug!(%digit, "outbound DTMF dropped: not supported on a browser leg");
                        }
                        TapCommand::JoinRoom { .. } => {
                            debug!("conference join refused: not supported on a browser leg");
                        }
                        // Unhold / LeaveRoom / Unpark are the inverse of
                        // operations we never accepted, so they are
                        // no-ops rather than errors.
                        _other => {
                            debug!("command not applicable to a browser leg");
                        }
                    }
                }
            }
        };

        self.leg.close();
        info!(
            connection_id = self.leg.connection_id(),
            ?disconnect,
            decode_errors = inbound.decode_errors,
            other_payload_packets = inbound.other_payload_packets,
            encode_errors = outbound.encode_errors,
            "browser media leg ended"
        );
        Ok(disconnect)
    }
}
