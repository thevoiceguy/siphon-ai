//! The browser's media leg: answer, connect, tear down.
//!
//! Wraps forge-webrtc's `PeerConnection` in the shape siphon-ai's call
//! machinery wants — an offer goes in, an answer comes out, and the
//! caller learns when media may flow (`DEV_PLAN_WebRTC.md` §4.2).
//!
//! # Complete gathering before the answer
//!
//! v1 does **no SIP trickle** (§4.2). Instead both ends gather fully
//! before signalling: SIP.js waits for its own gathering before
//! sending the INVITE — which is why the captured Chrome offer already
//! carries its candidates — and [`WebRtcLeg::answer`] waits for
//! `GatheringComplete` before building the answer, so the SDP the
//! browser receives is final. On a host-candidate-only server that is
//! milliseconds; with STUN it is one round trip, which is why the wait
//! is bounded rather than unconditional: a STUN server that black-holes
//! must not stall a call, and the host candidates already gathered are
//! a perfectly good answer.

use std::time::Duration;

use forge_core::AudioCodec;
use forge_webrtc::{ConnectionState, PeerConnection, PeerEvent};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::{Result, WebRtcGlueError, WebRtcSettings};

/// How long [`WebRtcLeg::answer`] waits for candidate gathering before
/// answering with what it has. Host candidates are immediate; this
/// budget exists for the STUN round trip, and expiring is a
/// degradation (fewer candidates), not a failure.
pub const GATHER_BUDGET: Duration = Duration::from_secs(2);

/// A leg that has answered and is establishing ICE/DTLS.
pub struct WebRtcLeg {
    peer: PeerConnection,
    codec: AudioCodec,
    payload_type: u8,
}

/// What [`WebRtcLeg::answer`] produces: the leg, the SDP to put in the
/// 200 OK, and the peer's event stream (candidates already gathered,
/// so what remains is state changes and media).
pub struct Answered {
    pub leg: WebRtcLeg,
    pub answer_sdp: String,
    pub events: mpsc::Receiver<PeerEvent>,
}

impl WebRtcLeg {
    /// Answer a browser's offer.
    ///
    /// Applies the remote description, waits (bounded) for local
    /// gathering, and produces the final answer SDP.
    /// `local_port` is where the media socket binds — the RTP port of
    /// a `PortReservation` drawn from the same pool a SIP call uses, or
    /// `0` to let the OS choose (tests, and only tests).
    pub async fn answer(
        offer_sdp: &str,
        settings: &WebRtcSettings,
        local_port: u16,
    ) -> Result<Answered> {
        Self::answer_with_gather_budget(offer_sdp, settings, local_port, GATHER_BUDGET).await
    }

    /// [`answer`](Self::answer) with an explicit gather budget — tests
    /// use a short one.
    pub async fn answer_with_gather_budget(
        offer_sdp: &str,
        settings: &WebRtcSettings,
        local_port: u16,
        gather_budget: Duration,
    ) -> Result<Answered> {
        let mut peer = PeerConnection::with_config(settings.peer_config_on_port(local_port))
            .await
            .map_err(|e| WebRtcGlueError::Setup(e.to_string()))?;

        // Applying the offer is what starts the transport (and so
        // gathering), so the events receiver must be taken after it.
        peer.set_remote_offer(offer_sdp)
            .await
            .map_err(|e| WebRtcGlueError::Offer(e.to_string()))?;
        let mut events = peer
            .take_events()
            .ok_or_else(|| WebRtcGlueError::Setup("peer event stream already taken".into()))?;

        // Drain candidate events until gathering finishes or the budget
        // expires. Candidates are inlined into the answer by
        // forge-webrtc, so nothing is lost by consuming the events —
        // and consuming them keeps the bounded channel from filling
        // while we wait.
        let gathered = wait_for_gathering(&mut events, gather_budget).await;
        if !gathered {
            warn!(
                budget_ms = gather_budget.as_millis(),
                "ICE gathering did not complete in time; answering with the \
                 candidates gathered so far"
            );
        }

        let answer_sdp = peer
            .create_answer()
            .await
            .map_err(|e| WebRtcGlueError::Answer(e.to_string()))?;
        let (codec, payload_type) = peer.negotiated_codec();
        debug!(
            connection_id = peer.connection_id(),
            ?codec,
            payload_type,
            gathered,
            "WebRTC leg answered"
        );

        Ok(Answered {
            leg: WebRtcLeg {
                peer,
                codec,
                payload_type,
            },
            answer_sdp,
            events,
        })
    }

    /// Wait for ICE + DTLS. This is the `[webrtc].setup_timeout`
    /// budget: a browser that signalled but never completes media must
    /// not hold a call slot (§4.4).
    pub async fn wait_connected(&self, timeout: Duration) -> Result<()> {
        self.peer
            .wait_connected(timeout)
            .await
            .map_err(|e| WebRtcGlueError::Connect(e.to_string()))
    }

    /// The negotiated codec and the payload type the browser expects.
    pub fn codec(&self) -> (AudioCodec, u8) {
        (self.codec, self.payload_type)
    }

    /// The PCM rate the WS bridge will see for this leg.
    ///
    /// **Not** the codec's RTP clock. Opus signals a 48 kHz clock on
    /// the wire but libopus decodes straight to the rate we ask for,
    /// so the bridge sees 16 kHz — exactly what a classic Opus call
    /// already delivers (`media-glue::sdp::Codec::audio_sample_rate`).
    /// Keeping the two paths on the same number is what lets a browser
    /// leg reuse the bridge's fixed 8/16 kHz frame contract untouched.
    pub fn bridge_sample_rate(&self) -> u32 {
        match self.codec {
            AudioCodec::Opus => 16_000,
            _ => 8_000,
        }
    }

    /// Current transport state.
    pub fn state(&self) -> ConnectionState {
        self.peer.get_state()
    }

    /// Stable id for logs/metrics.
    pub fn connection_id(&self) -> &str {
        self.peer.connection_id()
    }

    /// Borrow the peer connection (audio handles, renegotiation).
    pub fn peer(&self) -> &PeerConnection {
        &self.peer
    }

    /// Mutable borrow, for re-offer / rollback (hold, §4.2).
    pub fn peer_mut(&mut self) -> &mut PeerConnection {
        &mut self.peer
    }

    /// Close the transport. Idempotent.
    pub fn close(&mut self) {
        self.peer.close();
    }
}

/// Consume events until `GatheringComplete`. Returns whether it
/// arrived within the budget.
async fn wait_for_gathering(events: &mut mpsc::Receiver<PeerEvent>, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(PeerEvent::GatheringComplete)) => return true,
            // Candidates are inlined into the answer by forge-webrtc;
            // draining them here just keeps the channel clear.
            Ok(Some(_)) => continue,
            // Transport gone before we answered — let the answer
            // attempt produce the real error.
            Ok(None) => return false,
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHROME_OFFER: &str = include_str!("../fixtures/chrome-offer.sdp");

    #[tokio::test]
    async fn answers_the_real_chrome_offer_with_gathered_candidates() {
        let Answered {
            leg, answer_sdp, ..
        } = WebRtcLeg::answer(CHROME_OFFER, &WebRtcSettings::default(), 0)
            .await
            .expect("answer");

        assert!(answer_sdp.contains("a=setup:active"), "{answer_sdp}");
        assert!(
            answer_sdp.contains("a=candidate:"),
            "complete-gathering-before-answer means the answer carries \
             our candidates: {answer_sdp}"
        );
        // Opus by default → the bridge sees 16 kHz, same as a classic
        // Opus call.
        assert_eq!(leg.codec().0, AudioCodec::Opus);
        assert_eq!(leg.bridge_sample_rate(), 16_000);
    }

    #[tokio::test]
    async fn g711_leg_reports_the_8k_bridge_rate() {
        let settings = WebRtcSettings {
            prefer_g711: true,
            ..Default::default()
        };
        let Answered { leg, .. } = WebRtcLeg::answer(CHROME_OFFER, &settings, 0)
            .await
            .expect("answer");
        assert_eq!(leg.codec(), (AudioCodec::PCMU, 0));
        assert_eq!(leg.bridge_sample_rate(), 8_000);
    }

    /// A gather budget that expires must still produce an answer —
    /// degraded (fewer candidates), never a failed call.
    #[tokio::test]
    async fn expired_gather_budget_still_answers() {
        let Answered { answer_sdp, .. } = WebRtcLeg::answer_with_gather_budget(
            CHROME_OFFER,
            &WebRtcSettings::default(),
            0,
            Duration::from_millis(0),
        )
        .await
        .expect("answer even when gathering did not finish");
        assert!(answer_sdp.contains("a=ice-ufrag:"), "{answer_sdp}");
    }

    #[tokio::test]
    async fn a_non_webrtc_offer_is_refused() {
        let plain = "v=0\r\no=- 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\n\
t=0 0\r\nm=audio 9000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        let err = match WebRtcLeg::answer(plain, &WebRtcSettings::default(), 0).await {
            Err(e) => e,
            Ok(_) => panic!("a plain RTP offer has no ICE/fingerprint to build a leg from"),
        };
        assert!(matches!(err, WebRtcGlueError::Offer(_)), "{err:?}");
    }
}
