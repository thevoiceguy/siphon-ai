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
use siphon_ai_telemetry::{
    WEBRTC_DTLS_SECONDS, WEBRTC_ICE_SECONDS, WEBRTC_LEGS_ENDED_TOTAL, WEBRTC_LEGS_TOTAL,
    WEBRTC_TRANSCODE_SECONDS,
};
use siphon_ai_webrtc_glue::{InboundAudio, OutboundAudio, SetupOutcome, WebRtcLeg};
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
    /// Tear the call down after this long with no inbound RTP.
    ///
    /// A browser leg needs this at least as much as a SIP one: when a
    /// tab closes the page is gone with no BYE and no FIN we can see —
    /// ICE consent freshness is the only other signal, and nothing
    /// guarantees it fires promptly. Without this a vanished browser
    /// holds a call slot indefinitely, exactly the leak Phase 0 of the
    /// plan exists to prevent. `None` disables it, matching
    /// `[media].inactivity_timeout_secs = 0`.
    inactivity_timeout: Option<Duration>,
    /// The RTP port pair this leg occupies.
    ///
    /// A WebRTC leg binds one socket where a classic leg binds two, but
    /// it fills one call slot either way, so it holds a whole pair —
    /// see `siphon_ai_media_glue::PortReservation` for why the pool,
    /// not just the socket, is the thing that matters. Held rather than
    /// used: dropping the tap releases it, which is what makes the
    /// capacity gauge return to zero after a browser call without any
    /// teardown path having to remember.
    _ports: Option<siphon_ai_media_glue::PortReservation>,
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
            inactivity_timeout: None,
            _ports: None,
        }
    }

    /// Hold an RTP port pair for the life of this leg (§4.4).
    pub fn with_port_reservation(mut self, ports: siphon_ai_media_glue::PortReservation) -> Self {
        self._ports = Some(ports);
        self
    }

    /// Tear down after this long with no inbound RTP (see the field).
    pub fn with_inactivity_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.inactivity_timeout = timeout;
        self
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
        let outcome = self
            .leg
            .wait_for_setup(&mut self.events, self.setup_timeout)
            .await;
        let (codec, payload_type) = self.leg.codec();
        let codec_label = codec_label(codec);
        metrics::counter!(
            WEBRTC_LEGS_TOTAL,
            "codec" => codec_label,
            "result" => outcome.label(),
        )
        .increment(1);
        match &outcome {
            SetupOutcome::Connected { ice, dtls } => {
                if let Some(ice) = ice {
                    metrics::histogram!(WEBRTC_ICE_SECONDS).record(ice.as_secs_f64());
                }
                if let Some(dtls) = dtls {
                    metrics::histogram!(WEBRTC_DTLS_SECONDS).record(dtls.as_secs_f64());
                }
            }
            not_connected => {
                warn!(
                    connection_id = self.leg.connection_id(),
                    outcome = not_connected.label(),
                    detail = ?not_connected,
                    timeout_secs = self.setup_timeout.as_secs(),
                    "browser media never connected; giving the call slot back"
                );
                self.leg.close();
                // The far end signalled but never completed media — the
                // same shape as a stream that stalls, and the same cause
                // the controller already knows how to report.
                return Ok(TapDisconnect::InactivityTimeout);
            }
        }

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
        // Frames actually exchanged with the bridge. The error counters
        // alone cannot distinguish "clean call" from "no media at all",
        // which is the first question anyone asks of a silent call.
        let (mut frames_to_bridge, mut frames_to_browser) = (0u64, 0u64);
        let mut last_rtp = tokio::time::Instant::now();
        let mut ticker = tokio::time::interval(DRAIN_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Both halves of the end: what the controller is told, and
        // the bounded label the operator sees. They are not the same
        // thing — `CallEnded` covers a browser that closed cleanly and
        // a transport that failed, and only the second is a fault.
        let (disconnect, end_reason) = loop {
            tokio::select! {
                // Inbound SRTP → jitter buffer.
                event = self.events.recv() => {
                    match event {
                        Some(siphon_ai_webrtc_glue::PeerEvent::Rtp(pkt)) => {
                            last_rtp = tokio::time::Instant::now();
                            inbound.push(&pkt);
                        }
                        Some(siphon_ai_webrtc_glue::PeerEvent::Failed(why)) => {
                            warn!(connection_id = self.leg.connection_id(), %why,
                                  "browser media transport failed");
                            break (TapDisconnect::CallEnded, "transport_failed");
                        }
                        Some(siphon_ai_webrtc_glue::PeerEvent::Closed) | None => {
                            break (TapDisconnect::CallEnded, "peer_closed");
                        }
                        // RTCP and late ICE/DTLS notices: nothing to do
                        // here, forge-webrtc has already acted on them.
                        Some(_) => {}
                    }
                }

                // Steady cadence out to the WS bridge.
                _ = ticker.tick() => {
                    // Nothing from the browser for the whole window:
                    // the tab is gone. Give the slot back.
                    if let Some(limit) = self.inactivity_timeout {
                        if last_rtp.elapsed() >= limit {
                            warn!(
                                connection_id = self.leg.connection_id(),
                                timeout_secs = limit.as_secs(),
                                "no inbound media from the browser; tearing down"
                            );
                            break (TapDisconnect::InactivityTimeout, "inactivity");
                        }
                    }
                    for frame in inbound.drain() {
                        if let Some((rec, _)) = &self.recording {
                            let _ = rec.try_send(RecFrame::Caller(frame.clone()));
                        }
                        if caller_audio_tx.send(frame).await.is_err() {
                            break;
                        }
                        frames_to_bridge += 1;
                    }
                }

                // Bridge → browser.
                playout = playout_audio_rx.recv() => {
                    let Some(frame) = playout else {
                        break (TapDisconnect::ControllerHungUp, "controller");
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
                    frames_to_browser += 1;
                    if let Err(e) = outbound.send_frame(&frame).await {
                        warn!(connection_id = self.leg.connection_id(), error = %e,
                              "browser media send failed");
                        break (TapDisconnect::CallEnded, "send_failed");
                    }
                }

                // Controller commands.
                cmd = cmd_rx.recv() => {
                    let Some(cmd) = cmd else {
                        break (TapDisconnect::ControllerHungUp, "controller");
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
        metrics::counter!(WEBRTC_LEGS_ENDED_TOTAL, "reason" => end_reason).increment(1);
        // Codec time for the whole leg, recorded once here rather than
        // per frame: 50 histogram observations a second per call would
        // cost more than the work being measured (CLAUDE.md §4.3).
        let decode_secs = Duration::from_nanos(inbound.decode_nanos).as_secs_f64();
        let encode_secs = Duration::from_nanos(outbound.encode_nanos).as_secs_f64();
        metrics::histogram!(WEBRTC_TRANSCODE_SECONDS, "direction" => "decode").record(decode_secs);
        metrics::histogram!(WEBRTC_TRANSCODE_SECONDS, "direction" => "encode").record(encode_secs);
        info!(
            connection_id = self.leg.connection_id(),
            ?disconnect,
            end_reason,
            frames_to_bridge,
            frames_to_browser,
            decode_errors = inbound.decode_errors,
            other_payload_packets = inbound.other_payload_packets,
            encode_errors = outbound.encode_errors,
            decode_secs,
            encode_secs,
            "browser media leg ended"
        );
        Ok(disconnect)
    }
}

/// Bounded `codec` label for the WebRTC leg metrics. `other` exists
/// because the negotiated codec comes from forge-core's enum, which
/// carries more variants than a browser leg can ever agree on — an
/// unbounded label from an upstream enum is how metric cardinality
/// escapes (CLAUDE.md §4.5).
fn codec_label(codec: forge_core::AudioCodec) -> &'static str {
    match codec {
        forge_core::AudioCodec::Opus => "opus",
        forge_core::AudioCodec::PCMU => "pcmu",
        forge_core::AudioCodec::PCMA => "pcma",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::AudioCodec;

    #[test]
    fn codec_labels_are_bounded_and_lowercase() {
        assert_eq!(codec_label(AudioCodec::Opus), "opus");
        assert_eq!(codec_label(AudioCodec::PCMU), "pcmu");
        assert_eq!(codec_label(AudioCodec::PCMA), "pcma");
        // Anything a browser cannot negotiate collapses to one label
        // rather than adding a series per upstream enum variant.
        assert_eq!(codec_label(AudioCodec::G722), "other");
    }
}
